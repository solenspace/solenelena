//! Elena tri-tenant fire-test.
//!
//! Drives Solen + Hannlys + Omnii against an in-process Elena (with
//! testcontainer Postgres + Redis + NATS, wiremock LLM, embedded
//! plugins) under sustained concurrency, then asserts isolation and
//! reliability invariants.
//!
//! Designed to surface bugs that only appear when three apps with
//! very different workloads collide on the same runtime: cross-tenant
//! credential leak, plan-resolution races, NATS fanout interleaving,
//! audit-channel saturation, rate-limit isolation, and broadcast
//! channel lifecycle.
//!
//! Exits 0 only when every invariant in [`assertions::check_all`]
//! holds. Run config + per-tenant latency + recommended fixes are
//! written to `/tmp/elena-firetest-report.md`.

#![allow(deprecated)] // B1.6 deprecation soak
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::too_many_lines,
    clippy::too_many_arguments,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::doc_markdown,
    clippy::missing_errors_doc,
    clippy::module_name_repetitions,
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::format_push_string,
    clippy::manual_let_else,
    clippy::struct_field_names,
    clippy::needless_pass_by_value,
    clippy::useless_conversion,
    clippy::unused_self,
    unreachable_pub
)]

mod assertions;
mod harness;
mod report;
mod video_mock;
mod workload;

use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use tracing::info;

#[derive(Debug, Clone, Parser)]
#[command(version, about = "Elena tri-tenant fire-test (Solen + Hannlys + Omnii)")]
struct Args {
    /// Concurrent thread-loops per tenant. Total = 3 × this. Default
    /// 50 keeps the macOS ephemeral-port range (~16k) under the
    /// pressure cap; pushing past 100 reliably exhausts the local
    /// TIME_WAIT pool and the post-run cleanup DELETE can't dial back
    /// to the in-process gateway. Override with explicit value if
    /// you've raised `net.inet.ip.portrange.first` (macOS) or
    /// `net.ipv4.ip_local_port_range` (Linux).
    #[arg(long, default_value_t = 50)]
    threads_per_tenant: usize,

    /// How long the saturation phase runs, in seconds.
    #[arg(long, default_value_t = 90)]
    duration_secs: u64,

    /// Where to write the markdown report.
    #[arg(long, default_value = "/tmp/elena-firetest-report.md")]
    report_path: String,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(
            |_| {
                tracing_subscriber::EnvFilter::new(
                    "warn,elena_tri_tenant_firetest=info,elena_gateway=warn,\
                     elena_worker=warn,elena_admin=warn,elena_plugins=warn",
                )
            },
        ))
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    info!(
        threads_per_tenant = args.threads_per_tenant,
        duration_secs = args.duration_secs,
        "firetest starting"
    );

    match run(&args).await {
        Ok(report) => {
            let pass = report.summary.failed == 0;
            if let Err(e) = report::write_markdown(&args.report_path, &report) {
                eprintln!("firetest: report write failed: {e:#}");
            }
            println!("\nfiretest: {} ({} passed, {} failed)", if pass { "PASS" } else { "FAIL" }, report.summary.passed, report.summary.failed);
            println!("report: {}", args.report_path);
            if pass {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        Err(e) => {
            eprintln!("\nfiretest: bring-up FAIL — {e:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: &Args) -> anyhow::Result<report::Report> {
    eprintln!("firetest: bringing up testcontainers + gateway + worker…");
    let env = harness::Harness::start().await?;
    eprintln!("firetest: harness up at {}", env.listen_addr);

    eprintln!("firetest: provisioning Solen, Hannlys, Omnii…");
    let provisioned = workload::provision_all(&env).await?;
    eprintln!(
        "  ✓ tenants: solen={} hannlys-creator={} omnii={}",
        provisioned.solen.tenant_id, provisioned.hannlys.creator_id, provisioned.omnii.tenant_id
    );

    eprintln!(
        "firetest: spawning {} concurrent thread-loops/tenant for {}s…",
        args.threads_per_tenant, args.duration_secs
    );
    let stats = workload::run_drivers(
        &env,
        &provisioned,
        args.threads_per_tenant,
        Duration::from_secs(args.duration_secs),
    )
    .await?;
    eprintln!(
        "firetest: drivers done — solen turns={}/{} hannlys turns={}/{} omnii turns={}/{}",
        stats.solen.completed,
        stats.solen.started,
        stats.hannlys.completed,
        stats.hannlys.started,
        stats.omnii.completed,
        stats.omnii.started
    );

    eprintln!("firetest: running assertions…");
    let mut results = assertions::check_all(&env, &provisioned, &stats).await;

    eprintln!("firetest: cleaning up via DELETE /admin/v1/tenants/:id…");
    results.push(workload::cleanup_and_verify(&env, &provisioned).await);

    let report = report::Report {
        args: args.clone(),
        provisioned: provisioned.summary(),
        stats,
        results,
        summary: report::Summary { passed: 0, failed: 0 }, // recomputed in build
    };
    let report = report::finalize(report);

    env.shutdown().await;
    Ok(report)
}

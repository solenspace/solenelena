// SAFETY: `std::env::set_var` is `unsafe` on edition 2024 because it mutates
// shared process state. A build script runs single-threaded before any of the
// crate's code, so the concurrent-modification risk is nil.
#![allow(unsafe_code)]

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Point tonic-build at the vendored protoc so contributors don't need a
    // system protobuf compiler.
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["proto/elena_plugin.proto"], &["proto"])?;

    println!("cargo:rerun-if-changed=proto/elena_plugin.proto");
    Ok(())
}

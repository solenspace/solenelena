#!/bin/sh
# Install Elena's git hooks: redirect git to .githooks and ensure executability.
# Idempotent — safe to re-run.
set -eu

if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    printf 'install-hooks: not inside a git work tree\n' >&2
    exit 1
fi

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

if [ ! -d .githooks ]; then
    printf 'install-hooks: .githooks/ not found at %s\n' "$repo_root" >&2
    exit 1
fi

git config core.hooksPath .githooks

for hook in pre-commit commit-msg pre-push; do
    if [ -f ".githooks/$hook" ]; then
        chmod +x ".githooks/$hook"
    fi
done

cat <<EOF
Hooks installed (core.hooksPath = .githooks):
  pre-commit  cargo fmt --check + cargo clippy -D warnings
  commit-msg  conventional-commit subject validation
  pre-push    fmt + clippy + cargo test --workspace (Docker required)

Bypass any hook with: --no-verify  (or SKIP_HOOKS=1)
EOF

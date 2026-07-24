# The repo-wide checks live here; everything scoped to a subtree lives in that
# subtree's justfile module:
#   just conformance             — the full loopback conformance suite
#   just conformance::<recipe>   — individual targets, labs, builders
#   just examples::<recipe>      — demo components, hosts, and compositions
# Run `just --list` (or `just --list <module>`) to see every recipe.

mod conformance
mod examples

# List the available recipes.
default:
    @just --list

# Run every CI check locally, in the same order as .github/workflows/ci.yml.
ci:
    @just fmt-check clippy validate-wit examples::build-component examples::transpile examples::test-browser test

# Run the fast pre-commit checks (fmt, clippy, WIT, Rust tests); see AGENTS.md.
check: fmt-check clippy validate-wit test

# Check formatting across all crates.
fmt-check:
    cargo fmt --all -- --check

# Run clippy across all crates: the native crates (the workspace
# default-members), then the wasm-only crates per target.
#
# Run clippy across all crates.
clippy:
    cargo clippy -- -D warnings
    cargo clippy --target wasm32-unknown-unknown \
        -p echo-demo \
        -p echo-remote \
        -p conformance-guest \
        -- -D warnings
    cargo clippy --target wasm32-wasip2 \
        -p cli-signaling \
        -p rendezvous-http \
        -p echo-remote-driver \
        -p wasip3-webrtc-datachannels \
        -p webrtc-consumer \
        -p conformance-wasip3-mailbox \
        -p conformance-wasip3-driver \
        -- -D warnings

# Validate WIT packages.
validate-wit:
    wasm-tools component wit wit
    wasm-tools component wit examples/echo-demo/wit
    wasm-tools component wit examples/cli-signaling/wit
    wasm-tools component wit wasip3-impl/wit
    wasm-tools component wit examples/webrtc-consumer/wit
    wasm-tools component wit conformance/wit

# Run the Rust / Wasmtime tests for the native crates (the workspace
# default-members; includes the cli-signaling and echo-remote integration
# tests). nextest runs faster but does not execute doctests, so run those
# separately.
#
# Run the Rust / Wasmtime tests for the native crates.
test:
    cargo nextest run
    cargo test --doc

#!/usr/bin/env bash
#
# Install the toolchain and dependencies needed to build, run, and test this
# repository. Safe to run repeatedly (idempotent) and shared by local developers, the
# CI workflow (.github/workflows/ci.yml), and the Copilot cloud agent
# (.github/workflows/copilot-setup-steps.yml).
#
# What it installs:
#   - the pinned Rust toolchain and the wasm targets the guest components compile
#     to, as declared in rust-toolchain.toml:
#       * wasm32-unknown-unknown (echo-demo + the manual-signaling test guest)
#       * wasm32-wasip2          (cli-signaling)
#   - wasm-tools, used to wrap guest modules into components and to validate WIT
#   - just, the command runner used for development and CI recipes
#   - cargo-nextest, the faster test runner used by `just test`
#   - wac, the component linker used to compose the webrtc-consumer with the
#     wasip3 provider (`just examples::compose-webrtc`)
#   - wasmtime, the host runtime that runs the composed in-guest WebRTC
#     integration test (`just examples::test-webrtc-composed`)
#   - iproute2, nftables, and coturn, used by the conformance netns lab
#     (`just conformance::netns`; skip with SKIP_NETNS_LAB=1)
#   - the conformance Shadow lab (`just conformance::shadow`) needs the Shadow
#     network simulator, which this script does NOT install. Shadow ships no
#     upstream prebuilt binary and is slow to build, so it is built once by the
#     shadow-build workflow (scripts/build-shadow.sh) and published to the
#     `shadow-dev` GitHub prerelease; fetch it with scripts/download-shadow.sh or
#     build it locally with scripts/build-shadow.sh. The lab recipe prints this
#     guidance and fails if the binary is missing when it runs.
#   - the Node host's npm dependencies (jco + @roamhq/wrtc), and the conformance
#     jco adapter's npm dependencies (jco + @roamhq/wrtc + playwright-core)
#
# wasm-tools, just, and cargo-nextest are installed with cargo-binstall, which
# downloads the pinned prebuilt release binaries when available and automatically
# falls back to `cargo install` (compiling from source) otherwise. cargo-binstall
# itself is bootstrapped from its prebuilt release binary.
#
# Prerequisites (not installed here): a Rust toolchain via rustup, and Node 22+
# with npm. CI and copilot-setup-steps provision these before calling this
# script; local developers should install them first.
#
# Environment overrides:
#   WASM_TOOLS_VERSION   version of wasm-tools to install (default below)
#   JUST_VERSION         version of just to install (default below)
#   NEXTEST_VERSION      version of cargo-nextest to install (default below)
#   SKIP_NETNS_LAB=1       skip installing the conformance netns-lab tools (coturn…)
#   SKIP_NODE=1          skip installing the Node host's npm dependencies

set -euo pipefail

# Resolve the repository root from this script's location so it works from any
# working directory.
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

WASM_TOOLS_VERSION="${WASM_TOOLS_VERSION:-1.247.0}"
JUST_VERSION="${JUST_VERSION:-1.40.0}"
NEXTEST_VERSION="${NEXTEST_VERSION:-0.9.140}"
WAC_VERSION="${WAC_VERSION:-0.10.1}"
WASMTIME_VERSION="${WASMTIME_VERSION:-47.0.1}"

log() { printf '\n==> %s\n' "$1"; }

log "Installing pinned Rust toolchain and wasm targets (rust-toolchain.toml)"
# Running rustup in the repo root installs the toolchain, targets, and
# components declared in rust-toolchain.toml.
(cd "${REPO_ROOT}" && rustup show active-toolchain >/dev/null 2>&1 || rustup toolchain install)

log "Ensuring cargo-binstall is installed"
if command -v cargo-binstall >/dev/null 2>&1; then
  echo "cargo-binstall already present: $(cargo-binstall -V)"
else
  # Bootstrap cargo-binstall from its prebuilt release binary; this installer
  # drops the binary into the cargo bin dir.
  curl -fsSL --proto '=https' --tlsv1.2 \
    https://raw.githubusercontent.com/cargo-bins/cargo-binstall/main/install-from-binstall-release.sh | bash
fi

# Install a crate binary with cargo-binstall. It fetches a prebuilt artifact when
# one exists and otherwise falls back to `cargo install` automatically.
#
# This is only reached after the `command -v` guards below fail, i.e. the tool is
# not on PATH. `--force` is required because a restored cargo cache can contain
# the install metadata (.crates.toml) without the corresponding ~/.cargo/bin
# binary; without it, cargo-binstall would report the crate as "already
# installed" and skip, leaving the binary absent for later steps.
binstall() {
  cargo binstall --no-confirm --locked --force "$1"
}

log "Ensuring wasm-tools ${WASM_TOOLS_VERSION} is installed"
if command -v wasm-tools >/dev/null 2>&1; then
  echo "wasm-tools already present: $(wasm-tools --version)"
else
  binstall "wasm-tools@${WASM_TOOLS_VERSION}"
fi

log "Ensuring just ${JUST_VERSION} is installed"
if command -v just >/dev/null 2>&1; then
  echo "just already present: $(just --version)"
else
  binstall "just@${JUST_VERSION}"
fi

log "Ensuring cargo-nextest ${NEXTEST_VERSION} is installed"
if command -v cargo-nextest >/dev/null 2>&1; then
  echo "cargo-nextest already present: $(cargo-nextest --version)"
else
  binstall "cargo-nextest@${NEXTEST_VERSION}"
fi

log "Ensuring wac ${WAC_VERSION} is installed"
if command -v wac >/dev/null 2>&1; then
  echo "wac already present: $(wac --version)"
else
  binstall "wac-cli@${WAC_VERSION}"
fi

log "Ensuring wasmtime ${WASMTIME_VERSION} is installed"
if command -v wasmtime >/dev/null 2>&1; then
  echo "wasmtime already present: $(wasmtime --version)"
else
  binstall "wasmtime-cli@${WASMTIME_VERSION}"
fi

if [ "${SKIP_NETNS_LAB:-0}" = "1" ]; then
  log "Skipping conformance netns-lab dependencies (SKIP_NETNS_LAB=1)"
else
  # The conformance netns lab (`just conformance::netns`, provisioned in Rust by
  # the conformance-netns executor in conformance/adapters/common;
  # README.md) provisions a routed network-namespace topology with `ip`
  # (iproute2) and `nft` (nftables) and relays through coturn's `turnserver`.
  # These come from the distro package manager; install them on Debian/Ubuntu
  # when apt-get is available (harmless no-op elsewhere — provide them yourself).
  log "Ensuring netns-lab tools (iproute2, nftables, coturn) are installed"
  if command -v apt-get >/dev/null 2>&1; then
    APT_SUDO=""
    [ "$(id -u)" -eq 0 ] || APT_SUDO="sudo"
    ${APT_SUDO} apt-get update -y
    DEBIAN_FRONTEND=noninteractive ${APT_SUDO} apt-get install -y --no-install-recommends \
      iproute2 nftables coturn
    # apt starts a default host-namespace turnserver; the lab runs its own coturn
    # inside the signaling namespace, so stop the default to avoid confusion
    # (best-effort: no systemd in many CI containers).
    ${APT_SUDO} systemctl disable --now coturn 2>/dev/null || true
  else
    echo "apt-get not found; install iproute2, nftables, and coturn with your package manager"
  fi
fi

if [ "${SKIP_NODE:-0}" = "1" ]; then
  log "Skipping Node host dependencies (SKIP_NODE=1)"
else
  log "Installing Node host dependencies (jco-impl)"
  (cd "${REPO_ROOT}/jco-impl" && npm install)
  log "Installing conformance jco adapter dependencies (conformance/adapters/jco)"
  (cd "${REPO_ROOT}/conformance/adapters/jco" && npm install)
fi

# In GitHub Actions, $GITHUB_PATH is a file; appending a path to it makes that
# directory available in PATH for all subsequent steps in the job.  Without this,
# wasm-tools / just / cargo-nextest (installed to ~/.cargo/bin above) are not
# found by later `run:` steps even though they exist on disk.
if [ -n "${GITHUB_PATH:-}" ]; then
  echo "${HOME}/.cargo/bin" >> "${GITHUB_PATH}"
  # Shadow (installed to ~/.local/bin by scripts/download-shadow.sh or
  # scripts/build-shadow.sh) lives here too.
  echo "${HOME}/.local/bin" >> "${GITHUB_PATH}"
fi

log "Setup complete"

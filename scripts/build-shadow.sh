#!/usr/bin/env bash
#
# Build the Shadow network simulator (https://github.com/shadow/shadow) from
# source and install it under a prefix (default ~/.local: bin/shadow +
# lib/libshadow_*.so). Shadow ships no prebuilt binary, so the conformance Shadow
# lab (`just conformance::shadow`) needs it built.
#
# This is the single source of truth for the pinned Shadow revision and the
# build steps. It is used by:
#   - the shadow-build workflow (.github/workflows/shadow-build.yml), which runs
#     it and publishes the resulting binary to the `shadow-dev` GitHub
#     prerelease so CI and the Copilot agent can download it instead of
#     rebuilding (see scripts/download-shadow.sh);
#   - local developers who prefer to build Shadow themselves rather than
#     download the release.
#
# Ordinary `scripts/setup.sh` does NOT build Shadow; it only asserts the binary
# is present. Run this script (or scripts/download-shadow.sh) first if it is not.
#
# This path is Debian/Ubuntu (apt-get); see
# https://shadow.github.io/docs/guide/install_dependencies.html for other
# distributions.
#
# Usage:
#   scripts/build-shadow.sh [PREFIX]      # PREFIX defaults to ~/.local
#
# Environment overrides:
#   SHADOW_REF   git ref of shadow/shadow to build (default below)

set -euo pipefail

# Post-v3.3.0 shadow/shadow master, verified to build on Ubuntu 24.04 and run the
# conformance Shadow lab. Pinned by commit for reproducibility.
SHADOW_REF="${SHADOW_REF:-e2829ed32acde66124ce9c14cb5d2337cad7f8e0}"

PREFIX="${1:-${HOME}/.local}"

log() { printf '\n==> %s\n' "$1"; }

if ! command -v apt-get >/dev/null 2>&1; then
  echo "apt-get not found; build Shadow from source per https://shadow.github.io" >&2
  exit 1
fi

log "Installing Shadow build dependencies"
APT_SUDO=""
[ "$(id -u)" -eq 0 ] || APT_SUDO="sudo"
${APT_SUDO} apt-get update -y
DEBIAN_FRONTEND=noninteractive ${APT_SUDO} apt-get install -y --no-install-recommends \
  cmake findutils libclang-dev libc-dbg libglib2.0-0 libglib2.0-dev make \
  netbase python3 python3-networkx xz-utils util-linux gcc g++

log "Building Shadow (${SHADOW_REF}) into ${PREFIX}"
SHADOW_SRC="$(mktemp -d)"
trap 'rm -rf "${SHADOW_SRC}"' EXIT
git clone --filter=blob:none https://github.com/shadow/shadow.git "${SHADOW_SRC}"
(
  cd "${SHADOW_SRC}"
  git checkout --quiet "${SHADOW_REF}"
  ./setup build --jobs "$(nproc)" --prefix "${PREFIX}"
  ./setup install
)

echo "shadow installed: $("${PREFIX}/bin/shadow" --version | head -1)"

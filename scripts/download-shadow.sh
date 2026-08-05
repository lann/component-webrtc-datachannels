#!/usr/bin/env bash
#
# Download the prebuilt Shadow network simulator from this repository's
# `shadow-dev` GitHub prerelease and install it under a prefix (default
# ~/.local: bin/shadow + lib/libshadow_*.so).
#
# Shadow ships no upstream prebuilt binary and takes several minutes to build
# from source, so the shadow-build workflow
# (.github/workflows/shadow-build.yml) builds it once with
# scripts/build-shadow.sh and publishes the tarball to the `shadow-dev`
# prerelease. CI (the conformance Shadow lab) and the Copilot agent
# (copilot-setup-steps.yml) call this script to fetch that binary instead of
# rebuilding.
#
# Requires the GitHub CLI (`gh`) with a token (GH_TOKEN / GITHUB_TOKEN) able to
# read releases — both are present on GitHub Actions runners.
#
# Usage:
#   scripts/download-shadow.sh [PREFIX]     # PREFIX defaults to ~/.local
#
# Environment overrides:
#   SHADOW_RELEASE_REPO   owner/name to download from (default: this repo via
#                         GITHUB_REPOSITORY, else polymorph-components/polymorph-webrtc-datachannels)
#   SHADOW_RELEASE_TAG    release tag to download (default: shadow-dev)

set -euo pipefail

PREFIX="${1:-${HOME}/.local}"
REPO="${SHADOW_RELEASE_REPO:-${GITHUB_REPOSITORY:-polymorph-components/polymorph-webrtc-datachannels}}"
TAG="${SHADOW_RELEASE_TAG:-shadow-dev}"
ASSET="shadow-linux-x86_64.tar.gz"

log() { printf '\n==> %s\n' "$1"; }

if ! command -v gh >/dev/null 2>&1; then
  echo "gh (GitHub CLI) not found; install it or build Shadow with scripts/build-shadow.sh" >&2
  exit 1
fi

# Shadow links against glib at runtime; make sure it is present (best-effort).
if command -v apt-get >/dev/null 2>&1 && ! ldconfig -p 2>/dev/null | grep -q 'libglib-2.0'; then
  log "Installing Shadow runtime dependency (glib)"
  APT_SUDO=""
  [ "$(id -u)" -eq 0 ] || APT_SUDO="sudo"
  ${APT_SUDO} apt-get update -y
  # Ubuntu 24.04+ renamed the package to libglib2.0-0t64 (the time_t transition);
  # older releases still use libglib2.0-0. Try the new name first, fall back.
  DEBIAN_FRONTEND=noninteractive ${APT_SUDO} apt-get install -y --no-install-recommends \
    libglib2.0-0t64 || \
  DEBIAN_FRONTEND=noninteractive ${APT_SUDO} apt-get install -y --no-install-recommends \
    libglib2.0-0
fi

log "Downloading Shadow ${ASSET} from ${REPO} release ${TAG}"
TMP="$(mktemp -d)"
trap 'rm -rf "${TMP}"' EXIT
gh release download "${TAG}" --repo "${REPO}" --pattern "${ASSET}" --dir "${TMP}"

log "Installing Shadow into ${PREFIX}"
mkdir -p "${PREFIX}"
tar -xzf "${TMP}/${ASSET}" -C "${PREFIX}"

echo "shadow installed: $("${PREFIX}/bin/shadow" --version | head -1)"

# In GitHub Actions, appending to $GITHUB_PATH makes ~/.local/bin available to
# subsequent steps (e.g. the setup.sh presence assertion and the lab run).
if [ -n "${GITHUB_PATH:-}" ]; then
  echo "${PREFIX}/bin" >> "${GITHUB_PATH}"
fi

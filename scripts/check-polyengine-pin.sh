#!/usr/bin/env bash
# The one-version-everywhere gate for polyengine's JSR packages, replacing the
# retired one-tag-everywhere assertion that used to live in the (now
# deleted) deno translator-fetch script (see polyengine-jsr-contract.md).
#
# polyengine-impl/deno.json is JSR-published, so its manifest maps
# @polyengine/runtime to a caret RANGE (published dependency constraints must
# be ranges); the other two configs (conformance/driver-ct/polyengine and its
# browser/ import map) still map it to an exact pin, since they load the
# same on-disk polyengine module and must resolve identically. Manifests can
# therefore no longer be compared directly: this gate instead asserts that
# every jsr:@polyengine/* package (excluding @polyengine/protocol, a transitive
# dependency that deliberately floats independently) RESOLVES to the same
# version across all three repo deno.locks. Run as part of
# `just polyengine-check` (CI: gha::conformance-matrix), the natural
# fail-loud point since every leg's lock is on disk there.
set -euo pipefail

locks=(
    polyengine-impl/deno.lock
    conformance/driver-ct/polyengine/deno.lock
    conformance/driver-ct/polyengine/browser/deno.lock
)

v=$(grep -ohP '"@polyengine/(?!protocol)[a-zA-Z-]+@[^"]+"(?=: \{)' "${locks[@]}" | sed 's/.*@//;s/"$//' | sort -u)
n=$(printf '%s\n' "$v" | wc -l)
if [ "$n" != 1 ]; then
    echo "polyengine pin drift across ${locks[*]}: $v" >&2
    exit 1
fi
echo "polyengine pin OK: $v"

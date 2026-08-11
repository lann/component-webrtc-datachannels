#!/usr/bin/env bash
# The one-version-everywhere gate for deltic's JSR packages, replacing the
# retired one-tag-everywhere assertion that used to live in the (now
# deleted) deno translator-fetch script (see deltic-jsr-contract.md).
# Every deno.json that imports a jsr:@deltic/* package must agree on the
# exact version; run as part of `just deltic-check` (CI:
# gha::conformance-matrix), the natural fail-loud point since every leg's
# config is on disk there.
set -euo pipefail

configs=(
    deltic-impl/deno.json
    conformance/driver-ct/deltic/deno.json
    conformance/driver-ct/deltic/browser/deno.json
)

v=$(grep -ho 'jsr:@deltic/[a-z-]*@[^/"]*' "${configs[@]}" | sed 's/.*@//' | sort -u)
n=$(printf '%s\n' "$v" | wc -l)
if [ "$n" != 1 ]; then
    echo "deltic pin drift across ${configs[*]}: $v" >&2
    exit 1
fi
echo "deltic pin OK: $v"

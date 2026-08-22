# `conformance/driver-ct/polyengine` — the polyengine-native conformance leg

The `polyengine-deno` target in the loopback conformance matrix: the suite
runs runtime-linked under stock Deno — no transpile step, no generated
tree, no engine flag (the WIT contract's async exports run on the
callback ABI) — against
[`polyengine-impl/src/webrtc.ts`](../../../polyengine-impl/src/webrtc.ts) and the
fetch mailbox ([`signaling.ts`](signaling.ts)). These children carry
the same child contract the retired jco legs did (see git history)
under `rtc-ct-driver`'s loopback orchestration (a solo stream over
`solo/` plus a role pair over `pair/`, folded), the same JSONL wire on
stdout, the same 512 KiB inbound-buffer bound exported through
`WEBRTC_MAX_INBOUND_BUFFER_BYTES`.

The driver spawns each child as
`deno run --allow-all --frozen --config <here>/deno.json run.ts --select
<prefix>` with the `RTC_CT_*` environment (see `run.ts`'s header);
`--allow-all` is not a security boundary here — the leg exists to run a
native WebRTC addon (`node-datachannel`), whose code Deno's sandbox
cannot confine anyway.

## Running it

```sh
just conformance::run-polyengine
```

which installs the leg's pinned module graph + the `node-datachannel`
addon (`just conformance::polyengine-deps`, idempotent) and runs the
loopback leg through the driver, writing
`conformance/driver-ct/results/polyengine-deno.jsonl`. The translator shim
comes from the pinned `@polyengine/translator` JSR package through the
module graph — no fetch step, no network.

## The pin

polyengine ships as exact-pinned JSR prereleases (one per green upstream
commit; the hash in the version names the commit — see the [polyengine
README's "Consuming the unstable
prereleases"](https://github.com/polymorph-components/polyengine#readme)). The version is
pinned in **three** places, cross-checked by `just polyengine-check`'s pin
gate (`../../../scripts/check-polyengine-pin.sh`):

- `deno.json` (this directory) — import-map versions
  (`jsr:@polyengine/<pkg>@<version>`) for `@polyengine/ct-runner`,
  `@polyengine/runtime/embedder`, `@polyengine/runtime/shim`,
  `@polyengine/wasi-shims`, `@polyengine/translator`. `deno.lock` carries
  integrity hashes for that module graph, enforced with `--frozen`.
- [`../../../polyengine-impl/deno.json`](../../../polyengine-impl/deno.json) —
  the SAME `@polyengine/runtime/embedder` version (the module-identity
  constraint: polyengine's `wasi-shims` imports that specifier by bare name
  internally, so every config resolving it must agree, or the embedder
  module loads twice and `instanceof ComponentException` stops holding across
  the boundary).
- [`browser/deno.json`](browser/deno.json) — the SAME pins again, with
  the npm WebRTC backends stubbed out (never executed in a page).

Each `deno.json` also carries a
`"minimumDependencyAge": { "age": "P1D", "exclude": ["jsr:@polyengine/*"] }`
stanza: the JSR prereleases are typically published well under 24h
before consumption, and without the exclude the default supply-chain
age gate blocks resolution.

To bump: update the version in all three `deno.json` files, delete all
three `deno.lock` files (this directory, `browser/`, and
`polyengine-impl/`), re-run
`deno install --frozen --allow-scripts=npm:node-datachannel` in this
directory and in `polyengine-impl/`, and `deno install` in `browser/`, to
regenerate the locks (`just polyengine-check` also does the first two
installs). Then re-run `just conformance::run-polyengine` and commit the
diff (including the regenerated `matrix.md`, via
`just conformance::matrix-update`, if behavior changed).

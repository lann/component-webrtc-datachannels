# `conformance/driver-ct/deltic` — the deltic-native conformance leg

The `deltic-deno` target in the loopback conformance matrix: the suite
runs runtime-linked under stock Deno — no transpile step, no generated
tree, no engine flag (the WIT contract's async exports run on the
callback ABI) — against
[`deltic-impl/src/webrtc.ts`](../../../deltic-impl/src/webrtc.ts) and the
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
just conformance::run-deltic
```

which installs the leg's pinned module graph + the `node-datachannel`
addon (`just conformance::deltic-deps`, idempotent), fetches (and
caches) the pinned translator-shim release asset, and runs the loopback
leg through the driver, writing
`conformance/driver-ct/results/deltic-deno.jsonl`.

## The pin

deltic is pinned to a release tag in **three** places, cross-checked at
run time by `fetch-translator.ts`:

- `deno.json` (this directory) — import-map URLs
  (`raw.githubusercontent.com/lann/deltic/<tag>/…`) for `@deltic/ct-runner`,
  `@deltic/runtime/embedder`, `@deltic/runtime/shim`, `@deltic/wasi-shims`.
  `deno.lock` carries integrity hashes for that module graph, enforced
  with `--frozen`.
- [`../../../deltic-impl/deno.json`](../../../deltic-impl/deno.json) —
  the SAME `@deltic/runtime/embedder` URL (the module-identity
  constraint: deltic's `wasi-shims` imports that specifier by bare name
  internally, so every config resolving it must agree, or the embedder
  module loads twice and `instanceof WitError` stops holding across the
  boundary).
- `fetch-translator.ts` — `TAG` + `TRANSLATOR_SHA256` for the
  `deltic-translator-shim.wasm` release asset (cached under
  `target/deltic/<tag>/`).

To bump: update the tag in all three files (this `deno.json`,
`deltic-impl/deno.json`, and `fetch-translator.ts`) and the sha256 from
the release's `SHA256SUMS`, delete BOTH `deno.lock` files (this
directory and `deltic-impl/`), re-run
`deno install --allow-scripts=npm:node-datachannel` here and in
`deltic-impl/` to regenerate them, then re-run
`just conformance::run-deltic` and commit the diff (including the
regenerated `matrix.md`, via `just conformance::matrix-update`, if
behavior changed).

// The local replacement for the retired `@deltic/release-bundle-entry`
// raw-URL import: this is upstream tools/release-bundle/entry.ts's exact
// public surface, re-exported from the pinned JSR packages so `deno
// bundle` can produce one flat module for the page (see
// ../../../../conformance/justfile's `_deltic-browser-bundle` recipe).
// The worker/page protocol is unchanged; only the source of these
// exports moved from a sha-pinned GitHub release asset to the
// exact-pinned JSR graph (see ../README.md's pin section).
export * from "@deltic/runtime/embedder";
export { Translator } from "@deltic/runtime/shim";
export * from "@deltic/ct-runner";
export { wasiShims } from "@deltic/wasi-shims";
export type { WasiShims, WasiShimsOptions } from "@deltic/wasi-shims";

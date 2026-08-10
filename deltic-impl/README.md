# `deltic-impl` — the deltic-native `polymorph:webrtc-datachannels` host module

`src/webrtc.ts` is the [deltic](https://github.com/lann/deltic)-native
port of [`jco-impl/webrtc.js`](../jco-impl/webrtc.js): the same
behavioral reference host, rewritten over deltic's embedder API (typed
`Stream<T>` / `ReadableStream` rather than jco streams, and `WitError`
throws rather than `throw { tag, val }`). It was developed as deltic's
own `ports/webrtc` reference-host port and is upstreamed here per
[lann/deltic#14](https://github.com/lann/deltic/issues/14); the WIT
contract is [`wit/webrtc.wit`](../wit/webrtc.wit), and every doc comment
quoting a contract quotes that file. The conformance suite's
`deltic-deno` target asserts the behavioral parity with the other
implementations (`just conformance`).

## Backend

Isomorphic `RTCPeerConnection` resolution, as in the reference: a
browser global when one exists, otherwise the
[`node-datachannel`](https://www.npmjs.com/package/node-datachannel)
polyfill — a Node-API addon that works under Deno's node compatibility
(install it with `deno install --allow-scripts=npm:node-datachannel` in
this directory; the addon's install script needs the explicit grant).
`useWerift()` switches to the pure-TS
[`werift`](https://www.npmjs.com/package/werift) backend (no native
code); the conformance matrix runs the default backend only.

## Checks

```sh
just deltic-check      # type-check + unit tests (this dir), leg type-check
```

The unit tests (`tests/webrtc_test.ts`) are in-process loopback pairs —
a subset of the protocol surface; the conformance suite
(`conformance/driver-ct/deltic/`) is the real gate.

## Module identity

`deno.json` pins `@deltic/runtime/embedder` to a release URL that MUST
stay byte-identical with the one in
[`conformance/driver-ct/deltic/deno.json`](../conformance/driver-ct/deltic/deno.json):
deltic's `wasi-shims` imports that specifier by bare name internally, so
two divergent mappings load the embedder module twice and
`instanceof WitError` stops holding across the module boundary. The
bump procedure lives in
[`conformance/driver-ct/deltic/README.md`](../conformance/driver-ct/deltic/README.md);
`fetch-translator.ts` cross-checks both configs at run time.

# Test matrix

| Case | deltic-browser-x-reference | deltic-deno-x-reference | deltic-deno-x-wasmtime | reference | reference-x-deltic-browser | reference-x-deltic-deno | reference-x-wasip3-guest | reference-x-wasmtime | wasip3-guest-x-reference | wasip3-guest-x-wasmtime | wasmtime-x-deltic-deno | wasmtime-x-reference | wasmtime-x-wasip3-guest |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| pair (12 cases) | pass | pass | pass | pass | pass | pass | pass | pass | 11 pass, 1 xfail | pass | pass | pass | pass |

## Failures

None.

## Expected failures

- `wasip3-guest-x-reference` `pair/channel-close-flush`: rtc emits no SCTP stream reset on data-channel close, so the libwebrtc answerer never observes the close (https://github.com/polymorph-components/polymorph-webrtc-datachannels/issues/123)

## Summary

- `deltic-browser-x-reference`: 12 pass (12 total)
- `deltic-deno-x-reference`: 12 pass (12 total)
- `deltic-deno-x-wasmtime`: 12 pass (12 total)
- `reference`: 12 pass (12 total)
- `reference-x-deltic-browser`: 12 pass (12 total)
- `reference-x-deltic-deno`: 12 pass (12 total)
- `reference-x-wasip3-guest`: 12 pass (12 total)
- `reference-x-wasmtime`: 12 pass (12 total)
- `wasip3-guest-x-reference`: 11 pass, 1 xfail (12 total)
- `wasip3-guest-x-wasmtime`: 12 pass (12 total)
- `wasmtime-x-deltic-deno`: 12 pass (12 total)
- `wasmtime-x-reference`: 12 pass (12 total)
- `wasmtime-x-wasip3-guest`: 12 pass (12 total)

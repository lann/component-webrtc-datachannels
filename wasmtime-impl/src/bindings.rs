//! Raw `bindgen!` output for the `polymorph:webrtc-datachannels` package.
//!
//! The crate implements the `types` interface and, in the `connections`
//! interface, the `data-channel-options` and `peer-connection-config`
//! builders, the `data-channel` resource,
//! and the `peer-connection` resource. See
//! [`crate`] for the public API built on top of these bindings.

#[allow(missing_docs, reason = "generated code")]
mod generated {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "imports",
        imports: {
            // `send`/`receive`/`send-via-stream`/`drop` need all three: `async`
            // for the component-model async ABI, `store` for `Accessor` access
            // to the `ResourceTable` (and the `…WithStore` traits that host the
            // async methods), and `trappable` so the host functions can return
            // `wasmtime::Result` and surface host errors as traps. Dropping any
            // one of them fails to compile against these host impls.
            default: async | store | trappable,
            // `data-channel.label` is a synchronous function in the WIT and is
            // imported as such by guests, so it must be bound synchronously
            // (still `trappable`, but not `async`).
            "polymorph:webrtc-datachannels/connections@0.1.0.[method]data-channel.label": trappable,
            // `data-channel.close` is synchronous in the WIT (it initiates the
            // closing procedure and returns) and needs no store access.
            "polymorph:webrtc-datachannels/connections@0.1.0.[method]data-channel.close": trappable,
            // `data-channel.receive-via-stream` is synchronous in the WIT: it
            // hands back the inbound stream without awaiting, so it is bound
            // synchronously. It still needs `store` to allocate the returned
            // `stream<stream-message>` on the guest's behalf. The
            // `state-changes` streams (on both resources) are bound the same
            // way for the same reason.
            "polymorph:webrtc-datachannels/connections@0.1.0.[method]data-channel.receive-via-stream": store | trappable,
            "polymorph:webrtc-datachannels/connections@0.1.0.[method]data-channel.state-changes": store | trappable,
            // The `peer-connection` resource's synchronous functions are bound
            // synchronously. The `constructor`, `create-data-channel`, and
            // `close` need no store access; the stream-returning functions need
            // `store` to allocate the returned stream (and, for
            // `incoming-data-channels`, to push data-channel resources).
            "polymorph:webrtc-datachannels/connections@0.1.0.[constructor]peer-connection": trappable,
            "polymorph:webrtc-datachannels/connections@0.1.0.[method]peer-connection.create-data-channel": trappable,
            "polymorph:webrtc-datachannels/connections@0.1.0.[method]peer-connection.incoming-data-channels": store | trappable,
            "polymorph:webrtc-datachannels/connections@0.1.0.[method]peer-connection.local-ice-candidates": store | trappable,
            "polymorph:webrtc-datachannels/connections@0.1.0.[method]peer-connection.state-changes": store | trappable,
            "polymorph:webrtc-datachannels/connections@0.1.0.[method]peer-connection.close": trappable,
            // `data-channel-options` and `peer-connection-config` are plain
            // configuration builders: their constructors and every
            // getter/setter are synchronous WIT functions, so they are bound
            // synchronously (no `async`, no `store`).
            "polymorph:webrtc-datachannels/connections@0.1.0.[constructor]data-channel-options": trappable,
            "polymorph:webrtc-datachannels/connections@0.1.0.[method]data-channel-options.label": trappable,
            "polymorph:webrtc-datachannels/connections@0.1.0.[method]data-channel-options.set-label": trappable,
            "polymorph:webrtc-datachannels/connections@0.1.0.[method]data-channel-options.ordered": trappable,
            "polymorph:webrtc-datachannels/connections@0.1.0.[method]data-channel-options.set-ordered": trappable,
            "polymorph:webrtc-datachannels/connections@0.1.0.[method]data-channel-options.max-retransmits": trappable,
            "polymorph:webrtc-datachannels/connections@0.1.0.[method]data-channel-options.set-max-retransmits": trappable,
            "polymorph:webrtc-datachannels/connections@0.1.0.[constructor]peer-connection-config": trappable,
            "polymorph:webrtc-datachannels/connections@0.1.0.[method]peer-connection-config.ice-servers": trappable,
            "polymorph:webrtc-datachannels/connections@0.1.0.[method]peer-connection-config.set-ice-servers": trappable,
            "polymorph:webrtc-datachannels/connections@0.1.0.[method]peer-connection-config.ice-transport-policy": trappable,
            "polymorph:webrtc-datachannels/connections@0.1.0.[method]peer-connection-config.set-ice-transport-policy": trappable,
        },
        with: {
            "polymorph:webrtc-datachannels/connections.data-channel-options": crate::DataChannelOptions,
            "polymorph:webrtc-datachannels/connections.peer-connection-config": crate::PeerConnectionConfig,
            "polymorph:webrtc-datachannels/connections.data-channel": crate::DataChannel,
            "polymorph:webrtc-datachannels/connections.peer-connection": crate::PeerConnection,
        },
    });
}

pub use self::generated::polymorph::*;

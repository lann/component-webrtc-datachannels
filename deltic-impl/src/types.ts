// Structural types for `polymorph:webrtc-datachannels/types@0.1.0`, ported to
// the embedder conventions (contracts/embedder-api.md §"Value mapping").
//
// Authority: wit/webrtc.wit `interface types` (polymorph-webrtc-datachannels,
// read-only reference). Enums are kebab-case string literal unions; variants
// are `{ tag, val? }`; records are plain camelCase objects.

import type { Stream, StreamSource } from "@deltic/runtime/embedder";

// --- error -----------------------------------------------------------------

export type WebrtcError =
  | { tag: "closed" }
  | { tag: "timed-out" }
  | { tag: "invalid-signaling"; val: string }
  | { tag: "receiving-via-stream" }
  | { tag: "receive-buffer-overflow" }
  | { tag: "other"; val: string };

// --- message -----------------------------------------------------------------

export type Message =
  | { tag: "binary"; val: Uint8Array }
  | { tag: "string"; val: string };

export const Message = {
  binary(bytes: Uint8Array): Message {
    return { tag: "binary", val: bytes };
  },
  string(text: string): Message {
    return { tag: "string", val: text };
  },
};

// --- message-kind ------------------------------------------------------------

export type MessageKind = "binary" | "string";

// --- stream-message ------------------------------------------------------------

export interface StreamMessage {
  kind: MessageKind;
  length: number;
  data: StreamSource<number>;
}

/**
 * The lifted shape of `stream-message` when the guest hands one over (as a
 * parameter to `send-via-stream`): `data` arrives as a `Stream<u8>` handle,
 * not a producer the port constructs.
 */
export interface LiftedStreamMessage {
  kind: MessageKind;
  length: number;
  data: Stream<number>;
}

// --- send-via-stream-error -----------------------------------------------------

export interface SendViaStreamError {
  error: WebrtcError;
  sent: bigint;
}

// --- sdp-type ------------------------------------------------------------------

export type SdpType = "offer" | "answer" | "pranswer" | "rollback";

// --- session-description ---------------------------------------------------------

export interface SessionDescription {
  kind: SdpType;
  sdp: string;
}

// --- ice-candidate -----------------------------------------------------------------

export interface IceCandidate {
  candidate: string;
  sdpMid?: string;
  sdpMlineIndex?: number;
}

// --- config-error ------------------------------------------------------------------

export type ConfigError =
  | { tag: "not-supported" }
  | { tag: "invalid"; val: string };

// --- ice-server --------------------------------------------------------------------

export interface IceServer {
  urls: string[];
  username: string;
  credential: string;
}

// --- ice-transport-policy ------------------------------------------------------------

export type IceTransportPolicy = "all" | "relay";

// --- connection-state ----------------------------------------------------------------

export type ConnectionState =
  | "new"
  | "connecting"
  | "connected"
  | "disconnected"
  | "failed"
  | "closed";

// --- data-channel-state --------------------------------------------------------------

export type DataChannelState = "connecting" | "open" | "closing" | "closed";

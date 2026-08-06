//! The conformance suite as a `polymorph:test` component.
//!
//! One `#[case]` per corpus row; every body delegates to
//! [`conformance_suite_body`], the port of the incumbent guest's
//! assertions, keyed by the incumbent's flat id — so each delegator
//! documents exactly which row it carries.
//!
//! The name hierarchy is the execution topology: `solo/*` cases run in
//! one suite instance (two in-process peer connections, or a lone peer
//! for the error probes); `pair/*` cases run as two role-paired
//! instances of this same binary sharing a signaling room (role and
//! room seed arrive through the store environment; see
//! `conformance-suite-body`). The driver selects by prefix, so the
//! topology needs no side table. `conformance-guest-pair-ct` is the
//! pair-only sibling suite the interop and lab matrices run.
//!
//! No feature tags: every current target serves the whole surface (the
//! incumbent's manifests declared no `unsupported` tags). When a
//! capability gap appears, gate it the component-test way — a
//! `[features]` entry in the target manifest, tags on the affected
//! cases, and a `!feature` decline probe.

#[component_test_sdk::suite(name = "")]
mod webrtc {
    mod solo {
        #[case]
        async fn peer_offer_answer() -> Verdict {
            crate::verdict(conformance_suite_body::solo("peer-offer-answer").await)
        }
        #[case]
        async fn peer_create_data_channel() -> Verdict {
            crate::verdict(conformance_suite_body::solo("peer-create-data-channel").await)
        }
        #[case]
        async fn peer_local_ice_candidates() -> Verdict {
            crate::verdict(conformance_suite_body::solo("peer-local-ice-candidates").await)
        }
        #[case]
        async fn peer_add_ice_candidate() -> Verdict {
            crate::verdict(conformance_suite_body::solo("peer-add-ice-candidate").await)
        }
        #[case]
        async fn peer_wait_connected() -> Verdict {
            crate::verdict(conformance_suite_body::solo("peer-wait-connected").await)
        }
        #[case]
        async fn peer_wait_connected_latch() -> Verdict {
            crate::verdict(conformance_suite_body::solo("peer-wait-connected-latch").await)
        }
        #[case]
        async fn peer_streams_once() -> Verdict {
            crate::verdict(conformance_suite_body::solo("peer-streams-once").await)
        }
        #[case]
        async fn post_close_signaling() -> Verdict {
            crate::verdict(conformance_suite_body::solo("post-close-signaling").await)
        }
        #[case]
        async fn peer_close_releases() -> Verdict {
            crate::verdict(conformance_suite_body::solo("peer-close-releases").await)
        }
        #[case]
        async fn peer_invalid_sdp() -> Verdict {
            crate::verdict(conformance_suite_body::solo("peer-invalid-sdp").await)
        }
        #[case]
        async fn error_invalid_signaling() -> Verdict {
            crate::verdict(conformance_suite_body::solo("error-invalid-signaling").await)
        }
        #[case]
        async fn error_closed() -> Verdict {
            crate::verdict(conformance_suite_body::solo("error-closed").await)
        }
        #[case]
        async fn error_timed_out() -> Verdict {
            crate::verdict(conformance_suite_body::solo("error-timed-out").await)
        }
        #[case]
        async fn post_close_send() -> Verdict {
            crate::verdict(conformance_suite_body::solo("post-close-send").await)
        }
        #[case]
        async fn receive_buffer_overflow() -> Verdict {
            crate::verdict(conformance_suite_body::solo("receive-buffer-overflow").await)
        }
        #[case]
        async fn send_via_stream() -> Verdict {
            crate::verdict(conformance_suite_body::solo("send-via-stream").await)
        }
        #[case]
        async fn receive_via_stream() -> Verdict {
            crate::verdict(conformance_suite_body::solo("receive-via-stream").await)
        }
        #[case]
        async fn receive_via_stream_once() -> Verdict {
            crate::verdict(conformance_suite_body::solo("receive-via-stream-once").await)
        }
        #[case]
        async fn config_defaults() -> Verdict {
            crate::verdict(conformance_suite_body::solo("config-defaults").await)
        }
        #[case]
        async fn config_setters_contract() -> Verdict {
            crate::verdict(conformance_suite_body::solo("config-setters-contract").await)
        }
        #[case]
        async fn config_invalid_ice_server() -> Verdict {
            crate::verdict(conformance_suite_body::solo("config-invalid-ice-server").await)
        }
        #[case]
        async fn connection_state_changes() -> Verdict {
            crate::verdict(conformance_suite_body::solo("connection-state-changes").await)
        }
        #[case]
        async fn channel_state_changes() -> Verdict {
            crate::verdict(conformance_suite_body::solo("channel-state-changes").await)
        }
        #[case]
        async fn channel_post_close_receive() -> Verdict {
            crate::verdict(conformance_suite_body::solo("channel-post-close-receive").await)
        }
        #[case]
        async fn channel_drop_implies_close() -> Verdict {
            crate::verdict(conformance_suite_body::solo("channel-drop-implies-close").await)
        }
    }

    mod pair {
        #[case]
        async fn label_round_trip() -> Verdict {
            crate::verdict(conformance_suite_body::pair("label-round-trip").await)
        }
        #[case]
        async fn binary_message() -> Verdict {
            crate::verdict(conformance_suite_body::pair("binary-message").await)
        }
        #[case]
        async fn text_message() -> Verdict {
            crate::verdict(conformance_suite_body::pair("text-message").await)
        }
        #[case]
        async fn message_boundaries() -> Verdict {
            crate::verdict(conformance_suite_body::pair("message-boundaries").await)
        }
        #[case]
        async fn zero_length_message() -> Verdict {
            crate::verdict(conformance_suite_body::pair("zero-length-message").await)
        }
        #[case]
        async fn large_message() -> Verdict {
            crate::verdict(conformance_suite_body::pair("large-message").await)
        }
        #[case]
        async fn ordering() -> Verdict {
            crate::verdict(conformance_suite_body::pair("ordering").await)
        }
        #[case]
        async fn payload_integrity() -> Verdict {
            crate::verdict(conformance_suite_body::pair("payload-integrity").await)
        }
        #[case]
        async fn concurrent_send_receive() -> Verdict {
            crate::verdict(conformance_suite_body::pair("concurrent-send-receive").await)
        }
        #[case]
        async fn max_retransmits_accepted() -> Verdict {
            crate::verdict(conformance_suite_body::pair("max-retransmits-accepted").await)
        }
        #[case]
        async fn interop_handshake() -> Verdict {
            crate::verdict(conformance_suite_body::pair("interop-handshake").await)
        }
        #[case]
        async fn channel_close_flush() -> Verdict {
            crate::verdict(conformance_suite_body::pair("channel-close-flush").await)
        }
    }
}

/// Adapt a case body's detail-string result to a component-test verdict.
fn verdict(result: Result<(), String>) -> component_test_sdk::Verdict {
    result.map_err(component_test_sdk::Failure::Failed)
}

//! The pair-only sibling suite: exactly `conformance-guest-ct`'s
//! `pair/*` cases, packaged as a separate artifact so the interop
//! directions and the network labs — whose targets never run `solo/*`
//! cases — aggregate against a lockfile that expects only what they
//! execute. Same bodies, same names, same rooms: a pair instance of
//! this artifact interoperates with a `pair/*`-selected instance of the
//! full suite (and with the native reference peer).

#[component_test_sdk::suite(name = "")]
mod webrtc {
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

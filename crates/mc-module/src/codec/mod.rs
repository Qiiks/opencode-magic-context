pub mod opencode;
pub mod pi;
pub mod sidecar;

pub use opencode::{
    decode_opencode, decode_opencode_with_sidecar, encode_opencode, encode_opencode_with_session,
    MessageV2Json,
};
pub use pi::{decode_pi, decode_pi_with_sidecar, encode_pi, PiSessionEntryJson};
pub use sidecar::{DecodeSidecar, DecodedHarnessMessages, ExtractedBoundary};

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde::Deserialize;
    use serde_json::Value;

    use crate::ck_wire::CkWireMessage;
    use crate::injection::build_synthetic_todo_pair;

    use super::{
        decode_opencode, decode_pi, encode_opencode, encode_opencode_with_session, encode_pi,
    };

    #[derive(Deserialize)]
    struct OpenCodeGolden {
        coverage: Vec<String>,
        #[serde(default)]
        missing_capture_classes: Vec<String>,
        cases: Vec<OpenCodeCase>,
    }

    #[derive(Deserialize)]
    struct OpenCodeCase {
        messages: Vec<Value>,
    }

    #[derive(Deserialize)]
    struct PiGolden {
        coverage: Vec<String>,
        #[serde(default)]
        missing_capture_classes: Vec<String>,
        cases: Vec<PiCase>,
    }

    #[derive(Deserialize)]
    struct PiCase {
        entries: Vec<Value>,
    }

    #[test]
    fn opencode_golden_round_trips_wire_projected_parts_and_is_deterministic() {
        let golden: OpenCodeGolden =
            serde_json::from_str(include_str!("../../testdata/codec/opencode-golden.json"))
                .unwrap();
        assert_coverage_or_recorded_missing(
            &golden.coverage,
            &golden.missing_capture_classes,
            &[
                "text",
                "ignored_text",
                "empty_text",
                "reasoning_signature",
                "tool_completed",
                "tool_error",
                "file",
                "step_start",
                "compaction",
                "subtask",
                "step_finish",
                "patch",
            ],
        );

        for case in golden.cases {
            let decoded = decode_opencode(&case.messages);
            let decoded_again = decode_opencode(&case.messages);
            assert_eq!(decoded, decoded_again);
            assert!(decoded.boundary.is_some());

            let ck_messages: Vec<_> = decoded.messages.iter().map(|msg| msg.ck.clone()).collect();
            let encoded = encode_opencode(&ck_messages, &decoded.sidecar);
            let encoded_again = encode_opencode(&ck_messages, &decoded.sidecar);
            assert_eq!(encoded, encoded_again);
            assert_eq!(encoded, strip_opencode_compaction(case.messages));
        }
    }

    #[test]
    fn serve_native_golden_preserves_ingress_and_pins_synthetic_shapes() {
        #[derive(Deserialize)]
        struct ServeNativeGolden {
            session_id: String,
            messages: Vec<Value>,
            m0: Value,
            m1: Value,
            synthetic_todo: Value,
        }

        let golden: ServeNativeGolden = serde_json::from_str(include_str!(
            "../../testdata/codec/serve-native-golden.json"
        ))
        .unwrap();
        let decoded = decode_opencode(&golden.messages);
        let todo = build_synthetic_todo_pair(
            r#"[{"content":"Ship it","status":"in_progress","priority":"high"}]"#,
        )
        .unwrap();
        let mut output = vec![
            CkWireMessage::synthetic_user_text("<session-history>\nP1\n</session-history>"),
            CkWireMessage::synthetic_user_text("session delta"),
            todo.assistant_msg,
            todo.tool_msg,
        ];
        output.extend(decoded.messages.iter().map(|message| message.ck.clone()));

        let encoded =
            encode_opencode_with_session(&output, &decoded.sidecar, Some(&golden.session_id));
        assert_eq!(encoded[0], golden.m0);
        assert_eq!(encoded[1], golden.m1);
        assert_eq!(encoded[2], golden.synthetic_todo);
        assert_eq!(&encoded[3..], golden.messages.as_slice());
    }

    #[test]
    fn pi_golden_round_trips_non_compaction_entries_and_is_deterministic() {
        let golden: PiGolden =
            serde_json::from_str(include_str!("../../testdata/codec/pi-golden.json")).unwrap();
        assert_coverage_or_recorded_missing(
            &golden.coverage,
            &golden.missing_capture_classes,
            &[
                "text_signature",
                "thinking_signature",
                "redacted_thinking",
                "image",
                "tool_call_split_pipe",
                "thought_signature",
                "tool_result",
                "tool_result_details",
                "custom_message",
                "compaction",
                "aborted_assistant",
                "response_id_mid",
                "timestamp_fallback_mid",
            ],
        );

        for case in golden.cases {
            let decoded = decode_pi(&case.entries);
            let decoded_again = decode_pi(&case.entries);
            assert_eq!(decoded, decoded_again);
            assert!(decoded.boundary.is_some());

            let ck_messages: Vec<_> = decoded.messages.iter().map(|msg| msg.ck.clone()).collect();
            let encoded = encode_pi(&ck_messages, &decoded.sidecar);
            let encoded_again = encode_pi(&ck_messages, &decoded.sidecar);
            assert_eq!(encoded, encoded_again);
            assert_eq!(encoded, strip_pi_compaction(case.entries));
        }
    }

    fn assert_coverage_or_recorded_missing(
        actual: &[String],
        recorded_missing: &[String],
        required: &[&str],
    ) {
        let actual: BTreeSet<&str> = actual.iter().map(String::as_str).collect();
        let recorded_missing: BTreeSet<&str> =
            recorded_missing.iter().map(String::as_str).collect();
        let unresolved: Vec<&str> = required
            .iter()
            .copied()
            .filter(|item| !actual.contains(item) && !recorded_missing.contains(item))
            .collect();
        assert!(
            unresolved.is_empty(),
            "codec golden neither covers nor records missing classes: {unresolved:?}"
        );
    }

    fn strip_opencode_compaction(mut messages: Vec<Value>) -> Vec<Value> {
        for message in &mut messages {
            let Some(parts) = message.get_mut("parts").and_then(Value::as_array_mut) else {
                continue;
            };
            parts.retain(|part| part.get("type").and_then(Value::as_str) != Some("compaction"));
        }
        messages
    }

    fn strip_pi_compaction(entries: Vec<Value>) -> Vec<Value> {
        entries
            .into_iter()
            .filter(|entry| entry.get("type").and_then(Value::as_str) != Some("compaction"))
            .collect()
    }
}

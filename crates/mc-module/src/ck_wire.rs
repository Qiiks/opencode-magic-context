//! CK#1 ingress/egress wire types and the block-granular projection used inside MC.
//!
//! The transform receives full CK messages, but the cache machinery reasons about stable
//! block identities. This module owns that seam: parse the small CK typed core, flatten
//! each content block to a session-stable `mid#block_index` item, and retain the original
//! message objects so an unreduced response can pass them back without rebuilding them.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;

use mc_core::CkItem;
use mc_store::BlockIdentity;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

// The re-exported CK message/block serializers retain the original serde_json::Value
// for pass-through. That must remain a Value-level replay path, not a typed-struct
// round-trip, so harmless future CK fields are not silently dropped.
pub use mc_store::{
    CkKind, CkOutputKind, CkToolOutput, CkWireBlock, CkWireMessage, HarnessMeta, MediaBlock,
    MediaKind, MessageOrigin, OpaqueBlock, ProviderExtras, ResultBlock, ResultBlockKind,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CkIngressMessage {
    pub mid: String,
    pub ordinal: u64,
    pub ck: CkWireMessage,
}

/// The internal block item consumed by the cache-stability core. `bytes` is the
/// reduction-accounting basis, not provider-wire bytes; provider rendering is owned by
/// the producer after MC returns CK messages.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FlatBlock {
    pub id: String,
    pub mid: String,
    pub block_index: usize,
    pub ordinal: u64,
    pub role: String,
    pub kind_tag: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_input: Option<Value>,
    pub provider_executed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arc_id: Option<String>,
    pub bytes: String,
    pub synthetic: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_kind: Option<String>,
    #[serde(skip_serializing)]
    pub wire: CkWireBlock,
}

impl CkItem for FlatBlock {
    fn id(&self) -> &str {
        &self.id
    }

    fn ordinal(&self) -> u64 {
        self.ordinal
    }

    fn bytes(&self) -> &str {
        &self.bytes
    }

    fn synthetic(&self) -> bool {
        self.synthetic
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FlatProjection {
    pub blocks: Vec<FlatBlock>,
    pub identity_by_mid: BTreeMap<String, Vec<BlockIdentity>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CkWireError {
    MidContainsReservedHash(String),
    UnsupportedBlock {
        mid: String,
        block_index: usize,
        kind: String,
    },
    UnpairedToolResult {
        mid: String,
        block_index: usize,
        tool_call_id: String,
    },
}

impl std::fmt::Display for CkWireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CkWireError::MidContainsReservedHash(mid) => {
                write!(f, "message id contains reserved '#': {mid}")
            }
            CkWireError::UnsupportedBlock {
                mid,
                block_index,
                kind,
            } => write!(f, "unsupported CK block {kind} at {mid}#{block_index}"),
            CkWireError::UnpairedToolResult {
                mid,
                block_index,
                tool_call_id,
            } => write!(
                f,
                "tool_result {tool_call_id} at {mid}#{block_index} has no adjacent tool_call"
            ),
        }
    }
}

impl std::error::Error for CkWireError {}

pub fn project_messages(messages: &[CkIngressMessage]) -> Result<FlatProjection, CkWireError> {
    let mut blocks = Vec::new();
    let mut identity_by_mid: BTreeMap<String, Vec<BlockIdentity>> = BTreeMap::new();
    let mut pending_calls: BTreeMap<String, VecDeque<String>> = BTreeMap::new();

    for msg in messages {
        if msg.mid.contains('#') {
            return Err(CkWireError::MidContainsReservedHash(msg.mid.clone()));
        }

        let role = msg.ck.role.as_str();
        if role == "assistant" {
            pending_calls.clear();
            for (index, block) in msg.ck.content.iter().enumerate() {
                if let CkKind::ToolCall { id, .. } = &block.kind {
                    pending_calls
                        .entry(id.clone())
                        .or_default()
                        .push_back(block_id(&msg.mid, index));
                }
            }
        }

        let mut identities = Vec::new();
        for (index, block) in msg.ck.content.iter().enumerate() {
            ensure_supported(&msg.mid, index, block)?;
            let id = block_id(&msg.mid, index);
            let arc_id = arc_for_block(&msg.mid, index, &msg.ck, &mut pending_calls)?;
            let flat = flatten_block(msg, index, block, id, arc_id)?;
            if !flat.synthetic {
                identities.push(BlockIdentity {
                    kind_tag: flat.kind_tag.clone(),
                    byte_fingerprint: fingerprint(&flat.bytes),
                });
            }
            blocks.push(flat);
        }

        // A tool arc ends at the next non-tool-carrying turn, but the clear must run
        // AFTER this message's own blocks consume their pending calls: on the Anthropic
        // wire a tool_result may ride inside a USER message together with the user's
        // next text (Claude Code emits this when input arrives while a tool runs).
        // Clearing before the block walk made that legal shape unpairable — and because
        // ingress errors precede any state commit, one such message in the history
        // rejected every subsequent pass for the session's lifetime.
        if role != "assistant" && role != "tool" {
            pending_calls.clear();
        }
        if !msg.ck.meta.synthetic {
            identity_by_mid.insert(msg.mid.clone(), identities);
        }
    }

    Ok(FlatProjection {
        blocks,
        identity_by_mid,
    })
}

pub fn block_id(mid: &str, index: usize) -> String {
    format!("{mid}#{index}")
}

pub fn split_block_id(id: &str) -> Option<(&str, usize)> {
    let (mid, index) = id.rsplit_once('#')?;
    let index = index.parse().ok()?;
    Some((mid, index))
}

pub fn reduced_block(block: &CkWireBlock, reduced: &str, file_path: Option<&str>) -> CkWireBlock {
    let kind = match &block.kind {
        CkKind::ToolResult {
            id,
            tool_name,
            provider_executed,
            ..
        } => CkKind::ToolResult {
            id: id.clone(),
            tool_name: tool_name.clone(),
            output: CkToolOutput::bare(CkOutputKind::Text {
                text: reduced.to_string(),
            }),
            provider_executed: *provider_executed,
        },
        CkKind::ToolCall {
            id,
            name,
            provider_executed,
            ..
        } => {
            let mut input = serde_json::Map::new();
            input.insert("reduced".to_string(), Value::Bool(true));
            input.insert("summary".to_string(), Value::String(reduced.to_string()));
            if let Some(path) = file_path {
                input.insert("path".to_string(), Value::String(path.to_string()));
            }
            CkKind::ToolCall {
                id: id.clone(),
                name: name.clone(),
                input: Value::Object(input),
                provider_executed: *provider_executed,
            }
        }
        CkKind::Reasoning { .. } => CkKind::Reasoning {
            text: reduced.to_string(),
            signature: None,
        },
        CkKind::Text { .. } | CkKind::RedactedReasoning { .. } => CkKind::Text {
            text: reduced.to_string(),
        },
        CkKind::Media(_) | CkKind::Opaque(_) => CkKind::Text {
            text: reduced.to_string(),
        },
    };
    CkWireBlock::with_provider_extras(kind, block.provider_extras.clone())
}

pub fn text_from_message(msg: &CkWireMessage) -> Option<&str> {
    match msg.content.first()?.kind {
        CkKind::Text { ref text } => Some(text.as_str()),
        _ => None,
    }
}

fn flatten_block(
    msg: &CkIngressMessage,
    index: usize,
    block: &CkWireBlock,
    id: String,
    arc_id: Option<String>,
) -> Result<FlatBlock, CkWireError> {
    let bytes = serde_json::to_string(block).map_err(|_| CkWireError::UnsupportedBlock {
        mid: msg.mid.clone(),
        block_index: index,
        kind: block.kind.tag().to_string(),
    })?;
    let (name, file_path, tool_input, provider_executed, tool_call_id, output_kind) =
        match &block.kind {
            CkKind::ToolCall {
                id,
                name,
                input,
                provider_executed,
            } => (
                Some(name.clone()),
                extract_file_path(input),
                Some(input.clone()),
                *provider_executed,
                Some(id.clone()),
                None,
            ),
            CkKind::ToolResult {
                id,
                output,
                provider_executed,
                ..
            } => (
                None,
                None,
                None,
                *provider_executed,
                Some(id.clone()),
                Some(output.kind.tag().to_string()),
            ),
            _ => (None, None, None, false, None, None),
        };

    Ok(FlatBlock {
        id,
        mid: msg.mid.clone(),
        block_index: index,
        ordinal: msg.ordinal,
        role: msg.ck.role.clone(),
        kind_tag: block.kind.tag().to_string(),
        name,
        file_path,
        tool_input,
        provider_executed,
        arc_id,
        bytes,
        synthetic: msg.ck.meta.synthetic,
        tool_call_id,
        output_kind,
        wire: block.clone(),
    })
}

fn ensure_supported(mid: &str, block_index: usize, block: &CkWireBlock) -> Result<(), CkWireError> {
    match &block.kind {
        // Opaque is a first-class carrier (provider-native blocks the module must
        // never interpret): it projects like any block — verbatim bytes, arc data
        // internal to the block — and selection classifies it as never-reducible.
        // Media stays rejected until a canonical vector exists for it.
        CkKind::Opaque(_) => Ok(()),
        CkKind::Media(_) => Err(CkWireError::UnsupportedBlock {
            mid: mid.to_string(),
            block_index,
            kind: block.kind.tag().to_string(),
        }),
        CkKind::ToolResult { output, .. } => ensure_output_supported(mid, block_index, output),
        _ => Ok(()),
    }
}

fn ensure_output_supported(
    mid: &str,
    block_index: usize,
    output: &CkToolOutput,
) -> Result<(), CkWireError> {
    if let CkOutputKind::Content { blocks } = &output.kind {
        for block in blocks {
            match block.kind {
                ResultBlockKind::Media { .. } => {
                    return Err(CkWireError::UnsupportedBlock {
                        mid: mid.to_string(),
                        block_index,
                        kind: "tool_result.content.media".to_string(),
                    })
                }
                // Result-embedded Opaque carriers (e.g. screenshots inside MCP tool
                // results) get the same treatment as top-level Opaque blocks: verbatim
                // source-tagged bytes, never interpreted, projected back unchanged.
                // Rejecting them here wedged real Claude Code sessions, since any
                // image-returning tool poisons the history for every later pass.
                ResultBlockKind::Opaque { .. } => {}
                ResultBlockKind::Text { .. } => {}
            }
        }
    }
    Ok(())
}

fn arc_for_block(
    mid: &str,
    index: usize,
    msg: &CkWireMessage,
    pending_calls: &mut BTreeMap<String, VecDeque<String>>,
) -> Result<Option<String>, CkWireError> {
    match &msg.content[index].kind {
        CkKind::ToolCall { .. } if msg.role == "assistant" => Ok(Some(block_id(mid, index))),
        CkKind::ToolResult { id, .. } => {
            let Some(queue) = pending_calls.get_mut(id) else {
                return Err(CkWireError::UnpairedToolResult {
                    mid: mid.to_string(),
                    block_index: index,
                    tool_call_id: id.clone(),
                });
            };
            let Some(call_block_id) = queue.pop_front() else {
                return Err(CkWireError::UnpairedToolResult {
                    mid: mid.to_string(),
                    block_index: index,
                    tool_call_id: id.clone(),
                });
            };
            Ok(Some(call_block_id))
        }
        CkKind::Reasoning { .. } | CkKind::RedactedReasoning { .. } if msg.role == "assistant" => {
            Ok(adjacent_tool_call_arc(mid, index, &msg.content))
        }
        _ => Ok(None),
    }
}

fn adjacent_tool_call_arc(mid: &str, index: usize, content: &[CkWireBlock]) -> Option<String> {
    if index > 0 && matches!(content[index - 1].kind, CkKind::ToolCall { .. }) {
        return Some(block_id(mid, index - 1));
    }
    if index + 1 < content.len() && matches!(content[index + 1].kind, CkKind::ToolCall { .. }) {
        return Some(block_id(mid, index + 1));
    }
    None
}

fn extract_file_path(input: &Value) -> Option<String> {
    let obj = input.as_object()?;
    ["filePath", "file_path", "path"]
        .iter()
        .find_map(|key| obj.get(*key).and_then(Value::as_str).map(str::to_string))
}

fn fingerprint(bytes: &str) -> String {
    let digest = Sha256::digest(bytes.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

pub fn duplicate_ids(blocks: &[FlatBlock]) -> Option<String> {
    let mut seen = BTreeSet::new();
    for block in blocks {
        if !seen.insert(block.id.as_str()) {
            return Some(block.id.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_msg(mid: &str, ordinal: u64, role: &str, text: &str) -> CkIngressMessage {
        CkIngressMessage {
            mid: mid.to_string(),
            ordinal,
            ck: CkWireMessage::from_parts(
                role,
                vec![CkWireBlock::bare(CkKind::Text { text: text.into() })],
                None,
                ProviderExtras::new(),
                HarnessMeta::default(),
            ),
        }
    }

    fn assistant_with_call(mid: &str, ordinal: u64, call_id: &str) -> CkIngressMessage {
        CkIngressMessage {
            mid: mid.to_string(),
            ordinal,
            ck: CkWireMessage::from_parts(
                "assistant",
                vec![
                    CkWireBlock::bare(CkKind::Text {
                        text: "running a tool".into(),
                    }),
                    CkWireBlock::bare(CkKind::ToolCall {
                        id: call_id.to_string(),
                        name: "read".to_string(),
                        input: serde_json::json!({}),
                        provider_executed: false,
                    }),
                ],
                None,
                ProviderExtras::new(),
                HarnessMeta::default(),
            ),
        }
    }

    // Claude Code emits the tool_result INSIDE the next user message (alongside the
    // user's queued text) when input arrives while a tool is still running. The result
    // must pair against the prior assistant's call even though the carrying role is
    // "user"; the arc-window clear runs after the message's own blocks are walked.
    #[test]
    fn user_carried_tool_result_pairs_with_prior_assistant_call() {
        let user_with_result = CkIngressMessage {
            mid: "m2".to_string(),
            ordinal: 2,
            ck: CkWireMessage::from_parts(
                "user",
                vec![
                    CkWireBlock::bare(CkKind::ToolResult {
                        id: "toolu_1".to_string(),
                        tool_name: "read".to_string(),
                        output: CkToolOutput::bare(CkOutputKind::Text {
                            text: "file contents".into(),
                        }),
                        provider_executed: false,
                    }),
                    CkWireBlock::bare(CkKind::Text {
                        text: "queued user question".into(),
                    }),
                ],
                None,
                ProviderExtras::new(),
                HarnessMeta::default(),
            ),
        };
        let messages = vec![
            text_msg("m0", 0, "user", "start"),
            assistant_with_call("m1", 1, "toolu_1"),
            user_with_result,
        ];
        let projection = project_messages(&messages).expect("user-carried result must pair");
        let result_block = projection
            .blocks
            .iter()
            .find(|b| b.id == "m2#0")
            .expect("result block present");
        assert_eq!(
            result_block.arc_id.as_deref(),
            Some("m1#1"),
            "result pairs to the prior assistant's call block"
        );
        // The user message still ends the arc window: a later stray result must fail.
        let mut with_stray = messages.clone();
        with_stray.push(CkIngressMessage {
            mid: "m3".to_string(),
            ordinal: 3,
            ck: CkWireMessage::from_parts(
                "tool",
                vec![CkWireBlock::bare(CkKind::ToolResult {
                    id: "toolu_1".to_string(),
                    tool_name: "read".to_string(),
                    output: CkToolOutput::bare(CkOutputKind::Text {
                        text: "again".into(),
                    }),
                    provider_executed: false,
                })],
                None,
                ProviderExtras::new(),
                HarnessMeta::default(),
            ),
        });
        let err = project_messages(&with_stray).expect_err("arc window closed by user turn");
        assert!(matches!(err, CkWireError::UnpairedToolResult { .. }));
    }

    // A genuinely orphaned result in a user message (no prior assistant call) still
    // fails loud — the fix moved the arc-window clear, it did not weaken pairing.
    #[test]
    fn user_carried_tool_result_without_prior_call_still_rejects() {
        let messages = vec![
            text_msg("m0", 0, "user", "start"),
            CkIngressMessage {
                mid: "m1".to_string(),
                ordinal: 1,
                ck: CkWireMessage::from_parts(
                    "user",
                    vec![CkWireBlock::bare(CkKind::ToolResult {
                        id: "toolu_orphan".to_string(),
                        tool_name: "read".to_string(),
                        output: CkToolOutput::bare(CkOutputKind::Text { text: "x".into() }),
                        provider_executed: false,
                    })],
                    None,
                    ProviderExtras::new(),
                    HarnessMeta::default(),
                ),
            },
        ];
        let err = project_messages(&messages).expect_err("orphan result must reject");
        assert!(matches!(err, CkWireError::UnpairedToolResult { .. }));
    }

    // Opaque carriers inside tool_result content blocks (e.g. screenshots returned by
    // MCP tools) are first-class verbatim carriers, same as top-level Opaque blocks.
    // Media inside results stays rejected.
    #[test]
    fn opaque_inside_tool_result_content_is_accepted_and_projected() {
        let result_with_opaque = CkIngressMessage {
            mid: "m2".to_string(),
            ordinal: 2,
            ck: CkWireMessage::from_parts(
                "tool",
                vec![CkWireBlock::bare(CkKind::ToolResult {
                    id: "toolu_1".to_string(),
                    tool_name: "computer".to_string(),
                    output: CkToolOutput::bare(CkOutputKind::Content {
                        blocks: vec![
                            ResultBlock {
                                kind: ResultBlockKind::Text {
                                    text: "screenshot captured".into(),
                                },
                                provider_extras: ProviderExtras::new(),
                            },
                            ResultBlock {
                                kind: ResultBlockKind::Opaque {
                                    opaque: OpaqueBlock {
                                        source: serde_json::json!({"source": "wire", "wire": "anthropic"}),
                                        kind: "image".to_string(),
                                        raw: serde_json::json!([1, 2, 3]),
                                        arc: None,
                                    },
                                },
                                provider_extras: ProviderExtras::new(),
                            },
                        ],
                    }),
                    provider_executed: false,
                })],
                None,
                ProviderExtras::new(),
                HarnessMeta::default(),
            ),
        };
        let messages = vec![
            text_msg("m0", 0, "user", "start"),
            assistant_with_call("m1", 1, "toolu_1"),
            result_with_opaque,
        ];
        let projection =
            project_messages(&messages).expect("result-embedded opaque must be accepted");
        assert!(projection.blocks.iter().any(|b| b.id == "m2#0"));

        // Media inside a result is still rejected.
        let mut with_media = messages;
        if let CkKind::ToolResult { output, .. } = &mut with_media[2].ck.content[0].kind {
            if let CkOutputKind::Content { blocks } = &mut output.kind {
                blocks[1].kind = ResultBlockKind::Media {
                    media: MediaBlock {
                        kind: MediaKind::Image,
                        media_type: "image/png".to_string(),
                        filename: None,
                        source: serde_json::json!({}),
                    },
                };
            }
        }
        let err = project_messages(&with_media).expect_err("media in result stays rejected");
        assert!(matches!(err, CkWireError::UnsupportedBlock { .. }));
    }
}

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
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub type ProviderExtras = BTreeMap<String, BTreeMap<String, Value>>;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinal: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub synthetic: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageOrigin {
    pub provider: String,
    pub model: String,
    pub api: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CkWireMessage {
    pub role: String,
    pub content: Vec<CkWireBlock>,
    pub origin: Option<MessageOrigin>,
    pub provider_extras: ProviderExtras,
    pub meta: HarnessMeta,
    /// Original parsed JSON for pass-through messages. Serializing this value instead of
    /// typed fields makes the identity rule independent of whether every harmless unknown
    /// field has a typed home in this crate.
    original: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CkWireMessageData {
    pub role: String,
    pub content: Vec<CkWireBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<MessageOrigin>,
    #[serde(default, skip_serializing_if = "ProviderExtras::is_empty")]
    pub provider_extras: ProviderExtras,
    #[serde(default)]
    pub meta: HarnessMeta,
}

impl<'de> Deserialize<'de> for CkWireMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let original = Value::deserialize(deserializer)?;
        let data =
            CkWireMessageData::deserialize(original.clone()).map_err(serde::de::Error::custom)?;
        Ok(Self {
            role: data.role,
            content: data.content,
            origin: data.origin,
            provider_extras: data.provider_extras,
            meta: data.meta,
            original: Some(original),
        })
    }
}

impl Serialize for CkWireMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if let Some(original) = &self.original {
            return original.serialize(serializer);
        }
        CkWireMessageData {
            role: self.role.clone(),
            content: self.content.clone(),
            origin: self.origin.clone(),
            provider_extras: self.provider_extras.clone(),
            meta: self.meta.clone(),
        }
        .serialize(serializer)
    }
}

impl CkWireMessage {
    pub fn from_parts(
        role: impl Into<String>,
        content: Vec<CkWireBlock>,
        origin: Option<MessageOrigin>,
        provider_extras: ProviderExtras,
        meta: HarnessMeta,
    ) -> Self {
        Self {
            role: role.into(),
            content,
            origin,
            provider_extras,
            meta,
            original: None,
        }
    }

    pub fn synthetic_user_text(text: impl Into<String>) -> Self {
        Self::from_parts(
            "user",
            vec![CkWireBlock::bare(CkKind::Text { text: text.into() })],
            None,
            ProviderExtras::new(),
            HarnessMeta {
                synthetic: true,
                ..Default::default()
            },
        )
    }

    pub fn mark_modified(&mut self) {
        self.original = None;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CkWireBlock {
    pub kind: CkKind,
    pub provider_extras: ProviderExtras,
    original: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CkWireBlockData {
    pub kind: CkKind,
    #[serde(default, skip_serializing_if = "ProviderExtras::is_empty")]
    pub provider_extras: ProviderExtras,
}

impl<'de> Deserialize<'de> for CkWireBlock {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let original = Value::deserialize(deserializer)?;
        let data =
            CkWireBlockData::deserialize(original.clone()).map_err(serde::de::Error::custom)?;
        Ok(Self {
            kind: data.kind,
            provider_extras: data.provider_extras,
            original: Some(original),
        })
    }
}

impl Serialize for CkWireBlock {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        if let Some(original) = &self.original {
            return original.serialize(serializer);
        }
        CkWireBlockData {
            kind: self.kind.clone(),
            provider_extras: self.provider_extras.clone(),
        }
        .serialize(serializer)
    }
}

impl CkWireBlock {
    pub fn bare(kind: CkKind) -> Self {
        Self {
            kind,
            provider_extras: ProviderExtras::new(),
            original: None,
        }
    }

    pub fn with_provider_extras(kind: CkKind, provider_extras: ProviderExtras) -> Self {
        Self {
            kind,
            provider_extras,
            original: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CkKind {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    RedactedReasoning {
        data: String,
    },
    ToolCall {
        id: String,
        name: String,
        input: Value,
        #[serde(default)]
        provider_executed: bool,
    },
    ToolResult {
        id: String,
        tool_name: String,
        output: CkToolOutput,
        #[serde(default)]
        provider_executed: bool,
    },
    Media(MediaBlock),
    Opaque(OpaqueBlock),
}

impl CkKind {
    pub fn tag(&self) -> &'static str {
        match self {
            CkKind::Text { .. } => "text",
            CkKind::Reasoning { .. } => "reasoning",
            CkKind::RedactedReasoning { .. } => "redacted_reasoning",
            CkKind::ToolCall { .. } => "tool_call",
            CkKind::ToolResult { .. } => "tool_result",
            CkKind::Media(_) => "media",
            CkKind::Opaque(_) => "opaque",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CkToolOutput {
    pub kind: CkOutputKind,
    #[serde(default, skip_serializing_if = "ProviderExtras::is_empty")]
    pub provider_extras: ProviderExtras,
}

impl CkToolOutput {
    pub fn bare(kind: CkOutputKind) -> Self {
        Self {
            kind,
            provider_extras: ProviderExtras::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CkOutputKind {
    Text { text: String },
    Json { value: Value },
    ErrorText { text: String },
    ErrorJson { value: Value },
    ExecutionDenied { reason: Option<String> },
    Content { blocks: Vec<ResultBlock> },
}

impl CkOutputKind {
    pub fn tag(&self) -> &'static str {
        match self {
            CkOutputKind::Text { .. } => "text",
            CkOutputKind::Json { .. } => "json",
            CkOutputKind::ErrorText { .. } => "error_text",
            CkOutputKind::ErrorJson { .. } => "error_json",
            CkOutputKind::ExecutionDenied { .. } => "execution_denied",
            CkOutputKind::Content { .. } => "content",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultBlock {
    pub kind: ResultBlockKind,
    #[serde(default, skip_serializing_if = "ProviderExtras::is_empty")]
    pub provider_extras: ProviderExtras,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResultBlockKind {
    Text { text: String },
    Media { media: MediaBlock },
    Opaque { opaque: OpaqueBlock },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpaqueBlock {
    pub source: Value,
    pub kind: String,
    pub raw: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arc: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaBlock {
    pub kind: MediaKind,
    pub media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    pub source: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Audio,
    Video,
    File,
    Document,
}

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
        } else if role != "tool" {
            pending_calls.clear();
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
        CkKind::Media(_) | CkKind::Opaque(_) => Err(CkWireError::UnsupportedBlock {
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
                ResultBlockKind::Opaque { .. } => {
                    return Err(CkWireError::UnsupportedBlock {
                        mid: mid.to_string(),
                        block_index,
                        kind: "tool_result.content.opaque".to_string(),
                    })
                }
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

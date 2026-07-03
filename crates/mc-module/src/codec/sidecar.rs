use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::ck_wire::CkIngressMessage;

/// A decoded compaction marker from a harness transcript.
///
/// This is an input fact extracted from the harness's own compaction marker. It
/// is not a caller-supplied cache anchor: the module still decides whether the
/// boundary is present and how cache-core state should consume it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractedBoundary {
    pub harness: String,
    pub message_id: String,
    pub ordinal: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub part_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_id: Option<String>,
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecodedHarnessMessages {
    pub messages: Vec<CkIngressMessage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary: Option<ExtractedBoundary>,
    pub sidecar: DecodeSidecar,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecodeSidecar {
    pub harness: String,
    #[serde(default)]
    pub order: Vec<String>,
    #[serde(default)]
    pub messages: BTreeMap<String, HarnessMessageMeta>,
    #[serde(default)]
    pub mid_pins: BTreeMap<String, String>,
}

impl DecodeSidecar {
    pub fn new(harness: impl Into<String>) -> Self {
        Self {
            harness: harness.into(),
            order: Vec::new(),
            messages: BTreeMap::new(),
            mid_pins: BTreeMap::new(),
        }
    }

    pub fn remember_message(&mut self, mid: String, meta: HarnessMessageMeta) {
        if !self.messages.contains_key(&mid) {
            self.order.push(mid.clone());
        }
        self.messages.insert(mid, meta);
    }

    pub fn message_by_mid(&self, mid: &str) -> Option<&HarnessMessageMeta> {
        self.messages.get(mid)
    }

    pub fn message_for_index(&self, index: usize) -> Option<&HarnessMessageMeta> {
        self.order
            .get(index)
            .and_then(|mid| self.messages.get(mid.as_str()))
    }

    pub fn inherit_pin(&self, stable_key: &str) -> Option<String> {
        self.mid_pins.get(stable_key).cloned()
    }

    pub fn pin_mid(&mut self, stable_key: impl Into<String>, mid: impl Into<String>) {
        self.mid_pins.insert(stable_key.into(), mid.into());
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessMessageMeta {
    pub mid: String,
    pub ordinal: u64,
    pub role: String,
    pub raw: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_key: Option<String>,
    #[serde(default)]
    pub blocks: Vec<BlockMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockMeta {
    pub block_index: usize,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_index: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
    pub raw: Value,
}

pub fn stable_hash(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let digest = Sha256::digest(bytes);
    hex_prefix(&digest, digest.len())
}

pub fn stable_hash_prefix(value: &Value, chars: usize) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let digest = Sha256::digest(bytes);
    hex_prefix(&digest, chars.div_ceil(2))
        .chars()
        .take(chars)
        .collect()
}

fn hex_prefix(bytes: &[u8], count: usize) -> String {
    let mut out = String::with_capacity(count * 2);
    for byte in bytes.iter().take(count) {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

pub fn meta_for_ck<'a>(
    sidecar: &'a DecodeSidecar,
    msg: &'a crate::ck_wire::CkWireMessage,
    index: usize,
) -> Option<&'a HarnessMessageMeta> {
    msg.meta
        .harness_id
        .as_deref()
        .and_then(|mid| sidecar.message_by_mid(mid))
        .or_else(|| sidecar.message_for_index(index))
}

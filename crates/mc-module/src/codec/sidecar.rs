use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::ck_wire::{CkIngressMessage, CkWireBlock};

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

    pub fn synthetic_message_for_index(&self, index: usize) -> Option<&HarnessMessageMeta> {
        self.order
            .iter()
            .filter_map(|mid| self.messages.get(mid.as_str()))
            .filter(|meta| is_synthetic_message(meta))
            .nth(index)
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

pub(crate) struct MatchedBlockMetas<'a> {
    pub(crate) by_block: Vec<Option<&'a BlockMeta>>,
    retained_native_indices: BTreeSet<usize>,
    decoded_native_indices: BTreeSet<usize>,
}

impl MatchedBlockMetas<'_> {
    pub(crate) fn remove_unretained_native_parts<T>(&self, parts: Vec<T>) -> Vec<T> {
        parts
            .into_iter()
            .enumerate()
            .filter_map(|(native_index, part)| {
                let decoded_block_was_removed = self.decoded_native_indices.contains(&native_index)
                    && !self.retained_native_indices.contains(&native_index);
                (!decoded_block_was_removed).then_some(part)
            })
            .collect()
    }
}

pub(crate) fn match_block_metas<'a>(
    blocks: &[CkWireBlock],
    metas: &'a [BlockMeta],
    mut matches: impl FnMut(&CkWireBlock, &BlockMeta) -> bool,
) -> MatchedBlockMetas<'a> {
    let mut meta_cursor = 0;
    let by_block = blocks
        .iter()
        .map(|block| {
            let match_index = metas[meta_cursor..]
                .iter()
                .position(|meta| matches(block, meta))
                .map(|offset| meta_cursor + offset);
            if let Some(match_index) = match_index {
                meta_cursor = match_index + 1;
                Some(&metas[match_index])
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    let retained_native_indices = by_block
        .iter()
        .filter_map(|meta| meta.and_then(|meta| meta.native_index))
        .collect();
    let decoded_native_indices = metas.iter().filter_map(|meta| meta.native_index).collect();

    MatchedBlockMetas {
        by_block,
        retained_native_indices,
        decoded_native_indices,
    }
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
        .or_else(|| {
            (!msg.meta.synthetic)
                .then(|| sidecar.message_for_index(index))
                .flatten()
        })
}

fn is_synthetic_message(meta: &HarnessMessageMeta) -> bool {
    let Some(parts) = meta.raw.get("parts").and_then(Value::as_array) else {
        return false;
    };
    !parts.is_empty()
        && parts.iter().all(|part| {
            part.get("synthetic")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
}

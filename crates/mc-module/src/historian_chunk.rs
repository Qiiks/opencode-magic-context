//! Production historian chunk assembly from CK flat blocks.

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use mc_store::{McStore, StoredCompartment};
use mc_tokenizer::estimate_tokens;
use regex::Regex;
use serde_json::Value;

use crate::boundary::BoundaryResolution;
use crate::historian::{compute_chunk_fingerprint, ChunkSnapshotItem, HistorianFireRequest};
use crate::historian_prompt::{
    build_compartment_agent_prompt, build_reference_blocks_from_stored,
    render_historian_memory_block, CompartmentPromptInputs,
};
use crate::historian_validate::{
    ChunkLine, HistorianChunk, MessageRange, StoredCompartmentRange, ValidateOptions,
};
use crate::transform::ck_wire::{block_id, CkIngressMessage, CkKind, FlatBlock};

const MAX_COMMITS_PER_BLOCK: usize = 5;
const SYSTEM_DIRECTIVE_PREFIX: &str = "[SYSTEM DIRECTIVE: MAGIC-CONTEXT";
const OMO_INTERNAL_INITIATOR_MARKER: &str = "<!-- OMO_INTERNAL_INITIATOR -->";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkSnapshotOwnedItem {
    pub id: String,
    pub kind: String,
    pub bytes: String,
}

impl ChunkSnapshotOwnedItem {
    pub fn as_item(&self) -> ChunkSnapshotItem<'_> {
        ChunkSnapshotItem {
            id: &self.id,
            kind: &self.kind,
            bytes: &self.bytes,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistorianBuiltChunk {
    pub text: String,
    pub chunk: HistorianChunk,
    pub snapshot: Vec<ChunkSnapshotOwnedItem>,
    pub end_message_id: String,
    pub token_estimate: usize,
    pub has_more: bool,
    pub commit_cluster_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MessageMeta {
    ordinal: u64,
    message_id: String,
}

#[derive(Debug, Clone)]
struct FlatMessage<'a> {
    mid: &'a str,
    ordinal: u64,
    role: &'a str,
    blocks: Vec<&'a FlatBlock>,
}

#[derive(Debug, Clone)]
struct ChunkBlock {
    role: String,
    start_ordinal: u64,
    end_ordinal: u64,
    parts: Vec<String>,
    meta: Vec<MessageMeta>,
    commit_hashes: Vec<String>,
    is_tool_only: bool,
}

#[derive(Debug, Clone)]
struct CompactedText {
    text: String,
    commit_hashes: Vec<String>,
}

#[derive(Debug)]
struct Builder {
    budget: usize,
    total_tokens: usize,
    lines: Vec<String>,
    line_meta: Vec<ChunkLine>,
    pending_noise_meta: Vec<MessageMeta>,
    current_block: Option<ChunkBlock>,
    tool_only_ranges: Vec<MessageRange>,
    last_ordinal: u64,
    last_message_id: String,
    commit_cluster_count: usize,
    last_flushed_role: String,
    tool_call_summaries: HashMap<String, String>,
}

impl Builder {
    fn new(
        budget: usize,
        start_ordinal: u64,
        tool_call_summaries: HashMap<String, String>,
    ) -> Self {
        Self {
            budget,
            total_tokens: 0,
            lines: Vec::new(),
            line_meta: Vec::new(),
            pending_noise_meta: Vec::new(),
            current_block: None,
            tool_only_ranges: Vec::new(),
            last_ordinal: start_ordinal.saturating_sub(1),
            last_message_id: String::new(),
            commit_cluster_count: 0,
            last_flushed_role: String::new(),
            tool_call_summaries,
        }
    }

    fn push_message(&mut self, message: &FlatMessage<'_>) -> bool {
        let meta = MessageMeta {
            ordinal: message.ordinal,
            message_id: last_block_id(message),
        };

        if message.role == "tool" && !has_text_parts(message) {
            let summaries = extract_tool_result_summaries(message, &self.tool_call_summaries);
            if summaries.is_empty() {
                self.pending_noise_meta.push(meta);
                return true;
            }
            return self.absorb_tool_only(meta, message.ordinal, summaries);
        }

        if message.role == "user" && !has_meaningful_user_text(message) {
            let tc_summaries = extract_tool_call_summaries(message);
            if tc_summaries.is_empty() {
                self.pending_noise_meta.push(meta);
                return true;
            }
            return self.absorb_tool_only(meta, message.ordinal, tc_summaries);
        }

        let role = compact_role(message.role);
        let text_parts = text_parts(message);
        let tool_summaries = if text_parts.is_empty() {
            extract_tool_call_summaries(message)
        } else {
            Vec::new()
        };
        let mut all_parts = text_parts.clone();
        all_parts.extend(tool_summaries);
        let compacted = compact_text_for_summary(&all_parts.join(" / "), message.role);
        if compacted.text.is_empty() {
            self.pending_noise_meta.push(meta);
            return true;
        }

        let msg_has_narrative = !text_parts.is_empty();
        if let Some(current) = self
            .current_block
            .as_mut()
            .filter(|block| block.role == role)
        {
            current.end_ordinal = message.ordinal;
            current.parts.push(compacted.text);
            current.meta.append(&mut self.pending_noise_meta);
            current.meta.push(meta);
            current.commit_hashes =
                merge_commit_hashes(&current.commit_hashes, &compacted.commit_hashes);
            if msg_has_narrative {
                current.is_tool_only = false;
            }
            return true;
        }

        if !self.flush_current_block() {
            return false;
        }
        let start = self
            .pending_noise_meta
            .first()
            .map(|meta| meta.ordinal)
            .unwrap_or(message.ordinal);
        let mut meta_list = std::mem::take(&mut self.pending_noise_meta);
        meta_list.push(meta);
        self.current_block = Some(ChunkBlock {
            role,
            start_ordinal: start,
            end_ordinal: message.ordinal,
            parts: vec![compacted.text],
            meta: meta_list,
            commit_hashes: compacted.commit_hashes,
            is_tool_only: !msg_has_narrative,
        });
        true
    }

    fn absorb_tool_only(
        &mut self,
        meta: MessageMeta,
        ordinal: u64,
        summaries: Vec<String>,
    ) -> bool {
        let tc_text = if summaries.is_empty() {
            String::new()
        } else {
            summaries.join(" / ")
        };
        if let Some(current) = self
            .current_block
            .as_mut()
            .filter(|block| block.role == "A")
        {
            current.end_ordinal = ordinal;
            if !tc_text.is_empty() {
                current.parts.push(tc_text);
            }
            current.meta.append(&mut self.pending_noise_meta);
            current.meta.push(meta);
            return true;
        }
        if !self.flush_current_block() {
            return false;
        }
        let start = self
            .pending_noise_meta
            .first()
            .map(|meta| meta.ordinal)
            .unwrap_or(ordinal);
        let mut meta_list = std::mem::take(&mut self.pending_noise_meta);
        meta_list.push(meta);
        if tc_text.is_empty() {
            self.pending_noise_meta = meta_list;
            return true;
        }
        let parts = vec![tc_text];
        self.current_block = Some(ChunkBlock {
            role: "A".to_string(),
            start_ordinal: start,
            end_ordinal: ordinal,
            parts,
            meta: meta_list,
            commit_hashes: Vec::new(),
            is_tool_only: true,
        });
        true
    }

    fn flush_current_block(&mut self) -> bool {
        let Some(block) = self.current_block.take() else {
            return true;
        };
        let block_text = format_block(&block);
        let block_tokens = estimate_tokens(&block_text);
        if self.total_tokens + block_tokens > self.budget && self.total_tokens > 0 {
            self.current_block = Some(block);
            return false;
        }
        if block.role == "A" && !block.commit_hashes.is_empty() && self.last_flushed_role != "A" {
            self.commit_cluster_count += 1;
        }
        self.last_flushed_role.clone_from(&block.role);
        self.last_ordinal = block
            .meta
            .last()
            .map(|meta| meta.ordinal)
            .unwrap_or(block.end_ordinal);
        self.last_message_id = block
            .meta
            .last()
            .map(|meta| meta.message_id.clone())
            .unwrap_or_default();
        self.line_meta
            .extend(block.meta.iter().map(|meta| ChunkLine {
                ordinal: meta.ordinal,
                message_id: meta.message_id.clone(),
            }));
        self.lines.push(block_text);
        self.total_tokens += block_tokens;
        if block.is_tool_only {
            self.tool_only_ranges.push(MessageRange {
                start: block.start_ordinal,
                end: block.end_ordinal,
            });
        }
        true
    }
}

pub fn build_historian_chunk(
    messages: &[CkIngressMessage],
    blocks: &[FlatBlock],
    start_ordinal: u64,
    token_budget: usize,
    eligible_end_ordinal: u64,
) -> HistorianBuiltChunk {
    let total_count = messages
        .iter()
        .filter(|message| !message.ck.meta.synthetic)
        .map(|message| message.ordinal)
        .max()
        .unwrap_or(0);
    let start = start_ordinal;
    let tool_call_summaries = build_tool_call_summary_lookup(blocks);
    let mut builder = Builder::new(token_budget, start, tool_call_summaries);
    let blocks_by_mid = grouped_blocks_by_mid(blocks);
    for message in messages.iter().filter(|message| !message.ck.meta.synthetic) {
        if message.ordinal >= eligible_end_ordinal {
            break;
        }
        let role = message.ck.role.as_str();
        if message.ordinal < start || role == "system" {
            continue;
        }
        let flat_message = FlatMessage {
            mid: message.mid.as_str(),
            ordinal: message.ordinal,
            role,
            blocks: blocks_by_mid
                .get(message.mid.as_str())
                .cloned()
                .unwrap_or_default(),
        };
        if !builder.push_message(&flat_message) {
            break;
        }
    }
    let _ = builder.flush_current_block();
    let tool_only_ranges = merge_tool_only_ranges(&builder.tool_only_ranges);
    let end = builder.last_ordinal;
    let snapshot = blocks
        .iter()
        .filter(|block| {
            !block.synthetic
                && block.role != "system"
                && block.ordinal >= start
                && block.ordinal <= end
        })
        .map(|block| ChunkSnapshotOwnedItem {
            id: block.id.clone(),
            kind: block.kind_tag.clone(),
            bytes: block.bytes.clone(),
        })
        .collect();
    HistorianBuiltChunk {
        text: builder.lines.join("\n"),
        chunk: HistorianChunk {
            start_index: start,
            end_index: end,
            lines: builder.line_meta,
            tool_only_ranges,
        },
        snapshot,
        end_message_id: builder.last_message_id,
        token_estimate: builder.total_tokens,
        has_more: end < eligible_end_ordinal.saturating_sub(1).min(total_count),
        commit_cluster_count: builder.commit_cluster_count,
    }
}

#[derive(Debug, Clone)]
pub struct HistorianAssemblerConfig {
    pub session_id: String,
    pub project_path: String,
    pub project_slug: String,
    pub model_chain: Vec<String>,
    pub token_budget: usize,
    pub boundary: BoundaryResolution,
    pub memory_enabled: bool,
    pub extraction_free: bool,
    pub in_emergency: bool,
    pub failure_backoff_at_ms: i64,
    pub min_chunk_tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistorianNoFireReason {
    NoModels,
    EmptyEligibleRange {
        start_ordinal: u64,
        eligible_end_ordinal: u64,
    },
    EmptyChunk,
    BelowBudget {
        token_estimate: usize,
        minimum: usize,
    },
}

#[derive(Debug, Clone)]
pub struct AssembledHistorianFiring {
    pub prompt: String,
    pub model_chain: Vec<String>,
    pub chunk: HistorianBuiltChunk,
    pub chunk_fingerprint: String,
    pub expected_revert_epoch: u64,
    pub prior_compartments: Vec<StoredCompartmentRange>,
    pub validate_options: ValidateOptions,
    pub from_ordinal: u64,
    pub to_ordinal: u64,
    pub now_ms: i64,
    pub failure_backoff_at_ms: i64,
}

impl AssembledHistorianFiring {
    pub fn as_fire_request<'a>(
        &'a self,
        store: &'a McStore,
        session_id: &'a str,
        project_path: &'a str,
        project_slug: &'a str,
    ) -> HistorianFireRequest<'a> {
        HistorianFireRequest {
            store,
            session_id,
            project_path,
            project_slug,
            // The role-scoped historian system prompt. The assembler builds the USER
            // prompt (chunk + references); the system prompt is the vendored constant —
            // per-firing static, byte-identical across firings.
            system: crate::historian_prompt::HISTORIAN_SYSTEM_PROMPT,
            prompt: &self.prompt,
            model_chain: &self.model_chain,
            from_ordinal: self.from_ordinal,
            to_ordinal: self.to_ordinal,
            chunk_fingerprint: &self.chunk_fingerprint,
            expected_revert_epoch: self.expected_revert_epoch,
            observed_chunk_fingerprint: &self.chunk_fingerprint,
            validation_chunk: &self.chunk.chunk,
            prior_compartments: &self.prior_compartments,
            validate_options: self.validate_options,
            now_ms: self.now_ms,
            failure_backoff_at_ms: self.failure_backoff_at_ms,
        }
    }
}

#[derive(Debug, Clone)]
pub enum AssembleHistorianFiringOutcome {
    Fire(Box<AssembledHistorianFiring>),
    NoFire(HistorianNoFireReason),
}

pub fn assemble_historian_firing(
    store: &McStore,
    messages: &[CkIngressMessage],
    live: &[FlatBlock],
    config: HistorianAssemblerConfig,
    now_ms: i64,
) -> Result<AssembleHistorianFiringOutcome, mc_store::McStoreError> {
    if config.model_chain.is_empty() {
        return Ok(AssembleHistorianFiringOutcome::NoFire(
            HistorianNoFireReason::NoModels,
        ));
    }
    let snapshot = store.load_historian_assembly_snapshot(&config.session_id)?;
    let compartments = snapshot.compartments;
    let expected_revert_epoch = snapshot.revert_epoch;
    let eligible_end = config.boundary.eligible_head.end;
    let chunk_start =
        if let Some(last_end) = compartments.iter().map(|c| c.end_message as u64).max() {
            last_end.saturating_add(1)
        } else {
            let Some(first_live_eligible) = messages
                .iter()
                .filter(|message| !message.ck.meta.synthetic)
                .filter(|message| message.ck.role != "system")
                .map(|message| message.ordinal)
                .filter(|ordinal| *ordinal < eligible_end)
                .min()
            else {
                return Ok(AssembleHistorianFiringOutcome::NoFire(
                    HistorianNoFireReason::EmptyChunk,
                ));
            };
            first_live_eligible
        };
    if chunk_start >= eligible_end {
        return Ok(AssembleHistorianFiringOutcome::NoFire(
            HistorianNoFireReason::EmptyEligibleRange {
                start_ordinal: chunk_start,
                eligible_end_ordinal: eligible_end,
            },
        ));
    }
    let chunk = build_historian_chunk(
        messages,
        live,
        chunk_start,
        config.token_budget,
        eligible_end,
    );
    if chunk.text.is_empty() || chunk.chunk.lines.is_empty() {
        return Ok(AssembleHistorianFiringOutcome::NoFire(
            HistorianNoFireReason::EmptyChunk,
        ));
    }
    if chunk.token_estimate < config.min_chunk_tokens {
        return Ok(AssembleHistorianFiringOutcome::NoFire(
            HistorianNoFireReason::BelowBudget {
                token_estimate: chunk.token_estimate,
                minimum: config.min_chunk_tokens,
            },
        ));
    }

    let reference_blocks =
        build_reference_blocks_from_stored(&config.session_id, chunk_start as i64, &compartments);
    let memories = store.load_active_memories(&config.project_path, now_ms)?;
    let memory_block = render_historian_memory_block(&memories);
    let prompt = build_compartment_agent_prompt(&CompartmentPromptInputs {
        seed_examples: &reference_blocks.seed_examples,
        session_references: &reference_blocks.session_references,
        project_memory: &memory_block,
        input_source: &truncate_historian_input_if_needed(&chunk.text, config.token_budget),
        memory_enabled: config.memory_enabled,
        extraction_free: config.extraction_free,
    });
    let fingerprint_items: Vec<_> = chunk
        .snapshot
        .iter()
        .map(ChunkSnapshotOwnedItem::as_item)
        .collect();
    let chunk_fingerprint = compute_chunk_fingerprint(&fingerprint_items);
    let prior_compartments = compartments.iter().map(stored_range).collect();
    let sequence_offset = compartments
        .iter()
        .map(|c| c.sequence as u64)
        .max()
        .unwrap_or(0)
        .saturating_add(1);

    Ok(AssembleHistorianFiringOutcome::Fire(Box::new(
        AssembledHistorianFiring {
            prompt,
            model_chain: config.model_chain,
            from_ordinal: chunk.chunk.start_index,
            to_ordinal: chunk.chunk.end_index,
            chunk_fingerprint,
            expected_revert_epoch,
            prior_compartments,
            validate_options: ValidateOptions {
                sequence_offset,
                in_emergency: config.in_emergency,
            },
            now_ms,
            failure_backoff_at_ms: config.failure_backoff_at_ms,
            chunk,
        },
    )))
}

const HISTORIAN_TRUNCATION_MARKER: &str =
    "\n[… tokens truncated by Magic Context to fit the historian window …]";

pub fn truncate_historian_input_if_needed(input: &str, token_budget: usize) -> String {
    if estimate_tokens(input) <= token_budget {
        return input.to_string();
    }

    // TypeScript slices by UTF-16 code units. Rust strings cannot be sliced at
    // arbitrary UTF-16 offsets, so this port uses Unicode scalar-value indices
    // while preserving the marker bytes, `<=` budget checks, and midpoint math.
    let mut lo = 0usize;
    let mut hi = input.chars().count();
    let mut best = 0usize;
    while lo <= hi {
        let mid = (lo + hi) >> 1;
        let candidate = format!(
            "{}{}",
            input.chars().take(mid).collect::<String>(),
            HISTORIAN_TRUNCATION_MARKER
        );
        if estimate_tokens(&candidate) <= token_budget {
            best = mid;
            lo = mid + 1;
        } else if mid == 0 {
            break;
        } else {
            hi = mid - 1;
        }
    }

    format!(
        "{}{}",
        input.chars().take(best).collect::<String>(),
        HISTORIAN_TRUNCATION_MARKER
    )
}

pub(crate) fn stored_range(c: &StoredCompartment) -> StoredCompartmentRange {
    StoredCompartmentRange {
        start_message: c.start_message as u64,
        end_message: c.end_message as u64,
    }
}

fn grouped_blocks_by_mid(blocks: &[FlatBlock]) -> BTreeMap<&str, Vec<&FlatBlock>> {
    let mut grouped: BTreeMap<&str, Vec<&FlatBlock>> = BTreeMap::new();
    for block in blocks.iter().filter(|block| !block.synthetic) {
        grouped.entry(block.mid.as_str()).or_default().push(block);
    }
    grouped
}

fn last_block_id(message: &FlatMessage<'_>) -> String {
    message
        .blocks
        .last()
        .map(|block| block.id.clone())
        .unwrap_or_else(|| block_id(message.mid, 0))
}

fn text_parts(message: &FlatMessage<'_>) -> Vec<String> {
    message
        .blocks
        .iter()
        .filter_map(|block| match &block.wire.kind {
            CkKind::Text { text } => {
                let cleaned = if message.role == "user" {
                    clean_user_text(text)
                } else {
                    text.trim().to_string()
                };
                let normalized = normalize_text(&cleaned);
                (!normalized.is_empty()).then_some(normalized)
            }
            _ => None,
        })
        .collect()
}

fn has_text_parts(message: &FlatMessage<'_>) -> bool {
    !text_parts(message).is_empty()
}

fn has_meaningful_user_text(message: &FlatMessage<'_>) -> bool {
    text_parts(message)
        .iter()
        .any(|text| !text.is_empty() && !is_system_directive(text))
}

fn extract_tool_call_summaries(message: &FlatMessage<'_>) -> Vec<String> {
    let mut summaries = Vec::new();
    for block in &message.blocks {
        let CkKind::ToolCall { name, input, .. } = &block.wire.kind else {
            continue;
        };
        summaries.push(format_tool_summary(name, input));
    }
    summaries
}

fn extract_tool_result_summaries(
    message: &FlatMessage<'_>,
    tool_call_summaries: &HashMap<String, String>,
) -> Vec<String> {
    let mut summaries = Vec::new();
    for block in &message.blocks {
        let CkKind::ToolResult { tool_name, .. } = &block.wire.kind else {
            continue;
        };
        summaries.push(
            block
                .arc_id
                .as_deref()
                .and_then(|arc_id| tool_call_summaries.get(arc_id))
                .cloned()
                .unwrap_or_else(|| format!("TC: {tool_name}")),
        );
    }
    summaries
}

fn build_tool_call_summary_lookup(blocks: &[FlatBlock]) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for block in blocks.iter().filter(|block| !block.synthetic) {
        let CkKind::ToolCall { name, input, .. } = &block.wire.kind else {
            continue;
        };
        out.insert(block.id.clone(), format_tool_summary(name, input));
    }
    out
}

fn format_tool_summary(name: &str, input: &Value) -> String {
    if let Some(description) = input
        .get("description")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        format!("TC: {description}")
    } else if let Some(key_arg) = extract_key_arg(input) {
        format!("TC: {name}({key_arg})")
    } else {
        format!("TC: {name}")
    }
}

fn extract_key_arg(input: &Value) -> Option<String> {
    let object = input.as_object()?;
    for key in ["filePath", "path", "pattern", "query"] {
        if let Some(value) = object.get(key).and_then(Value::as_str) {
            return Some(truncate_arg(value));
        }
    }
    for key in ["symbol", "module", "action"] {
        if let Some(value) = object.get(key).and_then(Value::as_str) {
            return Some(value.to_string());
        }
    }
    None
}

fn truncate_arg(value: &str) -> String {
    if value.chars().count() <= 60 {
        value.to_string()
    } else {
        format!("{}…", value.chars().take(60).collect::<String>())
    }
}

fn clean_user_text(text: &str) -> String {
    system_reminder_regex()
        .replace_all(text, "")
        .replace(OMO_INTERNAL_INITIATOR_MARKER, "")
        .trim()
        .to_string()
}

fn is_system_directive(text: &str) -> bool {
    text.trim_start().starts_with(SYSTEM_DIRECTIVE_PREFIX)
}

fn normalize_text(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn compact_role(role: &str) -> String {
    match role {
        "assistant" => "A".to_string(),
        "user" => "U".to_string(),
        _ => role
            .chars()
            .next()
            .map(|ch| ch.to_uppercase().collect::<String>())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "M".to_string()),
    }
}

fn format_block(block: &ChunkBlock) -> String {
    let range = if block.start_ordinal == block.end_ordinal {
        format!("[{}]", block.start_ordinal)
    } else {
        format!("[{}-{}]", block.start_ordinal, block.end_ordinal)
    };
    let commit_suffix = if block.commit_hashes.is_empty() {
        String::new()
    } else {
        format!(" commits: {}", block.commit_hashes.join(", "))
    };
    format!(
        "{} {}:{} {}",
        range,
        block.role,
        commit_suffix,
        block.parts.join(" / ")
    )
}

fn extract_commit_hashes(text: &str) -> Vec<String> {
    let mut hashes = Vec::new();
    for capture in commit_hash_extract_regex().captures_iter(text) {
        let Some(hash) = capture.get(1).map(|value| value.as_str().to_lowercase()) else {
            continue;
        };
        if hashes.contains(&hash) {
            continue;
        }
        hashes.push(hash);
        if hashes.len() >= MAX_COMMITS_PER_BLOCK {
            break;
        }
    }
    hashes
}

fn compact_text_for_summary(text: &str, role: &str) -> CompactedText {
    let commit_hashes = if role == "assistant" {
        extract_commit_hashes(text)
    } else {
        Vec::new()
    };
    if commit_hashes.is_empty() || !commit_verb_regex().is_match(text) {
        return CompactedText {
            text: text.to_string(),
            commit_hashes,
        };
    }
    let without_hashes = commit_hash_extract_regex().replace_all(text, "");
    let without_hashes = empty_parens_regex().replace_all(&without_hashes, "");
    let without_hashes = space_before_comma_regex().replace_all(&without_hashes, ",");
    let without_hashes = repeated_comma_regex().replace_all(&without_hashes, ", ");
    let without_hashes = repeated_space_regex().replace_all(&without_hashes, " ");
    let without_hashes = space_before_punct_regex().replace_all(&without_hashes, "$1");
    let trimmed = without_hashes.trim();
    CompactedText {
        text: if trimmed.is_empty() {
            text.to_string()
        } else {
            trimmed.to_string()
        },
        commit_hashes,
    }
}

fn merge_commit_hashes(existing: &[String], next: &[String]) -> Vec<String> {
    let mut merged = existing.to_vec();
    for hash in next {
        if merged.contains(hash) {
            continue;
        }
        merged.push(hash.clone());
        if merged.len() >= MAX_COMMITS_PER_BLOCK {
            break;
        }
    }
    merged
}

fn merge_tool_only_ranges(ranges: &[MessageRange]) -> Vec<MessageRange> {
    let mut merged: Vec<MessageRange> = Vec::new();
    for range in ranges {
        if let Some(last) = merged.last_mut() {
            if range.start == last.end + 1 {
                last.end = range.end;
                continue;
            }
        }
        merged.push(range.clone());
    }
    merged
}

fn system_reminder_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?is)<system-reminder>[\s\S]*?</system-reminder>").unwrap())
}

fn commit_hash_extract_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)`?\b([0-9a-f]{7,12})\b`?").unwrap())
}

fn commit_verb_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\b(?:commit(?:ted|ting|s)?|cherry-?pick(?:ed|ing|s)?|merge[ds]?|merging|rebas(?:e|ed|es|ing))\b").unwrap())
}

fn empty_parens_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\(\s*\)").unwrap())
}

fn space_before_comma_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s+,").unwrap())
}

fn repeated_comma_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r",\s*,+").unwrap())
}

fn repeated_space_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s{2,}").unwrap())
}

fn space_before_punct_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\s+([,.;:])").unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transform::ck_wire::{
        project_messages, CkIngressMessage, CkWireBlock, CkWireMessage, HarnessMeta,
    };
    use mc_store::{CkKind, ProviderExtras};
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Deserialize)]
    struct GoldenRoot {
        cases: Vec<GoldenCase>,
        #[serde(default, rename = "truncationCases")]
        truncation_cases: Vec<GoldenTruncationCase>,
    }

    #[derive(Deserialize)]
    struct GoldenCase {
        label: String,
        budget: usize,
        offset: u64,
        #[serde(rename = "eligibleEnd")]
        eligible_end: u64,
        ck: Vec<CkIngressMessage>,
        expected: GoldenExpected,
    }

    #[derive(Deserialize)]
    struct GoldenExpected {
        #[serde(rename = "startIndex")]
        start_index: u64,
        #[serde(rename = "endIndex")]
        end_index: u64,
        #[serde(rename = "messageCount")]
        message_count: usize,
        #[serde(rename = "tokenEstimate")]
        token_estimate: usize,
        text: String,
        lines: Vec<GoldenLine>,
        #[serde(rename = "toolOnlyRanges")]
        tool_only_ranges: Vec<MessageRange>,
        #[serde(rename = "hasMore")]
        has_more: bool,
        #[serde(rename = "commitClusterCount")]
        commit_cluster_count: usize,
    }

    #[derive(Deserialize)]
    struct GoldenLine {
        ordinal: u64,
    }

    #[derive(Deserialize)]
    struct GoldenTruncationCase {
        label: String,
        budget: usize,
        input: String,
        expected: String,
    }

    fn msg(mid: &str, ordinal: u64, role: &str, blocks: Vec<CkKind>) -> CkIngressMessage {
        CkIngressMessage {
            mid: mid.to_string(),
            ordinal,
            ck: CkWireMessage::from_parts(
                role,
                blocks
                    .into_iter()
                    .map(|kind| CkWireBlock::with_provider_extras(kind, ProviderExtras::default()))
                    .collect(),
                None,
                ProviderExtras::default(),
                HarnessMeta::default(),
            ),
        }
    }

    fn text(value: &str) -> CkKind {
        CkKind::Text {
            text: value.to_string(),
        }
    }

    fn project_and_build(
        messages: &[CkIngressMessage],
        offset: u64,
        budget: usize,
        eligible_end: u64,
    ) -> HistorianBuiltChunk {
        let projection = project_messages(messages).unwrap();
        build_historian_chunk(messages, &projection.blocks, offset, budget, eligible_end)
    }

    #[test]
    fn chunk_uses_flat_block_ids_and_skips_system_messages() {
        let messages = vec![
            msg("u1", 1, "user", vec![text("hello")]),
            msg("sys", 2, "system", vec![text("identity")]),
            msg("a3", 3, "assistant", vec![text("done")]),
        ];
        let built = project_and_build(&messages, 1, 1_000, 4);
        assert_eq!(built.text, "[1] U: hello\n[3] A: done");
        assert_eq!(built.chunk.start_index, 1);
        assert!(!built.chunk.lines.iter().any(|line| line.ordinal == 2));
        assert!(built.chunk.lines.iter().any(|line| line.ordinal == 3));
        assert_eq!(built.chunk.lines[0].message_id, "u1#0");
        assert_eq!(built.end_message_id, "a3#0");
    }

    #[test]
    fn zero_block_messages_are_absorbed_as_pending_noise() {
        let messages = vec![
            msg("empty1", 1, "user", vec![]),
            msg("u2", 2, "user", vec![text("real user text")]),
        ];
        let projection = project_messages(&messages).unwrap();
        assert!(!projection.blocks.iter().any(|block| block.mid == "empty1"));
        let built = build_historian_chunk(&messages, &projection.blocks, 1, 1_000, 3);
        assert_eq!(built.text, "[1-2] U: real user text");
        assert_eq!(
            built
                .chunk
                .lines
                .iter()
                .map(|line| line.ordinal)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(built.chunk.lines[0].message_id, "empty1#0");
    }

    #[test]
    fn duplicate_tool_call_ids_resolve_results_by_arc_id() {
        let messages = vec![
            msg(
                "a1",
                1,
                "assistant",
                vec![CkKind::ToolCall {
                    id: "dup".to_string(),
                    name: "read".to_string(),
                    input: json!({"path":"one.rs"}),
                    provider_executed: false,
                }],
            ),
            msg(
                "t2",
                2,
                "tool",
                vec![CkKind::ToolResult {
                    id: "dup".to_string(),
                    tool_name: "read".to_string(),
                    output: mc_store::CkToolOutput::bare(mc_store::CkOutputKind::Text {
                        text: "one".to_string(),
                    }),
                    provider_executed: false,
                }],
            ),
            msg(
                "a3",
                3,
                "assistant",
                vec![CkKind::ToolCall {
                    id: "dup".to_string(),
                    name: "read".to_string(),
                    input: json!({"path":"two.rs"}),
                    provider_executed: false,
                }],
            ),
            msg(
                "t4",
                4,
                "tool",
                vec![CkKind::ToolResult {
                    id: "dup".to_string(),
                    tool_name: "read".to_string(),
                    output: mc_store::CkToolOutput::bare(mc_store::CkOutputKind::Text {
                        text: "two".to_string(),
                    }),
                    provider_executed: false,
                }],
            ),
        ];
        let built = project_and_build(&messages, 1, 1_000, 5);
        assert_eq!(
            built.text,
            "[1-4] A: TC: read(one.rs) / TC: read(one.rs) / TC: read(two.rs) / TC: read(two.rs)"
        );
    }

    #[test]
    fn tool_result_only_message_absorbs_into_preceding_assistant_block() {
        let messages = vec![
            msg(
                "a1",
                1,
                "assistant",
                vec![
                    CkKind::ToolCall {
                        id: "call1".to_string(),
                        name: "read".to_string(),
                        input: json!({"path":"src/lib.rs"}),
                        provider_executed: false,
                    },
                    text("I will inspect it"),
                ],
            ),
            msg(
                "t2",
                2,
                "tool",
                vec![CkKind::ToolResult {
                    id: "call1".to_string(),
                    tool_name: "read".to_string(),
                    output: mc_store::CkToolOutput::bare(mc_store::CkOutputKind::Text {
                        text: "file".to_string(),
                    }),
                    provider_executed: false,
                }],
            ),
            msg("u3", 3, "user", vec![text("thanks")]),
        ];
        let built = project_and_build(&messages, 1, 1_000, 4);
        assert_eq!(
            built.text,
            "[1-2] A: I will inspect it / TC: read(src/lib.rs)\n[3] U: thanks"
        );
        assert_eq!(built.chunk.lines[0].message_id, "a1#1");
        assert_eq!(built.chunk.lines[1].message_id, "t2#0");
    }

    #[test]
    fn budget_stop_and_tool_only_ranges_are_recorded() {
        let messages = vec![
            msg(
                "a1",
                1,
                "assistant",
                vec![CkKind::ToolCall {
                    id: "c1".to_string(),
                    name: "read".to_string(),
                    input: json!({"path":"one"}),
                    provider_executed: false,
                }],
            ),
            msg(
                "a2",
                2,
                "assistant",
                vec![CkKind::ToolCall {
                    id: "c2".to_string(),
                    name: "read".to_string(),
                    input: json!({"path":"two"}),
                    provider_executed: false,
                }],
            ),
            msg("u3", 3, "user", vec![text("narrative")]),
        ];
        let built = project_and_build(&messages, 1, 1_000, 4);
        assert_eq!(
            built.chunk.tool_only_ranges,
            vec![MessageRange { start: 1, end: 2 }]
        );
        let stopped = project_and_build(&messages, 1, 1, 4);
        assert!(stopped.has_more);
    }

    #[test]
    fn pending_noise_does_not_leak_when_budget_stops_before_next_block() {
        let messages = vec![
            msg("u1", 1, "user", vec![text("short")]),
            msg(
                "noise2",
                2,
                "user",
                vec![text("<!-- OMO_INTERNAL_INITIATOR -->")],
            ),
            msg(
                "a3",
                3,
                "assistant",
                vec![text(
                    "this assistant block is intentionally too long for the tiny chunk budget",
                )],
            ),
        ];
        let built = project_and_build(&messages, 1, 6, 4);
        assert_eq!(built.chunk.end_index, 1);
        assert_eq!(
            built
                .chunk
                .lines
                .iter()
                .map(|line| line.ordinal)
                .collect::<Vec<_>>(),
            vec![1]
        );
        assert!(built.has_more);
    }

    #[test]
    fn truncation_uses_marker_and_keeps_multibyte_boundaries() {
        let input = "αβγ🙂 historian chunk ".repeat(80);
        let budget = estimate_tokens(HISTORIAN_TRUNCATION_MARKER) + 12;
        let truncated = truncate_historian_input_if_needed(&input, budget);
        assert!(truncated.ends_with(HISTORIAN_TRUNCATION_MARKER));
        assert!(estimate_tokens(&truncated) <= budget);
        let prefix = truncated.strip_suffix(HISTORIAN_TRUNCATION_MARKER).unwrap();
        assert!(input.starts_with(prefix));
        let next = input
            .chars()
            .take(prefix.chars().count() + 1)
            .collect::<String>();
        assert!(estimate_tokens(&format!("{next}{HISTORIAN_TRUNCATION_MARKER}")) > budget);
    }

    #[test]
    fn commit_clusters_follow_assistant_blocks_after_user_turns() {
        let messages = vec![
            msg("u1", 1, "user", vec![text("go")]),
            msg("a2", 2, "assistant", vec![text("committed abcdef1")]),
            msg("u3", 3, "user", vec![text("more")]),
            msg("a4", 4, "assistant", vec![text("commit abcdef2")]),
        ];
        let built = project_and_build(&messages, 1, 1_000, 5);
        assert_eq!(built.commit_cluster_count, 2);
        assert!(built.text.contains("commits: abcdef1"));
    }

    #[test]
    fn historian_chunk_golden_fixture_matches_builder() {
        let root: GoldenRoot =
            serde_json::from_str(include_str!("../testdata/historian-chunk-golden.json")).unwrap();
        for case in &root.cases {
            let projection = project_messages(&case.ck).unwrap();
            let built = build_historian_chunk(
                &case.ck,
                &projection.blocks,
                case.offset,
                case.budget,
                case.eligible_end,
            );
            assert_eq!(
                built.chunk.start_index, case.expected.start_index,
                "{} start",
                case.label
            );
            assert_eq!(
                built.chunk.end_index, case.expected.end_index,
                "{} end",
                case.label
            );
            assert_eq!(
                built.chunk.lines.len(),
                case.expected.message_count,
                "{} count",
                case.label
            );
            assert_eq!(
                built.token_estimate, case.expected.token_estimate,
                "{} tokens",
                case.label
            );
            assert_eq!(built.text, case.expected.text, "{} text", case.label);
            assert_eq!(
                built
                    .chunk
                    .lines
                    .iter()
                    .map(|line| line.ordinal)
                    .collect::<Vec<_>>(),
                case.expected
                    .lines
                    .iter()
                    .map(|line| line.ordinal)
                    .collect::<Vec<_>>(),
                "{} ordinals",
                case.label
            );
            assert_eq!(
                built.chunk.tool_only_ranges, case.expected.tool_only_ranges,
                "{} tool ranges",
                case.label
            );
            assert_eq!(
                built.has_more, case.expected.has_more,
                "{} hasMore",
                case.label
            );
            assert_eq!(
                built.commit_cluster_count, case.expected.commit_cluster_count,
                "{} commit clusters",
                case.label
            );
        }
        for case in &root.truncation_cases {
            assert_eq!(
                truncate_historian_input_if_needed(&case.input, case.budget),
                case.expected,
                "{} truncation",
                case.label
            );
        }
    }
}

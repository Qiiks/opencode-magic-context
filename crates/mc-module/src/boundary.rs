//! Protected-tail boundary + compartment trigger: WHERE the compactable/
//! protected split sits and WHETHER a historian run should fire, decided purely
//! from the in-memory tail. Historian execution lives elsewhere; this is the
//! deterministic decision layer.
//!
//! All token measurement in this unit is a pure function of caller-provided
//! message/block bytes and caller-provided context. There is no I/O, wall clock,
//! store access, or ambient cache state here: the same inputs always produce the
//! same boundary and trigger decision.

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::sync::OnceLock;

use mc_tokenizer::estimate_tokens;
use regex::Regex;
use serde_json::Value;

use crate::selection::SelKind;

// --- Constants for protected-tail sizing and trigger thresholds. ---

const ALPHA: f64 = 0.3;
const FLOOR_RATIO: f64 = 0.08;
const FLOOR_MIN: f64 = 2_000.0;
const FLOOR_MAX: f64 = 12_000.0;
const ABS_CAP: f64 = 96_000.0;
const MAX_USABLE_RATIO: f64 = 0.4;
const RESERVED_HEADROOM_MIN: f64 = 1_000.0;
const RESERVED_HEADROOM_RATIO: f64 = 0.02;
const NON_EMERGENCY_MAX_CAP: f64 = 250_000.0;
const FORCE80_MAX_CAP: f64 = 500_000.0;
const FORCE95_MAX_CAP: f64 = 750_000.0;
const NORMAL_HYSTERESIS_TOKENS: f64 = 256.0;
const MIN_FORCE_ELIGIBLE_TOKENS_CAP: f64 = 1_000.0;

const TRIGGER_BUDGET_PERCENTAGE: f64 = 0.05;
const TRIGGER_BUDGET_MIN: f64 = 5_000.0;
const TRIGGER_BUDGET_MAX: f64 = 50_000.0;
const PROACTIVE_TRIGGER_OFFSET_PERCENTAGE: f64 = 2.0;
const POST_DROP_TARGET_RATIO: f64 = 0.75;
const MIN_PROACTIVE_TAIL_TOKEN_ESTIMATE: f64 = 6_000.0;
const MIN_PROACTIVE_TAIL_MESSAGE_COUNT: usize = 12;
const DEFAULT_MIN_COMMIT_CLUSTERS_FOR_TRIGGER: usize = 3;
const TAIL_SIZE_TRIGGER_MULTIPLIER: f64 = 3.0;
const FORCE_COMPARTMENT_PERCENTAGE: f64 = 80.0;
const BLOCK_UNTIL_DONE_PERCENTAGE: f64 = 95.0;
// Retained even though this module does not use the value; the full threshold
// constant set is covered by tests so related configuration values cannot drift independently.
#[allow(dead_code)]
const FORCE_MATERIALIZE_PERCENTAGE: f64 = 85.0;
const MAX_COMMITS_PER_BLOCK: usize = 5;

const SYSTEM_DIRECTIVE_PREFIX: &str = "[SYSTEM DIRECTIVE: MAGIC-CONTEXT";
const OMO_INTERNAL_INITIATOR_MARKER: &str = "<!-- OMO_INTERNAL_INITIATOR -->";

/// Message role used by boundary and trigger decisions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// A user-authored message.
    User,
    /// An assistant-authored message.
    Assistant,
    /// A system-authored message.
    System,
    /// Any provider-specific role not otherwise known to the module.
    Other(String),
}

impl Role {
    /// Convert a provider role string to the narrow role vocabulary this unit reads.
    pub fn from_provider(value: &str) -> Self {
        match value {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "system" => Role::System,
            other => Role::Other(other.to_string()),
        }
    }

    /// Return the provider role spelling used when formatting messages into historian `U:`/`A:`/`TC:` chunks.
    pub fn as_str(&self) -> &str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::System => "system",
            Role::Other(value) => value.as_str(),
        }
    }
}

/// One original pre-reduction content block inside a [`BoundaryMsg`].
#[derive(Debug, Clone)]
pub struct BoundaryBlock {
    /// Stable block id (follows the same id convention used by the sibling selection module).
    pub id: String,
    /// Block ordinal within the flat tail. Message-level algorithms use the parent
    /// message ordinal; this remains available for callers that preserve block order.
    pub ordinal: u64,
    /// Typed content kind (`SelKind`, shared with the selection module for cross-module consistency).
    pub kind: SelKind,
    /// True for provider/server-executed tool blocks; these cannot start an in-flight tool-call arc.
    pub provider_executed: bool,
    /// Original byte length supplied by the caller for diagnostics.
    pub byte_size: usize,
    /// Tool arc id for calls/results/reasoning that belong to the same invocation.
    pub arc_id: Option<String>,
    /// Original pre-reduction block bytes as UTF-8 text.
    ///
    /// Boundary and trigger token measurement always uses this value, never a
    /// rendered reduction placeholder. The raw session still contains these bytes,
    /// and the historian would read these bytes if the trigger fired.
    pub original: String,
    /// Optional rendered form after reduction. It is deliberately ignored by every
    /// decision function and exists only to make the raw-byte invariant testable.
    pub rendered: Option<String>,
    /// Mirrors OpenCode text parts marked `ignored`; ignored user text does not
    /// contribute to the live-prompt floor, which keeps the current user prompt protected.
    pub ignored: bool,
}

/// Message-grouped boundary input.
#[derive(Debug, Clone)]
pub struct BoundaryMsg {
    /// Absolute raw-session ordinal for the message.
    pub message_ordinal: u64,
    /// Provider message id. Used only for diagnostics; boundary and trigger logic do not read it.
    pub message_id: String,
    /// Provider message role.
    pub role: Role,
    /// Original blocks that belong to this message.
    pub blocks: Vec<BoundaryBlock>,
}

/// Inputs for resolving the protected-tail boundary.
#[derive(Debug, Clone)]
pub struct BoundaryContext {
    /// Main model context limit in tokens.
    pub context_limit: f64,
    /// Execute threshold percentage used to derive usable context.
    pub execute_threshold_percentage: f64,
    /// Current input usage percentage.
    pub usage_percentage: f64,
    /// Current input token count; fractional inputs are rounded to the nearest token.
    pub usage_input_tokens: f64,
    /// Last raw message ordinal already published in a compartment, or `None` before
    /// the first compartment. Ordinal 0 can be a real published end.
    pub last_compartment_end_ordinal: Option<u64>,
    /// Previous boundary ordinal from an earlier calculation; retained so that floor can be reapplied.
    pub prior_boundary_ordinal: u64,
    /// Whether the floor based on `prior_boundary_ordinal` is currently active.
    pub migration_floor_active: bool,
    /// Optional emergency shrink scale (`0.5` at 80% pressure, `0.25` at 95% pressure).
    pub emergency_tail_scale: Option<f64>,
    /// Optional pre-derived trigger budget; when omitted, [`derive_trigger_budget`] is used.
    pub trigger_budget: Option<f64>,
}

impl Default for BoundaryContext {
    fn default() -> Self {
        Self {
            context_limit: 128_000.0,
            execute_threshold_percentage: 65.0,
            usage_percentage: 0.0,
            usage_input_tokens: 0.0,
            last_compartment_end_ordinal: None,
            prior_boundary_ordinal: 1,
            migration_floor_active: false,
            emergency_tail_scale: None,
            trigger_budget: None,
        }
    }
}

/// Token target details for the protected-tail window.
#[derive(Debug, Clone, PartialEq)]
pub struct ProtectedTailTokenTarget {
    /// Usable context tokens: `context_limit × execute_threshold%`.
    pub usable: f64,
    /// Unfloored usage-sensitive target.
    pub raw_n: f64,
    /// Floor before the ceiling clamp.
    pub floor_n: f64,
    /// Ceiling after absolute, usable-ratio, and headroom clamps.
    pub ceiling_n: f64,
    /// Floor after it is capped by the ceiling.
    pub effective_floor: f64,
    /// Final unscaled target.
    pub n: f64,
    /// Reserved headroom kept out of the protected tail.
    pub headroom: f64,
    /// Trigger budget used by the headroom calculation.
    pub trigger_budget: f64,
    /// Fixed/ratio reserve before it is combined with the trigger budget.
    pub reserve: f64,
}

/// Result of resolving the compactable/protected split.
#[derive(Debug, Clone, PartialEq)]
pub struct BoundaryResolution {
    /// First protected raw-message ordinal; messages before this are eligible head.
    pub protected_start_ordinal: u64,
    /// Half-open compactable range, from `last_compartment_end + 1` up to the head cap.
    pub eligible_head: Range<u64>,
    /// Scaled protected-tail token target used to walk backward from the newest message.
    pub n_tokens: f64,
    /// True when normal, non-emergency pressure kept the newest non-ignored user prompt protected.
    pub floored_by_live_prompt: bool,
    /// True when a recent open tool invocation fenced the boundary or head.
    pub fenced_by_open_arc: bool,
    /// True raw tokens in `offset..protected_start_ordinal`.
    pub true_raw_eligible_tokens: f64,
    /// True when the per-run cap had to include one atomic message/arc larger than the cap.
    pub oversize_atomic_unit: bool,
    /// Absolute raw-message count observed in the input.
    pub raw_message_count: u64,
    /// Diagnostic reason for the primary boundary placement.
    pub boundary_reason: String,
}

/// Chunked tail measurement in the historian's `U:`/`A:`/`TC:` block format.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkEstimate {
    /// Token count used by trigger decisions. When scanning stops early, this
    /// saturates at `budget_stop` so `has_more` never under-reports the threshold.
    pub tokens: f64,
    /// True when the scan stopped before the eligible tail ended.
    pub has_more: bool,
    /// Formatted block strings produced by the chunk-formatting step and then tokenized.
    pub formatted_blocks: Vec<String>,
    /// Token count per formatted block before any saturation.
    pub block_tokens: Vec<f64>,
    /// Number of raw messages represented in formatted blocks.
    pub message_count: usize,
    /// Number of assistant commit clusters in the formatted prefix.
    pub commit_cluster_count: usize,
}

/// Inputs for checking whether the historian should fire.
#[derive(Debug, Clone)]
pub struct TriggerContext {
    /// Boundary context used for the primary protected-tail resolution.
    pub boundary: BoundaryContext,
    /// True when a historian/compartment run is already active.
    pub compartment_in_progress: bool,
    /// Projected post-drop usage percentage supplied by the caller, if available.
    pub projected_post_drop_percentage: Option<f64>,
    /// Whether commit clusters may trigger a run.
    pub commit_cluster_trigger_enabled: bool,
    /// Minimum assistant commit clusters required for the commit trigger.
    pub min_commit_clusters: usize,
}

impl Default for TriggerContext {
    fn default() -> Self {
        Self {
            boundary: BoundaryContext::default(),
            compartment_in_progress: false,
            projected_post_drop_percentage: None,
            commit_cluster_trigger_enabled: true,
            min_commit_clusters: DEFAULT_MIN_COMMIT_CLUSTERS_FOR_TRIGGER,
        }
    }
}

/// Reason a trigger decision fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerReason {
    /// Context pressure reached the projected-headroom threshold and drops are not enough.
    ProjectedHeadroom,
    /// Context pressure reached the force band (80% or higher).
    Force80,
    /// Enough assistant commit clusters accumulated in the eligible head.
    CommitClusters,
    /// Enough TC-chunked tail eligible for historian summarization accumulated.
    TailSize,
}

impl TriggerReason {
    /// Wire spelling used for serialized trigger results.
    pub fn as_str(self) -> &'static str {
        match self {
            TriggerReason::ProjectedHeadroom => "projected_headroom",
            TriggerReason::Force80 => "force_80",
            TriggerReason::CommitClusters => "commit_clusters",
            TriggerReason::TailSize => "tail_size",
        }
    }
}

/// Pure trigger result.
#[derive(Debug, Clone, PartialEq)]
pub struct TriggerDecision {
    /// True when the historian should fire.
    pub fire: bool,
    /// Fire reason, absent when `fire` is false.
    pub reason: Option<TriggerReason>,
    /// Last raw-message ordinal the run may consume, always before the protected tail.
    pub consume_through_ordinal: Option<u64>,
    /// The exact boundary snapshot that produced a fire decision. The assembler consumes
    /// this object directly so the trigger and chunk snapshot cannot resolve different ranges.
    pub boundary: Option<BoundaryResolution>,
    /// Progress toward the tail_size bar, present whenever a boundary was resolved (fire or
    /// not). Diagnostics-only: rendering it must never influence the decision itself.
    pub progress: Option<TriggerProgress>,
}

/// Why the trigger did or did not fire, in numbers. Surfaced through the transform
/// response's historian diagnostics so a stalled rig drive is diagnosable per pass
/// (eligible content vs the bar, and how much tail the protected boundary is holding back).
#[derive(Debug, Clone, PartialEq)]
pub struct TriggerProgress {
    /// TC-chunked tokens in the eligible head (what tail_size compares against the bar).
    pub eligible_chunk_tokens: f64,
    /// The tail_size fire bar (trigger_budget x multiplier).
    pub tail_size_bar: f64,
    /// Protected-tail token target N; shrinks as usage grows.
    pub n_tokens: f64,
    /// First protected ordinal (eligible head ends here).
    pub protected_start_ordinal: u64,
}

/// Derive the size-trigger budget from context size and execute threshold.
pub fn derive_trigger_budget(context_limit: f64, execute_threshold_percentage: f64) -> f64 {
    if !context_limit.is_finite() || context_limit <= 0.0 {
        return TRIGGER_BUDGET_MIN;
    }
    let threshold_fraction = execute_threshold_percentage.max(0.0) / 100.0;
    let usable = context_limit * threshold_fraction;
    let derived = (usable * TRIGGER_BUDGET_PERCENTAGE).round();
    derived.clamp(TRIGGER_BUDGET_MIN, TRIGGER_BUDGET_MAX)
}

fn first_live_message_ordinal(messages: &[BoundaryMsg]) -> Option<u64> {
    messages.iter().map(|message| message.message_ordinal).min()
}

fn compartment_offset(
    last_compartment_end_ordinal: Option<u64>,
    messages: &[BoundaryMsg],
) -> Option<u64> {
    last_compartment_end_ordinal
        .map(|end| end.saturating_add(1))
        .or_else(|| first_live_message_ordinal(messages))
}

/// Derive the protected-tail token target before optional emergency scaling.
pub fn derive_protected_tail_token_target(ctx: &BoundaryContext) -> ProtectedTailTokenTarget {
    let safe_context_limit = if ctx.context_limit.is_finite() && ctx.context_limit > 0.0 {
        ctx.context_limit
    } else {
        128_000.0
    };
    let safe_threshold = if ctx.execute_threshold_percentage.is_finite() {
        ctx.execute_threshold_percentage.max(0.0)
    } else {
        65.0
    };
    let usable = ((safe_context_limit * safe_threshold) / 100.0)
        .round()
        .max(1.0);
    let usage = clamp_percentage(ctx.usage_percentage);
    let trigger_budget = ctx
        .trigger_budget
        .unwrap_or_else(|| derive_trigger_budget(safe_context_limit, safe_threshold));
    let reserve = RESERVED_HEADROOM_MIN.max((usable * RESERVED_HEADROOM_RATIO).round());
    let raw_n = (usable * ALPHA * (1.0 - usage / 100.0)).round();
    let floor_n = FLOOR_MAX.min(FLOOR_MIN.max((usable * FLOOR_RATIO).round()));
    let headroom = (trigger_budget + reserve).min((usable * 0.5).floor());
    let ceiling_n = 1.0_f64.max(
        ABS_CAP
            .min((usable * MAX_USABLE_RATIO).floor())
            .min(usable - headroom),
    );
    let effective_floor = floor_n.min(ceiling_n);
    let n = ceiling_n.min(effective_floor.max(raw_n));
    ProtectedTailTokenTarget {
        usable,
        raw_n,
        floor_n,
        ceiling_n,
        effective_floor,
        n,
        headroom,
        trigger_budget,
        reserve,
    }
}

/// Resolve the protected-tail boundary from original pre-reduction message bytes.
///
/// The token walk intentionally ignores [`BoundaryBlock::rendered`]. Dropped or
/// skeletonized render placeholders are compose-time presentation; the durable raw
/// session still contains the original bytes, and those are the bytes the historian
/// would summarize if the trigger fired.
pub fn resolve_protected_tail_boundary(
    messages: &[BoundaryMsg],
    ctx: &BoundaryContext,
) -> BoundaryResolution {
    let index = TokenIndex::new(messages);
    let raw_message_count = index.raw_message_count;
    let offset = compartment_offset(ctx.last_compartment_end_ordinal, messages).unwrap_or(1);
    let usage_percentage = clamp_percentage(ctx.usage_percentage);
    let usage_input_tokens = ctx.usage_input_tokens.max(0.0).round();

    if raw_message_count == 0 {
        return BoundaryResolution {
            protected_start_ordinal: 1,
            eligible_head: offset..offset,
            n_tokens: 0.0,
            floored_by_live_prompt: false,
            fenced_by_open_arc: false,
            true_raw_eligible_tokens: 0.0,
            oversize_atomic_unit: false,
            raw_message_count,
            boundary_reason: format!("empty-session:{usage_input_tokens:.0}"),
        };
    }

    let target = derive_protected_tail_token_target(ctx);
    let scaled_n = ctx
        .emergency_tail_scale
        .map(|scale| (target.n * scale).floor().max(1.0))
        .unwrap_or(target.n);
    let arcs = build_tool_arcs(messages);
    let mut boundary = index.find_suffix_start_for_tokens(scaled_n);
    let recent_open_arc_cutoff = boundary;
    let mut boundary_reason = if boundary == index.first_ordinal {
        "whole-session-smaller-than-tail".to_string()
    } else {
        "size-walk".to_string()
    };

    let token_at_boundary = index.token_for_ordinal(boundary);
    if boundary < index.terminal_ordinal
        && token_at_boundary > (2.0 * scaled_n).max(64_000.0)
        && boundary < index.last_ordinal
    {
        boundary += 1;
        boundary_reason = "huge-message-exception".to_string();
    }

    let first_fence = fence_boundary_for_tool_arcs(boundary, &arcs, offset, recent_open_arc_cutoff);
    let mut fenced_by_open_arc = first_fence.open_arc;
    boundary = first_fence.boundary;

    let snapped = semantic_snap_boundary(messages, &index, boundary, scaled_n, offset);
    if snapped != boundary {
        boundary_reason = "semantic-snap".to_string();
    }
    let second_fence = fence_boundary_for_tool_arcs(snapped, &arcs, offset, recent_open_arc_cutoff);
    fenced_by_open_arc |= second_fence.open_arc;
    boundary = second_fence.boundary;

    let mut runtime_floor = offset;
    if ctx.migration_floor_active {
        runtime_floor = runtime_floor.max(ctx.prior_boundary_ordinal);
    }
    let mut protected_tail_start = boundary.max(runtime_floor);

    let mut floored_by_live_prompt = false;
    if ctx.emergency_tail_scale.is_none() && usage_percentage < FORCE_COMPARTMENT_PERCENTAGE {
        if let Some(last_meaningful_user) = messages
            .iter()
            .rev()
            .find(|message| message.role == Role::User && has_meaningful_user_text(&message.blocks))
            .map(|message| message.message_ordinal)
        {
            if last_meaningful_user >= offset && protected_tail_start > last_meaningful_user {
                protected_tail_start = last_meaningful_user;
                floored_by_live_prompt = true;
            }
        }
    }

    if protected_tail_start > offset
        && index.range_tokens(offset, protected_tail_start) <= NORMAL_HYSTERESIS_TOKENS
    {
        protected_tail_start = offset;
    }
    protected_tail_start = index.clamp_ordinal(protected_tail_start);

    let per_run_cap = select_per_run_cap(
        usage_percentage,
        scaled_n,
        ctx.context_limit,
        ctx.execute_threshold_percentage,
    );
    let head = apply_head_cap(HeadCapArgs {
        index: &index,
        protected_tail_start,
        offset,
        arcs: &arcs,
        cap_tokens: per_run_cap,
        recent_open_arc_cutoff,
    });
    fenced_by_open_arc |= head.fenced_by_open_arc;

    BoundaryResolution {
        protected_start_ordinal: protected_tail_start,
        eligible_head: offset..head.eligible_end_ordinal,
        n_tokens: scaled_n,
        floored_by_live_prompt,
        fenced_by_open_arc,
        true_raw_eligible_tokens: index.range_tokens(offset, protected_tail_start),
        oversize_atomic_unit: head.oversize_atomic_unit,
        raw_message_count,
        boundary_reason,
    }
}

/// Measure TC-chunked content for a message range.
pub fn chunked_message_estimate(
    messages: &[BoundaryMsg],
    start_ordinal: u64,
    eligible_end_ordinal: Option<u64>,
    budget_stop: f64,
) -> ChunkEstimate {
    let mut ordered = messages.to_vec();
    ordered.sort_by_key(|message| message.message_ordinal);
    let total_message_count = ordered
        .iter()
        .map(|message| message.message_ordinal)
        .max()
        .unwrap_or(ordered.len() as u64);
    let mut builder = ChunkBuilder::new(budget_stop);

    for message in &ordered {
        if eligible_end_ordinal.is_some_and(|end| message.message_ordinal >= end) {
            break;
        }
        if message.message_ordinal < start_ordinal {
            continue;
        }
        if !builder.push_message(message) {
            break;
        }
    }
    builder.finish(total_message_count, eligible_end_ordinal)
}

/// Check whether a compartment/historian run should fire from the in-memory tail.
///
/// This performs the authoritative scan of the provided messages. No persistent
/// metadata pre-filter is present here: a pre-filter can only skip work, while
/// this pure unit already has the in-memory tail needed for the full decision.
pub fn check_compartment_trigger(
    messages: &[BoundaryMsg],
    ctx: &TriggerContext,
) -> TriggerDecision {
    if ctx.compartment_in_progress {
        return no_fire();
    }

    let trigger_budget = ctx.boundary.trigger_budget.unwrap_or_else(|| {
        derive_trigger_budget(
            ctx.boundary.context_limit,
            ctx.boundary.execute_threshold_percentage,
        )
    });
    let offset =
        compartment_offset(ctx.boundary.last_compartment_end_ordinal, messages).unwrap_or(1);
    let has_live_at_or_after_offset = messages
        .iter()
        .map(|message| message.message_ordinal)
        .max()
        .is_some_and(|max_ordinal| max_ordinal >= offset);
    if !has_live_at_or_after_offset {
        return no_fire();
    }

    let mut primary_ctx = ctx.boundary.clone();
    primary_ctx.trigger_budget = Some(trigger_budget);
    primary_ctx.emergency_tail_scale = None;
    let boundary = resolve_protected_tail_boundary(messages, &primary_ctx);
    let has_protected_eligible_head =
        boundary.eligible_head.start < boundary.protected_start_ordinal;

    let scan_budget =
        MIN_PROACTIVE_TAIL_TOKEN_ESTIMATE.max(trigger_budget * TAIL_SIZE_TRIGGER_MULTIPLIER);
    let chunk = if has_protected_eligible_head {
        chunked_message_estimate(
            messages,
            boundary.eligible_head.start,
            Some(boundary.protected_start_ordinal),
            scan_budget,
        )
    } else {
        ChunkEstimate {
            tokens: 0.0,
            has_more: false,
            formatted_blocks: Vec::new(),
            block_tokens: Vec::new(),
            message_count: 0,
            commit_cluster_count: 0,
        }
    };
    let progress = TriggerProgress {
        eligible_chunk_tokens: chunk.tokens,
        tail_size_bar: trigger_budget * TAIL_SIZE_TRIGGER_MULTIPLIER,
        n_tokens: boundary.n_tokens,
        protected_start_ordinal: boundary.protected_start_ordinal,
    };
    let is_meaningful = chunk.has_more
        || boundary.true_raw_eligible_tokens >= MIN_PROACTIVE_TAIL_TOKEN_ESTIMATE
        || chunk.tokens >= MIN_PROACTIVE_TAIL_TOKEN_ESTIMATE
        || chunk.message_count >= MIN_PROACTIVE_TAIL_MESSAGE_COUNT;
    let relative_post_drop_target =
        ctx.boundary.execute_threshold_percentage * POST_DROP_TARGET_RATIO;

    if ctx.boundary.usage_percentage >= FORCE_COMPARTMENT_PERCENTAGE {
        if ctx
            .projected_post_drop_percentage
            .is_some_and(|pct| pct <= relative_post_drop_target)
        {
            return no_fire_with_progress(progress);
        }
        if has_runnable_compartment_window(&boundary, ctx.boundary.usage_percentage, None) {
            return fire_with_progress(TriggerReason::Force80, &boundary, progress);
        }
        let scale = if ctx.boundary.usage_percentage >= BLOCK_UNTIL_DONE_PERCENTAGE {
            0.25
        } else {
            0.5
        };
        let mut scaled_ctx = primary_ctx;
        scaled_ctx.emergency_tail_scale = Some(scale);
        let scaled_boundary = resolve_protected_tail_boundary(messages, &scaled_ctx);
        if has_runnable_compartment_window(
            &scaled_boundary,
            ctx.boundary.usage_percentage,
            Some(scale),
        ) {
            return fire_with_progress(TriggerReason::Force80, &scaled_boundary, progress);
        }
        return no_fire_with_progress(progress);
    }

    if ctx.commit_cluster_trigger_enabled
        && chunk.commit_cluster_count >= ctx.min_commit_clusters
        && chunk.tokens >= trigger_budget
    {
        return fire_with_progress(TriggerReason::CommitClusters, &boundary, progress.clone());
    }

    if chunk.tokens >= trigger_budget * TAIL_SIZE_TRIGGER_MULTIPLIER
        || (chunk.has_more && chunk.tokens > 0.0)
    {
        return fire_with_progress(TriggerReason::TailSize, &boundary, progress);
    }

    let proactive_trigger_percentage =
        get_proactive_compartment_trigger_percentage(ctx.boundary.execute_threshold_percentage);
    if ctx.boundary.usage_percentage < proactive_trigger_percentage {
        return no_fire_with_progress(progress);
    }

    if ctx
        .projected_post_drop_percentage
        .is_some_and(|pct| pct <= relative_post_drop_target)
    {
        return no_fire_with_progress(progress);
    }

    if !has_protected_eligible_head || !is_meaningful {
        return no_fire_with_progress(progress);
    }

    fire_with_progress(TriggerReason::ProjectedHeadroom, &boundary, progress)
}

fn no_fire() -> TriggerDecision {
    TriggerDecision {
        fire: false,
        reason: None,
        consume_through_ordinal: None,
        boundary: None,
        progress: None,
    }
}

fn no_fire_with_progress(progress: TriggerProgress) -> TriggerDecision {
    TriggerDecision {
        progress: Some(progress),
        ..no_fire()
    }
}

fn fire(reason: TriggerReason, boundary: &BoundaryResolution) -> TriggerDecision {
    let consume_through_ordinal = if boundary.eligible_head.end > boundary.eligible_head.start {
        Some(boundary.eligible_head.end - 1)
    } else {
        None
    };
    TriggerDecision {
        fire: true,
        reason: Some(reason),
        consume_through_ordinal,
        boundary: Some(boundary.clone()),
        progress: None,
    }
}

fn fire_with_progress(
    reason: TriggerReason,
    boundary: &BoundaryResolution,
    progress: TriggerProgress,
) -> TriggerDecision {
    TriggerDecision {
        progress: Some(progress),
        ..fire(reason, boundary)
    }
}

fn clamp_percentage(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    value.clamp(0.0, 100.0)
}

fn get_proactive_compartment_trigger_percentage(execute_threshold_percentage: f64) -> f64 {
    (execute_threshold_percentage - PROACTIVE_TRIGGER_OFFSET_PERCENTAGE).max(0.0)
}

fn derive_min_force_eligible_tokens(scaled_n: f64) -> f64 {
    MIN_FORCE_ELIGIBLE_TOKENS_CAP.min((scaled_n / 8.0).floor().max(1.0))
}

fn non_emergency_per_run_cap(usable: f64, n: f64) -> f64 {
    NON_EMERGENCY_MAX_CAP.min((2.0 * n).max((0.25 * usable).round().min(100_000.0)))
}

fn force80_per_run_cap(usable: f64, n: f64) -> f64 {
    FORCE80_MAX_CAP.min((3.0 * n).max((0.35 * usable).round().min(150_000.0)))
}

fn force95_per_run_cap(usable: f64, n: f64) -> f64 {
    FORCE95_MAX_CAP.min((4.0 * n).max((0.5 * usable).round().min(250_000.0)))
}

fn select_per_run_cap(
    usage_percentage: f64,
    n: f64,
    context_limit: f64,
    execute_threshold_percentage: f64,
) -> f64 {
    let usable = ((context_limit * execute_threshold_percentage) / 100.0)
        .round()
        .max(1.0);
    if usage_percentage >= BLOCK_UNTIL_DONE_PERCENTAGE {
        force95_per_run_cap(usable, n)
    } else if usage_percentage >= FORCE_COMPARTMENT_PERCENTAGE {
        force80_per_run_cap(usable, n)
    } else {
        non_emergency_per_run_cap(usable, n)
    }
}

fn has_runnable_compartment_window(
    boundary: &BoundaryResolution,
    usage_percentage: f64,
    emergency_tail_scale: Option<f64>,
) -> bool {
    if boundary.eligible_head.start >= boundary.protected_start_ordinal {
        return false;
    }
    if usage_percentage >= FORCE_COMPARTMENT_PERCENTAGE || emergency_tail_scale.is_some() {
        boundary.true_raw_eligible_tokens >= derive_min_force_eligible_tokens(boundary.n_tokens)
            || boundary.eligible_head.end > boundary.eligible_head.start
    } else {
        boundary.eligible_head.end > boundary.eligible_head.start
    }
}

#[derive(Debug)]
struct TokenIndex {
    raw_message_count: u64,
    first_ordinal: u64,
    last_ordinal: u64,
    terminal_ordinal: u64,
    ordinals: Vec<u64>,
    prefix: Vec<f64>,
    tokens_by_ordinal: HashMap<u64, f64>,
}

impl TokenIndex {
    fn new(messages: &[BoundaryMsg]) -> Self {
        let mut totals_by_ordinal = BTreeMap::new();
        for message in messages {
            let total = message
                .blocks
                .iter()
                .map(|block| estimate_tokens(&block.original) as f64)
                .sum::<f64>();
            *totals_by_ordinal
                .entry(message.message_ordinal)
                .or_insert(0.0) += total;
        }

        let ordinals: Vec<u64> = totals_by_ordinal.keys().copied().collect();
        let first_ordinal = ordinals.first().copied().unwrap_or(1);
        let last_ordinal = ordinals.last().copied().unwrap_or(0);
        let terminal_ordinal = ordinals
            .last()
            .map(|ordinal| ordinal.saturating_add(1))
            .unwrap_or(1);
        let mut prefix = Vec::with_capacity(ordinals.len() + 1);
        prefix.push(0.0);
        let mut tokens_by_ordinal = HashMap::new();
        for ordinal in &ordinals {
            let total = totals_by_ordinal.get(ordinal).copied().unwrap_or(0.0);
            tokens_by_ordinal.insert(*ordinal, total);
            let previous = prefix.last().copied().unwrap_or(0.0);
            prefix.push(previous + total);
        }

        Self {
            raw_message_count: ordinals.len() as u64,
            first_ordinal,
            last_ordinal,
            terminal_ordinal,
            ordinals,
            prefix,
            tokens_by_ordinal,
        }
    }

    fn token_for_ordinal(&self, ordinal: u64) -> f64 {
        self.tokens_by_ordinal.get(&ordinal).copied().unwrap_or(0.0)
    }

    fn total_tokens(&self) -> f64 {
        self.prefix.last().copied().unwrap_or(0.0)
    }

    fn lower_bound(&self, ordinal: u64) -> usize {
        self.ordinals
            .partition_point(|candidate| *candidate < ordinal)
    }

    fn exclusive_end_for_prefix_index(&self, index: usize) -> u64 {
        if index == 0 {
            self.first_ordinal
        } else {
            self.ordinals[index - 1].saturating_add(1)
        }
    }

    fn clamp_ordinal(&self, ordinal: u64) -> u64 {
        if self.ordinals.is_empty() {
            return ordinal;
        }
        ordinal.max(self.first_ordinal).min(self.terminal_ordinal)
    }

    fn suffix_tokens_from_ordinal(&self, ordinal: u64) -> f64 {
        if self.ordinals.is_empty() {
            return 0.0;
        }
        if ordinal <= self.first_ordinal {
            return self.total_tokens();
        }
        if ordinal >= self.terminal_ordinal {
            return 0.0;
        }
        let start_index = self.lower_bound(ordinal);
        self.total_tokens() - self.prefix[start_index]
    }

    fn range_tokens(&self, start_inclusive: u64, end_exclusive: u64) -> f64 {
        if self.ordinals.is_empty() || end_exclusive <= start_inclusive {
            return 0.0;
        }
        let start = start_inclusive.max(self.first_ordinal);
        let end = end_exclusive.max(start).min(self.terminal_ordinal);
        if end <= start {
            return 0.0;
        }
        let start_index = self.lower_bound(start);
        let end_index = self.lower_bound(end);
        if end_index <= start_index {
            return 0.0;
        }
        self.prefix[end_index] - self.prefix[start_index]
    }

    fn find_suffix_start_for_tokens(&self, tokens: f64) -> u64 {
        if self.ordinals.is_empty() {
            return 1;
        }
        if !tokens.is_finite() || tokens <= 0.0 {
            return self.terminal_ordinal;
        }
        let target = tokens.floor().max(0.0);
        let total = self.total_tokens();
        if total < target {
            return self.first_ordinal;
        }
        let cut = total - target;
        let mut lo = 0_usize;
        let mut hi = self.prefix.len() - 1;
        let mut best = 0_usize;
        while lo <= hi {
            let mid = (lo + hi) >> 1;
            if self.prefix[mid] <= cut {
                best = mid;
                lo = mid + 1;
            } else if mid == 0 {
                break;
            } else {
                hi = mid - 1;
            }
        }
        self.ordinals
            .get(best)
            .copied()
            .unwrap_or(self.terminal_ordinal)
    }

    fn find_head_end_for_cap(
        &self,
        start_inclusive: u64,
        end_exclusive: u64,
        cap_tokens: f64,
    ) -> u64 {
        if self.ordinals.is_empty() {
            return start_inclusive;
        }
        let start = start_inclusive
            .max(self.first_ordinal)
            .min(self.terminal_ordinal);
        let end = end_exclusive.max(start).min(self.terminal_ordinal);
        if !cap_tokens.is_finite() || cap_tokens <= 0.0 {
            return start;
        }
        let start_index = self.lower_bound(start);
        let end_index = self.lower_bound(end);
        if start_index >= end_index {
            return start;
        }
        let start_prefix = self.prefix[start_index];
        let cut = start_prefix + cap_tokens.floor();
        let mut lo = start_index;
        let mut hi = end_index;
        let mut best_index = start_index;
        while lo <= hi {
            let mid = (lo + hi) >> 1;
            if self.prefix[mid] <= cut {
                best_index = mid;
                lo = mid + 1;
            } else if mid == 0 {
                break;
            } else {
                hi = mid - 1;
            }
        }
        if best_index == start_index {
            return self.ordinals[start_index].saturating_add(1).min(end);
        }
        self.exclusive_end_for_prefix_index(best_index).min(end)
    }
}

#[derive(Debug, Clone)]
struct ToolArc {
    inv_ordinal: u64,
    res_ordinal: Option<u64>,
}

fn build_tool_arcs(messages: &[BoundaryMsg]) -> Vec<ToolArc> {
    #[derive(Default)]
    struct PartialArc {
        inv: Vec<u64>,
        res: Vec<u64>,
    }

    let mut partial: BTreeMap<String, PartialArc> = BTreeMap::new();
    for message in messages {
        for block in &message.blocks {
            if block.provider_executed {
                continue;
            }
            let Some(arc_id) = &block.arc_id else {
                continue;
            };
            let entry = partial.entry(arc_id.clone()).or_default();
            match &block.kind {
                SelKind::ToolCall { .. } => entry.inv.push(message.message_ordinal),
                SelKind::ToolResult { .. } => entry.res.push(message.message_ordinal),
                _ => {}
            }
        }
    }

    let mut arcs = Vec::new();
    for (_arc_id, mut entry) in partial {
        entry.inv.sort_unstable();
        entry.res.sort_unstable();
        for inv in entry.inv {
            let res_pos = entry.res.iter().position(|res| *res >= inv);
            let res_ordinal = res_pos.map(|idx| entry.res.remove(idx));
            arcs.push(ToolArc {
                inv_ordinal: inv,
                res_ordinal,
            });
        }
    }
    arcs.sort_by(|a, b| {
        a.inv_ordinal.cmp(&b.inv_ordinal).then_with(|| {
            a.res_ordinal
                .unwrap_or(u64::MAX)
                .cmp(&b.res_ordinal.unwrap_or(u64::MAX))
        })
    });
    arcs
}

#[derive(Debug, Clone, Copy)]
struct FenceResult {
    boundary: u64,
    open_arc: bool,
}

fn fence_boundary_for_tool_arcs(
    candidate: u64,
    arcs: &[ToolArc],
    publication_floor_ordinal: u64,
    recent_open_arc_cutoff: u64,
) -> FenceResult {
    let mut boundary = candidate;
    for arc in arcs {
        if let Some(res_ordinal) = arc.res_ordinal {
            if arc.inv_ordinal < boundary && boundary <= res_ordinal {
                boundary = res_ordinal + 1;
            }
            continue;
        }
        if arc.inv_ordinal < recent_open_arc_cutoff {
            continue;
        }
        if arc.inv_ordinal >= publication_floor_ordinal && arc.inv_ordinal < boundary {
            return FenceResult {
                boundary: arc.inv_ordinal,
                open_arc: true,
            };
        }
        if arc.inv_ordinal >= boundary {
            return FenceResult {
                boundary: arc.inv_ordinal,
                open_arc: true,
            };
        }
    }
    FenceResult {
        boundary,
        open_arc: false,
    }
}

fn semantic_snap_boundary(
    messages: &[BoundaryMsg],
    index: &TokenIndex,
    candidate: u64,
    scaled_n: f64,
    publication_floor_ordinal: u64,
) -> u64 {
    let mut ordered: Vec<&BoundaryMsg> = messages.iter().collect();
    ordered.sort_by_key(|message| message.message_ordinal);
    let mut snapped = candidate;
    for message in &ordered {
        if message.message_ordinal > candidate {
            break;
        }
        if message.message_ordinal < publication_floor_ordinal {
            continue;
        }
        if !is_semantic_boundary_candidate(message) {
            continue;
        }
        snapped = message.message_ordinal;
    }
    if snapped == candidate {
        return candidate;
    }
    let extra_tokens =
        index.suffix_tokens_from_ordinal(snapped) - index.suffix_tokens_from_ordinal(candidate);
    if extra_tokens > (1.5 * scaled_n).round().min(48_000.0) {
        return candidate;
    }
    let snapped_is_huge_user = ordered.iter().any(|message| {
        message.message_ordinal == snapped
            && message.role == Role::User
            && index.token_for_ordinal(snapped) > (2.0 * scaled_n).max(64_000.0)
    });
    if snapped_is_huge_user {
        return candidate;
    }
    snapped
}

fn is_semantic_boundary_candidate(message: &BoundaryMsg) -> bool {
    if message.role == Role::User && has_meaningful_user_text(&message.blocks) {
        return true;
    }
    message.blocks.iter().any(|block| {
        matches!(
            block.kind,
            SelKind::ToolCall { .. } | SelKind::ToolResult { .. }
        )
    })
}

#[derive(Debug)]
struct HeadCapArgs<'a> {
    index: &'a TokenIndex,
    protected_tail_start: u64,
    offset: u64,
    arcs: &'a [ToolArc],
    cap_tokens: f64,
    recent_open_arc_cutoff: u64,
}

#[derive(Debug, Clone, Copy)]
struct HeadCapResult {
    eligible_end_ordinal: u64,
    oversize_atomic_unit: bool,
    fenced_by_open_arc: bool,
}

fn apply_head_cap(args: HeadCapArgs<'_>) -> HeadCapResult {
    if args.offset >= args.protected_tail_start {
        return HeadCapResult {
            eligible_end_ordinal: args.offset,
            oversize_atomic_unit: false,
            fenced_by_open_arc: false,
        };
    }
    let mut end =
        args.index
            .find_head_end_for_cap(args.offset, args.protected_tail_start, args.cap_tokens);
    let mut oversize_atomic_unit =
        end == args.offset + 1 && args.index.token_for_ordinal(args.offset) > args.cap_tokens;
    let mut fenced_by_open_arc = false;
    for arc in args.arcs {
        if let Some(res_ordinal) = arc.res_ordinal {
            if arc.inv_ordinal < end && end <= res_ordinal {
                end = args.protected_tail_start.min(res_ordinal + 1);
                if args
                    .index
                    .range_tokens(args.offset.max(arc.inv_ordinal), end)
                    > args.cap_tokens
                {
                    oversize_atomic_unit = true;
                }
            }
            continue;
        }
        if arc.inv_ordinal >= args.recent_open_arc_cutoff
            && arc.inv_ordinal >= args.offset
            && arc.inv_ordinal < end
        {
            end = end.min(arc.inv_ordinal);
            fenced_by_open_arc = true;
        }
    }
    if end <= args.offset && args.offset < args.protected_tail_start {
        return HeadCapResult {
            eligible_end_ordinal: args.offset,
            oversize_atomic_unit,
            fenced_by_open_arc,
        };
    }
    HeadCapResult {
        eligible_end_ordinal: end.min(args.protected_tail_start),
        oversize_atomic_unit,
        fenced_by_open_arc,
    }
}

#[derive(Debug, Clone)]
struct ChunkBlock {
    role: String,
    start_ordinal: u64,
    end_ordinal: u64,
    parts: Vec<String>,
    meta: Vec<(u64, String)>,
    commit_hashes: Vec<String>,
    is_tool_only: bool,
}

struct ChunkBuilder {
    budget_stop: f64,
    total_tokens: f64,
    measured_tokens: f64,
    messages_processed: usize,
    last_ordinal: u64,
    current_block: Option<ChunkBlock>,
    pending_noise_meta: Vec<(u64, String)>,
    formatted_blocks: Vec<String>,
    block_tokens: Vec<f64>,
    commit_cluster_count: usize,
    last_flushed_role: String,
    stopped_early: bool,
}

impl ChunkBuilder {
    fn new(budget_stop: f64) -> Self {
        Self {
            budget_stop,
            total_tokens: 0.0,
            measured_tokens: 0.0,
            messages_processed: 0,
            last_ordinal: 0,
            current_block: None,
            pending_noise_meta: Vec::new(),
            formatted_blocks: Vec::new(),
            block_tokens: Vec::new(),
            commit_cluster_count: 0,
            last_flushed_role: String::new(),
            stopped_early: false,
        }
    }

    fn push_message(&mut self, message: &BoundaryMsg) -> bool {
        let meta = (message.message_ordinal, message.message_id.clone());
        if message.role == Role::User && !has_meaningful_user_text(&message.blocks) {
            let tc_summaries = extract_tool_call_summaries(&message.blocks);
            if tc_summaries.is_empty() {
                self.pending_noise_meta.push(meta);
                return true;
            }
            let tc_text = tc_summaries.join(" / ");
            if let Some(current) = self
                .current_block
                .as_mut()
                .filter(|block| block.role == "A")
            {
                current.end_ordinal = message.message_ordinal;
                current.parts.push(tc_text);
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
                .map(|(ordinal, _)| *ordinal)
                .unwrap_or(message.message_ordinal);
            let mut meta_list = std::mem::take(&mut self.pending_noise_meta);
            meta_list.push(meta);
            self.current_block = Some(ChunkBlock {
                role: "A".to_string(),
                start_ordinal: start,
                end_ordinal: message.message_ordinal,
                parts: vec![tc_text],
                meta: meta_list,
                commit_hashes: Vec::new(),
                is_tool_only: true,
            });
            return true;
        }

        let role = compact_role(message.role.as_str());
        let text_parts = text_parts(message);
        let tool_summaries = if text_parts.is_empty() {
            extract_tool_call_summaries(&message.blocks)
        } else {
            Vec::new()
        };
        let mut all_parts = text_parts.clone();
        all_parts.extend(tool_summaries);
        let compacted = compact_text_for_summary(&all_parts.join(" / "), message.role.as_str());
        let text = compacted.text;
        if text.is_empty() {
            self.pending_noise_meta.push(meta);
            return true;
        }
        let msg_has_narrative = !text_parts.is_empty();
        if let Some(current) = self
            .current_block
            .as_mut()
            .filter(|block| block.role == role)
        {
            current.end_ordinal = message.message_ordinal;
            current.parts.push(text);
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
            .map(|(ordinal, _)| *ordinal)
            .unwrap_or(message.message_ordinal);
        let mut meta_list = std::mem::take(&mut self.pending_noise_meta);
        meta_list.push(meta);
        self.current_block = Some(ChunkBlock {
            role,
            start_ordinal: start,
            end_ordinal: message.message_ordinal,
            parts: vec![text],
            meta: meta_list,
            commit_hashes: compacted.commit_hashes,
            is_tool_only: !msg_has_narrative,
        });
        true
    }

    fn flush_current_block(&mut self) -> bool {
        let Some(current_block) = self.current_block.take() else {
            return true;
        };
        let block_text = format_block(&current_block);
        let block_tokens = estimate_tokens(&block_text) as f64;
        if self.total_tokens + block_tokens > self.budget_stop && self.total_tokens > 0.0 {
            self.current_block = Some(current_block);
            self.stopped_early = true;
            return false;
        }
        if current_block.role == "A"
            && !current_block.commit_hashes.is_empty()
            && self.last_flushed_role != "A"
        {
            self.commit_cluster_count += 1;
        }
        self.last_flushed_role.clone_from(&current_block.role);
        self.last_ordinal = current_block
            .meta
            .last()
            .map(|(ordinal, _)| *ordinal)
            .unwrap_or(current_block.end_ordinal);
        self.messages_processed += current_block.meta.len();
        self.formatted_blocks.push(block_text);
        self.block_tokens.push(block_tokens);
        self.total_tokens += block_tokens;
        self.measured_tokens += block_tokens;
        true
    }

    fn finish(
        mut self,
        total_message_count: u64,
        eligible_end_ordinal: Option<u64>,
    ) -> ChunkEstimate {
        let _ = self.flush_current_block();
        let terminal = eligible_end_ordinal
            .map(|end| end.saturating_sub(1).min(total_message_count))
            .unwrap_or(total_message_count);
        let has_more = self.last_ordinal < terminal;
        let tokens = if has_more && self.total_tokens < self.budget_stop && self.total_tokens > 0.0
        {
            self.budget_stop
        } else {
            self.total_tokens
        };
        ChunkEstimate {
            tokens,
            has_more,
            formatted_blocks: self.formatted_blocks,
            block_tokens: self.block_tokens,
            message_count: self.messages_processed,
            commit_cluster_count: self.commit_cluster_count,
        }
    }
}

#[derive(Debug, Clone)]
struct CompactedText {
    text: String,
    commit_hashes: Vec<String>,
}

fn has_meaningful_user_text(blocks: &[BoundaryBlock]) -> bool {
    blocks.iter().any(|block| {
        if block.ignored || !matches!(block.kind, SelKind::Text) {
            return false;
        }
        let cleaned = clean_user_text(&block.original);
        !cleaned.trim().is_empty() && !is_system_directive(&cleaned)
    })
}

fn text_parts(message: &BoundaryMsg) -> Vec<String> {
    message
        .blocks
        .iter()
        .filter_map(|block| {
            if block.ignored || !matches!(block.kind, SelKind::Text) {
                return None;
            }
            let text = block.original.trim();
            if text.is_empty() {
                return None;
            }
            let cleaned = if message.role == Role::User {
                clean_user_text(text)
            } else {
                text.to_string()
            };
            let normalized = normalize_text(&cleaned);
            if normalized.is_empty() {
                None
            } else {
                Some(normalized)
            }
        })
        .collect()
}

fn extract_tool_call_summaries(blocks: &[BoundaryBlock]) -> Vec<String> {
    let mut summaries = Vec::new();
    for block in blocks {
        let SelKind::ToolCall { name, input } = &block.kind else {
            continue;
        };
        if let Some(description) = input
            .get("description")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            summaries.push(format!("TC: {description}"));
            continue;
        }
        let key_arg = extract_key_arg(input);
        if let Some(key_arg) = key_arg {
            summaries.push(format!("TC: {name}({key_arg})"));
        } else {
            summaries.push(format!("TC: {name}"));
        }
    }
    summaries
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
    let max_len = 60;
    if value.chars().count() <= max_len {
        return value.to_string();
    }
    let mut out = value.chars().take(max_len).collect::<String>();
    out.push('…');
    out
}

fn clean_user_text(text: &str) -> String {
    let without_reminders = system_reminder_regex().replace_all(text, "");
    without_reminders
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
    if next.is_empty() {
        return existing.to_vec();
    }
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
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(?:commit(?:ted|ting|s)?|cherry-?pick(?:ed|ing|s)?|merge[ds]?|merging|rebas(?:e|ed|es|ing))\b",
        )
        .unwrap()
    })
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
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct GoldenRoot {
        constants: BTreeMap<String, f64>,
        chunk_cases: Vec<ChunkCase>,
        boundary_cases: Vec<BoundaryCase>,
        trigger_cases: Vec<TriggerCase>,
    }

    #[derive(Deserialize)]
    struct ChunkCase {
        label: String,
        messages: Vec<MessageJson>,
        budget_stop: f64,
        expected: ChunkExpected,
    }

    #[derive(Deserialize)]
    struct ChunkExpected {
        formatted_blocks: Vec<String>,
        block_tokens: Vec<f64>,
        tokens: f64,
        has_more: bool,
        message_count: usize,
        commit_cluster_count: usize,
    }

    #[derive(Deserialize)]
    struct BoundaryCase {
        label: String,
        messages: Vec<MessageJson>,
        ctx: BoundaryCtxJson,
        expected: BoundaryExpected,
    }

    #[derive(Deserialize)]
    struct BoundaryExpected {
        protected_start_ordinal: u64,
        eligible_head_start: u64,
        eligible_head_end: u64,
        n_tokens: f64,
        floored_by_live_prompt: bool,
        fenced_by_open_arc: bool,
        true_raw_eligible_tokens: f64,
        oversize_atomic_unit: bool,
    }

    #[derive(Deserialize)]
    struct TriggerCase {
        label: String,
        messages: Vec<MessageJson>,
        ctx: TriggerCtxJson,
        expected: TriggerExpected,
    }

    #[derive(Deserialize)]
    struct TriggerExpected {
        fire: bool,
        reason: Option<String>,
        consume_through_ordinal: Option<u64>,
    }

    #[derive(Deserialize)]
    struct TriggerCtxJson {
        boundary: BoundaryCtxJson,
        compartment_in_progress: bool,
        projected_post_drop_percentage: Option<f64>,
        commit_cluster_trigger_enabled: bool,
        min_commit_clusters: usize,
    }

    #[derive(Deserialize)]
    struct BoundaryCtxJson {
        context_limit: f64,
        execute_threshold_percentage: f64,
        usage_percentage: f64,
        usage_input_tokens: f64,
        last_compartment_end_ordinal: Option<u64>,
        prior_boundary_ordinal: u64,
        migration_floor_active: bool,
        emergency_tail_scale: Option<f64>,
        trigger_budget: Option<f64>,
    }

    #[derive(Deserialize)]
    struct MessageJson {
        message_ordinal: u64,
        message_id: String,
        role: String,
        blocks: Vec<BlockJson>,
    }

    #[derive(Deserialize)]
    struct BlockJson {
        id: String,
        ordinal: u64,
        kind: Value,
        provider_executed: bool,
        byte_size: usize,
        arc_id: Option<String>,
        original: String,
        rendered: Option<String>,
        ignored: Option<bool>,
    }

    fn parse_kind(value: &Value) -> SelKind {
        if let Some(s) = value.as_str() {
            return match s {
                "Reasoning" => SelKind::Reasoning,
                "Text" => SelKind::Text,
                "RedactedReasoning" => SelKind::RedactedReasoning,
                "Media" => SelKind::Media,
                _ => SelKind::Opaque,
            };
        }
        if let Some(obj) = value.as_object() {
            if let Some(tc) = obj.get("ToolCall") {
                return SelKind::ToolCall {
                    name: tc
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    input: tc.get("input").cloned().unwrap_or(Value::Null),
                };
            }
            if let Some(tr) = obj.get("ToolResult") {
                return SelKind::ToolResult {
                    tool_name: tr
                        .get("tool_name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                };
            }
        }
        SelKind::Opaque
    }

    fn messages(json: &[MessageJson]) -> Vec<BoundaryMsg> {
        json.iter()
            .map(|message| BoundaryMsg {
                message_ordinal: message.message_ordinal,
                message_id: message.message_id.clone(),
                role: Role::from_provider(&message.role),
                blocks: message
                    .blocks
                    .iter()
                    .map(|block| BoundaryBlock {
                        id: block.id.clone(),
                        ordinal: block.ordinal,
                        kind: parse_kind(&block.kind),
                        provider_executed: block.provider_executed,
                        byte_size: block.byte_size,
                        arc_id: block.arc_id.clone(),
                        original: block.original.clone(),
                        rendered: block.rendered.clone(),
                        ignored: block.ignored.unwrap_or(false),
                    })
                    .collect(),
            })
            .collect()
    }

    fn boundary_ctx(json: &BoundaryCtxJson) -> BoundaryContext {
        BoundaryContext {
            context_limit: json.context_limit,
            execute_threshold_percentage: json.execute_threshold_percentage,
            usage_percentage: json.usage_percentage,
            usage_input_tokens: json.usage_input_tokens,
            last_compartment_end_ordinal: json.last_compartment_end_ordinal,
            prior_boundary_ordinal: json.prior_boundary_ordinal,
            migration_floor_active: json.migration_floor_active,
            emergency_tail_scale: json.emergency_tail_scale,
            trigger_budget: json.trigger_budget,
        }
    }

    fn golden() -> GoldenRoot {
        serde_json::from_str(include_str!("../testdata/boundary-golden.json"))
            .expect("parse boundary-golden.json")
    }

    fn assert_const(constants: &BTreeMap<String, f64>, name: &str, value: f64) {
        let got = constants
            .get(name)
            .unwrap_or_else(|| panic!("missing constant {name}"));
        assert!(
            (got - value).abs() < f64::EPSILON,
            "constant {name} drifted: TS={got} Rust={value}"
        );
    }

    #[test]
    fn boundary_constants_match_ts_sources() {
        let constants = golden().constants;
        assert_const(&constants, "ALPHA", ALPHA);
        assert_const(&constants, "FLOOR_RATIO", FLOOR_RATIO);
        assert_const(&constants, "FLOOR_MIN", FLOOR_MIN);
        assert_const(&constants, "FLOOR_MAX", FLOOR_MAX);
        assert_const(&constants, "ABS_CAP", ABS_CAP);
        assert_const(&constants, "MAX_USABLE_RATIO", MAX_USABLE_RATIO);
        assert_const(&constants, "RESERVED_HEADROOM_MIN", RESERVED_HEADROOM_MIN);
        assert_const(
            &constants,
            "RESERVED_HEADROOM_RATIO",
            RESERVED_HEADROOM_RATIO,
        );
        assert_const(&constants, "NON_EMERGENCY_MAX_CAP", NON_EMERGENCY_MAX_CAP);
        assert_const(&constants, "FORCE80_MAX_CAP", FORCE80_MAX_CAP);
        assert_const(&constants, "FORCE95_MAX_CAP", FORCE95_MAX_CAP);
        assert_const(
            &constants,
            "NORMAL_HYSTERESIS_TOKENS",
            NORMAL_HYSTERESIS_TOKENS,
        );
        assert_const(
            &constants,
            "MIN_FORCE_ELIGIBLE_TOKENS_CAP",
            MIN_FORCE_ELIGIBLE_TOKENS_CAP,
        );
        assert_const(
            &constants,
            "TRIGGER_BUDGET_PERCENTAGE",
            TRIGGER_BUDGET_PERCENTAGE,
        );
        assert_const(&constants, "TRIGGER_BUDGET_MIN", TRIGGER_BUDGET_MIN);
        assert_const(&constants, "TRIGGER_BUDGET_MAX", TRIGGER_BUDGET_MAX);
        assert_const(
            &constants,
            "PROACTIVE_TRIGGER_OFFSET_PERCENTAGE",
            PROACTIVE_TRIGGER_OFFSET_PERCENTAGE,
        );
        assert_const(&constants, "POST_DROP_TARGET_RATIO", POST_DROP_TARGET_RATIO);
        assert_const(
            &constants,
            "MIN_PROACTIVE_TAIL_TOKEN_ESTIMATE",
            MIN_PROACTIVE_TAIL_TOKEN_ESTIMATE,
        );
        assert_const(
            &constants,
            "MIN_PROACTIVE_TAIL_MESSAGE_COUNT",
            MIN_PROACTIVE_TAIL_MESSAGE_COUNT as f64,
        );
        assert_const(
            &constants,
            "DEFAULT_MIN_COMMIT_CLUSTERS_FOR_TRIGGER",
            DEFAULT_MIN_COMMIT_CLUSTERS_FOR_TRIGGER as f64,
        );
        assert_const(
            &constants,
            "TAIL_SIZE_TRIGGER_MULTIPLIER",
            TAIL_SIZE_TRIGGER_MULTIPLIER,
        );
        assert_const(
            &constants,
            "FORCE_COMPARTMENT_PERCENTAGE",
            FORCE_COMPARTMENT_PERCENTAGE,
        );
        assert_const(
            &constants,
            "BLOCK_UNTIL_DONE_PERCENTAGE",
            BLOCK_UNTIL_DONE_PERCENTAGE,
        );
        assert_const(
            &constants,
            "FORCE_MATERIALIZE_PERCENTAGE",
            FORCE_MATERIALIZE_PERCENTAGE,
        );
        assert_const(
            &constants,
            "MAX_COMMITS_PER_BLOCK",
            MAX_COMMITS_PER_BLOCK as f64,
        );
    }

    #[test]
    fn chunk_golden_matches_ts_formatting() {
        for case in golden().chunk_cases {
            let msgs = messages(&case.messages);
            let estimate = chunked_message_estimate(&msgs, 1, None, case.budget_stop);
            assert_eq!(
                estimate.formatted_blocks, case.expected.formatted_blocks,
                "formatted block mismatch in {}",
                case.label
            );
            assert_eq!(
                estimate.block_tokens, case.expected.block_tokens,
                "block tokens in {}",
                case.label
            );
            assert_eq!(
                estimate.tokens, case.expected.tokens,
                "tokens in {}",
                case.label
            );
            assert_eq!(
                estimate.has_more, case.expected.has_more,
                "has_more in {}",
                case.label
            );
            assert_eq!(
                estimate.message_count, case.expected.message_count,
                "message_count in {}",
                case.label
            );
            assert_eq!(
                estimate.commit_cluster_count, case.expected.commit_cluster_count,
                "commit clusters in {}",
                case.label
            );
        }
    }

    #[test]
    fn boundary_golden_matches_ts_resolution() {
        for case in golden().boundary_cases {
            let msgs = messages(&case.messages);
            let got = resolve_protected_tail_boundary(&msgs, &boundary_ctx(&case.ctx));
            assert_eq!(
                got.protected_start_ordinal, case.expected.protected_start_ordinal,
                "protected start in {}",
                case.label
            );
            assert_eq!(
                got.eligible_head.start, case.expected.eligible_head_start,
                "eligible start in {}",
                case.label
            );
            assert_eq!(
                got.eligible_head.end, case.expected.eligible_head_end,
                "eligible end in {}",
                case.label
            );
            assert_eq!(got.n_tokens, case.expected.n_tokens, "N in {}", case.label);
            assert_eq!(
                got.floored_by_live_prompt, case.expected.floored_by_live_prompt,
                "live floor in {}",
                case.label
            );
            assert_eq!(
                got.fenced_by_open_arc, case.expected.fenced_by_open_arc,
                "open fence in {}",
                case.label
            );
            assert_eq!(
                got.true_raw_eligible_tokens, case.expected.true_raw_eligible_tokens,
                "true raw eligible in {}",
                case.label
            );
            assert_eq!(
                got.oversize_atomic_unit, case.expected.oversize_atomic_unit,
                "oversize in {}",
                case.label
            );
        }
    }

    #[test]
    fn trigger_golden_matches_ts_decision_core() {
        for case in golden().trigger_cases {
            let msgs = messages(&case.messages);
            let ctx = TriggerContext {
                boundary: boundary_ctx(&case.ctx.boundary),
                compartment_in_progress: case.ctx.compartment_in_progress,
                projected_post_drop_percentage: case.ctx.projected_post_drop_percentage,
                commit_cluster_trigger_enabled: case.ctx.commit_cluster_trigger_enabled,
                min_commit_clusters: case.ctx.min_commit_clusters,
            };
            let got = check_compartment_trigger(&msgs, &ctx);
            assert_eq!(got.fire, case.expected.fire, "fire in {}", case.label);
            assert_eq!(
                got.reason.map(TriggerReason::as_str).map(str::to_string),
                case.expected.reason,
                "reason in {}",
                case.label
            );
            assert_eq!(
                got.consume_through_ordinal, case.expected.consume_through_ordinal,
                "consume through in {}",
                case.label
            );
        }
    }

    fn text_msg(ord: u64, role: Role, text: &str) -> BoundaryMsg {
        BoundaryMsg {
            message_ordinal: ord,
            message_id: format!("m-{ord}"),
            role,
            blocks: vec![BoundaryBlock {
                id: format!("m-{ord}#text"),
                ordinal: ord,
                kind: SelKind::Text,
                provider_executed: false,
                byte_size: text.len(),
                arc_id: None,
                original: text.to_string(),
                rendered: None,
                ignored: false,
            }],
        }
    }

    fn tool_call_msg(ord: u64, arc_id: &str) -> BoundaryMsg {
        BoundaryMsg {
            message_ordinal: ord,
            message_id: format!("m-{ord}"),
            role: Role::Assistant,
            blocks: vec![BoundaryBlock {
                id: format!("{arc_id}#call"),
                ordinal: ord,
                kind: SelKind::ToolCall {
                    name: "bash".to_string(),
                    input: serde_json::json!({"description":"run build"}),
                },
                provider_executed: false,
                byte_size: 32,
                arc_id: Some(arc_id.to_string()),
                original: "{\"description\":\"run build\"}".to_string(),
                rendered: None,
                ignored: false,
            }],
        }
    }

    fn ctx_for_tests() -> BoundaryContext {
        BoundaryContext {
            context_limit: 20_000.0,
            execute_threshold_percentage: 50.0,
            usage_percentage: 81.0,
            usage_input_tokens: 8_100.0,
            last_compartment_end_ordinal: None,
            prior_boundary_ordinal: 1,
            migration_floor_active: false,
            emergency_tail_scale: None,
            trigger_budget: None,
        }
    }

    #[test]
    fn boundary_determinism_same_tail_same_resolution() {
        let tail = vec![
            text_msg(1, Role::User, "start"),
            text_msg(2, Role::Assistant, &"alpha ".repeat(900)),
            text_msg(3, Role::Assistant, &"beta ".repeat(900)),
        ];
        let ctx = ctx_for_tests();
        let first = resolve_protected_tail_boundary(&tail, &ctx);
        let second = resolve_protected_tail_boundary(&tail, &ctx);
        assert_eq!(first, second);
    }

    #[test]
    fn adding_newer_items_never_moves_protected_start_below_anchor() {
        let mut tail = vec![
            text_msg(1, Role::User, "published"),
            text_msg(2, Role::Assistant, &"old ".repeat(800)),
        ];
        let mut ctx = ctx_for_tests();
        ctx.last_compartment_end_ordinal = Some(1);
        let before = resolve_protected_tail_boundary(&tail, &ctx);
        tail.push(text_msg(3, Role::Assistant, &"new ".repeat(1200)));
        let after = resolve_protected_tail_boundary(&tail, &ctx);
        assert!(before.protected_start_ordinal >= 2);
        assert!(after.protected_start_ordinal >= 2);
    }

    #[test]
    fn open_arc_staleness_flips_when_newer_growth_pushes_it_older_than_size_walk() {
        let mut recent_tail = vec![
            text_msg(1, Role::User, &"begin ".repeat(400)),
            tool_call_msg(2, "arc-open"),
            text_msg(3, Role::Assistant, &"small ".repeat(100)),
        ];
        let mut ctx = ctx_for_tests();
        ctx.emergency_tail_scale = Some(0.25);
        let recent = resolve_protected_tail_boundary(&recent_tail, &ctx);
        assert!(recent.fenced_by_open_arc, "recent open arc should fence");
        assert_eq!(recent.protected_start_ordinal, 2);

        for ord in 4..14 {
            recent_tail.push(text_msg(ord, Role::Assistant, &"growth ".repeat(800)));
        }
        let stale = resolve_protected_tail_boundary(&recent_tail, &ctx);
        assert!(
            stale.protected_start_ordinal > 2,
            "new growth should push the size-walk start after the abandoned open arc"
        );
        assert!(
            !stale.fenced_by_open_arc,
            "stale open arc must be compactable"
        );
    }

    #[test]
    fn trigger_never_consumes_the_protected_tail() {
        let tail = vec![
            text_msg(1, Role::User, "begin"),
            text_msg(2, Role::Assistant, &"alpha beta gamma ".repeat(4_000)),
            text_msg(3, Role::User, "next task"),
            text_msg(4, Role::Assistant, &"delta epsilon ".repeat(4_000)),
        ];
        let mut trigger = TriggerContext::default();
        trigger.boundary.context_limit = 20_000.0;
        trigger.boundary.execute_threshold_percentage = 50.0;
        trigger.boundary.usage_percentage = 81.0;
        let boundary = resolve_protected_tail_boundary(&tail, &trigger.boundary);
        let decision = check_compartment_trigger(&tail, &trigger);
        if let Some(consume) = decision.consume_through_ordinal {
            assert!(consume < boundary.protected_start_ordinal);
        }
    }

    #[test]
    fn chunk_has_more_saturates_at_budget_stop() {
        let tail = vec![
            text_msg(1, Role::User, &"one ".repeat(200)),
            text_msg(2, Role::Assistant, &"two ".repeat(200)),
            text_msg(3, Role::User, &"three ".repeat(200)),
        ];
        let estimate = chunked_message_estimate(&tail, 1, None, 50.0);
        assert!(estimate.has_more);
        assert!(estimate.tokens >= 50.0);
    }

    #[test]
    fn zero_based_trigger_counts_ordinal_zero_content() {
        let tail = (0..=5)
            .map(|ord| text_msg(ord, Role::Assistant, &"zero based content ".repeat(4_000)))
            .collect::<Vec<_>>();
        let mut trigger = TriggerContext::default();
        trigger.boundary.context_limit = 20_000.0;
        trigger.boundary.execute_threshold_percentage = 50.0;
        trigger.boundary.usage_percentage = 81.0;

        let decision = check_compartment_trigger(&tail, &trigger);

        assert!(
            decision.fire,
            "ordinal-0 content contributes to the trigger"
        );
        let boundary = decision.boundary.expect("fire carries boundary");
        assert_eq!(boundary.eligible_head.start, 0);
        assert!(boundary.true_raw_eligible_tokens > 0.0);
    }

    #[test]
    fn compartment_ending_at_ordinal_zero_starts_next_window_at_one() {
        let tail = (0..=5)
            .map(|ord| text_msg(ord, Role::Assistant, &"published floor ".repeat(1_000)))
            .collect::<Vec<_>>();
        let mut ctx = ctx_for_tests();
        ctx.last_compartment_end_ordinal = Some(0);
        ctx.trigger_budget = Some(1_000.0);

        let boundary = resolve_protected_tail_boundary(&tail, &ctx);
        let chunk = chunked_message_estimate(
            &tail,
            boundary.eligible_head.start,
            Some(boundary.protected_start_ordinal),
            10_000.0,
        );

        assert_eq!(boundary.eligible_head.start, 1);
        assert!(!chunk
            .formatted_blocks
            .iter()
            .any(|block| block.contains("[0]")));
    }

    #[test]
    fn boundary_measures_original_bytes_not_rendered_reduction_placeholders() {
        let original = "raw tool output ".repeat(400);
        let mut reduced = text_msg(1, Role::Assistant, &original);
        reduced.blocks[0].rendered = Some("[dropped]".to_string());
        let unreduced = text_msg(1, Role::Assistant, &original);
        let ctx = ctx_for_tests();
        let a = resolve_protected_tail_boundary(&[reduced], &ctx);
        let b = resolve_protected_tail_boundary(&[unreduced], &ctx);
        assert_eq!(a.true_raw_eligible_tokens, b.true_raw_eligible_tokens);
        assert_eq!(a.protected_start_ordinal, b.protected_start_ordinal);
    }
}

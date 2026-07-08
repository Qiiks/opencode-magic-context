//! Historian output validation: parse the historian's compartment XML and
//! validate it against the raw chunk and already-persisted compartment ranges
//! before any side effect can publish it.
//!
//! The functions in this module are deliberately pure. They receive the raw
//! historian text plus caller-provided chunk/store metadata, and return either a
//! fully mapped publish plan or a validation error. That keeps persistence code
//! fail-closed: malformed ranges, stale chunks, bad message-id endpoints, and
//! boundary-healing decisions are resolved before any database write is possible.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

const SAFETY_HEAL_GAP: u64 = 15;
const BOUNDARY_HEALING_SLACK: u64 = 2;

/// A raw ordinal range, inclusive on both ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageRange {
    pub start: u64,
    pub end: u64,
}

/// One formatted chunk line that can be mapped back to a provider message id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkLine {
    pub ordinal: u64,
    /// CONTRACT: the FLAT BLOCK ID (`<mid>#<index>`) of the line's last block — never a
    /// bare harness/CK message id. This value becomes the published compartment's
    /// end_message_id and, when that compartment folds, the coverage boundary anchor.
    /// Boundary presence is checked against live flat block ids, so any other vocabulary
    /// mints an anchor that can never be present (the transform's mint-absent guard then
    /// fails the fold loudly). The production chunk builder must derive this from the
    /// flattened projection, not from the raw CK message id.
    pub message_id: String,
}

/// The raw-history slice that the historian was asked to summarize.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistorianChunk {
    pub start_index: u64,
    pub end_index: u64,
    pub lines: Vec<ChunkLine>,
    /// All non-synthetic input ordinals visible when this chunk was built, in
    /// provider order. Claude Code proxy submissions can permanently retire
    /// ordinals when message identities are re-minted, so validation filters
    /// this sparse set to the claimed range instead of assuming 0..n density.
    #[serde(default)]
    pub present_ordinals: Vec<u64>,
    /// Gaps fully inside one of these ranges are safe to heal at any size because
    /// the omitted raw lines were tool-only transcript noise rather than narrative.
    #[serde(default)]
    pub tool_only_ranges: Vec<MessageRange>,
}

/// An already-persisted compartment range with the raw start and end ordinals
/// needed to validate store ordering before appending new compartments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCompartmentRange {
    pub start_message: u64,
    pub end_message: u64,
}

/// Options that are known by the runner but are not present in the historian XML.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ValidateOptions {
    /// Sequence number to assign to the first emitted compartment in this publish.
    #[serde(default)]
    pub sequence_offset: u64,
    /// When true, emergency recovery favors fast raw-history reduction over the
    /// highest-quality final boundary for the newest compartment.
    #[serde(default)]
    pub in_emergency: bool,
}

/// A parsed compartment before endpoint ids are resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedCompartment {
    pub start_message: u64,
    pub end_message: u64,
    pub title: String,
    /// In v2 compartments the main body is duplicated into `p1`; v1/legacy
    /// compartments store their body text only in this flat `content` field.
    pub content: String,
    #[serde(default)]
    pub p1: Option<String>,
    #[serde(default)]
    pub p2: Option<String>,
    #[serde(default)]
    pub p3: Option<String>,
    #[serde(default)]
    pub p4: Option<String>,
    #[serde(default)]
    pub importance: Option<u64>,
    #[serde(default)]
    pub episode_type: Option<String>,
}

/// A fact extracted from the `<facts>` block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactCandidate {
    pub category: String,
    pub content: String,
    /// Optional forward-compatible anchor. Current TypeScript facts are
    /// unanchored; when absent and the last compartment is discarded during
    /// boundary healing, the fact is skipped because its source compartment
    /// cannot be proven.
    #[serde(default)]
    pub origin_compartment_index: Option<u64>,
}

/// A historian-extracted event. The event kind is the XML element name; fields
/// are child element text keyed by element name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedEvent {
    pub kind: String,
    #[serde(default)]
    pub at_compartment: Option<u64>,
    #[serde(default)]
    pub fields: BTreeMap<String, String>,
}

/// A durable standing-question candidate for later primer generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrimerCandidate {
    pub question: String,
    /// 1-based index into this historian output's emitted compartments.
    #[serde(default)]
    pub origin_compartment_index: Option<u64>,
}

/// Optional user-memory observation extracted from the chunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserObservationCandidate {
    pub content: String,
    /// Optional forward-compatible anchor. Current TypeScript observations are
    /// unanchored; when the last compartment is discarded during boundary
    /// healing, an unanchored observation is skipped because its source
    /// compartment cannot be proven.
    #[serde(default)]
    pub origin_compartment_index: Option<u64>,
}

/// Parsed XML-ish historian output, before validation mutates/heals ranges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedCompartmentOutput {
    #[serde(default)]
    pub compartments: Vec<ParsedCompartment>,
    #[serde(default)]
    pub facts: Vec<FactCandidate>,
    #[serde(default)]
    pub events: Vec<ParsedEvent>,
    #[serde(default)]
    pub unprocessed_from: Option<u64>,
    #[serde(default)]
    pub user_observations: Vec<UserObservationCandidate>,
    #[serde(default)]
    pub primer_candidates: Vec<PrimerCandidate>,
}

/// A compartment whose raw endpoints have been resolved to provider message ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatedCompartment {
    pub sequence: u64,
    pub start_message: u64,
    pub end_message: u64,
    pub start_message_id: String,
    pub end_message_id: String,
    pub title: String,
    pub content: String,
    #[serde(default)]
    pub p1: Option<String>,
    #[serde(default)]
    pub p2: Option<String>,
    #[serde(default)]
    pub p3: Option<String>,
    #[serde(default)]
    pub p4: Option<String>,
    #[serde(default)]
    pub importance: Option<u64>,
    #[serde(default)]
    pub episode_type: Option<String>,
}

/// The side-effect-free publish plan produced by validation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatedChunk {
    pub compartments: Vec<ValidatedCompartment>,
    pub facts: Vec<FactCandidate>,
    pub events: Vec<ParsedEvent>,
    pub primer_candidates: Vec<PrimerCandidate>,
    pub user_observations: Vec<UserObservationCandidate>,
    /// The next raw ordinal to read after the compartments that are safe to persist.
    pub unprocessed_from: u64,
    /// True when the provisional last compartment was intentionally withheld so it
    /// can be re-derived with real lookahead in the next run.
    pub discarded_last: bool,
}

/// Validation failures are plain, serializable messages because callers surface
/// them in repair prompts and telemetry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistorianValidationError {
    pub message: String,
}

impl std::fmt::Display for HistorianValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for HistorianValidationError {}

fn validation_error(message: impl Into<String>) -> HistorianValidationError {
    HistorianValidationError {
        message: message.into(),
    }
}

/// Parse the historian's XML-ish output using the same permissive extraction
/// semantics as the TypeScript host parser. Malformed XML simply yields fewer
/// usable structures; validation decides whether that is acceptable.
pub fn parse_compartment_output(
    text: &str,
) -> Result<ParsedCompartmentOutput, HistorianValidationError> {
    let mut compartments = Vec::new();
    let mut facts = Vec::new();

    for caps in compartment_regex().captures_iter(text) {
        let attrs = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
        let inner = caps.get(2).map(|m| m.as_str()).unwrap_or_default();

        let start_message = match capture_u64(attr_start_regex(), attrs) {
            Some(v) => v,
            None => continue,
        };
        let end_message = match capture_u64(attr_end_regex(), attrs) {
            Some(v) => v,
            None => continue,
        };
        let title = match capture_string(attr_title_regex(), attrs) {
            Some(v) if !v.is_empty() => unescape_xml(&v),
            _ => continue,
        };
        if title.is_empty() {
            continue;
        }

        let episode_type = capture_string(attr_episode_regex(), attrs).map(|s| unescape_xml(&s));
        let importance = capture_u64(attr_importance_regex(), attrs);

        let p1 = extract_tier(inner, 0);
        if let Some(p1_value) = p1.filter(|s| !s.is_empty()) {
            let p2 = extract_tier(inner, 1);
            let p3 = extract_tier(inner, 2);
            let p4 = extract_tier(inner, 3);
            let p2_value = p2.clone().unwrap_or_else(|| p1_value.clone());
            let p3_value = p3
                .clone()
                .unwrap_or_else(|| p2.clone().unwrap_or_else(|| p1_value.clone()));
            let p4_value = p4.unwrap_or_default();
            compartments.push(ParsedCompartment {
                start_message,
                end_message,
                title,
                content: p1_value.clone(),
                p1: Some(p1_value),
                p2: Some(p2_value),
                p3: Some(p3_value),
                p4: Some(p4_value),
                importance,
                episode_type,
            });
            continue;
        }

        let content = unescape_xml(inner.trim());
        if !content.is_empty() {
            compartments.push(ParsedCompartment {
                start_message,
                end_message,
                title,
                content,
                p1: None,
                p2: None,
                p3: None,
                p4: None,
                importance,
                episode_type,
            });
        }
    }

    let facts_scope = if let Some(caps) = facts_block_regex().captures(text) {
        caps.get(1)
            .map(|m| m.as_str().to_string())
            .unwrap_or_default()
    } else {
        let without_events = events_block_regex().replace_all(text, "");
        compartment_regex()
            .replace_all(&without_events, "")
            .to_string()
    };

    for category_caps in category_block_regex().captures_iter(&facts_scope) {
        let category = category_caps.get(1).map(|m| m.as_str()).unwrap_or_default();
        let closing = category_caps.get(3).map(|m| m.as_str()).unwrap_or_default();
        if category != closing {
            continue;
        }
        let block = category_caps.get(2).map(|m| m.as_str()).unwrap_or_default();
        for item_caps in fact_item_regex().captures_iter(block) {
            let raw = item_caps.get(1).map(|m| m.as_str()).unwrap_or_default();
            let unescaped = unescape_xml(raw.trim());
            let (origin_compartment_index, content) = split_anchor_prefix(&unescaped);
            if !content.is_empty() {
                facts.push(FactCandidate {
                    category: category.to_string(),
                    content,
                    origin_compartment_index,
                });
            }
        }
    }

    let unprocessed_from = capture_u64(unprocessed_regex(), text);

    let mut user_observations = Vec::new();
    if let Some(caps) = user_observations_regex().captures(text) {
        let block = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
        for item_caps in user_obs_item_regex().captures_iter(block) {
            let raw = item_caps.get(1).map(|m| m.as_str()).unwrap_or_default();
            let unescaped = unescape_xml(raw.trim());
            let (origin_compartment_index, content) = split_anchor_prefix(&unescaped);
            if !content.is_empty() {
                user_observations.push(UserObservationCandidate {
                    content,
                    origin_compartment_index,
                });
            }
        }
    }

    let mut primer_candidates = Vec::new();
    if let Some(caps) = primer_candidates_regex().captures(text) {
        let block = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
        let mut saw_element = false;
        for primer_caps in primer_element_regex().captures_iter(block) {
            saw_element = true;
            let question = primer_caps
                .get(2)
                .map(|m| unescape_xml(m.as_str().trim()))
                .unwrap_or_default();
            if !question.is_empty() {
                primer_candidates.push(PrimerCandidate {
                    question,
                    origin_compartment_index: primer_caps
                        .get(1)
                        .and_then(|m| m.as_str().parse::<u64>().ok()),
                });
            }
        }
        if !saw_element {
            for item_caps in primer_item_regex().captures_iter(block) {
                let question = item_caps
                    .get(1)
                    .map(|m| unescape_xml(m.as_str().trim()))
                    .unwrap_or_default();
                if !question.is_empty() {
                    primer_candidates.push(PrimerCandidate {
                        question,
                        origin_compartment_index: None,
                    });
                }
            }
        }
    }

    let events = parse_events(text);
    compartments.sort_by_key(|c| c.start_message);

    Ok(ParsedCompartmentOutput {
        compartments,
        facts,
        events,
        unprocessed_from,
        user_observations,
        primer_candidates,
    })
}

/// Parse, heal safe gaps, map endpoint ordinals to message ids, enforce coverage,
/// apply discard-last boundary healing, and return only data safe to persist.
pub fn validate_historian_output(
    text: &str,
    chunk: &HistorianChunk,
    prior_compartments: &[StoredCompartmentRange],
    options: ValidateOptions,
) -> Result<ValidatedChunk, HistorianValidationError> {
    let present_ordinals = chunk_present_ordinals(chunk);
    if let Some(error) = validate_chunk_coverage(chunk) {
        return Err(validation_error(format!(
            "Historian chunk coverage invalid: {error}"
        )));
    }

    if let Some(error) = validate_stored_compartments(prior_compartments) {
        return Err(validation_error(format!(
            "Existing compartments are invalid: {error}"
        )));
    }

    if let Some(last) = prior_compartments.last() {
        if chunk.start_index <= last.end_message {
            return Err(validation_error(format!(
                "Historian chunk starts at raw message {} but existing compartments end at {}; expected a strictly newer raw message",
                chunk.start_index, last.end_message
            )));
        }
        if let Some(expected_start) = next_present_after(&present_ordinals, last.end_message) {
            if chunk.start_index != expected_start {
                return Err(validation_error(format!(
                    "Historian chunk starts at raw message {} but existing compartments end at {}; expected next present raw message {}",
                    chunk.start_index, last.end_message, expected_start
                )));
            }
        }
    }

    let mut parsed = parse_compartment_output(text)?;
    if parsed.compartments.is_empty() {
        return Err(validation_error(
            "Historian returned no usable compartments.",
        ));
    }

    heal_compartment_gaps(
        &mut parsed.compartments,
        &chunk.tool_only_ranges,
        &present_ordinals,
    );

    let emitted =
        map_parsed_compartments_to_chunk(&parsed.compartments, chunk, options.sequence_offset)
            .map_err(|error| {
                validation_error(format!(
                    "Historian returned invalid compartment output: {error}"
                ))
            })?;

    if let Some(error) = validate_parsed_compartments(
        &parsed.compartments,
        chunk.start_index,
        chunk.end_index,
        &present_ordinals,
        parsed.unprocessed_from,
    ) {
        return Err(validation_error(format!(
            "Historian returned invalid compartment output: {error}"
        )));
    }

    let mut compartments = emitted;
    let emitted_count = compartments.len();
    let mut discarded_last = false;
    if !options.in_emergency && compartments.len() >= 2 {
        let last_end = compartments
            .last()
            .map(|c| c.end_message)
            .unwrap_or(chunk.end_index);
        let lookahead_margin = chunk.end_index.saturating_sub(last_end);
        if lookahead_margin <= BOUNDARY_HEALING_SLACK {
            compartments.pop();
            discarded_last = true;
        }
    }

    let offset = prior_compartments
        .last()
        .map(|c| c.end_message.saturating_add(1))
        .unwrap_or(chunk.start_index);
    let last_new_end = compartments.last().map(|c| c.end_message).unwrap_or(0);
    if last_new_end < offset {
        return Err(validation_error(format!(
            "no forward progress beyond raw message {}",
            offset.saturating_sub(1)
        )));
    }

    let persisted_count = compartments.len() as u64;
    let facts = parsed
        .facts
        .into_iter()
        .filter(|fact| {
            keep_side_channel(
                fact.origin_compartment_index,
                persisted_count,
                discarded_last,
            )
        })
        .collect();
    let events = parsed
        .events
        .into_iter()
        .filter(|event| keep_side_channel(event.at_compartment, persisted_count, false))
        .collect();
    let primer_candidates = parsed
        .primer_candidates
        .into_iter()
        .filter(|candidate| {
            keep_side_channel(
                candidate.origin_compartment_index,
                persisted_count,
                discarded_last,
            )
        })
        .take(1)
        .collect();
    let user_observations = parsed
        .user_observations
        .into_iter()
        .filter(|observation| {
            keep_side_channel(
                observation.origin_compartment_index,
                persisted_count,
                discarded_last,
            )
        })
        .collect();

    debug_assert!(compartments.len() <= emitted_count);

    Ok(ValidatedChunk {
        compartments,
        facts,
        events,
        primer_candidates,
        user_observations,
        // This value is a publication floor, not a promise that the next integer
        // ordinal exists. Consumer legs may retire ordinals permanently, so
        // downstream scans treat it as a lower bound and advance to the next
        // present input message.
        unprocessed_from: last_new_end.saturating_add(1),
        discarded_last,
    })
}

/// Validate already-persisted ranges before appending new output.
///
/// This store-pure check anchors at the first stored compartment: only the live-aware
/// fold can decide whether that first start matches the session's true first message.
pub fn validate_stored_compartments(compartments: &[StoredCompartmentRange]) -> Option<String> {
    let first = compartments.first()?;
    if first.end_message < first.start_message {
        return Some(format!(
            "invalid range {}-{}",
            first.start_message, first.end_message
        ));
    }

    let mut previous_end = first.end_message;
    for compartment in &compartments[1..] {
        if compartment.end_message < compartment.start_message {
            return Some(format!(
                "invalid range {}-{}",
                compartment.start_message, compartment.end_message
            ));
        }
        if compartment.start_message <= previous_end {
            return Some(format!(
                "overlap before message {} (saw {}-{})",
                previous_end.saturating_add(1),
                compartment.start_message,
                compartment.end_message
            ));
        }
        previous_end = compartment.end_message;
    }

    None
}

fn chunk_present_ordinals(chunk: &HistorianChunk) -> Vec<u64> {
    if !chunk.present_ordinals.is_empty() {
        return chunk.present_ordinals.clone();
    }
    chunk.lines.iter().map(|line| line.ordinal).collect()
}

fn validate_strictly_increasing_ordinals(ordinals: &[u64], label: &str) -> Option<String> {
    for pair in ordinals.windows(2) {
        let previous = pair[0];
        let current = pair[1];
        if current == previous {
            return Some(format!(
                "{label} contain duplicate raw message ordinal {current}"
            ));
        }
        if current < previous {
            return Some(format!(
                "{label} decrease from raw message {previous} to {current}"
            ));
        }
    }
    None
}

fn next_present_after(ordinals: &[u64], after: u64) -> Option<u64> {
    ordinals.iter().copied().find(|ordinal| *ordinal > after)
}

/// Ensure the chunk's ordinal lines cover exactly the present input ordinals in
/// the advertised raw range. Consumer legs can retire ordinal numbers permanently,
/// so a missing integer is valid when it is absent from the real input set.
pub fn validate_chunk_coverage(chunk: &HistorianChunk) -> Option<String> {
    if chunk.present_ordinals.is_empty() {
        return validate_dense_chunk_coverage(chunk);
    }
    validate_chunk_coverage_against(chunk, &chunk.present_ordinals)
}

fn validate_dense_chunk_coverage(chunk: &HistorianChunk) -> Option<String> {
    let line_ordinals: Vec<u64> = chunk.lines.iter().map(|line| line.ordinal).collect();
    if let Some(error) = validate_strictly_increasing_ordinals(&line_ordinals, "chunk lines") {
        return Some(error);
    }
    if chunk.lines.is_empty() {
        return None;
    }

    let mut expected_ordinal = chunk.start_index;
    for line in &chunk.lines {
        if line.ordinal != expected_ordinal {
            return Some(format!(
                "chunk omits raw message {expected_ordinal} while still claiming coverage through {}",
                chunk.end_index
            ));
        }
        expected_ordinal = expected_ordinal.saturating_add(1);
    }

    if expected_ordinal.saturating_sub(1) != chunk.end_index {
        return Some(format!(
            "chunk omits raw message {} while still claiming coverage through {}",
            expected_ordinal, chunk.end_index
        ));
    }

    None
}

fn validate_chunk_coverage_against(
    chunk: &HistorianChunk,
    present_ordinals: &[u64],
) -> Option<String> {
    if let Some(error) = validate_strictly_increasing_ordinals(present_ordinals, "input ordinals") {
        return Some(error);
    }

    let line_ordinals: Vec<u64> = chunk.lines.iter().map(|line| line.ordinal).collect();
    if let Some(error) = validate_strictly_increasing_ordinals(&line_ordinals, "chunk lines") {
        return Some(error);
    }

    if let Some(outside) = line_ordinals
        .iter()
        .find(|ordinal| **ordinal < chunk.start_index || **ordinal > chunk.end_index)
    {
        return Some(format!(
            "chunk line raw message {outside} is outside claimed coverage {}-{}",
            chunk.start_index, chunk.end_index
        ));
    }

    let expected: Vec<u64> = present_ordinals
        .iter()
        .copied()
        .filter(|ordinal| *ordinal >= chunk.start_index && *ordinal <= chunk.end_index)
        .collect();

    for (line, expected) in line_ordinals.iter().zip(expected.iter()) {
        if line == expected {
            continue;
        }
        if line > expected {
            return Some(format!(
                "chunk omits raw message {expected} while still claiming coverage through {}",
                chunk.end_index
            ));
        }
        return Some(format!(
            "chunk includes raw message {line} that is not present in input range {}-{}",
            chunk.start_index, chunk.end_index
        ));
    }

    if let Some(missing) = expected.get(line_ordinals.len()) {
        return Some(format!(
            "chunk omits raw message {missing} while still claiming coverage through {}",
            chunk.end_index
        ));
    }

    if let Some(extra) = line_ordinals.get(expected.len()) {
        return Some(format!(
            "chunk includes raw message {extra} that is not present in input range {}-{}",
            chunk.start_index, chunk.end_index
        ));
    }

    None
}

fn parse_events(text: &str) -> Vec<ParsedEvent> {
    let Some(block_caps) = events_block_regex().captures(text) else {
        return Vec::new();
    };
    let block = block_caps.get(1).map(|m| m.as_str()).unwrap_or_default();
    let mut events = Vec::new();

    // Rust's regex engine intentionally has no backreferences, while the TS parser
    // uses one to require `</kind>` to match the opening event element. Match only
    // event *open* tags here, then search for the corresponding literal close tag.
    // Event child fields do not carry `at_compartment`, so they cannot be mistaken
    // for event opens.
    for event_caps in event_open_regex().captures_iter(block) {
        let Some(full_match) = event_caps.get(0) else {
            continue;
        };
        let kind = event_caps.get(1).map(|m| m.as_str()).unwrap_or_default();
        let close_tag = format!("</{kind}>");
        let body_start = full_match.end();
        let Some(relative_body_end) = block[body_start..].find(&close_tag) else {
            continue;
        };
        let body = &block[body_start..body_start + relative_body_end];

        let mut fields = BTreeMap::new();
        for field_caps in event_field_regex().captures_iter(body) {
            let name = field_caps.get(1).map(|m| m.as_str()).unwrap_or_default();
            let closing = field_caps.get(3).map(|m| m.as_str()).unwrap_or_default();
            if name != closing {
                continue;
            }
            let value = field_caps
                .get(2)
                .map(|m| unescape_xml(m.as_str().trim()))
                .unwrap_or_default();
            if !value.is_empty() {
                fields.insert(name.to_string(), value);
            }
        }
        events.push(ParsedEvent {
            kind: kind.to_string(),
            at_compartment: event_caps
                .get(2)
                .and_then(|m| m.as_str().parse::<u64>().ok()),
            fields,
        });
    }
    events
}

fn heal_compartment_gaps(
    compartments: &mut [ParsedCompartment],
    tool_only_ranges: &[MessageRange],
    present_ordinals: &[u64],
) {
    for i in 1..compartments.len() {
        let gap_start = compartments[i - 1].end_message.saturating_add(1);
        let gap_end = compartments[i].start_message.saturating_sub(1);
        if gap_end < gap_start {
            continue;
        }
        let omitted_present: Vec<u64> = present_ordinals
            .iter()
            .copied()
            .filter(|ordinal| *ordinal >= gap_start && *ordinal <= gap_end)
            .collect();
        if omitted_present.is_empty() {
            continue;
        }
        let fully_inside_tool_only = omitted_present.iter().all(|ordinal| {
            tool_only_ranges
                .iter()
                .any(|range| range.start <= *ordinal && range.end >= *ordinal)
        });
        if fully_inside_tool_only || omitted_present.len() as u64 <= SAFETY_HEAL_GAP {
            compartments[i - 1].end_message = *omitted_present
                .last()
                .expect("non-empty omitted present ordinals checked above");
        }
    }
}

fn map_parsed_compartments_to_chunk(
    compartments: &[ParsedCompartment],
    chunk: &HistorianChunk,
    sequence_offset: u64,
) -> Result<Vec<ValidatedCompartment>, String> {
    let mut mapped = Vec::with_capacity(compartments.len());
    for (index, compartment) in compartments.iter().enumerate() {
        let start_line = chunk
            .lines
            .iter()
            .find(|line| line.ordinal == compartment.start_message);
        let end_line = chunk
            .lines
            .iter()
            .find(|line| line.ordinal == compartment.end_message);
        let (Some(start_line), Some(end_line)) = (start_line, end_line) else {
            return Err(format!(
                "Compartment range {}-{} does not map to raw session lines {}-{}",
                compartment.start_message,
                compartment.end_message,
                chunk.start_index,
                chunk.end_index
            ));
        };
        mapped.push(ValidatedCompartment {
            sequence: sequence_offset + index as u64,
            start_message: compartment.start_message,
            end_message: compartment.end_message,
            start_message_id: start_line.message_id.clone(),
            end_message_id: end_line.message_id.clone(),
            title: compartment.title.clone(),
            content: compartment.content.clone(),
            p1: compartment.p1.clone(),
            p2: compartment.p2.clone(),
            p3: compartment.p3.clone(),
            p4: compartment.p4.clone(),
            importance: compartment.importance,
            episode_type: compartment.episode_type.clone(),
        });
    }
    Ok(mapped)
}

fn validate_parsed_compartments(
    compartments: &[ParsedCompartment],
    chunk_start: u64,
    chunk_end: u64,
    present_ordinals: &[u64],
    unprocessed_from: Option<u64>,
) -> Option<String> {
    let chunk_ordinals: Vec<u64> = present_ordinals
        .iter()
        .copied()
        .filter(|ordinal| *ordinal >= chunk_start && *ordinal <= chunk_end)
        .collect();
    let mut expected_start = chunk_ordinals.first().copied();

    for compartment in compartments {
        if compartment.end_message < compartment.start_message {
            return Some(format!(
                "invalid range {}-{}",
                compartment.start_message, compartment.end_message
            ));
        }
        if compartment.start_message < chunk_start || compartment.end_message > chunk_end {
            return Some(format!(
                "range {}-{} is outside chunk {}-{}",
                compartment.start_message, compartment.end_message, chunk_start, chunk_end
            ));
        }
        if !chunk_ordinals.contains(&compartment.start_message) {
            return Some(format!(
                "range start {} is not a present raw message in chunk {}-{}",
                compartment.start_message, chunk_start, chunk_end
            ));
        }
        if !chunk_ordinals.contains(&compartment.end_message) {
            return Some(format!(
                "range end {} is not a present raw message in chunk {}-{}",
                compartment.end_message, chunk_start, chunk_end
            ));
        }
        let Some(expected) = expected_start else {
            return Some(format!(
                "range {}-{} starts after chunk coverage already ended",
                compartment.start_message, compartment.end_message
            ));
        };
        if compartment.start_message != expected {
            if compartment.start_message < expected {
                return Some(format!(
                    "overlap before message {expected} (saw {}-{})",
                    compartment.start_message, compartment.end_message
                ));
            }
            return Some(format!(
                "gap before present message {} (expected {expected})",
                compartment.start_message
            ));
        }
        expected_start = next_present_after(&chunk_ordinals, compartment.end_message);
    }

    if let Some(unprocessed_from) = unprocessed_from {
        if let Some(expected) = expected_start {
            if unprocessed_from != expected {
                return Some(format!(
                    "<unprocessed_from> {unprocessed_from} does not match next uncovered message {expected}"
                ));
            }
            return None;
        }
        if unprocessed_from == chunk_end.saturating_add(1) {
            return None;
        }
        if unprocessed_from < chunk_start || unprocessed_from > chunk_end {
            return Some(format!(
                "<unprocessed_from> {unprocessed_from} is outside chunk {chunk_start}-{chunk_end}"
            ));
        }
        return Some(format!(
            "<unprocessed_from> {unprocessed_from} does not match completed chunk boundary {}",
            chunk_end.saturating_add(1)
        ));
    }

    if let Some(expected) = expected_start {
        return Some(format!(
            "output left uncovered messages {expected}-{chunk_end} without <unprocessed_from>"
        ));
    }

    None
}

fn keep_side_channel(
    origin_compartment_index: Option<u64>,
    persisted_count: u64,
    discarded_last: bool,
) -> bool {
    match origin_compartment_index {
        Some(index) => index <= persisted_count,
        None => !discarded_last,
    }
}

fn capture_string(regex: &Regex, haystack: &str) -> Option<String> {
    regex
        .captures(haystack)
        .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
}

fn capture_u64(regex: &Regex, haystack: &str) -> Option<u64> {
    regex
        .captures(haystack)
        .and_then(|caps| caps.get(1).and_then(|m| m.as_str().parse::<u64>().ok()))
}

fn extract_tier(inner: &str, index: usize) -> Option<String> {
    tier_regexes()[index].captures(inner).map(|caps| {
        let body = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
        unescape_xml(body.trim())
    })
}

fn split_anchor_prefix(text: &str) -> (Option<u64>, String) {
    if let Some(caps) = side_channel_anchor_regex().captures(text) {
        let anchor = caps.get(1).and_then(|m| m.as_str().parse::<u64>().ok());
        let content = caps
            .get(2)
            .map(|m| m.as_str().trim().to_string())
            .unwrap_or_default();
        return (anchor, content);
    }
    (None, text.trim().to_string())
}

fn unescape_xml(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&apos;", "'")
        .replace("&quot;", "\"")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn compartment_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?s)<compartment\s+([^>]*?)\s*>(.*?)</compartment>"#).unwrap())
}

fn attr_start_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\bstart="(\d+)""#).unwrap())
}

fn attr_end_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\bend="(\d+)""#).unwrap())
}

fn attr_title_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\btitle="([^"]*)""#).unwrap())
}

fn attr_episode_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\bepisode_type="([^"]*)""#).unwrap())
}

fn attr_importance_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"\bimportance="(\d+)""#).unwrap())
}

fn tier_regexes() -> &'static [Regex; 4] {
    static RE: OnceLock<[Regex; 4]> = OnceLock::new();
    RE.get_or_init(|| {
        [
            Regex::new(r#"(?s)<p1\s*/>|<p1\s*>(.*?)</p1>"#).unwrap(),
            Regex::new(r#"(?s)<p2\s*/>|<p2\s*>(.*?)</p2>"#).unwrap(),
            Regex::new(r#"(?s)<p3\s*/>|<p3\s*>(.*?)</p3>"#).unwrap(),
            Regex::new(r#"(?s)<p4\s*/>|<p4\s*>(.*?)</p4>"#).unwrap(),
        ]
    })
}

fn facts_block_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?s)<facts>(.*?)</facts>"#).unwrap())
}

fn events_block_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?s)<events>(.*?)</events>"#).unwrap())
}

fn category_block_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"(?s)<(PROJECT_RULES|ARCHITECTURE|CONSTRAINTS|CONFIG_VALUES|NAMING)>(.*?)</(PROJECT_RULES|ARCHITECTURE|CONSTRAINTS|CONFIG_VALUES|NAMING)>"#,
        )
        .unwrap()
    })
}

fn fact_item_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?m)^\s*\*\s*(.+)$"#).unwrap())
}

fn unprocessed_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"<unprocessed_from>(\d+)</unprocessed_from>"#).unwrap())
}

fn user_observations_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?s)<user_observations>(.*?)</user_observations>"#).unwrap())
}

fn user_obs_item_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?m)^\s*\*\s*(.+)$"#).unwrap())
}

fn primer_candidates_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?s)<primer_candidates>(.*?)</primer_candidates>"#).unwrap())
}

fn primer_element_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?s)<primer\s+at_compartment="(\d+)"\s*>(.*?)</primer>"#).unwrap()
    })
}

fn primer_item_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?m)^\s*(?:\*|-|\d+\.)\s*(.+)$"#).unwrap())
}

fn event_open_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"<([a-z_]+)\s+at_compartment="(\d+)"\s*>"#).unwrap())
}

fn event_field_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"(?s)<([a-z_]+)\s*>(.*?)</([a-z_]+)>"#).unwrap())
}

fn side_channel_anchor_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r#"^\s*[\[(]\s*(?:at_compartment|origin_compartment)\s*=\s*"?(\d+)"?\s*[\])]\s*(.+)$"#,
        )
        .unwrap()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Deserialize)]
    struct GoldenInput {
        text: String,
        chunk: HistorianChunk,
        #[serde(default)]
        prior_compartments: Vec<StoredCompartmentRange>,
        #[serde(default)]
        options: ValidateOptions,
    }

    #[derive(Debug, Deserialize, PartialEq, Eq)]
    struct GoldenVerdict {
        ok: bool,
        #[serde(default)]
        error: Option<String>,
        #[serde(default)]
        result: Option<ValidatedChunk>,
    }

    #[derive(Debug, Deserialize)]
    struct GoldenCase {
        label: String,
        input: GoldenInput,
        parsed: ParsedCompartmentOutput,
        validation: GoldenVerdict,
    }

    fn verdict(result: Result<ValidatedChunk, HistorianValidationError>) -> GoldenVerdict {
        match result {
            Ok(result) => GoldenVerdict {
                ok: true,
                error: None,
                result: Some(result),
            },
            Err(error) => GoldenVerdict {
                ok: false,
                error: Some(error.message),
                result: None,
            },
        }
    }

    fn chunk(start: u64, end: u64) -> HistorianChunk {
        HistorianChunk {
            start_index: start,
            end_index: end,
            lines: (start..=end)
                .map(|ordinal| ChunkLine {
                    ordinal,
                    message_id: format!("msg-{ordinal}"),
                })
                .collect(),
            present_ordinals: (start..=end).collect(),
            tool_only_ranges: Vec::new(),
        }
    }

    fn xml(compartments: &[(u64, u64, &str)], unprocessed_from: u64, extra: &str) -> String {
        let body = compartments
            .iter()
            .map(|(start, end, title)| {
                format!(
                    r#"<compartment start="{start}" end="{end}" title="{title}" episode_type="feature" importance="50"><p1>{title} full</p1><p2>{title} short</p2><p3>{title}</p3><p4 /></compartment>"#
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "<output><compartments>{body}</compartments>{extra}<meta><unprocessed_from>{unprocessed_from}</unprocessed_from></meta></output>"
        )
    }

    #[test]
    fn validate_golden_matches_typescript_oracle() {
        let raw = include_str!("../testdata/validate-golden.json");
        let cases: Vec<GoldenCase> = serde_json::from_str(raw).expect("parse validate golden");
        assert!(!cases.is_empty(), "empty validate golden");

        for case in &cases {
            let parsed = parse_compartment_output(&case.input.text)
                .unwrap_or_else(|error| panic!("{} parse failed: {error}", case.label));
            assert_eq!(parsed, case.parsed, "parsed mismatch in {}", case.label);

            let got = verdict(validate_historian_output(
                &case.input.text,
                &case.input.chunk,
                &case.input.prior_compartments,
                case.input.options,
            ));
            assert_eq!(
                got, case.validation,
                "validation mismatch in {}",
                case.label
            );
        }
    }

    #[test]
    fn validation_is_deterministic() {
        let text = xml(&[(1, 2, "alpha"), (3, 4, "beta")], 5, "");
        let chunk = chunk(1, 7);
        let first = validate_historian_output(&text, &chunk, &[], ValidateOptions::default());
        let second = validate_historian_output(&text, &chunk, &[], ValidateOptions::default());
        assert_eq!(first, second);
    }

    #[test]
    fn stored_compartment_validation_is_basis_agnostic_and_allows_sparse_gaps() {
        assert_eq!(
            validate_stored_compartments(&[
                StoredCompartmentRange {
                    start_message: 0,
                    end_message: 4,
                },
                StoredCompartmentRange {
                    start_message: 5,
                    end_message: 8,
                },
            ]),
            None
        );

        assert_eq!(
            validate_stored_compartments(&[
                StoredCompartmentRange {
                    start_message: 5,
                    end_message: 7,
                },
                StoredCompartmentRange {
                    start_message: 9,
                    end_message: 10,
                },
            ]),
            None,
            "store-pure validation cannot distinguish retired ordinals from gaps",
        );

        let overlap = validate_stored_compartments(&[
            StoredCompartmentRange {
                start_message: 0,
                end_message: 4,
            },
            StoredCompartmentRange {
                start_message: 4,
                end_message: 6,
            },
        ])
        .expect("overlap rejected");
        assert!(overlap.contains("overlap before message 5"));
    }

    #[test]
    fn chunk_coverage_rejects_duplicate_and_decreasing_ordinals() {
        let duplicate = HistorianChunk {
            start_index: 1,
            end_index: 2,
            lines: vec![
                ChunkLine {
                    ordinal: 1,
                    message_id: "m1#0".into(),
                },
                ChunkLine {
                    ordinal: 1,
                    message_id: "m1-dup#0".into(),
                },
                ChunkLine {
                    ordinal: 2,
                    message_id: "m2#0".into(),
                },
            ],
            present_ordinals: vec![1, 1, 2],
            tool_only_ranges: Vec::new(),
        };
        let duplicate_error = validate_chunk_coverage(&duplicate).expect("duplicate rejected");
        assert!(duplicate_error.contains("duplicate raw message ordinal 1"));

        let decreasing = HistorianChunk {
            start_index: 1,
            end_index: 3,
            lines: vec![
                ChunkLine {
                    ordinal: 1,
                    message_id: "m1#0".into(),
                },
                ChunkLine {
                    ordinal: 3,
                    message_id: "m3#0".into(),
                },
                ChunkLine {
                    ordinal: 2,
                    message_id: "m2#0".into(),
                },
            ],
            present_ordinals: vec![1, 2, 3],
            tool_only_ranges: Vec::new(),
        };
        let decreasing_error = validate_chunk_coverage(&decreasing).expect("decrease rejected");
        assert!(decreasing_error.contains("chunk lines decrease from raw message 3 to 2"));
    }

    #[test]
    fn discard_last_progress_guard_boundary_k1_vs_k2() {
        let one = xml(&[(1, 4, "single")], 5, "");
        let two = xml(&[(1, 2, "first"), (3, 4, "second")], 5, "");
        let chunk = chunk(1, 4);

        let one_result = validate_historian_output(&one, &chunk, &[], ValidateOptions::default())
            .expect("single compartment remains publishable");
        let two_result = validate_historian_output(&two, &chunk, &[], ValidateOptions::default())
            .expect("two compartments keep progress after discard");

        assert!(!one_result.discarded_last, "k=1 must not discard");
        assert!(
            two_result.discarded_last,
            "k=2 may discard the provisional tail"
        );
        assert_eq!(two_result.compartments.len(), 1);
        assert_eq!(two_result.unprocessed_from, 3);
    }

    #[test]
    fn discarded_last_filters_anchored_tail_side_channels_but_keeps_earlier_ones() {
        let extra = r#"
<facts>
<PROJECT_RULES>
* [at_compartment=1] Keep the earlier rule.
* [at_compartment=2] Drop the provisional rule.
</PROJECT_RULES>
</facts>
<events>
<causal_incident at_compartment="1"><summary>kept event</summary></causal_incident>
<trajectory_correction at_compartment="2"><summary>dropped event</summary></trajectory_correction>
</events>
<user_observations>
* [at_compartment=1] Keep the earlier observation.
* [at_compartment=2] Drop the provisional observation.
</user_observations>
<primer_candidates>
<primer at_compartment="1">How does the kept subsystem work?</primer>
<primer at_compartment="2">How does the dropped subsystem work?</primer>
</primer_candidates>
"#;
        let text = xml(&[(1, 2, "first"), (3, 4, "second")], 5, extra);
        let result =
            validate_historian_output(&text, &chunk(1, 4), &[], ValidateOptions::default())
                .expect("discard-last should still make forward progress");

        assert!(result.discarded_last);
        assert_eq!(
            result
                .facts
                .iter()
                .map(|f| f.content.as_str())
                .collect::<Vec<_>>(),
            vec!["Keep the earlier rule."]
        );
        assert_eq!(
            result
                .user_observations
                .iter()
                .map(|o| o.content.as_str())
                .collect::<Vec<_>>(),
            vec!["Keep the earlier observation."]
        );
        assert_eq!(
            result
                .primer_candidates
                .iter()
                .map(|p| p.question.as_str())
                .collect::<Vec<_>>(),
            vec!["How does the kept subsystem work?"]
        );
        assert_eq!(
            result
                .events
                .iter()
                .map(|e| e.kind.as_str())
                .collect::<Vec<_>>(),
            vec!["causal_incident"]
        );
    }
}

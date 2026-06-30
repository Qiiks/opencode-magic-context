//! The `<project-memory>` baseline block and the `<memory-updates>` corrections block.
//!
//! Faithful port of the memory render in `inject-compartments.ts`
//! (`renderMemoryLineV2` / `renderMemoryBlockV2` / `renderMemoryUpdatesBlock`). Pure
//! over the stored rows:
//!  - the baseline block lists each rendered memory as a `<memory>` line (the m0 source);
//!    the SAME line shape feeds the budget accounting, so a trim measures the bytes it
//!    actually injects.
//!  - the corrections block renders the coalesced mutation set as a forward delta the
//!    model trusts over the (stale-but-cached) baseline: `<updated>` for a content
//!    change, `<superseded by=>` when the replacement is itself in the baseline else
//!    `<removed>`, `<removed>` for an archive/delete.
//!
//! Routing + timing (which memories are in the baseline, when the corrections fold in)
//! is the slice-4d integration decision, already ruled; the byte render here is pure.

use crate::decay_render::{render_decayed_compartments, DecayRenderCompartment};
use mc_store::{StoredMemory, StoredMemoryMutation};
use std::collections::HashSet;

/// The body for an empty session history. The `<session-history>` tag is always present
/// (never omitted) so the provider prompt-cache has a stable breakpoint to anchor on —
/// an absent block would shift the bytes after it and bust the cache.
pub const M0_EMPTY_BODY: &str = "<session-history></session-history>";
/// Default history budget when a caller doesn't supply one.
pub const DEFAULT_HISTORY_BUDGET_TOKENS: f64 = 60_000.0;

fn escape_xml_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn escape_xml_content(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render ONE memory's baseline line exactly as it lands in the `<project-memory>`
/// block — the same shape the budget accounting measures, so a trim counts the bytes it
/// injects (a lighter shape would under-count and overflow the budget). `source_name`
/// is the repo attribution for a workspace-union memory (None = own project).
pub fn render_memory_line(memory: &StoredMemory, source_name: Option<&str>) -> String {
    let source_attr = match source_name {
        Some(name) => format!(" source=\"{}\"", escape_xml_attr(name)),
        None => String::new(),
    };
    format!(
        "  <memory id=\"{}\" category=\"{}\"{} importance=\"{}\">{}</memory>",
        memory.id,
        escape_xml_attr(&memory.category),
        source_attr,
        memory.importance.unwrap_or(50),
        escape_xml_content(&memory.content)
    )
}

/// Render the `<project-memory>` (or workspace-`wrapper`) baseline block from the
/// already-selected, already-ordered memory set. Empty set → empty string.
/// `source_name_by_id` supplies per-memory repo attribution for a workspace union.
pub fn render_memory_block(
    memories: &[StoredMemory],
    wrapper: &str,
    source_name_by_id: &std::collections::HashMap<i64, String>,
) -> String {
    if memories.is_empty() {
        return String::new();
    }
    let mut lines = Vec::with_capacity(memories.len() + 2);
    lines.push(format!("<{wrapper}>"));
    for m in memories {
        lines.push(render_memory_line(
            m,
            source_name_by_id.get(&m.id).map(String::as_str),
        ));
    }
    lines.push(format!("</{wrapper}>"));
    lines.join("\n")
}

/// Render the `<user-profile>` baseline block: one `- <content>` line per user memory
/// (already budget-trimmed by the caller). Empty set → empty string.
pub fn render_user_profile_block(profile_lines: &[String], wrapper: &str) -> String {
    if profile_lines.is_empty() {
        return String::new();
    }
    let mut lines = Vec::with_capacity(profile_lines.len() + 2);
    lines.push(format!("<{wrapper}>"));
    for content in profile_lines {
        lines.push(format!("- {}", escape_xml_content(content)));
    }
    lines.push(format!("</{wrapper}>"));
    lines.join("\n")
}

/// Inputs to [`render_m0`]: the four sub-blocks' source content, already budget-trimmed
/// by the caller (the trim needs the token estimator, a separate subsystem). The render
/// here is the pure COMPOSITION: order + framing + the decay-pressure→budget mapping.
pub struct M0Inputs<'a> {
    /// The pre-rendered `<project-docs>` block (empty string when absent).
    pub project_docs: &'a str,
    /// User-profile memory contents (trimmed); rendered as `- <content>` lines.
    pub user_profile: &'a [String],
    /// The compartment history (trimmed/ordered chronological), decay-rendered here.
    pub compartments: &'a [DecayRenderCompartment],
    /// The project memories (selected + ordered + trimmed) for the `<project-memory>` block.
    pub memories: &'a [StoredMemory],
    /// Map from memory id to its source project name, for memories that come from OTHER
    /// projects sharing a workspace with this one (empty when every memory is the
    /// current project's own).
    pub source_name_by_id: &'a std::collections::HashMap<i64, String>,
    /// The history budget in tokens (before the pressure multiplier).
    pub history_budget_tokens: f64,
    /// The drift-pressure multiplier (≥1): a tighter effective budget → more decay
    /// demotion. Maps to `effective_budget = budget / max(1, multiplier)`, keeping the
    /// decay curve the single source of pressure math.
    pub decay_pressure_multiplier: f64,
}

/// Compose the m0 baseline: `<project-docs>` + `<user-profile>` + `<session-history>` +
/// `<project-memory>`, joined by blank lines and trimmed. The session-history block is
/// always present (empty history uses the `M0_EMPTY_BODY` placeholder — see its doc for
/// why); the other three are omitted when empty. `estimate_tokens` is used inside the
/// decay renderer for its budget-fit check (injected; under a loose budget the render is
/// pure and estimator-independent). This function only composes; sub-block budget trims
/// happen in the caller (they need the token estimator, a separate subsystem).
pub fn render_m0(inputs: &M0Inputs, estimate_tokens: impl Fn(&str) -> usize) -> String {
    let mut sections: Vec<String> = Vec::new();
    if !inputs.project_docs.is_empty() {
        sections.push(inputs.project_docs.to_string());
    }
    let user_profile = render_user_profile_block(inputs.user_profile, "user-profile");
    if !user_profile.is_empty() {
        sections.push(user_profile);
    }

    let effective_budget = inputs.history_budget_tokens / inputs.decay_pressure_multiplier.max(1.0);
    let session_history =
        render_decayed_compartments(inputs.compartments, effective_budget, estimate_tokens);
    sections.push(if session_history.is_empty() {
        M0_EMPTY_BODY.to_string()
    } else {
        format!("<session-history>\n{session_history}\n</session-history>")
    });

    let memories_block =
        render_memory_block(inputs.memories, "project-memory", inputs.source_name_by_id);
    if !memories_block.is_empty() {
        sections.push(memories_block);
    }
    sections.join("\n\n").trim().to_string()
}

/// Render the `<memory-updates>` corrections block from the coalesced mutation set.
/// `rendered_ids` is the baseline manifest — a `superseded` mutation renders as
/// `<superseded by=>` only when the replacement is ALSO in the baseline (so the model
/// can resolve it), else as `<removed>`. Empty mutation set → empty string.
pub fn render_memory_updates(
    mutations: &[StoredMemoryMutation],
    rendered_ids: &HashSet<i64>,
) -> String {
    if mutations.is_empty() {
        return String::new();
    }
    let mut lines =
        vec!["These memories changed since the snapshot below — trust these:".to_string()];
    for m in mutations {
        match m.mutation_type.as_str() {
            "update" => lines.push(format!(
                "  <updated id=\"{}\">{}</updated>",
                m.target_memory_id,
                escape_xml_content(m.new_content.as_deref().unwrap_or(""))
            )),
            "superseded" => match m.superseded_by_id {
                Some(by) if rendered_ids.contains(&by) => lines.push(format!(
                    "  <superseded id=\"{}\" by=\"{by}\"/>",
                    m.target_memory_id
                )),
                _ => lines.push(format!("  <removed id=\"{}\"/>", m.target_memory_id)),
            },
            // archive / delete (and any non-update, non-resolvable-superseded)
            _ => lines.push(format!("  <removed id=\"{}\"/>", m.target_memory_id)),
        }
    }
    format!("<memory-updates>\n{}\n</memory-updates>", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn mem(id: i64, category: &str, content: &str, importance: Option<i32>) -> StoredMemory {
        StoredMemory {
            id,
            category: category.to_string(),
            content: content.to_string(),
            importance,
            status: "active".to_string(),
            ..Default::default()
        }
    }
    fn mutation(
        id: i64,
        kind: &str,
        target: i64,
        content: &str,
        by: Option<i64>,
    ) -> StoredMemoryMutation {
        StoredMemoryMutation {
            id,
            mutation_type: kind.to_string(),
            target_memory_id: target,
            superseded_by_id: by,
            new_content: Some(content.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn empty_blocks_render_empty() {
        assert_eq!(
            render_memory_block(&[], "project-memory", &Default::default()),
            ""
        );
        assert_eq!(render_memory_updates(&[], &HashSet::new()), "");
    }

    #[test]
    fn memory_block_lines_and_attrs() {
        let memories = vec![
            mem(1, "ARCHITECTURE", "the spine holds", Some(80)),
            mem(2, "CONSTRAINTS", "x < y & z", None), // None importance → 50; XML escape
        ];
        let block = render_memory_block(&memories, "project-memory", &Default::default());
        assert!(block.starts_with("<project-memory>\n"));
        assert!(block.ends_with("\n</project-memory>"));
        assert!(block.contains(
            "<memory id=\"1\" category=\"ARCHITECTURE\" importance=\"80\">the spine holds</memory>"
        ));
        assert!(
            block.contains("importance=\"50\">x &lt; y &amp; z</memory>"),
            "{block}"
        );
    }

    #[test]
    fn memory_block_source_attribution() {
        let mut src = std::collections::HashMap::new();
        src.insert(1i64, "svc-auth".to_string());
        let block = render_memory_block(
            &[mem(1, "ARCHITECTURE", "c", Some(50))],
            "project-memory",
            &src,
        );
        assert!(block.contains("source=\"svc-auth\""), "{block}");
    }

    #[test]
    fn user_profile_block() {
        assert_eq!(render_user_profile_block(&[], "user-profile"), "");
        let prof = vec!["prefers root cause".to_string(), "x < y".to_string()];
        let block = render_user_profile_block(&prof, "user-profile");
        assert_eq!(
            block,
            "<user-profile>\n- prefers root cause\n- x &lt; y\n</user-profile>"
        );
    }

    #[test]
    fn m0_composition_orders_and_frames_sub_blocks() {
        let comps = vec![DecayRenderCompartment {
            start_message: 1,
            end_message: 9,
            title: "T".into(),
            p1: Some("HIST".into()),
            importance: Some(50),
            ..Default::default()
        }];
        let inputs = M0Inputs {
            project_docs: "<project-docs>\n<file name=\"A.md\">x</file>\n</project-docs>",
            user_profile: &["likes tests".to_string()],
            compartments: &comps,
            memories: &[mem(1, "ARCHITECTURE", "m1", Some(80))],
            source_name_by_id: &Default::default(),
            history_budget_tokens: 60_000.0,
            decay_pressure_multiplier: 1.0,
        };
        let m0 = render_m0(&inputs, |_| 0);
        // order: project-docs, user-profile, session-history, project-memory
        let i_docs = m0.find("<project-docs>").unwrap();
        let i_prof = m0.find("<user-profile>").unwrap();
        let i_hist = m0.find("<session-history>").unwrap();
        let i_mem = m0.find("<project-memory>").unwrap();
        assert!(
            i_docs < i_prof && i_prof < i_hist && i_hist < i_mem,
            "sub-block order: {m0}"
        );
        assert!(m0.contains(">\nHIST\n<"), "history rendered: {m0}");
    }

    #[test]
    fn m0_empty_history_uses_placeholder_not_absent() {
        let inputs = M0Inputs {
            project_docs: "",
            user_profile: &[],
            compartments: &[],
            memories: &[],
            source_name_by_id: &Default::default(),
            history_budget_tokens: 60_000.0,
            decay_pressure_multiplier: 1.0,
        };
        // no docs/profile/memory + no compartments → just the empty-history placeholder
        assert_eq!(render_m0(&inputs, |_| 0), M0_EMPTY_BODY);
    }

    #[test]
    fn memory_updates_three_branches() {
        let rendered: HashSet<i64> = [1, 2, 9].into_iter().collect();
        let muts = vec![
            mutation(10, "update", 1, "new content", None),
            mutation(11, "superseded", 2, "", Some(9)), // 9 in baseline → <superseded by>
            mutation(12, "superseded", 3, "", Some(99)), // 99 NOT in baseline → <removed>
            mutation(13, "archive", 4, "", None),
        ];
        let block = render_memory_updates(&muts, &rendered);
        assert!(block.starts_with("<memory-updates>\nThese memories changed"));
        assert!(block.contains("<updated id=\"1\">new content</updated>"));
        assert!(block.contains("<superseded id=\"2\" by=\"9\"/>"));
        assert!(
            block.contains("<removed id=\"3\"/>"),
            "unresolvable supersede → removed: {block}"
        );
        assert!(
            block.contains("<removed id=\"4\"/>"),
            "archive → removed: {block}"
        );
    }

    // --- differential golden vs the TS reference render ---

    #[derive(Deserialize)]
    struct RawMem {
        id: i64,
        category: String,
        content: String,
        importance: Option<i32>,
    }
    #[derive(Deserialize)]
    struct RawMut {
        id: i64,
        #[serde(rename = "type")]
        mutation_type: String,
        target: i64,
        #[serde(default)]
        content: String,
        by: Option<i64>,
    }
    #[derive(Deserialize)]
    struct MemCase {
        memories: Vec<RawMem>,
        block: String,
    }
    #[derive(Deserialize)]
    struct UpdCase {
        mutations: Vec<RawMut>,
        rendered_ids: Vec<i64>,
        block: String,
    }
    #[derive(Deserialize)]
    struct MemGolden {
        memory_block_cases: Vec<MemCase>,
        memory_updates_cases: Vec<UpdCase>,
    }

    #[test]
    fn memory_render_golden_matches_reference() {
        let raw = include_str!("../testdata/memory-render-golden.json");
        let golden: MemGolden = serde_json::from_str(raw).expect("parse memory-render-golden.json");
        assert!(!golden.memory_block_cases.is_empty());

        for (n, c) in golden.memory_block_cases.iter().enumerate() {
            let memories: Vec<StoredMemory> = c
                .memories
                .iter()
                .map(|r| mem(r.id, &r.category, &r.content, r.importance))
                .collect();
            let got = render_memory_block(&memories, "project-memory", &Default::default());
            assert_eq!(got, c.block, "memory block mismatch case {n}");
        }
        for (n, c) in golden.memory_updates_cases.iter().enumerate() {
            let muts: Vec<StoredMemoryMutation> = c
                .mutations
                .iter()
                .map(|r| mutation(r.id, &r.mutation_type, r.target, &r.content, r.by))
                .collect();
            let rendered: HashSet<i64> = c.rendered_ids.iter().copied().collect();
            let got = render_memory_updates(&muts, &rendered);
            assert_eq!(got, c.block, "memory updates mismatch case {n}");
        }
    }
}

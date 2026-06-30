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

use mc_store::{StoredMemory, StoredMemoryMutation};
use std::collections::HashSet;

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

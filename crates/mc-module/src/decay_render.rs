//! Deterministic decay renderer: turns a chronological compartment set into the
//! `<compartment>` history bytes that fill m0/m1.
//!
//! Faithful port of the shared `decay-render.ts`. It picks a tier per compartment
//! from age + importance + budget pressure (via [`mc_core::decay`], pressure computed
//! ONCE per pass), renders the chosen paraphrase tier (P1..P4; P5 = archived =
//! omitted), and demotes oldest-first under a hard token budget as a drift guard.
//!
//! This is byte-producing, so it lives in mc-module (mc-core stays pure decision
//! math). The byte-identity invariant that matters is intra-module determinism (same
//! compartments + budget → same bytes across passes); a differential golden cross-
//! checks the v2 paraphrase path against the TS reference.
//!
//! The budget-guard loop needs a token estimator, which is its own subsystem and a
//! later port — so it is INJECTED (`estimate_tokens`). The renderer stays pure; with
//! a budget loose enough that the guard never fires, the output is estimator-
//! independent and purely curve-driven (which is what the golden exercises).

use mc_core::decay::{compute_budget_pressure, rendered_tier, DecayInput};
use mc_store::StoredCompartment;

/// Default history budget when a caller doesn't supply one.
pub const DEFAULT_HISTORY_BUDGET_TOKENS: u32 = 60_000;

/// The minimal compartment shape the renderer needs. `p1..p4` are the paraphrase
/// tiers (None / empty = not a v2-tiered row); `legacy = Some(1)` marks a pre-v2
/// flat-content row; `importance` defaults to 50 when absent.
#[derive(Debug, Clone, Default)]
pub struct DecayRenderCompartment {
    pub start_message: i64,
    pub end_message: i64,
    pub title: String,
    pub content: String,
    pub p1: Option<String>,
    pub p2: Option<String>,
    pub p3: Option<String>,
    pub p4: Option<String>,
    pub importance: Option<i32>,
    pub legacy: Option<i32>,
}

impl From<&StoredCompartment> for DecayRenderCompartment {
    /// Project a stored compartment into the renderer's input shape. Empty tier
    /// strings stay empty (the `is_tiered_row`/`tier_body` logic distinguishes an
    /// empty p1 = not-tiered from a non-empty p1 with an empty p4 = self-close).
    fn from(c: &StoredCompartment) -> Self {
        DecayRenderCompartment {
            start_message: c.start_message,
            end_message: c.end_message,
            title: c.title.clone(),
            content: c.content.clone(),
            p1: c.p1.clone(),
            p2: c.p2.clone(),
            p3: c.p3.clone(),
            p4: c.p4.clone(),
            importance: Some(c.importance),
            legacy: Some(c.legacy),
        }
    }
}

/// Render a session's stored compartments (chronological, oldest first — the order
/// [`mc_store::McStore::load_compartments`] returns) into the m0/m1 history body.
pub fn render_stored_compartments(
    compartments: &[StoredCompartment],
    history_budget_tokens: f64,
    estimate_tokens: impl Fn(&str) -> usize,
) -> String {
    let mapped: Vec<DecayRenderCompartment> = compartments
        .iter()
        .map(DecayRenderCompartment::from)
        .collect();
    render_decayed_compartments(&mapped, history_budget_tokens, estimate_tokens)
}

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

/// A row is v2-tiered ONLY when `p1` is a non-empty string. Rows with empty/null `p1`
/// (legacy rows, or the malformed pseudo-v2 state left by an interrupted upgrade —
/// `legacy=0` but tiers never populated) render via flat `content`, never as an empty
/// tier body. A VALID v2 row can still have an empty `p4` (a legitimate self-close);
/// that is handled by the tier-body path, since such a row has a non-empty `p1`.
fn is_tiered_row(c: &DecayRenderCompartment) -> bool {
    c.p1.as_deref().is_some_and(|p| !p.is_empty())
}

/// The v2 paraphrase tier body, with denser-tier and content fallbacks: the requested
/// tier if present, else the densest populated denser tier, else flat content.
fn tier_body(c: &DecayRenderCompartment, tier: u8) -> String {
    let tiers = [
        c.p1.as_deref(),
        c.p2.as_deref(),
        c.p3.as_deref(),
        c.p4.as_deref(),
    ];
    let idx = (tier as usize).saturating_sub(1);
    if let Some(requested) = tiers.get(idx).copied().flatten() {
        return requested.trim().to_string();
    }
    // walk denser (lower-index) tiers for a non-empty body
    for i in (0..idx).rev() {
        if let Some(t) = tiers[i] {
            if !t.is_empty() {
                return t.trim().to_string();
            }
        }
    }
    c.content.trim().to_string()
}

/// Truncate to at most `max` characters (Unicode scalar values), trimming trailing
/// whitespace and appending `…`. Char-boundary safe (vs the TS UTF-16 slice; they
/// agree on the BMP-without-surrogate-pairs content the golden covers).
fn truncate_with_ellipsis(content: &str, max: usize) -> String {
    if content.chars().count() <= max {
        return content.to_string();
    }
    let cut: String = content.chars().take(max).collect();
    format!("{}…", cut.trim_end())
}

/// Legacy flat-content tier rendering (no paraphrase columns): P1 = full, P2 = ≤1200
/// chars, P3+ = ≤420 chars.
fn legacy_body_for_tier(content: &str, tier: u8) -> String {
    if tier <= 1 {
        content.to_string()
    } else if tier == 2 {
        truncate_with_ellipsis(content, 1_200)
    } else {
        truncate_with_ellipsis(content, 420)
    }
}

/// Legacy compartments start at P3 if the body has a `U:` line, else P4.
fn legacy_tier(c: &DecayRenderCompartment) -> u8 {
    if c.content.lines().any(|l| l.starts_with("U:")) {
        3
    } else {
        4
    }
}

/// Render a single compartment at an explicit tier. Exposed for the m1 "new
/// compartments" block, which always renders newest compartments at P1 (full
/// fidelity — no decay applies to brand-new deltas).
pub fn render_compartment_at_tier(c: &DecayRenderCompartment, tier: u8) -> String {
    render_one_compartment(c, tier)
}

fn render_one_compartment(c: &DecayRenderCompartment, tier: u8) -> String {
    let base_attrs = format!(
        "start=\"{}\" end=\"{}\" title=\"{}\"",
        c.start_message,
        c.end_message,
        escape_xml_attr(&c.title)
    );
    if tier >= 5 {
        return String::new(); // archived
    }

    // Legacy rows AND malformed pseudo-v2 rows (legacy=0 but no usable p1) render via
    // flat `content`, never as an empty tier — otherwise a `legacy=0, p1=''` row would
    // self-close empty here and silently drop the compartment body from m0/m1.
    if c.legacy == Some(1) || !is_tiered_row(c) {
        let flat = c.content.trim();
        if tier >= 4 || flat.is_empty() {
            return format!("<compartment {base_attrs} />");
        }
        return format!(
            "<compartment {base_attrs}>\n{}\n</compartment>",
            escape_xml_content(&legacy_body_for_tier(flat, tier))
        );
    }

    let body = tier_body(c, tier);
    if body.is_empty() {
        return format!("<compartment {base_attrs} />");
    }
    format!(
        "<compartment {base_attrs}>\n{}\n</compartment>",
        escape_xml_content(&body)
    )
}

/// Compute the rendered tier for each compartment, given budget pressure derived once
/// from the whole set. `compartments` are chronological (oldest first); the decay
/// curve indexes from newest (1 = newest). Legacy rows are governed by deterministic
/// truncation, not the curve, and are EXCLUDED from the pressure inputs so unrelated
/// legacy cost can't demote v2 paraphrases (budget honesty for mixed sessions).
fn compute_tiers(compartments: &[DecayRenderCompartment], history_budget: f64) -> Vec<u8> {
    let v2_indices: Vec<usize> = compartments
        .iter()
        .enumerate()
        .filter(|(_, c)| c.legacy != Some(1))
        .map(|(i, _)| i)
        .collect();
    let v2_total = v2_indices.len();

    // curve index per original index: 1-based from newest v2 row.
    let mut curve_index_by_original = std::collections::HashMap::new();
    let mut curve_inputs = Vec::with_capacity(v2_total);
    for (v2_ordinal, &original_index) in v2_indices.iter().enumerate() {
        let curve_index = (v2_total - v2_ordinal) as u32;
        curve_index_by_original.insert(original_index, curve_index);
        let importance = compartments[original_index]
            .importance
            .unwrap_or(50)
            .clamp(1, 100);
        curve_inputs.push(DecayInput {
            index: curve_index,
            importance,
        });
    }
    let pressure = if history_budget > 0.0 {
        compute_budget_pressure(&curve_inputs, history_budget)
    } else {
        1.0
    };

    compartments
        .iter()
        .enumerate()
        .map(|(i, c)| {
            if c.legacy == Some(1) {
                legacy_tier(c)
            } else {
                rendered_tier(
                    *curve_index_by_original.get(&i).unwrap_or(&1),
                    c.importance.unwrap_or(50),
                    pressure,
                    0.0,
                )
            }
        })
        .collect()
}

/// Render the decayed compartment-history body (no `<session-history>` wrapper —
/// callers add their own framing). Demotes oldest-first under the budget as a drift
/// guard, measured by the injected `estimate_tokens` (the estimator is its own
/// subsystem). Never renders session facts (v2 faithful).
pub fn render_decayed_compartments(
    compartments: &[DecayRenderCompartment],
    history_budget_tokens: f64,
    estimate_tokens: impl Fn(&str) -> usize,
) -> String {
    if compartments.is_empty() {
        return String::new();
    }
    let mut tiers = compute_tiers(compartments, history_budget_tokens);

    let render = |tiers: &[u8]| -> String {
        let mut parts = Vec::new();
        for (i, c) in compartments.iter().enumerate() {
            let rendered = render_one_compartment(c, tiers[i]);
            if !rendered.is_empty() {
                parts.push(rendered);
            }
        }
        parts.join("\n\n")
    };

    let mut body = render(&tiers);
    // Budget guard: the curve already targets the budget, but estimate drift or a very
    // tight budget can overshoot. Demote oldest-first until it fits.
    let mut guard = compartments.len() * 5;
    while history_budget_tokens > 0.0
        && estimate_tokens(&body) as f64 > history_budget_tokens
        && guard > 0
    {
        let mut demoted = false;
        for t in tiers.iter_mut() {
            if *t < 5 {
                *t += 1;
                demoted = true;
                break;
            }
        }
        if !demoted {
            break;
        }
        body = render(&tiers);
        guard -= 1;
    }
    body
}

/// Extract a top-level m0 block slice (e.g. "session-history") for budget measurement
/// and token attribution. Returns the full `<tag>…</tag>` slice or None. Manual
/// shortest-match (the non-greedy `<tag>[\s\S]*?</tag>`): the first `</tag>` after the
/// first `<tag>`.
pub fn extract_m0_block(m0_text: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = m0_text.find(&open)?;
    let after_open = start + open.len();
    let close_rel = m0_text[after_open..].find(&close)?;
    let end = after_open + close_rel + close.len();
    Some(m0_text[start..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    fn comp(
        start: i64,
        end: i64,
        title: &str,
        p1: &str,
        importance: i32,
    ) -> DecayRenderCompartment {
        DecayRenderCompartment {
            start_message: start,
            end_message: end,
            title: title.to_string(),
            content: String::new(),
            p1: Some(p1.to_string()),
            importance: Some(importance),
            ..Default::default()
        }
    }
    /// A loose budget so the guard never fires (output is purely curve-driven).
    fn no_guard(_: &str) -> usize {
        0
    }

    #[test]
    fn newest_renders_at_p1_full() {
        let c = DecayRenderCompartment {
            start_message: 1,
            end_message: 9,
            title: "T".into(),
            p1: Some("VERBOSE".into()),
            p2: Some("dense".into()),
            importance: Some(50),
            ..Default::default()
        };
        // index 1 (newest) → tier 1 → p1 body
        let out = render_decayed_compartments(std::slice::from_ref(&c), 60_000.0, no_guard);
        assert!(out.contains(">\nVERBOSE\n<"), "newest renders p1: {out}");
        assert!(out.contains("start=\"1\" end=\"9\" title=\"T\""));
    }

    #[test]
    fn archived_tier_is_omitted() {
        assert_eq!(
            render_compartment_at_tier(&comp(1, 2, "x", "body", 50), 5),
            ""
        );
    }

    #[test]
    fn empty_tier_body_self_closes() {
        let mut c = comp(3, 4, "Title", "p1body", 50);
        c.p4 = Some(String::new());
        // explicit P4 with empty body → self-close (valid v2 self-close)
        assert_eq!(
            render_compartment_at_tier(&c, 4),
            "<compartment start=\"3\" end=\"4\" title=\"Title\" />"
        );
    }

    #[test]
    fn xml_escaping_in_attr_and_body() {
        let c = DecayRenderCompartment {
            start_message: 1,
            end_message: 2,
            title: "a<b>&\"c\"".into(),
            p1: Some("x < y & z".into()),
            importance: Some(50),
            ..Default::default()
        };
        let out = render_compartment_at_tier(&c, 1);
        assert!(
            out.contains("title=\"a&lt;b&gt;&amp;&quot;c&quot;\""),
            "{out}"
        );
        assert!(out.contains("x &lt; y &amp; z"), "{out}");
    }

    #[test]
    fn legacy_row_truncates_and_picks_tier() {
        let c = DecayRenderCompartment {
            start_message: 1,
            end_message: 2,
            title: "L".into(),
            content: "U: hello\n".to_string() + &"x".repeat(2000),
            legacy: Some(1),
            ..Default::default()
        };
        // has a U: line → legacy starts at P3 → ≤420 chars + ellipsis
        let out = render_decayed_compartments(std::slice::from_ref(&c), 60_000.0, no_guard);
        assert!(out.ends_with("…\n</compartment>"), "P3 truncates: {out}");
    }

    #[test]
    fn malformed_pseudo_v2_renders_flat_not_empty() {
        // legacy=0 but p1 empty (interrupted upgrade) → flat content, not empty tier
        let c = DecayRenderCompartment {
            start_message: 1,
            end_message: 2,
            title: "M".into(),
            content: "flat body".into(),
            p1: Some(String::new()),
            legacy: Some(0),
            ..Default::default()
        };
        let out = render_compartment_at_tier(&c, 1);
        assert!(out.contains(">\nflat body\n<"), "renders flat: {out}");
    }

    #[test]
    fn budget_guard_demotes_oldest_first() {
        // three compartments; a synthetic estimator (chars) forces demotion. Oldest
        // (index 0, chronologically first) demotes first.
        let comps = vec![
            comp(1, 2, "OLD", "oldverbosebody", 50),
            comp(3, 4, "MID", "midverbosebody", 50),
            comp(5, 6, "NEW", "newverbosebody", 50),
        ];
        let chars = |s: &str| s.chars().count();
        // tiny budget forces demotion until it fits
        let out = render_decayed_compartments(&comps, 80.0, chars);
        assert!(
            chars(&out) as f64 <= 80.0 || out.is_empty(),
            "fits budget: {}",
            chars(&out)
        );
        // the newest should retain more fidelity than the oldest after demotion
        assert!(out.contains("title=\"NEW\""), "newest survives: {out}");
    }

    #[test]
    fn stored_compartment_projects_and_renders() {
        // a StoredCompartment converts directly into the renderer's input shape and
        // renders the same as a hand-built compartment
        let stored = StoredCompartment {
            sequence: 1,
            start_message: 1,
            end_message: 9,
            title: "Stored".into(),
            content: "P1 full".into(),
            p1: Some("P1 full".into()),
            p2: Some("P2".into()),
            importance: 50,
            legacy: 0,
            ..Default::default()
        };
        let out = render_stored_compartments(std::slice::from_ref(&stored), 60_000.0, no_guard);
        assert!(out.contains("title=\"Stored\""), "{out}");
        assert!(
            out.contains(">\nP1 full\n<"),
            "newest stored row at p1: {out}"
        );
        // an empty-p1 stored row is treated as not-tiered → flat content
        let legacy_ish = StoredCompartment {
            sequence: 1,
            title: "Flat".into(),
            content: "flat".into(),
            p1: Some(String::new()),
            legacy: 0,
            ..Default::default()
        };
        let out2 =
            render_stored_compartments(std::slice::from_ref(&legacy_ish), 60_000.0, no_guard);
        assert!(out2.contains(">\nflat\n<"), "empty p1 → flat: {out2}");
    }

    #[test]
    fn extract_m0_block_shortest_match() {
        let m0 = "<a>x</a><session-history>HIST</session-history><b>y</b>";
        assert_eq!(
            extract_m0_block(m0, "session-history").as_deref(),
            Some("<session-history>HIST</session-history>")
        );
        assert_eq!(extract_m0_block(m0, "missing"), None);
    }

    // --- differential golden vs the TS reference (v2 paraphrase path, guard off) ---

    #[derive(Deserialize)]
    struct RawComp {
        #[serde(rename = "startMessage")]
        start: i64,
        #[serde(rename = "endMessage")]
        end: i64,
        title: String,
        #[serde(default)]
        content: String,
        p1: Option<String>,
        p2: Option<String>,
        p3: Option<String>,
        p4: Option<String>,
        importance: Option<i32>,
        legacy: Option<i32>,
    }
    #[derive(Deserialize)]
    struct RenderCase {
        compartments: Vec<RawComp>,
        budget: f64,
        body: String,
    }
    #[derive(Deserialize)]
    struct RenderGolden {
        cases: Vec<RenderCase>,
    }

    #[test]
    fn render_golden_matches_reference() {
        // Generated by crates/mc-core/testdata/gen-golden.ts. All cases use a loose
        // budget so the TS estimateTokens guard never fires → the Rust output (guard
        // off) is the same purely-curve-driven body. Exercises the v2 paraphrase path,
        // legacy truncation (ASCII), archive omission, and XML escaping.
        let raw = include_str!("../testdata/render-golden.json");
        let golden: RenderGolden = serde_json::from_str(raw).expect("parse render-golden.json");
        assert!(!golden.cases.is_empty(), "empty render golden");

        for (n, case) in golden.cases.iter().enumerate() {
            let comps: Vec<DecayRenderCompartment> = case
                .compartments
                .iter()
                .map(|r| DecayRenderCompartment {
                    start_message: r.start,
                    end_message: r.end,
                    title: r.title.clone(),
                    content: r.content.clone(),
                    p1: r.p1.clone(),
                    p2: r.p2.clone(),
                    p3: r.p3.clone(),
                    p4: r.p4.clone(),
                    importance: r.importance,
                    legacy: r.legacy,
                })
                .collect();
            let got = render_decayed_compartments(&comps, case.budget, no_guard);
            assert_eq!(got, case.body, "render mismatch in case {n}");
        }
    }
}

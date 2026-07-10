import { describe, expect, it } from "bun:test";
import { type DecayRenderCompartment, renderDecayedCompartments } from "./decay-render";

function legacyCompartment(i: number): DecayRenderCompartment {
    return {
        startMessage: i,
        endMessage: i,
        title: `legacy ${i}`,
        content: "legacy summary",
        legacy: 1,
    };
}

describe("decay-render", () => {
    it("excludes legacy rows from v2 pressure and age indexing", () => {
        const v2: DecayRenderCompartment = {
            startMessage: 1,
            endMessage: 1,
            title: "v2 oldest",
            content: "content fallback",
            p1: "P1_KEEP",
            p2: "P2_LOWER",
            p3: "P3_LOWER",
            p4: "P4_LOWER",
            importance: 50,
        };
        const compartments = [
            v2,
            ...Array.from({ length: 80 }, (_, i) => legacyCompartment(i + 2)),
        ];

        const rendered = renderDecayedCompartments({
            compartments,
            historyBudgetTokens: 3000,
        });

        expect(rendered).toContain("P1_KEEP");
        expect(rendered).not.toContain("P2_LOWER");
        expect(rendered).not.toContain("P3_LOWER");
        expect(rendered).not.toContain("P4_LOWER");
    });

    it("renders a malformed pseudo-v2 row (legacy=0, p1='') via flat content, not empty", () => {
        // Interrupted-upgrade state: legacy=0 but tiers never populated (p1='').
        // The parser treats p1='' as NOT tiered; the renderer must fall back to
        // `content` instead of emitting an empty self-closing compartment that
        // silently drops the body. Render at P1 (newest) to exercise the body path.
        const pseudoV2: DecayRenderCompartment = {
            startMessage: 1,
            endMessage: 5,
            title: "pseudo v2",
            content: "PSEUDO_BODY_KEEP",
            p1: "",
            p2: "",
            p3: "",
            p4: "",
            legacy: 0,
            importance: 90,
        };
        const rendered = renderDecayedCompartments({
            compartments: [pseudoV2],
            historyBudgetTokens: 100_000,
        });
        expect(rendered).toContain("PSEUDO_BODY_KEEP");
        // Must NOT be a bare self-close — the body has to be present.
        expect(rendered).not.toMatch(/<compartment[^>]*\/>/);
    });

    it("renders complete date ranges in canonical order across tiered, legacy, and self-close arms", () => {
        const attrs =
            'start="1" end="2" start-date="2026-01-02" end-date="2026-01-03" title="dated"';
        const base: DecayRenderCompartment = {
            startMessage: 1,
            endMessage: 2,
            startDate: "2026-01-02",
            endDate: "2026-01-03",
            title: "dated",
            content: "U: prompt\nflat body",
        };

        const tiered = renderDecayedCompartments({
            compartments: [{ ...base, p1: "tiered body", legacy: 0 }],
            historyBudgetTokens: 100_000,
        });
        const legacy = renderDecayedCompartments({
            compartments: [{ ...base, legacy: 1 }],
            historyBudgetTokens: 100_000,
        });
        const selfClosing = renderDecayedCompartments({
            compartments: [{ ...base, content: "", legacy: 1 }],
            historyBudgetTokens: 100_000,
        });

        expect(tiered).toContain(`<compartment ${attrs}>`);
        expect(legacy).toContain(`<compartment ${attrs}>`);
        expect(selfClosing).toBe(`<compartment ${attrs} />`);
    });

    it("omits date attributes when a range is absent or partial", () => {
        const base: DecayRenderCompartment = {
            startMessage: 1,
            endMessage: 2,
            title: "undated",
            content: "body",
            legacy: 1,
        };
        const withoutDates = renderDecayedCompartments({
            compartments: [base],
            historyBudgetTokens: 100_000,
        });
        const partial = renderDecayedCompartments({
            compartments: [{ ...base, startDate: "2026-01-02" }],
            historyBudgetTokens: 100_000,
        });

        expect(withoutDates).not.toContain("start-date=");
        expect(withoutDates).not.toContain("end-date=");
        expect(partial).not.toContain("start-date=");
        expect(partial).not.toContain("end-date=");
    });

    it("keeps a VALID v2 row with empty p4 self-closing when demoted to P4", () => {
        // A genuine v2 row (non-empty p1) with an empty p4 must still self-close
        // at tier 4 — the isTieredRow fix must NOT regress this. Force P4 via a
        // tiny budget across many rows so the oldest demotes to archive/P4.
        const rows: DecayRenderCompartment[] = Array.from({ length: 30 }, (_, i) => ({
            startMessage: i + 1,
            endMessage: i + 1,
            title: `v2 ${i}`,
            content: `P1_BODY_${i}`,
            p1: `P1_BODY_${i}`,
            p2: `P2_${i}`,
            p3: `P3_${i}`,
            p4: "",
            legacy: 0,
            importance: 10,
        }));
        const rendered = renderDecayedCompartments({
            compartments: rows,
            historyBudgetTokens: 200,
        });
        // At least one row should have demoted to a self-closing form (valid P4).
        expect(rendered).toMatch(/<compartment[^>]*\/>/);
    });
});

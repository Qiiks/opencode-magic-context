import { describe, expect, it } from "bun:test";
import {
    buildChannel1Reminder,
    buildChannel2Reminder,
    CHANNEL1_FLOOR_TOKENS,
    CHANNEL1_MIN_TOKENS,
    CHANNEL2_MIN_RECLAIMABLE,
    CHANNEL2_USABLE_FRACTION,
    channel1RefireTokens,
    decideChannel1,
    shouldTriggerChannel2,
} from "./ctx-reduce-nudge";

describe("decideChannel1 — agent-tail hygiene ratio", () => {
    const base = {
        baselineU: 0,
        baselineT: 100_000,
        turnDeltaU: 0,
        turnDeltaT: 0,
        lastNudgeUndropped: 0,
        lastNudgeLevel: "" as const,
        hasRecentReduce: false,
        evaluable: true,
    };

    it("uses the four owner-set bands without wall-pressure input", () => {
        expect(decideChannel1({ ...base, baselineU: 20_000 }).level).toBe("gentle");
        expect(decideChannel1({ ...base, baselineU: 40_000 }).level).toBe("firm");
        expect(decideChannel1({ ...base, baselineU: 55_000 }).level).toBe("urgent");
        expect(decideChannel1({ ...base, baselineU: 60_000 }).level).toBe("urgent");
    });

    it("is window-size invariant for proportional tail states", () => {
        const sol = decideChannel1({ ...base, baselineU: 39_600, baselineT: 72_000 });
        const fable = decideChannel1({ ...base, baselineU: 119_900, baselineT: 218_000 });
        expect(sol.fire).toBe(true);
        expect(fable.fire).toBe(true);
        expect(sol.level).toBe("urgent");
        expect(fable.level).toBe(sol.level);
    });

    it("fires for the flagship 162k/249k rendered-tail incident", () => {
        const decision = decideChannel1({ ...base, baselineU: 162_000, baselineT: 249_000 });
        expect(decision.fire).toBe(true);
        expect(decision.level).toBe("urgent");
    });

    it("fires at low total-window usage because the pressure gate is deleted", () => {
        const decision = decideChannel1({ ...base, baselineU: 140_000, baselineT: 200_000 });
        expect(decision.fire).toBe(true);
        expect(decision.level).toBe("urgent");
    });

    it("holds a generation-invalidated baseline", () => {
        const decision = decideChannel1({
            ...base,
            baselineU: 70_000,
            generationInvalidated: true,
            evaluable: false,
        });
        expect(decision.fire).toBe(false);
    });

    it("guards T=0 and U=0", () => {
        expect(decideChannel1({ ...base, baselineU: 0, baselineT: 0 }).fire).toBe(false);
        expect(decideChannel1({ ...base, baselineU: 0 }).fire).toBe(false);
    });

    it("uses max(T,1) and clamps a defensive U>T ratio", () => {
        const decision = decideChannel1({
            ...base,
            baselineU: 100_000,
            baselineT: 60_000,
        });
        expect(decision.fire).toBe(true);
        expect(decision.level).toBe("urgent");
        expect(decision.severity).toBe(1);
    });

    it("MIN_T keeps a 59k tail quiet and allows a 61k tail", () => {
        const under = decideChannel1({ ...base, baselineU: 55_000, baselineT: 59_000 });
        const over = decideChannel1({ ...base, baselineU: 55_000, baselineT: 61_000 });
        expect(CHANNEL1_MIN_TOKENS).toBe(60_000);
        expect(under.fire).toBe(false);
        expect(over.fire).toBe(true);
        expect(over.level).toBe("urgent");
    });

    it("prevents a post-fold first-read spike", () => {
        const decision = decideChannel1({ ...base, baselineU: 30_000, baselineT: 35_000 });
        expect(decision.fire).toBe(false);
    });

    it("uses the 25k U floor", () => {
        expect(CHANNEL1_FLOOR_TOKENS).toBe(25_000);
        expect(decideChannel1({ ...base, baselineU: 24_000 }).fire).toBe(false);
        expect(decideChannel1({ ...base, baselineU: 26_000 }).fire).toBe(true);
    });

    it("combines persisted baseline and typed turn deltas", () => {
        const decision = decideChannel1({
            ...base,
            baselineU: 20_000,
            baselineT: 55_000,
            turnDeltaU: 10_000,
            turnDeltaT: 10_000,
        });
        expect(decision.undroppedTokens).toBe(30_000);
        expect(decision.tailTokens).toBe(65_000);
        expect(decision.fire).toBe(true);
        expect(decision.level).toBe("firm");
    });

    it("suppresses same-band noise until the rebased cadence interval", () => {
        const quiet = decideChannel1({
            ...base,
            baselineU: 48_000,
            baselineT: 120_000,
            lastNudgeUndropped: 40_000,
            lastNudgeLevel: "firm",
        });
        const refire = decideChannel1({
            ...base,
            baselineU: 65_000,
            baselineT: 140_000,
            lastNudgeUndropped: 40_000,
            lastNudgeLevel: "firm",
        });
        expect(quiet.fire).toBe(false);
        expect(refire.fire).toBe(true);
        expect(refire.level).toBe("firm");
    });

    it("fires an escalation before cadence is reached", () => {
        const decision = decideChannel1({
            ...base,
            baselineU: 55_000,
            baselineT: 100_000,
            lastNudgeUndropped: 50_000,
            lastNudgeLevel: "firm",
        });
        expect(decision.fire).toBe(true);
        expect(decision.level).toBe("urgent");
    });

    it("uses max(25k, 0.08 × T) cadence", () => {
        expect(channel1RefireTokens(60_000)).toBe(25_000);
        expect(channel1RefireTokens(100_000)).toBe(25_000);
        expect(channel1RefireTokens(1_000_000)).toBe(80_000);
    });

    it("post-reduce suppression clears cadence and band state", () => {
        const decision = decideChannel1({
            ...base,
            baselineU: 70_000,
            lastNudgeUndropped: 60_000,
            lastNudgeLevel: "urgent",
            hasRecentReduce: true,
        });
        expect(decision.fire).toBe(false);
        expect(decision.nextLastNudge).toBe(0);
        expect(decision.nextLastNudgeLevel).toBe("");
    });

    it("a measured U collapse re-arms the cycle", () => {
        const decision = decideChannel1({
            ...base,
            baselineU: 30_000,
            baselineT: 100_000,
            lastNudgeUndropped: 80_000,
            lastNudgeLevel: "urgent",
        });
        expect(decision.fire).toBe(true);
        expect(decision.level).toBe("gentle");
    });
});

describe("reminder rendering", () => {
    it("renders the passive reminder and amount", () => {
        const reminder = buildChannel1Reminder("firm", 42_000, [
            { tagNumber: 123, toolName: "read" },
            { tagNumber: 145, toolName: null },
        ]);
        expect(reminder).toContain("<system-reminder>");
        expect(reminder).toContain("</system-reminder>");
        expect(reminder).toContain("~42k");
        expect(reminder).toContain("oldest reclaimable: §123§ read · §145§ tool.");
    });

    it("renders the Channel-2 carrier", () => {
        const reminder = buildChannel2Reminder(55_000);
        expect(reminder).toContain("<system-reminder>");
        expect(reminder).toContain("~55k");
        expect(reminder).toContain("ctx_reduce");
    });
});

describe("shouldTriggerChannel2 — new baseline seam with legacy predicate", () => {
    it("uses effective baseline plus typed deltas through the new input shape", () => {
        expect(
            shouldTriggerChannel2({
                baselineU: 20_000,
                baselineT: 90_000,
                deltas: { u: 10_000, t: 0 },
            }),
        ).toBe(true);
        expect(
            shouldTriggerChannel2({
                baselineU: 20_000,
                baselineT: 300_000,
                deltas: { u: 34_000, t: 0 },
            }),
        ).toBe(false);
    });

    it("retains the existing floor and fraction", () => {
        expect(CHANNEL2_MIN_RECLAIMABLE).toBe(10_000);
        expect(CHANNEL2_USABLE_FRACTION).toBeCloseTo(1 / 3, 5);
        expect(
            shouldTriggerChannel2({
                baselineU: CHANNEL2_MIN_RECLAIMABLE - 1,
                baselineT: 1_000,
                deltas: { u: 0, t: 0 },
            }),
        ).toBe(false);
    });
});

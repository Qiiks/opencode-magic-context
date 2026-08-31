import { escalationBands } from "../../shared/escalation-bands";

export type BypassReason = "force-materialize" | "explicit-bust" | "subagent" | "none";

export interface BypassInput {
    contextUsage: { percentage: number };
    sessionMeta: { isSubagent: boolean };
    historyRefreshSessions: Set<string>;
    sessionId: string;
    effectiveExecuteThresholdPercentage: number;
}

export function detectMidTurnBypassReason(input: BypassInput): BypassReason {
    const { forceMaterializationPercentage } = escalationBands(
        input.effectiveExecuteThresholdPercentage,
    );
    if (input.contextUsage.percentage >= forceMaterializationPercentage) return "force-materialize";
    if (input.historyRefreshSessions.has(input.sessionId)) return "explicit-bust";
    if (input.sessionMeta.isSubagent) return "subagent";
    return "none";
}

export interface ApplyMidTurnDeferralInput {
    base: "execute" | "defer";
    bypassReason: BypassReason;
    midTurn: boolean;
}

export type SchedulerDeferReason = "scheduler_defer" | "mid_turn_boundary";

export interface ApplyMidTurnDeferralOutput {
    midTurnAdjustedSchedulerDecision: "execute" | "defer";
    sideEffect: "set-flag" | "none";
    /** Preserve the reason that corresponds to the final mid-turn scheduler decision so refusal logs remain consistent and do not recompute state after the decision changes. */
    deferReason: SchedulerDeferReason | null;
}

export function applyMidTurnDeferral(input: ApplyMidTurnDeferralInput): ApplyMidTurnDeferralOutput {
    // Scope: bypass evaluation is nested under base === "execute".
    if (input.base === "defer") {
        return {
            midTurnAdjustedSchedulerDecision: "defer",
            sideEffect: "none",
            deferReason: "scheduler_defer",
        };
    }
    // base === "execute"
    if (input.bypassReason !== "none") {
        return {
            midTurnAdjustedSchedulerDecision: "execute",
            sideEffect: "none",
            deferReason: null,
        };
    }
    if (input.midTurn) {
        return {
            midTurnAdjustedSchedulerDecision: "defer",
            sideEffect: "set-flag",
            deferReason: "mid_turn_boundary",
        };
    }
    return {
        midTurnAdjustedSchedulerDecision: "execute",
        sideEffect: "none",
        deferReason: null,
    };
}

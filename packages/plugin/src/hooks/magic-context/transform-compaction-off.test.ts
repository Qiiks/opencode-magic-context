/// <reference types="bun-types" />

/**
 * Compaction-off mode transform behavior (issue #266 S3).
 *
 * Additive-only proof: with compaction off, the transform keeps m[0]/m[1]
 * memory/docs injection (the zero-compartment path) and identity/measurement
 * recording, while every mutating compaction surface stays inert — no drops,
 * folds, strips, nudges, tag writes, markers, temporal overlays, synthetic
 * todos, emergency actions or blocking. Several tests are mutation-direction:
 * un-gating the covered surface makes the assertion go red.
 */

import { afterEach, describe, expect, it, mock } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import {
    replaceAllCompartments,
} from "../../features/magic-context/compartment-storage";
import { insertMemory } from "../../features/magic-context/memory";
import {
    __resetProjectIdentityForTests,
    resolveProjectIdentity,
} from "../../features/magic-context/memory/project-identity";
import { __resetMessageIndexAsyncForTests } from "../../features/magic-context/message-index-async";
import type { Scheduler } from "../../features/magic-context/scheduler";
import {
    closeDatabase,
    getOrCreateSessionMeta,
    getPendingOps,
    getTagsBySession,
    openDatabase,
    queuePendingOp,
    recordOverflowDetected,
    updateSessionMeta,
} from "../../features/magic-context/storage";
import {
    getChannel2NudgeState,
    getCompactionModeRecord,
    getOverflowState,
    setCompactionModeRecord,
} from "../../features/magic-context/storage-meta-persisted";
import { createTagger } from "../../features/magic-context/tagger";
import type { ContextUsage } from "../../features/magic-context/types";
import type { PluginContext } from "../../plugin/types";
import { clearModelsDevCache } from "../../shared/models-dev-cache";
import { Database } from "../../shared/sqlite";
import { closeQuietly } from "../../shared/sqlite-helpers";
import { COMPACTION_OFF_FLIP_NOTICE } from "./compaction-off-transition";
import { createTransform } from "./transform";

type TestMessage = {
    info: {
        id?: string;
        role: string;
        sessionID?: string;
        tools?: Record<string, unknown>;
    };
    parts: Array<
        | { type: "text"; text: string }
        | { type: "tool"; callID: string; state: { input?: unknown; output: string } }
    >;
};

const tempDirs: string[] = [];
const originalXdgDataHome = process.env.XDG_DATA_HOME;

afterEach(() => {
    __resetMessageIndexAsyncForTests();
    __resetProjectIdentityForTests();
    closeDatabase();
    clearModelsDevCache();
    if (originalXdgDataHome === undefined) delete process.env.XDG_DATA_HOME;
    else process.env.XDG_DATA_HOME = originalXdgDataHome;
    for (const dir of tempDirs) {
        try {
            rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
        } catch {
            /* ignore */
        }
    }
    tempDirs.length = 0;
});

function useTempDataHome(prefix: string): void {
    const dir = mkdtempSync(join(tmpdir(), prefix));
    tempDirs.push(dir);
    process.env.XDG_DATA_HOME = dir;
    process.env.XDG_CACHE_HOME = dir;
}

/** Minimal opencode.db so transform paths that read raw history don't throw. */
function createOpenCodeDbForSession(sessionId: string): void {
    const dbPath = join(process.env.XDG_DATA_HOME!, "opencode", "opencode.db");
    mkdirSync(dirname(dbPath), { recursive: true });
    const db = new Database(dbPath);
    try {
        db.exec(`
            CREATE TABLE IF NOT EXISTS message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS part (
                id TEXT PRIMARY KEY,
                message_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
        `);
        db.prepare(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?)",
        ).run(
            "m-user-1",
            sessionId,
            1,
            1,
            JSON.stringify({ id: "m-user-1", role: "user", sessionID: sessionId }),
        );
    } finally {
        closeQuietly(db);
    }
}

function makeOffTransform(args: {
    sessionId: string;
    compactionOff?: boolean;
    schedulerDecision?: "execute" | "defer";
    percentage?: number;
    client?: PluginContext["client"];
    isSubagent?: boolean;
}) {
    const scheduler: Scheduler = {
        shouldExecute: mock(() => (args.schedulerDecision ?? "defer") as "execute" | "defer"),
    };
    const db = openDatabase();
    if (args.isSubagent) {
        updateSessionMeta(db, args.sessionId, { isSubagent: true });
    }
    const transform = createTransform({
        tagger: createTagger(),
        scheduler,
        contextUsageMap: new Map<string, { usage: ContextUsage; updatedAt: number }>([
            [
                args.sessionId,
                {
                    usage: {
                        percentage: args.percentage ?? 25,
                        inputTokens: (args.percentage ?? 25) * 2_000,
                    },
                    updatedAt: Date.now(),
                },
            ],
        ]),
        db,
        historyRefreshSessions: new Set<string>(),
        pendingMaterializationSessions: new Set<string>(),
        lastHeuristicsTurnId: new Map<string, string>(),
        clearReasoningAge: 50,
        protectedTags: 0,
        directory: "/repo/project",
        memoryConfig: { enabled: true, injectionBudgetTokens: 500, autoPromote: true },
        compactionOff: args.compactionOff ?? true,
        client: args.client,
    });
    return { db, transform };
}

function makeMessages(sessionId: string): TestMessage[] {
    return [
        {
            info: { id: "m-user-1", role: "user", sessionID: sessionId },
            parts: [{ type: "text", text: "plan the change" }],
        },
        {
            info: { id: "m-assistant-1", role: "assistant", sessionID: sessionId },
            parts: [
                { type: "text", text: "running a tool" },
                {
                    type: "tool",
                    callID: "call-1",
                    state: { input: { path: "src/index.ts" }, output: "tool output body" },
                },
            ],
        },
        {
            info: { id: "m-user-2", role: "user", sessionID: sessionId },
            parts: [{ type: "text", text: "continue" }],
        },
    ];
}

function textOf(message: TestMessage, index: number): string {
    const part = message.parts[index];
    return part && part.type === "text" ? part.text : "";
}

function allText(messages: TestMessage[]): string {
    return messages
        .map((message) =>
            message.parts
                .map((part) => (part.type === "text" ? part.text : part.state?.output ?? ""))
                .join("\n"),
        )
        .join("\n");
}

describe("compaction-off transform — additive-only proof (issue #266 S3)", () => {
    it("memory injection SURVIVES compaction-off (mutation direction: gating the injection off makes this red)", async () => {
        useTempDataHome("co-memory-survival-");
        const db = openDatabase();
        insertMemory(db, {
            projectPath: resolveProjectIdentity("/repo/project"),
            category: "USER_DIRECTIVES",
            content: "Always use Bun",
        });
        closeDatabase();

        const { transform } = makeOffTransform({ sessionId: "ses-1" });
        const messages = makeMessages("ses-1");

        await transform({}, { messages });

        // m[0]/m[1] prepended; the <project-memory> block is present even
        // though compaction is off — the core issue-266 assertion.
        expect(messages).toHaveLength(5);
        expect(textOf(messages[0], 0)).toContain("<project-memory>");
        expect(textOf(messages[0], 0)).toContain("Always use Bun");
    });

    it("with historical compartment rows: <project-memory> present, NO rendered <session-history>, NO raw-tail trim", async () => {
        useTempDataHome("co-historical-compartments-");
        const db = openDatabase();
        insertMemory(db, {
            projectPath: resolveProjectIdentity("/repo/project"),
            category: "USER_DIRECTIVES",
            content: "Always use Bun",
        });
        // Historical compartment rows exist (an upgrade session).
        replaceAllCompartments(db, "ses-1", [
            {
                sequence: 0,
                startMessage: 0,
                endMessage: 1,
                startMessageId: "m-user-1",
                endMessageId: "m-assistant-1",
                title: "Ancient setup work",
                content: "U: did setup\nA: completed setup.",
            },
        ]);
        closeDatabase();

        const { transform } = makeOffTransform({ sessionId: "ses-1" });
        const messages = makeMessages("ses-1");

        await transform({}, { messages });

        const m0 = textOf(messages[0], 0);
        expect(m0).toContain("<project-memory>");
        // Compartment history preparation is BYPASSED: no decay render of the
        // historical rows into m[0] (the empty session-history slot of the
        // zero-compartment path is the stable m[0] skeleton; it carries no
        // compartment content).
        expect(m0).not.toContain("Ancient setup work");
        expect(m0).not.toContain("completed setup");
        expect(m0).toContain("<session-history></session-history>");
        // No raw-tail trim / boundary splice: every input message survives.
        const ids = messages.map((message) => message.info.id);
        expect(ids).toContain("m-user-1");
        expect(ids).toContain("m-assistant-1");
        expect(ids).toContain("m-user-2");
    });

    it("multi-pass run: output = input + m0/m1 only — zero drops, folds, nudges, strips, tags, markers, compartment rows", async () => {
        useTempDataHome("co-multi-pass-");
        const db = openDatabase();
        insertMemory(db, {
            projectPath: resolveProjectIdentity("/repo/project"),
            category: "USER_DIRECTIVES",
            content: "Always use Bun",
        });
        closeDatabase();

        const { transform } = makeOffTransform({
            sessionId: "ses-1",
            schedulerDecision: "execute",
            percentage: 90, // deep in the emergency band — nothing may fire
        });

        for (let pass = 0; pass < 3; pass += 1) {
            const messages = makeMessages("ses-1");
            await transform({}, { messages });

            // Additive-only shape: exactly the two injected head messages.
            expect(messages).toHaveLength(5);
            const wire = allText(messages);
            // No §N§ prefixes anywhere on the wire.
            expect(wire).not.toMatch(/§\d+§/);
            // Tool output intact — no drops, no caveman, no truncation.
            expect(wire).toContain("tool output body");
            // No strip machinery artifacts.
            expect(wire).not.toContain("[cleared]");
            expect(wire).not.toContain("[dropped");
            // No synthetic todowrite injection, no temporal overlays.
            expect(wire).not.toContain("todowrite");
            expect(wire).not.toMatch(/<!-- \+\d+m -->/);
            // Memory still injected on every pass.
            expect(textOf(messages[0], 0)).toContain("<project-memory>");
        }

        const db2 = openDatabase();
        // Zero tag rows written (tagger fully gated off).
        expect(getTagsBySession(db2, "ses-1")).toHaveLength(0);
        // No pending ops accumulated or applied.
        expect(getPendingOps(db2, "ses-1")).toHaveLength(0);
        // No Channel-2 intent.
        expect(getChannel2NudgeState(db2, "ses-1")).toBe("");
        // No emergency latch armed despite 90% usage.
        expect(getOverflowState(db2, "ses-1").needsEmergencyRecovery).toBe(false);
        // The mode record committed on the first pass.
        expect(getCompactionModeRecord(db2, "ses-1")).toBe("off");
        // (Usage measurement persistence lives in the event handler's
        // message.updated path and stays ungated — transform is not the
        // writer, so it is asserted in the event-handler suite, not here.)
    });

    it("additive injection replays BYTE-IDENTICAL across defer-equivalent passes", async () => {
        useTempDataHome("co-byte-stability-");
        const db = openDatabase();
        insertMemory(db, {
            projectPath: resolveProjectIdentity("/repo/project"),
            category: "USER_DIRECTIVES",
            content: "Always use Bun",
        });
        closeDatabase();

        const { transform } = makeOffTransform({ sessionId: "ses-1" });

        const first = makeMessages("ses-1");
        await transform({}, { messages: first });
        const m0Pass1 = textOf(first[0], 0);
        const m1Pass1 = textOf(first[1], 0);

        // Two more defer-equivalent passes (identical input shape).
        for (let pass = 0; pass < 2; pass += 1) {
            const messages = makeMessages("ses-1");
            await transform({}, { messages });
            expect(textOf(messages[0], 0)).toBe(m0Pass1);
            expect(textOf(messages[1], 0)).toBe(m1Pass1);
        }
    });

    it("steady-state off passes never apply queued pending ops (mutation direction: un-gating the drain makes this red)", async () => {
        useTempDataHome("co-pending-ops-gate-");
        const db = openDatabase();
        insertMemory(db, {
            projectPath: resolveProjectIdentity("/repo/project"),
            category: "USER_DIRECTIVES",
            content: "Always use Bun",
        });
        // Pre-record the mode so this pass is steady-state off (no transition
        // cleanup of the op we are about to queue).
        getOrCreateSessionMeta(db, "ses-1");
        setCompactionModeRecord(db, "ses-1", "off");
        closeDatabase();

        // Pass 1 (ON mode) mints a tag for the tool output.
        const onTransform = makeOffTransform({
            sessionId: "ses-1",
            compactionOff: false,
            schedulerDecision: "defer",
        });
        const seed = makeMessages("ses-1");
        await onTransform.transform({}, { messages: seed });
        const dbAfterSeed = openDatabase();
        const tags = getTagsBySession(dbAfterSeed, "ses-1");
        expect(tags.length).toBeGreaterThan(0);
        // Reset the record: the seed pass ran ON-mode and flipped it to "on".
        setCompactionModeRecord(dbAfterSeed, "ses-1", "off");
        // Queue a drop intent against one of the minted tags.
        const targetTag = tags.find((tag) => tag.type === "tool") ?? tags[0];
        expect(targetTag).toBeDefined();
        queuePendingOp(dbAfterSeed, "ses-1", targetTag!.tagNumber, "drop");
        expect(getPendingOps(dbAfterSeed, "ses-1")).toHaveLength(1);
        closeDatabase();

        // Pass 2 (OFF mode, scheduler says execute): the op must NOT apply.
        const { transform } = makeOffTransform({
            sessionId: "ses-1",
            schedulerDecision: "execute",
            percentage: 70,
        });
        const messages = makeMessages("ses-1");
        await transform({}, { messages });

        const dbAfter = openDatabase();
        expect(getPendingOps(dbAfter, "ses-1")).toHaveLength(1);
        const wire = allText(messages);
        expect(wire).toContain("tool output body");
        expect(wire).not.toContain("[dropped");
    });

    it("flip-back lazy mint: untagged wire content gets tag rows once the mode flips back on", async () => {
        useTempDataHome("co-flip-back-mint-");
        const db = openDatabase();
        insertMemory(db, {
            projectPath: resolveProjectIdentity("/repo/project"),
            category: "USER_DIRECTIVES",
            content: "Always use Bun",
        });
        closeDatabase();

        // Off pass: zero tag rows.
        const off = makeOffTransform({ sessionId: "ses-1" });
        const offMessages = makeMessages("ses-1");
        await off.transform({}, { messages: offMessages });
        const dbMid = openDatabase();
        expect(getTagsBySession(dbMid, "ses-1")).toHaveLength(0);
        expect(getCompactionModeRecord(dbMid, "ses-1")).toBe("off");
        closeDatabase();

        // Flip back on: the tagger lazily mints on first observation of the
        // untagged wire content (the same mechanism that handles mid-session
        // ctx_reduce enablement). No backfill beyond the live window.
        const on = makeOffTransform({ sessionId: "ses-1", compactionOff: false });
        const onMessages = makeMessages("ses-1");
        await on.transform({}, { messages: onMessages });
        const dbAfter = openDatabase();
        expect(getTagsBySession(dbAfter, "ses-1").length).toBeGreaterThan(0);
        expect(getCompactionModeRecord(dbAfter, "ses-1")).toBe("on");
        // §N§ prefixes return with the flip (natural bust of the transition).
        expect(allText(onMessages)).toMatch(/§\d+§/);
    });

    it("compaction-off SUBAGENT session receives additive m0/m1 memory and no mutating gates", async () => {
        useTempDataHome("co-subagent-");
        const db = openDatabase();
        insertMemory(db, {
            projectPath: resolveProjectIdentity("/repo/project"),
            category: "USER_DIRECTIVES",
            content: "Always use Bun",
        });
        closeDatabase();

        const { transform } = makeOffTransform({
            sessionId: "ses-sub",
            isSubagent: true,
            schedulerDecision: "execute",
            percentage: 90,
        });
        const messages = makeMessages("ses-sub");
        await transform({}, { messages });

        // Subagents GAIN the knowledge surface in compaction-off mode...
        expect(messages).toHaveLength(5);
        expect(textOf(messages[0], 0)).toContain("<project-memory>");
        expect(textOf(messages[0], 0)).toContain("Always use Bun");
        // ...and lose every MC reclaim path: no drops, no prefixes.
        const wire = allText(messages);
        expect(wire).toContain("tool output body");
        expect(wire).not.toMatch(/§\d+§/);
        const dbAfter = openDatabase();
        expect(getTagsBySession(dbAfter, "ses-sub")).toHaveLength(0);
        expect(getOverflowState(dbAfter, "ses-sub").needsEmergencyRecovery).toBe(false);
    });

    it("transition pass: flip notice goes OUT OF BAND and the message array is indistinguishable from a steady-state off pass", async () => {
        useTempDataHome("co-transition-notice-");
        createOpenCodeDbForSession("ses-1");
        const db = openDatabase();
        insertMemory(db, {
            projectPath: resolveProjectIdentity("/repo/project"),
            category: "USER_DIRECTIVES",
            content: "Always use Bun",
        });
        // Seed durable MC state so the off-transition clears something.
        getOrCreateSessionMeta(db, "ses-1");
        queuePendingOp(db, "ses-1", 9, "drop");
        recordOverflowDetected(db, "ses-1", 120000);
        closeDatabase();

        const promptMock = mock(async () => ({ data: {} }));
        const client = {
            session: { prompt: promptMock },
        } as unknown as PluginContext["client"];

        const { transform } = makeOffTransform({ sessionId: "ses-1", client });
        const transitionMessages = makeMessages("ses-1");
        await transform({}, { messages: transitionMessages });
        // Let the fire-and-forget notice settle.
        await new Promise((resolve) => setTimeout(resolve, 80));

        // The notice was delivered out of band with the contractual wording.
        expect(promptMock).toHaveBeenCalledTimes(1);
        const promptCall = promptMock.mock.calls[0][0] as {
            body?: { parts?: Array<{ text?: string }>; noReply?: boolean };
        };
        const noticeText = promptCall.body?.parts?.map((part) => part.text ?? "").join("\n") ?? "";
        expect(noticeText).toContain("compaction-off mode is now active");
        expect(noticeText).toContain(
            "the first turn after disabling may trigger one native compaction cycle on long sessions",
        );

        // The message array itself carries NO notice: run a steady-state pass
        // on the same session and compare shapes byte-for-byte.
        const steadyMessages = makeMessages("ses-1");
        await transform({}, { messages: steadyMessages });
        expect(JSON.stringify(transitionMessages)).toBe(JSON.stringify(steadyMessages));
        // No synthetic turn, no nudge channel state.
        const dbAfter = openDatabase();
        expect(getChannel2NudgeState(dbAfter, "ses-1")).toBe("");
        expect(getCompactionModeRecord(dbAfter, "ses-1")).toBe("off");
    });

    it("clean fresh session booting off-mode: record written, NO notice emitted", async () => {
        useTempDataHome("co-clean-boot-");
        createOpenCodeDbForSession("ses-1");
        const db = openDatabase();
        insertMemory(db, {
            projectPath: resolveProjectIdentity("/repo/project"),
            category: "USER_DIRECTIVES",
            content: "Always use Bun",
        });
        closeDatabase();

        const promptMock = mock(async () => ({ data: {} }));
        const client = {
            session: { prompt: promptMock },
        } as unknown as PluginContext["client"];

        const { transform } = makeOffTransform({ sessionId: "ses-1", client });
        const messages = makeMessages("ses-1");
        await transform({}, { messages: messages });
        await new Promise((resolve) => setTimeout(resolve, 80));

        expect(promptMock).not.toHaveBeenCalled();
        const dbAfter = openDatabase();
        expect(getCompactionModeRecord(dbAfter, "ses-1")).toBe("off");
    });

    it("a persisted emergency latch is cleared by the off-transition, never honored (no abort, no 95% bump)", async () => {
        useTempDataHome("co-latch-clear-");
        const db = openDatabase();
        insertMemory(db, {
            projectPath: resolveProjectIdentity("/repo/project"),
            category: "USER_DIRECTIVES",
            content: "Always use Bun",
        });
        getOrCreateSessionMeta(db, "ses-1");
        recordOverflowDetected(db, "ses-1", 100000, "anthropic/claude");
        expect(getOverflowState(db, "ses-1").needsEmergencyRecovery).toBe(true);
        closeDatabase();

        const { transform } = makeOffTransform({
            sessionId: "ses-1",
            schedulerDecision: "execute",
            percentage: 97,
        });
        const messages = makeMessages("ses-1");
        // Must NOT throw (no emergency fail-closed abort in this mode).
        await transform({}, { messages });

        const dbAfter = openDatabase();
        expect(getOverflowState(dbAfter, "ses-1").needsEmergencyRecovery).toBe(false);
        // The pass stayed additive: injection happened, nothing dropped.
        expect(textOf(messages[0], 0)).toContain("<project-memory>");
        expect(allText(messages)).toContain("tool output body");
    });
});

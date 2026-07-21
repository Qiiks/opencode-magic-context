import { createHash } from "node:crypto";
import { trimMemoriesToBudgetV2 } from "../../../hooks/magic-context/inject-compartments";
import type { PluginContext } from "../../../plugin/types";
import * as shared from "../../../shared";
import { extractLatestAssistantText } from "../../../shared/assistant-message-extractor";
import { modelBodyField } from "../../../shared/resolve-fallbacks";
import type { Database } from "../../../shared/sqlite";
import { runLeaseGuardedWrite, startLeaseHeartbeat } from "../dreamer/lease";
import { getMemoriesByProject } from "../memory/storage-memory";
import type { Memory } from "../memory/types";
import {
    MURAL_AUTHORING_PROMPT,
    type MuralManifestEntry,
    type MuralSourceMemory,
    parseMuralManifest,
    validateMuralManifest,
} from "./mural-prompt";
import { MURAL_HEIGHT, MURAL_WIDTH, renderMural } from "./render-mural";
import { upsertMural } from "./storage-mural";

export const MURAL_MIN_MEMORIES = 20;
export const DEFAULT_MURAL_MEMORY_BUDGET = 8_000;

export type MuralAuthorCall = (
    prompt: string,
    model: string | undefined,
    fallbackModels: readonly string[] | undefined,
) => Promise<string>;

export function muralOverflowMemories(
    memories: readonly Memory[],
    budgetTokens = DEFAULT_MURAL_MEMORY_BUDGET,
): Memory[] {
    const selected = trimMemoriesToBudgetV2(
        "mural-selection",
        [...memories],
        budgetTokens,
    ).selected;
    const selectedIds = new Set(selected.map((memory) => memory.id));
    return memories.filter((memory) => !selectedIds.has(memory.id));
}

function sourceForPrompt(memories: readonly Memory[]): MuralSourceMemory[] {
    return memories.map((memory) => ({
        id: memory.id,
        category: memory.category,
        importance: memory.importance,
        content: memory.content,
    }));
}

function promptForSource(source: readonly MuralSourceMemory[], feedback?: string): string {
    const lines = source.map(
        (memory) =>
            `#${memory.id} [${memory.category}] importance=${memory.importance}: ${memory.content}`,
    );
    return `${MURAL_AUTHORING_PROMPT}\n\nSOURCE MEMORIES:\n${lines.join("\n")}\n${feedback ? `\nRETRY: fix the exact violating ids ${feedback}. Return the complete corrected manifest.\n` : ""}`;
}

/** Author once, then retry exactly once with the violating ids when validation
 * rejects cue budgets or manifest polarity. */
export async function authorMuralWithRetry(args: {
    source: readonly MuralSourceMemory[];
    call: MuralAuthorCall;
    model?: string;
    fallbackModels?: readonly string[];
}): Promise<MuralManifestEntry[]> {
    let feedback: string | undefined;
    for (let attempt = 0; attempt < 2; attempt++) {
        const raw = await args.call(
            promptForSource(args.source, feedback),
            args.model,
            args.fallbackModels,
        );
        try {
            const entries = parseMuralManifest(raw);
            validateMuralManifest(args.source, entries);
            return entries;
        } catch (error) {
            if (attempt === 1) throw error;
            const message = error instanceof Error ? error.message : String(error);
            const ids = [...message.matchAll(/\b(?:id|ids)\s+([\d,]+)/gi)].flatMap((match) =>
                (match[1] ?? "").split(","),
            );
            feedback = ids.length > 0 ? ids.join(",") : "the reported validation defects";
        }
    }
    throw new Error("mural authoring exhausted retry");
}

export interface RenderMuralArgs {
    db: Database;
    client: PluginContext["client"];
    projectIdentity: string;
    projectDirectory: string;
    holderId: string;
    leaseKey: string;
    deadline: number;
    model?: string;
    fallbackModels?: readonly string[];
    configuredModel?: string;
    memoryBudgetTokens?: number;
}

export async function renderMuralTask(
    args: RenderMuralArgs,
): Promise<{ status: "completed" | "skipped"; reason?: string; renderedAt?: number }> {
    const memories = getMemoriesByProject(args.db, args.projectIdentity, ["active", "permanent"]);
    const overflow = muralOverflowMemories(
        memories,
        args.memoryBudgetTokens ?? DEFAULT_MURAL_MEMORY_BUDGET,
    );
    if (overflow.length < MURAL_MIN_MEMORIES)
        return { status: "skipped", reason: "overflow pool has fewer than 20 memories" };
    const abortController = new AbortController();
    const heartbeat = startLeaseHeartbeat(args.db, args.holderId, args.leaseKey, () =>
        abortController.abort(),
    );
    try {
        const call: MuralAuthorCall = async (prompt, model, fallbackModels) => {
            if (Date.now() >= args.deadline) throw new Error("mural task deadline exceeded");
            const response = await args.client.session.create({
                body: { title: "magic-context-dream-render-mural" },
                query: { directory: args.projectDirectory },
            });
            const created = shared.normalizeSDKResponse(response, null as { id?: string } | null, {
                preferResponseOnMissingData: true,
            });
            const sessionId = created?.id;
            if (!sessionId) throw new Error("mural author session was not created");
            try {
                await shared.promptSyncWithValidatedOutputRetry(
                    args.client,
                    {
                        path: { id: sessionId },
                        query: { directory: args.projectDirectory },
                        body: {
                            ...modelBodyField(model),
                            parts: [{ type: "text", text: prompt, synthetic: true }],
                        },
                    },
                    {
                        timeoutMs: Math.max(
                            1,
                            Math.min(args.deadline - Date.now(), 20 * 60 * 1000),
                        ),
                        signal: abortController.signal,
                        fallbackModels,
                        callContext: "dreamer:render-mural",
                        fetchOutput: async () => {
                            const messages = await args.client.session.messages({
                                path: { id: sessionId },
                                query: { directory: args.projectDirectory, limit: 20 },
                            });
                            return shared.normalizeSDKResponse(messages, [] as unknown[], {
                                preferResponseOnMissingData: true,
                            });
                        },
                        validateOutput: (messages) => {
                            const text = extractLatestAssistantText(messages);
                            if (!text) throw new Error("mural author returned no assistant output");
                            return text;
                        },
                    },
                );
                const messages = await args.client.session.messages({
                    path: { id: sessionId },
                    query: { directory: args.projectDirectory, limit: 20 },
                });
                const text = extractLatestAssistantText(
                    shared.normalizeSDKResponse(messages, [] as unknown[], {
                        preferResponseOnMissingData: true,
                    }),
                );
                if (!text) throw new Error("mural author returned no manifest");
                return text;
            } finally {
                await args.client.session.delete({ path: { id: sessionId } }).catch(() => {});
            }
        };
        const entries = await authorMuralWithRetry({
            source: sourceForPrompt(overflow),
            call,
            model: args.model ?? args.configuredModel,
            fallbackModels: args.fallbackModels,
        });
        if (heartbeat.lost) throw new Error("mural lease lost during authoring");
        const rendered = renderMural(
            entries.map((entry) => ({
                id: entry.id,
                category: entry.category,
                room: entry.room,
                importance: entry.importance,
                ...(entry.cue !== undefined ? { cue: entry.cue } : {}),
                ...(entry.mergeInto !== undefined ? { mergeInto: entry.mergeInto } : {}),
            })),
        );
        const renderedAt = Date.now();
        const contentHash = createHash("sha256").update(rendered.png).digest("hex");
        runLeaseGuardedWrite(args.db, args.holderId, args.leaseKey, () => {
            upsertMural(args.db, {
                projectPath: args.projectIdentity,
                image: Buffer.from(rendered.png),
                contentHash,
                renderedAt,
                model: args.model ?? args.configuredModel ?? null,
                memoryIds: overflow.map((memory) => memory.id),
                width: MURAL_WIDTH,
                height: MURAL_HEIGHT,
            });
        });
        return { status: "completed", renderedAt };
    } finally {
        heartbeat.stop();
    }
}

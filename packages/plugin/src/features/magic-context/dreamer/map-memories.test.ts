/// <reference types="bun-types" />

import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";

import { Database } from "../../../shared/sqlite";
import { closeQuietly } from "../../../shared/sqlite-helpers";
import { insertMemory } from "../memory";
import {
    getMemoryVerifications,
    recordMemoryMapping,
} from "../memory/storage-memory-verifications";
import { runMigrations } from "../migrations";
import { initializeDatabase } from "../storage-db";
import { acquireLease } from "./lease";
import {
    applyBatchMappings,
    computeMapBatchSliceMs,
    MAP_BATCH_FLOOR_MS,
    type MapMemoriesArgs,
    mapMemories,
    selectMapMemoryInputs,
    shouldRequeueIndependentMapping,
} from "./map-memories";

const tempDirs: string[] = [];

function freshDb(): Database {
    const db = new Database(":memory:");
    initializeDatabase(db);
    runMigrations(db);
    return db;
}

function tempProject(): string {
    const dir = mkdtempSync(path.join(tmpdir(), "mc-map-memories-"));
    tempDirs.push(dir);
    mkdirSync(path.join(dir, "src"), { recursive: true });
    writeFileSync(path.join(dir, "src", "fact.ts"), "export const fact = true;", "utf8");
    return dir;
}

function mapArgs(db: Database, sessionDirectory: string, projectIdentity: string): MapMemoriesArgs {
    const holderId = "map-holder";
    const leaseKey = `map-${Math.random()}`;
    expect(acquireLease(db, holderId, leaseKey)).toBe(true);
    return {
        db,
        client: {} as never,
        projectIdentity,
        parentSessionId: undefined,
        sessionDirectory,
        holderId,
        leaseKey,
        // Every direct mapping test that reaches the loop needs enough budget to
        // clear the production floor; deadline-stop fixtures override this value.
        deadline: Date.now() + MAP_BATCH_FLOOR_MS + 60_000,
    };
}

afterEach(() => {
    for (const dir of tempDirs.splice(0)) {
        rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
    }
});

function assistantMessages(text: string) {
    return [
        {
            info: { role: "assistant", time: { created: Date.now() } },
            parts: [{ type: "text", text }],
        },
    ];
}

function scriptedMapClient(manifestFor: (promptCall: number, ids: number[]) => string): {
    client: unknown;
    promptCalls: () => number;
} {
    let promptCalls = 0;
    let lastIds: number[] = [];
    return {
        client: {
            session: {
                create: async () => ({ data: { id: "map-child" } }),
                prompt: async (args: { body?: { parts?: Array<{ text?: string }> } }) => {
                    promptCalls += 1;
                    const prompt = args.body?.parts?.[0]?.text ?? "";
                    lastIds = [...prompt.matchAll(/^\[(\d+)\]/gm)].map((match) => Number(match[1]));
                    return {};
                },
                messages: async () => ({
                    data: assistantMessages(manifestFor(promptCalls, lastIds)),
                }),
                delete: async () => ({}),
            },
        },
        promptCalls: () => promptCalls,
    };
}

function successfulMapClient(onPrompt?: () => void) {
    let manifest = "";
    return {
        session: {
            create: async () => ({ data: { id: "map-child" } }),
            prompt: async (args: { body?: { parts?: Array<{ text?: string }> } }) => {
                const prompt = args.body?.parts?.[0]?.text ?? "";
                const ids = [...prompt.matchAll(/^\[(\d+)\]/gm)].map((match) => Number(match[1]));
                manifest = `<mappings>${ids.map((id) => `<memory id="${id}" independent="true"/>`).join("")}</mappings>`;
                onPrompt?.();
                return {};
            },
            messages: async () => ({ data: assistantMessages(manifest) }),
            delete: async () => ({}),
        },
    };
}

/** The exact timeout-class error from the shared prompt helper. */
function timeoutMapClient(onPrompt?: () => void) {
    return {
        session: {
            create: async () => ({ data: { id: "map-child" } }),
            prompt: async () => {
                onPrompt?.();
                throw new Error("prompt timed out after 99997ms");
            },
            messages: async () => ({ data: [] }),
            delete: async () => ({}),
        },
    };
}

describe("mapMemories disposition", () => {
    test("banks a completed batch and reports the deadline remainder", async () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:map-deadline";
            const dir = tempProject();
            for (let index = 0; index < 81; index += 1) {
                insertMemory(db, {
                    projectPath: projectIdentity,
                    category: "ARCHITECTURE",
                    content: `Independent fact ${index}.`,
                    sourceSessionId: "ses",
                });
            }
            const args = mapArgs(db, dir, projectIdentity);
            args.client = successfulMapClient(() => {
                args.deadline = Date.now() - 1;
            }) as never;

            const result = await mapMemories(args);

            expect(result).toEqual({
                mapped: 0,
                independent: 80,
                batches: 1,
                remaining: 1,
                complete: false,
                stopReason: "deadline",
            });
        } finally {
            closeQuietly(db);
        }
    });

    test("floors a 12-batch default-deadline backfill and banks its first completed batch", async () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:map-floor-primary";
            const dir = tempProject();
            // 881 memories produce 12 batches. The old even split assigned only
            // 100 seconds (1,200,000 / 12) to each batch, below the agentic floor.
            const defaultDeadlineMs = 20 * 60 * 1000;
            expect(Math.floor(defaultDeadlineMs / 12)).toBe(100_000);
            expect(computeMapBatchSliceMs(defaultDeadlineMs, 12)).toBe(MAP_BATCH_FLOOR_MS);
            for (let index = 0; index < 881; index += 1) {
                insertMemory(db, {
                    projectPath: projectIdentity,
                    category: "ARCHITECTURE",
                    content: `Large-backlog mapping fact ${index}.`,
                    sourceSessionId: "ses",
                });
            }
            const args = mapArgs(db, dir, projectIdentity);
            args.deadline = Date.now() + defaultDeadlineMs;
            let promptCalls = 0;
            args.client = successfulMapClient(() => {
                promptCalls += 1;
                // One full floor-sized batch completed; no fair slice remains for
                // batch two, so its untouched inputs must stay banked for resume.
                args.deadline = Date.now() + 60_000;
            }) as never;

            const result = await mapMemories(args);

            expect(promptCalls).toBe(1);
            expect(result).toEqual({
                mapped: 0,
                independent: 80,
                batches: 1,
                remaining: 801,
                complete: false,
                stopReason: "deadline",
            });
            expect(selectMapMemoryInputs(db, projectIdentity, dir)).toHaveLength(801);
        } finally {
            closeQuietly(db);
        }
    });

    test("stops before the next batch when the remaining deadline cannot fit the floor", async () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:map-floor-stop";
            const dir = tempProject();
            for (let index = 0; index < 81; index += 1) {
                insertMemory(db, {
                    projectPath: projectIdentity,
                    category: "ARCHITECTURE",
                    content: `Floor-boundary mapping fact ${index}.`,
                    sourceSessionId: "ses",
                });
            }
            const args = mapArgs(db, dir, projectIdentity);
            let promptCalls = 0;
            args.client = successfulMapClient(() => {
                promptCalls += 1;
                args.deadline = Date.now() + 60_000;
            }) as never;

            const result = await mapMemories(args);

            expect(promptCalls).toBe(1);
            expect(result).toEqual({
                mapped: 0,
                independent: 80,
                batches: 1,
                remaining: 1,
                complete: false,
                stopReason: "deadline",
            });
            expect(selectMapMemoryInputs(db, projectIdentity, dir)).toHaveLength(1);
        } finally {
            closeQuietly(db);
        }
    });

    test("two consecutive batch timeouts trip the starvation circuit breaker", async () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:map-timeout-breaker";
            const dir = tempProject();
            // Three batches prove the third is left unattempted by the two-timeout breaker.
            for (let index = 0; index < 241; index += 1) {
                insertMemory(db, {
                    projectPath: projectIdentity,
                    category: "ARCHITECTURE",
                    content: `Timeout mapping fact ${index}.`,
                    sourceSessionId: "ses",
                });
            }
            const args = mapArgs(db, dir, projectIdentity);
            let promptCalls = 0;
            args.client = timeoutMapClient(() => {
                promptCalls += 1;
            }) as never;

            const result = await mapMemories(args);

            expect(promptCalls).toBe(2);
            expect(result).toEqual({
                mapped: 0,
                independent: 0,
                batches: 0,
                remaining: 241,
                complete: false,
                stopReason: "timeout-circuit-breaker",
            });
        } finally {
            closeQuietly(db);
        }
    });

    test("a later run resumes the durable remainder after a floored partial run", async () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:map-floor-resume";
            const dir = tempProject();
            for (let index = 0; index < 161; index += 1) {
                insertMemory(db, {
                    projectPath: projectIdentity,
                    category: "ARCHITECTURE",
                    content: `Resumable mapping fact ${index}.`,
                    sourceSessionId: "ses",
                });
            }
            const args = mapArgs(db, dir, projectIdentity);
            args.client = successfulMapClient(() => {
                args.deadline = Date.now() + 60_000;
            }) as never;

            const first = await mapMemories(args);
            expect(first).toMatchObject({ independent: 80, batches: 1, remaining: 81 });
            expect(selectMapMemoryInputs(db, projectIdentity, dir)).toHaveLength(81);

            args.deadline = Date.now() + 2 * MAP_BATCH_FLOOR_MS;
            args.client = successfulMapClient() as never;
            const second = await mapMemories(args);

            expect(second).toEqual({
                mapped: 0,
                independent: 81,
                batches: 2,
                remaining: 0,
                complete: true,
            });
            expect(selectMapMemoryInputs(db, projectIdentity, dir)).toEqual([]);
        } finally {
            closeQuietly(db);
        }
    });

    test("reports complete after fully draining the selected set", async () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:map-complete";
            const dir = tempProject();
            insertMemory(db, {
                projectPath: projectIdentity,
                category: "ARCHITECTURE",
                content: "Independent fact.",
                sourceSessionId: "ses",
            });
            const args = mapArgs(db, dir, projectIdentity);
            args.client = successfulMapClient() as never;

            expect(await mapMemories(args)).toEqual({
                mapped: 0,
                independent: 1,
                batches: 1,
                remaining: 0,
                complete: true,
            });
        } finally {
            closeQuietly(db);
        }
    });

    test("reports a swallowed batch failure as incomplete", async () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:map-failure";
            const dir = tempProject();
            insertMemory(db, {
                projectPath: projectIdentity,
                category: "ARCHITECTURE",
                content: "Independent fact.",
                sourceSessionId: "ses",
            });
            const args = mapArgs(db, dir, projectIdentity);
            args.client = {
                session: {
                    create: async () => {
                        throw new Error("provider unavailable");
                    },
                },
            } as never;

            const result = await mapMemories(args);
            expect(result.complete).toBe(false);
            expect(result.remaining).toBe(1);
        } finally {
            closeQuietly(db);
        }
    });
});

describe("mapMemories retry-time validation", () => {
    test("wrong-but-rooted empty parse fires the fallback model", async () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:map-retry-empty";
            const dir = tempProject();
            insertMemory(db, {
                projectPath: projectIdentity,
                category: "ARCHITECTURE",
                content: "Independent fact.",
                sourceSessionId: "ses",
            });
            const args = mapArgs(db, dir, projectIdentity);
            const scripted = scriptedMapClient((call, ids) =>
                call === 1
                    ? `<mappings>\n<map id="1">\nsrc/fact.ts\n</map>\n</mappings>`
                    : `<mappings>${ids.map((id) => `<memory id="${id}" independent="true"/>`).join("")}</mappings>`,
            );
            args.client = scripted.client as never;
            args.fallbackModels = ["anthropic/claude-sonnet-4-6"];

            const result = await mapMemories(args);
            expect(scripted.promptCalls()).toBe(2);
            expect(result).toEqual({
                mapped: 0,
                independent: 1,
                batches: 1,
                remaining: 0,
                complete: true,
            });
        } finally {
            closeQuietly(db);
        }
    });

    test("coverage mismatch fires the fallback model", async () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:map-retry-coverage";
            const dir = tempProject();
            const first = insertMemory(db, {
                projectPath: projectIdentity,
                category: "ARCHITECTURE",
                content: "First fact.",
                sourceSessionId: "ses",
            });
            insertMemory(db, {
                projectPath: projectIdentity,
                category: "ARCHITECTURE",
                content: "Second fact.",
                sourceSessionId: "ses",
            });
            const args = mapArgs(db, dir, projectIdentity);
            const scripted = scriptedMapClient((call, ids) =>
                call === 1
                    ? `<mappings><memory id="${first.id}" independent="true"/></mappings>`
                    : `<mappings>${ids.map((id) => `<memory id="${id}" independent="true"/>`).join("")}</mappings>`,
            );
            args.client = scripted.client as never;
            args.fallbackModels = ["anthropic/claude-sonnet-4-6"];

            const result = await mapMemories(args);
            expect(scripted.promptCalls()).toBe(2);
            expect(result.independent).toBe(2);
            expect(result.complete).toBe(true);
        } finally {
            closeQuietly(db);
        }
    });
});

describe("applyBatchMappings", () => {
    test("complete manifest writes the mapping", async () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:test";
            const dir = tempProject();
            const memory = insertMemory(db, {
                projectPath: projectIdentity,
                category: "ARCHITECTURE",
                content: "Fact lives in src/fact.ts.",
                sourceSessionId: "ses",
            });

            const result = await applyBatchMappings(
                mapArgs(db, dir, projectIdentity),
                [
                    {
                        id: memory.id,
                        category: memory.category,
                        content: memory.content,
                        candidates: [],
                    },
                ],
                `<mappings><memory id="${memory.id}" files="src/fact.ts"/></mappings>`,
            );

            expect(result).toEqual({ mapped: 1, independent: 0 });
            expect(getMemoryVerifications(db, [memory.id]).get(memory.id)?.files).toEqual([
                "src/fact.ts",
            ]);
        } finally {
            closeQuietly(db);
        }
    });

    test("truncated manifest rejects before replacing an existing mapping", async () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:test";
            const dir = tempProject();
            const memory = insertMemory(db, {
                projectPath: projectIdentity,
                category: "ARCHITECTURE",
                content: "Fact lives in src/fact.ts.",
                sourceSessionId: "ses",
            });
            recordMemoryMapping(db, memory.id, [], 1_000);

            await expect(
                applyBatchMappings(
                    mapArgs(db, dir, projectIdentity),
                    [
                        {
                            id: memory.id,
                            category: memory.category,
                            content: memory.content,
                            candidates: [],
                        },
                    ],
                    `<mappings><memory id="${memory.id}" files="src/fact.ts"/>`,
                ),
            ).rejects.toThrow(/closing root/);

            const state = getMemoryVerifications(db, [memory.id]).get(memory.id);
            expect(state?.files).toEqual([]);
            expect(state?.hasSentinel).toBe(true);
            expect(state?.mappedAt).toBe(1_000);
        } finally {
            closeQuietly(db);
        }
    });

    test("apply-time coverage belt still rejects missing and extra ids", async () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:test-belt";
            const dir = tempProject();
            const memory = insertMemory(db, {
                projectPath: projectIdentity,
                category: "ARCHITECTURE",
                content: "Fact lives in src/fact.ts.",
                sourceSessionId: "ses",
            });
            const extra = insertMemory(db, {
                projectPath: projectIdentity,
                category: "ARCHITECTURE",
                content: "Another fact.",
                sourceSessionId: "ses",
            });
            const batch = [
                {
                    id: memory.id,
                    category: memory.category,
                    content: memory.content,
                    candidates: [] as string[],
                },
            ];
            const args = mapArgs(db, dir, projectIdentity);

            await expect(
                applyBatchMappings(
                    args,
                    batch,
                    `<mappings><memory id="${memory.id}" files="src/fact.ts"/><memory id="${extra.id}" independent="true"/></mappings>`,
                ),
            ).rejects.toThrow(/unknown id/);
            await expect(applyBatchMappings(args, batch, `<mappings></mappings>`)).rejects.toThrow(
                /missing id|parsed zero entries/,
            );
        } finally {
            closeQuietly(db);
        }
    });

    test("nested file children persist as a real mapping, not independent", async () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:test-nested";
            const dir = tempProject();
            const memory = insertMemory(db, {
                projectPath: projectIdentity,
                category: "ARCHITECTURE",
                content: "Fact lives in src/fact.ts.",
                sourceSessionId: "ses",
            });

            const result = await applyBatchMappings(
                mapArgs(db, dir, projectIdentity),
                [
                    {
                        id: memory.id,
                        category: memory.category,
                        content: memory.content,
                        candidates: [],
                    },
                ],
                `<mappings><memory id="${memory.id}"><file path="src/fact.ts"/></memory></mappings>`,
            );

            expect(result).toEqual({ mapped: 1, independent: 0 });
            const state = getMemoryVerifications(db, [memory.id]).get(memory.id);
            expect(state?.files).toEqual(["src/fact.ts"]);
            expect(state?.hasSentinel).toBe(false);
        } finally {
            closeQuietly(db);
        }
    });
});

describe("independent re-queue heal", () => {
    test("predicate selects a path-seeded independent and skips a conceptual bystander", () => {
        const dir = tempProject();
        expect(
            shouldRequeueIndependentMapping(
                { hasSentinel: true, files: [] },
                "Fact lives in src/fact.ts.",
                dir,
            ),
        ).toBe(true);
        expect(
            shouldRequeueIndependentMapping(
                { hasSentinel: true, files: [] },
                "Anthropic returns 400 on empty content.",
                dir,
            ),
        ).toBe(false);
        expect(
            shouldRequeueIndependentMapping(
                { hasSentinel: false, files: ["src/fact.ts"] },
                "Fact lives in src/fact.ts.",
                dir,
            ),
        ).toBe(false);
    });

    test("selectMapMemoryInputs re-queues the corrupted row and leaves the bystander mapped", () => {
        const db = freshDb();
        try {
            const projectIdentity = "git:map-requeue";
            const dir = tempProject();
            const corrupted = insertMemory(db, {
                projectPath: projectIdentity,
                category: "ARCHITECTURE",
                content: "Fact lives in src/fact.ts.",
                sourceSessionId: "ses",
            });
            const bystander = insertMemory(db, {
                projectPath: projectIdentity,
                category: "CONSTRAINTS",
                content: "Anthropic returns 400 on empty content.",
                sourceSessionId: "ses",
            });
            recordMemoryMapping(db, corrupted.id, [], 1_000);
            recordMemoryMapping(db, bystander.id, [], 1_000);

            const selected = selectMapMemoryInputs(db, projectIdentity, dir);
            expect(selected.map((row) => row.id)).toEqual([corrupted.id]);
            expect(selected[0]?.candidates).toEqual(["src/fact.ts"]);

            const bystanderState = getMemoryVerifications(db, [bystander.id]).get(bystander.id);
            expect(bystanderState?.hasSentinel).toBe(true);
            expect(bystanderState?.files).toEqual([]);
        } finally {
            closeQuietly(db);
        }
    });
});

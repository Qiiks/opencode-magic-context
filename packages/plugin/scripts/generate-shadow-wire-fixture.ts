import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { appendCompartments } from "../src/features/magic-context/compartment-storage";
import { insertMemory } from "../src/features/magic-context/memory/storage-memory";
import {
    closeDatabase,
    getOrCreateSessionMeta,
    openDatabase,
} from "../src/features/magic-context/storage";
import { queueM0Mutation } from "../src/features/magic-context/storage-m0-mutation-log";
import { queueMemoryMutation } from "../src/features/magic-context/storage-memory-mutation-log";
import { Database } from "../src/shared/sqlite";
import { closeQuietly } from "../src/shared/sqlite-helpers";
import { __shadowSenderTest, type ShadowTransformPass } from "../src/hooks/magic-context/shadow-sender";
import type { MessageLike } from "../src/hooks/magic-context/tag-messages";

const FIXED_NOW = 1_735_689_600_000;
const SESSION_ID = "shadow-wire-fixture-session";
const PROJECT_PATH = "/fixture/project";
export const SHADOW_WIRE_FIXTURE_PATH = resolve(
    import.meta.dir,
    "../../../crates/mc-module/testdata/shadow-wire-fixture.json",
);

function createOpenCodeFixtureDb(dataHome: string): void {
    const dbPath = join(dataHome, "opencode", "opencode.db");
    mkdirSync(dirname(dbPath), { recursive: true });
    const db = new Database(dbPath);
    try {
        db.exec(`
            CREATE TABLE message (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
            CREATE TABLE part (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                message_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                time_created INTEGER NOT NULL,
                time_updated INTEGER NOT NULL,
                data TEXT NOT NULL
            );
        `);
        const insertMessage = db.prepare(
            "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?)",
        );
        const insertPart = db.prepare(
            "INSERT INTO part (message_id, session_id, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?)",
        );
        const rows = [
            {
                id: "message-1",
                role: "user",
                parts: [{ type: "text", text: "fixture input" }],
            },
            {
                id: "message-2",
                role: "assistant",
                parts: [
                    { type: "text", text: "fixture output" },
                    {
                        type: "tool",
                        callID: "fixture-call",
                        state: { status: "completed", output: "fixture tool output" },
                    },
                ],
            },
        ];
        rows.forEach((row, messageIndex) => {
            const timestamp = new Date(2025, 0, messageIndex + 2, 12).getTime();
            insertMessage.run(
                row.id,
                SESSION_ID,
                timestamp,
                timestamp,
                JSON.stringify({
                    id: row.id,
                    role: row.role,
                    sessionID: SESSION_ID,
                    finish: row.role === "assistant" ? "stop" : undefined,
                }),
            );
            row.parts.forEach((part, partIndex) => {
                insertPart.run(
                    row.id,
                    SESSION_ID,
                    timestamp * 10 + partIndex,
                    timestamp * 10 + partIndex,
                    JSON.stringify(part),
                );
            });
        });
    } finally {
        closeQuietly(db);
    }
}

function fixtureMessages(): MessageLike[] {
    return [
        {
            info: { id: "message-1", role: "user", sessionID: SESSION_ID },
            parts: [{ type: "text", text: "fixture input" }],
        },
        {
            info: { id: "message-2", role: "assistant", sessionID: SESSION_ID },
            parts: [
                { type: "text", text: "fixture output" },
                {
                    type: "tool",
                    callID: "fixture-call",
                    state: { status: "completed", output: "fixture tool output" },
                },
            ],
        },
    ];
}

export async function generateShadowWireFixture(): Promise<string> {
    const tempRoot = mkdtempSync(join(tmpdir(), "shadow-wire-fixture-"));
    const originalDataHome = process.env.XDG_DATA_HOME;
    const originalCacheHome = process.env.XDG_CACHE_HOME;
    const originalNow = Date.now;
    process.env.XDG_DATA_HOME = tempRoot;
    process.env.XDG_CACHE_HOME = tempRoot;
    Date.now = () => FIXED_NOW;

    try {
        closeDatabase();
        createOpenCodeFixtureDb(tempRoot);
        const db = openDatabase();
        if (!db) throw new Error("fixture database did not open");
        db.prepare(
            `INSERT INTO workspaces (name, created_at, updated_at, share_categories)
             VALUES (?, ?, ?, ?)`,
        ).run("Fixture workspace", FIXED_NOW, FIXED_NOW, '["CONSTRAINTS"]');
        const workspaceId = Number(
            (db.prepare("SELECT id FROM workspaces WHERE name = ?").get("Fixture workspace") as {
                id: number;
            }).id,
        );
        db.prepare(
            `INSERT INTO workspace_members
                (workspace_id, project_path, display_name, display_path, added_at)
             VALUES (?, ?, ?, ?, ?)`,
        ).run(workspaceId, PROJECT_PATH, "fixture-project", PROJECT_PATH, FIXED_NOW);
        db.prepare(
            `INSERT INTO workspace_members
                (workspace_id, project_path, display_name, display_path, added_at)
             VALUES (?, ?, ?, ?, ?)`,
        ).run(
            workspaceId,
            "/fixture/foreign",
            "fixture-foreign",
            "/fixture/foreign",
            FIXED_NOW,
        );

        appendCompartments(db, SESSION_ID, [
            {
                sequence: 1,
                startMessage: 1,
                endMessage: 1,
                startMessageId: "message-1",
                endMessageId: "message-1",
                title: "Fixture compartment",
                content: "Fixture full compartment content",
                p1: "Fixture P1",
                p2: "Fixture P2",
                p3: "Fixture P3",
                p4: "Fixture P4",
                importance: 87,
                episodeType: "feature,verification",
            },
            {
                sequence: 2,
                startMessage: 2,
                endMessage: 2,
                startMessageId: "message-2",
                endMessageId: "message-2",
                title: "Fixture tail compartment",
                content: "Fixture tail compartment content",
                p1: "Fixture tail P1",
                p2: "Fixture tail P2",
                p3: "Fixture tail P3",
                p4: "Fixture tail P4",
                importance: 66,
                episodeType: "verification",
            },
        ]);

        const memory = insertMemory(db, {
            projectPath: PROJECT_PATH,
            category: "ARCHITECTURE",
            content: "Fixture workspace memory",
            importance: 91,
            sourceSessionId: SESSION_ID,
            sourceType: "historian",
            expiresAt: FIXED_NOW + 86_400_000,
            metadataJson: '{"fixture":true}',
        });
        db.prepare(
            `UPDATE memories
                SET scope = 'ecosystem', shareable = 1, seen_count = 3,
                    retrieval_count = 4, first_seen_at = ?, created_at = ?, updated_at = ?,
                    last_seen_at = ?, last_retrieved_at = ?, status = 'permanent',
                    verification_status = 'verified', verified_at = ?, classified_at = ?,
                    superseded_by_memory_id = ?, merged_from = ?
              WHERE id = ?`,
        ).run(
            FIXED_NOW - 8_000,
            FIXED_NOW - 7_000,
            FIXED_NOW - 6_000,
            FIXED_NOW - 5_000,
            FIXED_NOW - 4_000,
            FIXED_NOW - 3_000,
            FIXED_NOW - 2_000,
            99,
            "[7,8]",
            memory.id,
        );
        const foreignMemory = insertMemory(db, {
            projectPath: "/fixture/foreign",
            category: "CONSTRAINTS",
            content: "Fixture foreign workspace constraint",
            importance: 84,
            sourceSessionId: "fixture-foreign-session",
            sourceType: "dreamer",
            expiresAt: FIXED_NOW + 172_800_000,
            metadataJson: '{"foreign":true}',
        });
        db.prepare(
            `UPDATE memories
                SET scope = 'project', shareable = 1, seen_count = 2,
                    retrieval_count = 1, first_seen_at = ?, created_at = ?, updated_at = ?,
                    last_seen_at = ?, last_retrieved_at = ?, status = 'active',
                    verification_status = 'verified', verified_at = ?, classified_at = ?,
                    superseded_by_memory_id = ?, merged_from = ?
              WHERE id = ?`,
        ).run(
            FIXED_NOW - 18_000,
            FIXED_NOW - 17_000,
            FIXED_NOW - 16_000,
            FIXED_NOW - 15_000,
            FIXED_NOW - 14_000,
            FIXED_NOW - 13_000,
            FIXED_NOW - 12_000,
            101,
            "[9,10]",
            foreignMemory.id,
        );
        queueMemoryMutation(db, {
            projectPath: PROJECT_PATH,
            mutationType: "superseded",
            targetMemoryId: memory.id,
            supersededById: 99,
            category: "ARCHITECTURE",
            newContent: "Fixture superseding content",
            queuedAt: FIXED_NOW - 1_000,
        });
        queueM0Mutation(db, {
            sessionId: SESSION_ID,
            mutationType: "recomp_boundary_change",
            targetId: 1,
            queuedAt: FIXED_NOW - 500,
        });
        getOrCreateSessionMeta(db, SESSION_ID);
        db.prepare("UPDATE session_meta SET last_todo_state = ? WHERE session_id = ?").run(
            '[{"content":"fixture todo","status":"pending"}]',
            SESSION_ID,
        );

        const state = __shadowSenderTest.createSessionQueueState();
        state.shadowGeneration = 7;
        state.seedPassPending = true;
        state.lastAckedSeq = 11;
        state.lastAckedWatermarks = {
            compartment_sequence: -1,
            memory_id: 0,
            m0_mutation_id: 0,
            memory_mutation_id: 0,
            last_todo_state_hash: "",
        };
        const messages = fixtureMessages();
        const inputMessages = structuredClone(messages);
        const declaredTrim = {
            flat_boundary_id: "message-2#2",
            boundary_bare_message_id: "message-2",
            boundary_absolute_ordinal: 2,
            next_absolute_ordinal: 3,
        };
        const pass: ShadowTransformPass = {
            sessionId: SESSION_ID,
            isSubagent: false,
            db,
            projectRoot: "/fixture/root",
            projectPath: PROJECT_PATH,
            inputMessages,
            outputMessages: messages,
            normalizationTargets: [],
            passInputs: {
                now_ms: FIXED_NOW,
                model_key: "fixture/provider-model",
                usage: { input_tokens: 12_345, limit: 200_000 },
                effective_execute_threshold: 65,
                history_budget_tokens: 19_500,
                cache_ttl: "ephemeral",
                provider_error: "fixture provider warning",
            },
            tsDecision: {
                class: "hard",
                marker_state: {
                    marker_message_id: "message-2",
                    advanced_this_pass: true,
                },
                materialize_reason: "fixture",
                emergency: false,
            },
            declaredTrimBefore: declaredTrim,
        };
        const resolved = await __shadowSenderTest.resolveOrdinalsForShadow({
            sessionId: SESSION_ID,
            messages: inputMessages,
            generation: state.shadowGeneration,
            memoGeneration: state.idOrdinalMemoGeneration,
            memo: state.idOrdinalMemo,
        });
        if (!resolved.ok) throw new Error(`fixture ordinal resolution failed: ${resolved.reason}`);
        const preparedPass = {
            ...pass,
            annotatedInput: resolved.annotatedInput,
            declaredTrim,
        };
        const sync = await __shadowSenderTest.buildStateSyncPayload({
            state,
            pass: preparedPass,
            force: true,
        });
        if (
            sync === null ||
            sync === "m0_mutation" ||
            sync === "mismatch" ||
            sync === "unresolved" ||
            sync === "seed_budget"
        ) {
            throw new Error(`fixture state sync failed: ${String(sync)}`);
        }

        const fixture = {
            state_sync: __shadowSenderTest.toFlatWireBody(sync),
            shadow_transform: __shadowSenderTest.toFlatWireBody(
                __shadowSenderTest.buildShadowTransformBody({ state, pass: preparedPass }),
            ),
            shadow_reset: __shadowSenderTest.toFlatWireBody(
                __shadowSenderTest.buildShadowResetBody({ state, reason: "fixture_reset" }),
            ),
            local_watermarks: sync.watermarks,
        };
        return `${JSON.stringify(fixture, null, 2)}\n`;
    } finally {
        closeDatabase();
        Date.now = originalNow;
        if (originalDataHome === undefined) delete process.env.XDG_DATA_HOME;
        else process.env.XDG_DATA_HOME = originalDataHome;
        if (originalCacheHome === undefined) delete process.env.XDG_CACHE_HOME;
        else process.env.XDG_CACHE_HOME = originalCacheHome;
        rmSync(tempRoot, { recursive: true, force: true });
    }
}

export async function writeShadowWireFixture(): Promise<void> {
    const bytes = await generateShadowWireFixture();
    mkdirSync(dirname(SHADOW_WIRE_FIXTURE_PATH), { recursive: true });
    writeFileSync(SHADOW_WIRE_FIXTURE_PATH, bytes);
}

if (import.meta.main) await writeShadowWireFixture();

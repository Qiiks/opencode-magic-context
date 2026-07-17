/// <reference types="bun-types" />

import { afterEach, describe, expect, it } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { Database } from "../src/shared/sqlite";
import { closeQuietly } from "../src/shared/sqlite-helpers";
import { cloneSession } from "./clone-session";

const temporaryDirectories: string[] = [];

function makeFixture(): { opencodePath: string; contextPath: string; sourceSessionId: string } {
    const root = mkdtempSync(join(tmpdir(), "clone-session-fixture-"));
    temporaryDirectories.push(root);
    const opencodePath = join(root, "opencode.db");
    const contextPath = join(root, "context.db");
    const sourceSessionId = "ses_source";

    const opencode = new Database(opencodePath);
    opencode.exec(`
        PRAGMA foreign_keys = ON;
        CREATE TABLE project (id TEXT PRIMARY KEY, worktree TEXT NOT NULL);
        CREATE TABLE session (
            id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL REFERENCES project(id),
            parent_id TEXT,
            slug TEXT NOT NULL,
            directory TEXT NOT NULL,
            title TEXT NOT NULL,
            version TEXT NOT NULL,
            share_url TEXT,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            time_compacting INTEGER,
            time_archived INTEGER,
            metadata TEXT
        );
        CREATE TABLE message (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            data TEXT NOT NULL
        );
        CREATE TABLE part (
            id TEXT PRIMARY KEY,
            message_id TEXT NOT NULL REFERENCES message(id) ON DELETE CASCADE,
            session_id TEXT NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            data TEXT NOT NULL
        );
        CREATE TABLE todo (
            session_id TEXT NOT NULL,
            content TEXT NOT NULL,
            status TEXT NOT NULL,
            priority TEXT NOT NULL,
            position INTEGER NOT NULL,
            time_created INTEGER NOT NULL,
            time_updated INTEGER NOT NULL,
            PRIMARY KEY (session_id, position)
        );
        CREATE TABLE event_sequence (aggregate_id TEXT PRIMARY KEY, seq INTEGER NOT NULL, owner_id TEXT);
        CREATE TABLE event (
            id TEXT PRIMARY KEY,
            aggregate_id TEXT NOT NULL REFERENCES event_sequence(aggregate_id) ON DELETE CASCADE,
            seq INTEGER NOT NULL,
            type TEXT NOT NULL,
            data TEXT NOT NULL
        );
    `);
    opencode.prepare("INSERT INTO project (id, worktree) VALUES (?, ?)").run("project_1", "/tmp/drive-project");
    opencode
        .prepare(
            "INSERT INTO session (id, project_id, parent_id, slug, directory, title, version, time_created, time_updated, metadata) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .run(
            sourceSessionId,
            "project_1",
            null,
            "source-slug",
            "/tmp/drive-project",
            "Synthetic source",
            "1.2.3",
            10,
            20,
            JSON.stringify({ sourceSessionId }),
        );
    const insertMessage = opencode.prepare(
        "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?)",
    );
    insertMessage.run(
        "msg_source_1",
        sourceSessionId,
        100,
        100,
        JSON.stringify({ role: "user", id: "msg_source_1" }),
    );
    insertMessage.run(
        "msg_source_2",
        sourceSessionId,
        200,
        200,
        JSON.stringify({ role: "assistant", parentID: "msg_source_1" }),
    );
    opencode
        .prepare(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .run(
            "prt_source_1",
            "msg_source_1",
            sourceSessionId,
            101,
            101,
            JSON.stringify({ type: "text", text: "hello" }),
        );
    opencode
        .prepare(
            "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?, ?)",
        )
        .run(
            "prt_source_2",
            "msg_source_2",
            sourceSessionId,
            201,
            201,
            JSON.stringify({ type: "tool", callID: "tool_source_1" }),
        );
    opencode
        .prepare(
            "INSERT INTO todo (session_id, content, status, priority, position, time_created, time_updated) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .run(sourceSessionId, "drive todo", "pending", "high", 0, 10, 10);
    opencode
        .prepare("INSERT INTO event_sequence (aggregate_id, seq, owner_id) VALUES (?, ?, ?)")
        .run(sourceSessionId, 1, "");
    opencode
        .prepare("INSERT INTO event (id, aggregate_id, seq, type, data) VALUES (?, ?, ?, ?, ?)")
        .run(
            "evt_source_1",
            sourceSessionId,
            0,
            "message.updated.1",
            JSON.stringify({ sessionID: sourceSessionId, info: { id: "msg_source_1" } }),
        );
    closeQuietly(opencode);

    const context = new Database(contextPath);
    context.exec(`
        PRAGMA foreign_keys = ON;
        CREATE TABLE compartments (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            sequence INTEGER NOT NULL,
            start_message INTEGER NOT NULL,
            end_message INTEGER NOT NULL,
            title TEXT NOT NULL,
            content TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            start_message_id TEXT DEFAULT '',
            end_message_id TEXT DEFAULT '',
            harness TEXT NOT NULL DEFAULT 'opencode',
            importance INTEGER NOT NULL DEFAULT 50,
            episode_type TEXT,
            p1 TEXT,
            p2 TEXT,
            p3 TEXT,
            p4 TEXT,
            legacy INTEGER NOT NULL DEFAULT 0,
            UNIQUE(session_id, sequence)
        );
        CREATE TABLE tags (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT,
            message_id TEXT,
            type TEXT,
            status TEXT DEFAULT 'active',
            byte_size INTEGER,
            tag_number INTEGER,
            reasoning_byte_size INTEGER DEFAULT 0,
            drop_mode TEXT DEFAULT 'full',
            tool_name TEXT,
            input_byte_size INTEGER DEFAULT 0,
            caveman_depth INTEGER DEFAULT 0,
            harness TEXT NOT NULL DEFAULT 'opencode',
            tool_owner_message_id TEXT DEFAULT NULL,
            entry_fingerprint TEXT,
            token_count INTEGER,
            input_token_count INTEGER,
            reasoning_token_count INTEGER,
            UNIQUE(session_id, tag_number)
        );
        CREATE TABLE source_contents (tag_id INTEGER, session_id TEXT, content TEXT, created_at INTEGER, harness TEXT NOT NULL DEFAULT 'opencode', PRIMARY KEY(session_id, tag_id));
        CREATE TABLE pending_ops (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT, tag_id INTEGER, operation TEXT, queued_at INTEGER, harness TEXT NOT NULL DEFAULT 'opencode');
        CREATE TABLE session_meta (
            session_id TEXT PRIMARY KEY,
            harness TEXT NOT NULL DEFAULT 'opencode',
            counter INTEGER DEFAULT 0,
            cleared_reasoning_through_tag INTEGER DEFAULT 0,
            tool_reclaim_watermark INTEGER DEFAULT 0,
            pi_stable_id_scheme INTEGER,
            stripped_placeholder_ids TEXT,
            stale_reduce_stripped_ids TEXT,
            processed_image_stripped_ids TEXT,
            pending_pi_compaction_marker_state TEXT,
            last_todo_state TEXT,
            todo_synthetic_call_id TEXT,
            todo_synthetic_anchor_message_id TEXT,
            todo_synthetic_state_json TEXT,
            compaction_marker_state TEXT,
            channel2_nudge_state TEXT DEFAULT '',
            channel2_nudge_claimed_at INTEGER DEFAULT 0,
            channel2_nudge_claim_token TEXT DEFAULT '',
            emergency_drain_active INTEGER DEFAULT 0,
            cached_m0_bytes BLOB,
            cached_m1_bytes BLOB,
            nudge_anchor_message_id TEXT,
            prior_boundary_ordinal INTEGER DEFAULT 1
        );
        CREATE TABLE session_projects (session_id TEXT NOT NULL, harness TEXT NOT NULL, project_path TEXT NOT NULL, updated_at INTEGER NOT NULL, PRIMARY KEY(session_id, harness));
        CREATE TABLE compression_depth (session_id TEXT NOT NULL, message_ordinal INTEGER NOT NULL, depth INTEGER NOT NULL, harness TEXT NOT NULL, PRIMARY KEY(session_id, message_ordinal));
        CREATE TABLE session_facts (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, category TEXT NOT NULL, content TEXT NOT NULL, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL, harness TEXT NOT NULL);
        CREATE TABLE notes (id INTEGER PRIMARY KEY AUTOINCREMENT, type TEXT NOT NULL, status TEXT NOT NULL, content TEXT NOT NULL, session_id TEXT, project_path TEXT, created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
        CREATE TABLE transform_decisions (session_id TEXT NOT NULL, harness TEXT NOT NULL, message_id TEXT NOT NULL, ts_ms INTEGER NOT NULL, decision TEXT NOT NULL, PRIMARY KEY(session_id, harness, message_id));
    `);
    context
        .prepare(
            "INSERT INTO compartments (session_id, sequence, start_message, end_message, title, content, created_at, start_message_id, end_message_id, harness) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .run(sourceSessionId, 0, 1, 2, "one", "summary", 10, "msg_source_1", "msg_source_2", "opencode");
    const insertFixtureTag = context.prepare(
        "INSERT INTO tags (session_id, message_id, type, status, byte_size, tag_number, harness, tool_owner_message_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    );
    // A prior session makes the source tag's global id differ from its per-session tag number.
    insertFixtureTag.run("ses_other", "msg_other:p0", "message", "active", 1, 99, "opencode", null);
    const sourceTag = insertFixtureTag.run(
        sourceSessionId,
        "msg_source_1:p0",
        "message",
        "active",
        5,
        1,
        "opencode",
        null,
    );
    const toolTag = context
        .prepare(
            "INSERT INTO tags (session_id, message_id, type, status, byte_size, tag_number, harness, tool_owner_message_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .run(sourceSessionId, "tool_source_1", "tool", "active", 5, 2, "opencode", "msg_source_2");
    context
        .prepare("INSERT INTO source_contents (tag_id, session_id, content, created_at, harness) VALUES (?, ?, ?, ?, ?)")
        .run(Number(sourceTag.lastInsertRowid), sourceSessionId, "source text", 10, "opencode");
    context
        .prepare("INSERT INTO pending_ops (session_id, tag_id, operation, queued_at, harness) VALUES (?, ?, ?, ?, ?)")
        .run(sourceSessionId, Number(toolTag.lastInsertRowid), "drop", 11, "opencode");
    context
        .prepare(
            "INSERT INTO session_meta (session_id, harness, counter, cleared_reasoning_through_tag, tool_reclaim_watermark, stripped_placeholder_ids, compaction_marker_state, channel2_nudge_state, emergency_drain_active, cached_m0_bytes, cached_m1_bytes, nudge_anchor_message_id) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .run(
            sourceSessionId,
            "opencode",
            2,
            1,
            2,
            JSON.stringify(["msg_source_1"]),
            JSON.stringify({ boundaryMessageId: "msg_source_1", compactionPartId: "prt_source_1" }),
            "claimed",
            1,
            Buffer.from("warm"),
            Buffer.from("warm"),
            "msg_source_2",
        );
    context
        .prepare("INSERT INTO session_projects (session_id, harness, project_path, updated_at) VALUES (?, ?, ?, ?)")
        .run(sourceSessionId, "opencode", "/tmp/drive-project", 10);
    context
        .prepare("INSERT INTO compression_depth (session_id, message_ordinal, depth, harness) VALUES (?, ?, ?, ?)")
        .run(sourceSessionId, 1, 2, "opencode");
    context
        .prepare("INSERT INTO session_facts (session_id, category, content, created_at, updated_at, harness) VALUES (?, ?, ?, ?, ?, ?)")
        .run(sourceSessionId, "fact", "durable", 10, 10, "opencode");
    context
        .prepare("INSERT INTO notes (type, status, content, session_id, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)")
        .run("session", "active", "note", sourceSessionId, 10, 10);
    context
        .prepare("INSERT INTO transform_decisions (session_id, harness, message_id, ts_ms, decision) VALUES (?, ?, ?, ?, ?)")
        .run(sourceSessionId, "opencode", "msg_source_2", 10, "defer");
    closeQuietly(context);

    return { opencodePath, contextPath, sourceSessionId };
}

afterEach(() => {
    for (const directory of temporaryDirectories.splice(0)) {
        rmSync(directory, { recursive: true, force: true });
    }
});

describe("clone-session", () => {
    it("clones OpenCode and Magic Context state with independent remapped ids", () => {
        const fixture = makeFixture();
        const result = cloneSession({
            sessionId: fixture.sourceSessionId,
            opencodeDbPath: fixture.opencodePath,
            contextDbPath: fixture.contextPath,
            suffix: "test",
        });
        const destinationSessionId = result.plan.destinationSessionId;
        expect(result.dryRun).toBe(false);

        const opencode = new Database(fixture.opencodePath, { readonly: true });
        const sourceMessage = opencode
            .prepare("SELECT id, data FROM message WHERE session_id = ? AND id = ?")
            .get(fixture.sourceSessionId, "msg_source_1") as { id: string; data: string };
        const clonedMessages = opencode
            .prepare("SELECT id, data FROM message WHERE session_id = ? ORDER BY time_created")
            .all(destinationSessionId) as Array<{ id: string; data: string }>;
        expect(clonedMessages).toHaveLength(2);
        expect(clonedMessages[0].id).not.toBe("msg_source_1");
        expect(JSON.parse(clonedMessages[1].data).parentID).toBe(clonedMessages[0].id);
        expect(JSON.parse(sourceMessage.data).id).toBe("msg_source_1");
        const clonedParts = opencode
            .prepare("SELECT id FROM part WHERE session_id = ? ORDER BY time_created")
            .all(destinationSessionId) as Array<{ id: string }>;
        expect(clonedParts.map((part) => part.id)).not.toEqual(["prt_source_1", "prt_source_2"]);
        const clonedEvent = opencode
            .prepare("SELECT id, data FROM event WHERE aggregate_id = ?")
            .get(destinationSessionId) as { id: string; data: string };
        expect(clonedEvent.id).not.toBe("evt_source_1");
        expect(JSON.parse(clonedEvent.data).sessionID).toBe(destinationSessionId);
        expect(JSON.parse(clonedEvent.data).info.id).toBe(clonedMessages[0].id);
        opencode.close();

        const context = new Database(fixture.contextPath, { readonly: true });
        const clonedTags = context
            .prepare("SELECT id, message_id, type, tool_owner_message_id FROM tags WHERE session_id = ? ORDER BY tag_number")
            .all(destinationSessionId) as Array<{
            id: number;
            message_id: string;
            type: string;
            tool_owner_message_id: string | null;
        }>;
        expect(clonedTags).toHaveLength(2);
        expect(clonedTags[0].message_id).toBe(`${clonedMessages[0].id}:p0`);
        expect(clonedTags[1].tool_owner_message_id).toBe(clonedMessages[1].id);
        const clonedContent = context
            .prepare("SELECT tag_id FROM source_contents WHERE session_id = ?")
            .get(destinationSessionId) as { tag_id: number };
        expect(clonedContent.tag_id).toBe(clonedTags[0].id);
        const clonedPending = context
            .prepare("SELECT tag_id FROM pending_ops WHERE session_id = ?")
            .get(destinationSessionId) as { tag_id: number };
        expect(clonedPending.tag_id).toBe(clonedTags[1].id);
        const clonedCompartment = context
            .prepare("SELECT start_message_id, end_message_id, start_message, end_message FROM compartments WHERE session_id = ?")
            .get(destinationSessionId) as {
            start_message_id: string;
            end_message_id: string;
            start_message: number;
            end_message: number;
        };
        expect(clonedCompartment.start_message_id).toBe(clonedMessages[0].id);
        expect(clonedCompartment.end_message_id).toBe(clonedMessages[1].id);
        expect([clonedCompartment.start_message, clonedCompartment.end_message]).toEqual([1, 2]);
        const meta = context
            .prepare("SELECT compaction_marker_state, channel2_nudge_state, emergency_drain_active, cached_m0_bytes, cached_m1_bytes FROM session_meta WHERE session_id = ?")
            .get(destinationSessionId) as {
            compaction_marker_state: string;
            channel2_nudge_state: string;
            emergency_drain_active: number;
            cached_m0_bytes: Buffer | null;
            cached_m1_bytes: Buffer | null;
        };
        expect(JSON.parse(meta.compaction_marker_state).boundaryMessageId).toBe(clonedMessages[0].id);
        expect(JSON.parse(meta.compaction_marker_state).compactionPartId).toBe(clonedParts[0].id);
        expect(meta.channel2_nudge_state).toBe("");
        expect(meta.emergency_drain_active).toBe(0);
        expect(meta.cached_m0_bytes).toBeNull();
        expect(meta.cached_m1_bytes).toBeNull();
        expect(
            (context.prepare("SELECT COUNT(*) AS count FROM session_projects WHERE session_id = ?").get(destinationSessionId) as { count: number }).count,
        ).toBe(1);
        context.close();

        const mutable = new Database(fixture.opencodePath);
        mutable.prepare("UPDATE message SET data = ? WHERE session_id = ? AND id = ?").run("changed", destinationSessionId, clonedMessages[0].id);
        expect(
            (mutable.prepare("SELECT data FROM message WHERE id = ?").get("msg_source_1") as { data: string }).data,
        ).toContain("msg_source_1");
        mutable.close();
    });

    it("plans without writing either database", () => {
        const fixture = makeFixture();
        const before = new Database(fixture.opencodePath, { readonly: true });
        const sourceCount = (before.prepare("SELECT COUNT(*) AS count FROM session").get() as { count: number }).count;
        before.close();
        const result = cloneSession({
            sessionId: fixture.sourceSessionId,
            opencodeDbPath: fixture.opencodePath,
            contextDbPath: fixture.contextPath,
            dryRun: true,
        });
        expect(result.dryRun).toBe(true);
        const after = new Database(fixture.opencodePath, { readonly: true });
        expect((after.prepare("SELECT COUNT(*) AS count FROM session").get() as { count: number }).count).toBe(sourceCount);
        expect((after.prepare("SELECT COUNT(*) AS count FROM message WHERE session_id = ?").get(fixture.sourceSessionId) as { count: number }).count).toBe(2);
        after.close();
    });
});

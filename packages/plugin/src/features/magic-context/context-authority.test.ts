import { describe, expect, test } from "bun:test";
import { Database, withPrivilegedWriter } from "../../shared/sqlite";
import type { AuthorityModuleClient, AuthorityStatus, ChangefeedPage } from "./context-authority";
import {
    applyMirrorPage,
    drainAuthority,
    ensureContextStoreUuid,
    getAuthorityManagedMarker,
    installAuthorityManagedMarker,
    prepareAuthority,
    reconcileAuthorityProject,
} from "./context-authority";
import { runMigrations } from "./migrations";
import { initializeDatabase } from "./storage-db";

function db(): Database {
    const value = new Database(":memory:");
    initializeDatabase(value);
    runMigrations(value);
    return value;
}

function authority(state: AuthorityStatus["state"], generation: number): AuthorityStatus {
    return { context_store_uuid: "store", project: "/repo", domain: "memories", state, generation };
}

function protocol(seedCalls: { bytes: number[] }): AuthorityModuleClient {
    let generation = 1;
    return {
        authorityStatus: async () => ({ authority: null }),
        authorityPrepare: async (args) => {
            if (args.phase === "begin") return { authority: authority("PREPARING", generation) };
            if (args.phase === "abort") return { authority: authority("TS", ++generation) };
            if (args.phase === "ack") return { authority: authority("MODULE", ++generation) };
            return {
                authority: {
                    ...authority("PREPARING", generation),
                    checksum_expected: String(args.checksum_expected),
                    checksum_actual: String(args.checksum_expected),
                    checksum_ok: 1,
                },
            };
        },
        authoritySeed: async (args) => {
            seedCalls.bytes.push(new TextEncoder().encode(JSON.stringify(args.rows)).byteLength);
            return { seeded: Array.isArray(args.rows) ? args.rows.length : 0, module_row_ids: [] };
        },
        mirrorPull: async (args) => ({
            page: {
                domain: args.domain,
                cursor: args.cursor,
                next_cursor: args.cursor,
                has_more: false,
                rows: [],
            },
        }),
    };
}

describe("memory authority protocol", () => {
    test("bounds authority seed frames below the management frame cap", async () => {
        const database = db();
        const seedCalls = { bytes: [] as number[] };
        const rows = Array.from({ length: 3 }, (_, id) => ({
            source_row_id: id + 1,
            snapshot: { id: id + 1, content: "x".repeat(400_000) },
        }));
        await prepareAuthority({
            db: database,
            projectPath: "/repo",
            domains: ["memories"],
            module: protocol(seedCalls),
            seedPages: async () => rows,
        });
        expect(seedCalls.bytes.length).toBeGreaterThan(1);
        expect(Math.max(...seedCalls.bytes)).toBeLessThan(1024 * 1024);
    });

    test("module checksum mismatch aborts, removes the marker, and restores TS writes", async () => {
        const database = db();
        const seedCalls = { bytes: [] as number[] };
        const module = protocol(seedCalls);
        const prepare = module.authorityPrepare;
        module.authorityPrepare = async (args) => {
            const response = await prepare(args);
            if (args.phase === "complete") {
                return {
                    authority: {
                        ...response.authority,
                        checksum_actual: "module-digest-does-not-match",
                        checksum_ok: 0,
                    },
                };
            }
            return response;
        };
        await expect(
            prepareAuthority({
                db: database,
                projectPath: "/repo",
                domains: ["memories"],
                module,
                seedPages: async () => [
                    { source_row_id: 1, snapshot: { id: 1, project_path: "/repo" } },
                ],
            }),
        ).rejects.toThrow("verification failed");
        expect(getAuthorityManagedMarker(database, "/repo")).toBeNull();
        database
            .prepare(
                "INSERT INTO memories(project_path, category, content, normalized_hash, first_seen_at, created_at, updated_at, last_seen_at) VALUES (?, 'CONSTRAINTS', 'ts works', 'h', 0, 0, 0, 0)",
            )
            .run("/repo");
        expect(database.prepare("SELECT COUNT(*) AS count FROM memories").get()).toEqual({
            count: 1,
        });
    });

    test("mirror updates delete stale vectors and translate references atomically", () => {
        const database = db();
        const storeUuid = ensureContextStoreUuid(database);
        withPrivilegedWriter(database, () => {
            database
                .prepare(
                    "INSERT INTO memories (project_path, category, content, normalized_hash, first_seen_at, created_at, updated_at, last_seen_at) VALUES (?, ?, ?, ?, 0, 0, 0, 0)",
                )
                .run("/repo", "CONSTRAINTS", "old", "h1");
            database
                .prepare(
                    "INSERT INTO memory_embeddings(memory_id, embedding, model_id) VALUES (1, ?, ?)",
                )
                .run(Buffer.from([1]), "test");
        });
        const snapshot = (
            id: number,
            content: string,
            hash: string,
            extra: Record<string, unknown> = {},
        ) => ({
            id,
            project_path: "/repo",
            category: "CONSTRAINTS",
            content,
            normalized_hash: hash,
            importance: 50,
            scope: "project",
            shareable: 0,
            source_session_id: null,
            source_type: "agent",
            seen_count: 1,
            retrieval_count: 0,
            first_seen_at: 0,
            created_at: 0,
            updated_at: 1,
            last_seen_at: 1,
            last_retrieved_at: null,
            status: "active",
            expires_at: null,
            verification_status: "unverified",
            verified_at: null,
            classified_at: null,
            superseded_by_memory_id: null,
            merged_from: null,
            metadata_json: null,
            context_store_uuid: storeUuid,
            context_row_id: id,
            ...extra,
        });
        const page = (
            cursor: number,
            next_cursor: number,
            rows: ChangefeedPage["rows"],
        ): ChangefeedPage => ({ domain: "memories", cursor, next_cursor, has_more: false, rows });
        applyMirrorPage({
            db: database,
            page: page(0, 1, [
                {
                    feed_seq: 1,
                    domain: "memories",
                    op: "insert",
                    module_row_id: 1,
                    full_row_snapshot: snapshot(1, "old", "h1"),
                    content_hash: "h1",
                },
            ]),
        });
        applyMirrorPage({
            db: database,
            page: page(1, 2, [
                {
                    feed_seq: 2,
                    domain: "memories",
                    op: "update",
                    module_row_id: 1,
                    full_row_snapshot: snapshot(1, "new", "h2"),
                    content_hash: "h2",
                },
            ]),
        });
        expect(database.prepare("SELECT content FROM memories WHERE id = 1").get()).toEqual({
            content: "new",
        });
        expect(
            database
                .prepare("SELECT COUNT(*) AS count FROM memory_embeddings WHERE memory_id = 1")
                .get(),
        ).toEqual({ count: 0 });
        withPrivilegedWriter(database, () => {
            database
                .prepare(
                    "INSERT INTO memory_embeddings(memory_id, embedding, model_id) VALUES (1, ?, ?)",
                )
                .run(Buffer.from([2]), "test");
        });
        applyMirrorPage({
            db: database,
            page: page(2, 3, [
                {
                    feed_seq: 3,
                    domain: "memories",
                    op: "tombstone",
                    module_row_id: 1,
                    full_row_snapshot: snapshot(1, "new", "h2"),
                    content_hash: "h2",
                },
            ]),
        });
        expect(
            database.prepare("SELECT COUNT(*) AS count FROM memories WHERE id = 1").get(),
        ).toEqual({ count: 0 });
        expect(
            database
                .prepare("SELECT COUNT(*) AS count FROM memory_embeddings WHERE memory_id = 1")
                .get(),
        ).toEqual({ count: 0 });
    });

    test("drain finish removes the marker only after module ownership returns to TS", async () => {
        const database = db();
        installAuthorityManagedMarker(database, "/repo");
        let generation = 1;
        let memoryState: AuthorityStatus["state"] = "MODULE";
        const module: AuthorityModuleClient = {
            authorityStatus: async (args) => ({
                authority:
                    args.domain === "memories"
                        ? { ...authority(memoryState, generation), domain: args.domain }
                        : { ...authority("TS", generation), domain: args.domain },
            }),
            authorityPrepare: async () => ({ authority: authority("MODULE", generation) }),
            authoritySeed: async () => ({ seeded: 0 }),
            mirrorPull: async () => ({
                page: { domain: "memories", cursor: 0, next_cursor: 0, has_more: false, rows: [] },
            }),
            authorityDrain: async (args) => {
                memoryState = args.action === "finish" ? "TS" : "DRAINING";
                return { authority: authority(memoryState, ++generation) };
            },
        };
        const result = await drainAuthority({
            db: database,
            projectPath: "/repo",
            domain: "memories",
            module,
            checksum: "same",
        });
        expect(result.state).toBe("TS");
        expect(getAuthorityManagedMarker(database, "/repo")).toBeNull();
    });

    test("installs the marker before reading the stable seed set", async () => {
        const database = db();
        database
            .prepare(
                "INSERT INTO memories(project_path, category, content, normalized_hash, first_seen_at, created_at, updated_at, last_seen_at) VALUES (?, 'CONSTRAINTS', 'before marker', 'h1', 0, 0, 0, 0)",
            )
            .run("/repo");
        let seededIds: number[] = [];
        await prepareAuthority({
            db: database,
            projectPath: "/repo",
            domains: ["memories"],
            module: protocol({ bytes: [] }),
            seedPages: async () => {
                expect(() =>
                    database
                        .prepare(
                            "INSERT INTO memories(project_path, category, content, normalized_hash, first_seen_at, created_at, updated_at, last_seen_at) VALUES (?, 'CONSTRAINTS', 'after marker', 'h2', 0, 0, 0, 0)",
                        )
                        .run("/repo"),
                ).toThrow("managed by the Rust module");
                const rows = database
                    .prepare("SELECT * FROM memories WHERE project_path = ? ORDER BY id")
                    .all("/repo") as Array<Record<string, unknown>>;
                seededIds = rows.map((row) => Number(row.id));
                return rows.map((snapshot) => ({ source_row_id: snapshot.id, snapshot }));
            },
        });
        expect(seededIds).toEqual([1]);
    });

    test("does not hold a SQLite transaction while module transport is delayed", async () => {
        const database = db();
        database.exec("CREATE TABLE unrelated_writer_probe(id INTEGER PRIMARY KEY, value TEXT)");
        let releaseBegin: (() => void) | undefined;
        const beginGate = new Promise<void>((resolve) => {
            releaseBegin = resolve;
        });
        const module = protocol({ bytes: [] });
        const ordinaryPrepare = module.authorityPrepare;
        module.authorityPrepare = async (args) => {
            if (args.phase === "begin") await beginGate;
            return ordinaryPrepare(args);
        };
        const preparation = prepareAuthority({
            db: database,
            projectPath: "/repo",
            domains: ["memories"],
            module,
            seedPages: async () => [],
        });
        await Promise.resolve();
        database
            .prepare("INSERT INTO unrelated_writer_probe(value) VALUES ('writer was not blocked')")
            .run();
        releaseBegin?.();
        await preparation;
        expect(
            database.prepare("SELECT COUNT(*) AS count FROM unrelated_writer_probe").get(),
        ).toEqual({
            count: 1,
        });
    });

    test("restart reconciliation reinstalls a missing marker before tools can write", async () => {
        const database = db();
        const module = protocol({ bytes: [] });
        module.authorityStatus = async (args) => ({
            authority: { ...authority("MODULE", 2), domain: args.domain },
        });
        await reconcileAuthorityProject({ db: database, projectPath: "/repo", module });
        expect(getAuthorityManagedMarker(database, "/repo")).not.toBeNull();
        expect(() =>
            database
                .prepare(
                    "INSERT INTO memories(project_path, category, content, normalized_hash, first_seen_at, created_at, updated_at, last_seen_at) VALUES (?, 'CONSTRAINTS', 'blocked', 'h', 0, 0, 0, 0)",
                )
                .run("/repo"),
        ).toThrow("managed by the Rust module");
    });

    test("resolves superseded references introduced on a later mirror page", () => {
        const database = db();
        const memory = (id: number, supersededBy: number | null) => ({
            id,
            project_path: "/repo",
            category: "CONSTRAINTS",
            content: `memory ${id}`,
            normalized_hash: `h${id}`,
            scope: "project",
            shareable: 0,
            seen_count: 1,
            retrieval_count: 0,
            first_seen_at: 0,
            created_at: 0,
            updated_at: 0,
            last_seen_at: 0,
            status: "active",
            verification_status: "unverified",
            superseded_by_memory_id: supersededBy,
        });
        applyMirrorPage({
            db: database,
            page: {
                domain: "memories",
                cursor: 0,
                next_cursor: 1,
                has_more: true,
                rows: [
                    {
                        feed_seq: 1,
                        domain: "memories",
                        op: "insert",
                        module_row_id: 10,
                        full_row_snapshot: memory(10, 20),
                        content_hash: "h10",
                    },
                ],
            },
        });
        expect(database.prepare("SELECT superseded_by_memory_id FROM memories").get()).toEqual({
            superseded_by_memory_id: null,
        });
        applyMirrorPage({
            db: database,
            page: {
                domain: "memories",
                cursor: 1,
                next_cursor: 2,
                has_more: false,
                rows: [
                    {
                        feed_seq: 2,
                        domain: "memories",
                        op: "insert",
                        module_row_id: 20,
                        full_row_snapshot: memory(20, null),
                        content_hash: "h20",
                    },
                ],
            },
        });
        const rows = database
            .prepare("SELECT id, superseded_by_memory_id FROM memories ORDER BY id")
            .all() as Array<{ id: number; superseded_by_memory_id: number | null }>;
        expect(rows[0]?.superseded_by_memory_id).toBe(rows[1]?.id);
    });
});

/// <reference types="bun-types" />

import { describe, expect, it } from "bun:test";
import type { ContextDatabase } from "../features/magic-context/storage";
import type { Tagger } from "../features/magic-context/tagger";
import { tagTranscript } from "./tag-transcript";
import type { Transcript, TranscriptPart, TranscriptPartKind } from "./transcript";

class FakeTagger implements Tagger {
    private nextTag = 1;
    private toolTags = new Map<string, number>();
    readonly owners: string[] = [];
    readonly byteSizes = new Map<number, number>();

    assignTag(): number {
        return this.nextTag++;
    }

    getTag(): number | undefined {
        return undefined;
    }

    assignToolTag(
        _sessionId: string,
        callId: string,
        ownerMsgId: string,
        byteSize: number,
    ): number {
        const key = `${ownerMsgId}\0${callId}`;
        const existing = this.toolTags.get(key);
        if (existing !== undefined) return existing;
        const tag = this.nextTag++;
        this.toolTags.set(key, tag);
        this.byteSizes.set(tag, byteSize);
        this.owners.push(ownerMsgId);
        return tag;
    }

    getToolTag(_sessionId: string, callId: string, ownerMsgId: string): number | undefined {
        return this.toolTags.get(`${ownerMsgId}\0${callId}`);
    }

    bindTag(): void {}

    bindToolTag(_sessionId: string, callId: string, ownerMsgId: string, tagNumber: number): void {
        this.toolTags.set(`${ownerMsgId}\0${callId}`, tagNumber);
    }

    getAssignments(): ReadonlyMap<string, number> {
        return this.toolTags;
    }

    resetCounter(): void {
        this.nextTag = 1;
    }

    getCounter(): number {
        return this.nextTag - 1;
    }

    initFromDb(): void {}

    cleanup(): void {}
}

class TestPart implements TranscriptPart {
    constructor(
        readonly kind: TranscriptPartKind,
        readonly id: string | undefined,
        private text: string,
        private readonly toolName = "read",
    ) {}

    getText(): string | undefined {
        return this.text;
    }

    setText(newText: string): boolean {
        if (this.text === newText) return false;
        this.text = newText;
        return true;
    }

    setToolOutput(newText: string): boolean {
        return this.setText(newText);
    }

    getToolMetadata(): { toolName: string | undefined; inputByteSize: number } {
        return {
            toolName: this.toolName,
            inputByteSize: this.kind === "tool_use" ? this.text.length : 0,
        };
    }

    replaceWithSentinel(sentinelText: string): boolean {
        return this.setText(sentinelText);
    }
}

class ThrowingToolOutputPart extends TestPart {
    setToolOutput(): boolean {
        throw new Error("setToolOutput on assistant part");
    }
}

class NonTextToolResultPart extends TestPart {
    constructor(
        id: string,
        readonly content: unknown,
    ) {
        super("tool_result", id, "");
    }

    getText(): string | undefined {
        return undefined;
    }

    setText(): boolean {
        return false;
    }
}

class FakeDb {
    readonly byteSizeUpdates: Array<{ byteSize: number; sessionId: string; tagNumber: number }> =
        [];

    prepare(sql: string): { run: (...args: unknown[]) => void } {
        return {
            run: (...args: unknown[]) => {
                if (!sql.startsWith("UPDATE tags SET byte_size =")) return;
                const [byteSize, sessionId, tagNumber] = args;
                if (
                    typeof byteSize === "number" &&
                    typeof sessionId === "string" &&
                    typeof tagNumber === "number"
                ) {
                    this.byteSizeUpdates.push({ byteSize, sessionId, tagNumber });
                }
            },
        };
    }
}

describe("tagTranscript tool aggregation", () => {
    it("keeps repeated callIds in separate owner-scoped aggregate targets", () => {
        const tagger = new FakeTagger();
        const firstUse = new TestPart("tool_use", "read:32", '{"file":"long-a"}');
        const firstResult = new TestPart("tool_result", "read:32", "r1");
        const secondUse = new TestPart("tool_use", "read:32", '{"file":"long-b"}');
        const secondResult = new TestPart("tool_result", "read:32", "r2");
        const transcript: Transcript = {
            harness: "pi",
            messages: [
                { info: { id: "assistant-1", role: "assistant" }, parts: [firstUse] },
                { info: { id: "user-1", role: "user" }, parts: [firstResult] },
                { info: { id: "assistant-2", role: "assistant" }, parts: [secondUse] },
                { info: { id: "user-2", role: "user" }, parts: [secondResult] },
            ],
            commit() {},
        };

        const { targets } = tagTranscript(
            "session-1",
            transcript,
            tagger,
            new FakeDb() as unknown as ContextDatabase,
        );

        const firstTag = tagger.getToolTag("session-1", "read:32", "assistant-1");
        const secondTag = tagger.getToolTag("session-1", "read:32", "assistant-2");
        expect(firstTag).toBeDefined();
        expect(secondTag).toBeDefined();
        expect(firstTag).not.toBe(secondTag);
        expect(tagger.owners).toEqual(["assistant-1", "assistant-2"]);
        expect(targets.size).toBe(2);

        expect(targets.get(firstTag ?? -1)?.drop()).toBe("removed");
        expect(firstUse.getText()).toBe(`[dropped §${firstTag}§]`);
        expect(firstResult.getText()).toBe(`[dropped §${firstTag}§]`);
        expect(secondUse.getText()).toBe('{"file":"long-b"}');
        expect(secondResult.getText()).toContain("r2");
    });

    it("truncates assistant tool_use parts via text fallback when setToolOutput asserts", () => {
        const tagger = new FakeTagger();
        const toolUse = new ThrowingToolOutputPart("tool_use", "read:99", '{"file":"long-a"}');
        const transcript: Transcript = {
            harness: "pi",
            messages: [{ info: { id: "assistant-1", role: "assistant" }, parts: [toolUse] }],
            commit() {},
        };

        const { targets } = tagTranscript(
            "session-1",
            transcript,
            tagger,
            new FakeDb() as unknown as ContextDatabase,
        );
        const tag = tagger.getToolTag("session-1", "read:99", "assistant-1");

        let result: "truncated" | "absent" | undefined;
        expect(() => {
            result = targets.get(tag ?? -1)?.truncate?.();
        }).not.toThrow();
        expect(result).toBe("truncated");
        // Skeleton-drop now renders the one canonical placeholder, not a
        // separate "[truncated]" vocabulary.
        expect(toolUse.getText()).toBe(`[dropped \u00a7${tag}\u00a7]`);
    });

    it("drops every contiguous folded tool_result block for the paired callId", () => {
        const tagger = new FakeTagger();
        const toolUse = new TestPart("tool_use", "read:multi", '{"file":"long-a"}');
        const firstResult = new TestPart("tool_result", "read:multi", "r1");
        const secondResult = new TestPart("tool_result", "read:multi", "r2");
        const transcript: Transcript = {
            harness: "pi",
            messages: [
                { info: { id: "assistant-1", role: "assistant" }, parts: [toolUse] },
                { info: { id: "user-1", role: "user" }, parts: [firstResult, secondResult] },
            ],
            commit() {},
        };

        const { targets } = tagTranscript(
            "session-1",
            transcript,
            tagger,
            new FakeDb() as unknown as ContextDatabase,
        );
        const tag = tagger.getToolTag("session-1", "read:multi", "assistant-1");

        expect(targets.size).toBe(1);
        expect(targets.get(tag ?? -1)?.drop()).toBe("removed");
        expect(toolUse.getText()).toBe(`[dropped §${tag}§]`);
        expect(firstResult.getText()).toBe(`[dropped §${tag}§]`);
        expect(secondResult.getText()).toBe(`[dropped §${tag}§]`);
    });

    it("pairs a reused callId result with the nearest previous unresolved owner", () => {
        const tagger = new FakeTagger();
        const olderUse = new TestPart("tool_use", "read:reused", '{"file":"older"}');
        const nearestUse = new TestPart("tool_use", "read:reused", '{"file":"nearest"}');
        const result = new TestPart("tool_result", "read:reused", "nearest result");
        const transcript: Transcript = {
            harness: "pi",
            messages: [
                { info: { id: "assistant-old", role: "assistant" }, parts: [olderUse] },
                { info: { id: "assistant-near", role: "assistant" }, parts: [nearestUse] },
                { info: { id: "user-result", role: "user" }, parts: [result] },
            ],
            commit() {},
        };

        const { targets } = tagTranscript(
            "session-1",
            transcript,
            tagger,
            new FakeDb() as unknown as ContextDatabase,
        );

        const olderTag = tagger.getToolTag("session-1", "read:reused", "assistant-old");
        const nearestTag = tagger.getToolTag("session-1", "read:reused", "assistant-near");
        expect(olderTag).toBeDefined();
        expect(nearestTag).toBeDefined();
        expect(olderTag).not.toBe(nearestTag);

        expect(targets.get(nearestTag ?? -1)?.drop()).toBe("removed");
        expect(olderUse.getText()).toBe('{"file":"older"}');
        expect(nearestUse.getText()).toBe(`[dropped §${nearestTag}§]`);
        expect(result.getText()).toBe(`[dropped §${nearestTag}§]`);
    });

    it("reuses owner-scoped identities without collapsing duplicate callIds", () => {
        const tagger = new FakeTagger();
        const seedDb = new FakeDb();
        const seedTranscript: Transcript = {
            harness: "pi",
            messages: [
                {
                    info: { id: "assistant-old", role: "assistant" },
                    parts: [new TestPart("tool_use", "duplicate-call", '{"turn":1}')],
                },
                {
                    info: { id: "user-old", role: "user" },
                    parts: [new TestPart("tool_result", "duplicate-call", "old result")],
                },
                {
                    info: { id: "assistant-new", role: "assistant" },
                    parts: [new TestPart("tool_use", "duplicate-call", '{"turn":2}')],
                },
                {
                    info: { id: "user-new", role: "user" },
                    parts: [new TestPart("tool_result", "duplicate-call", "new result")],
                },
            ],
            commit() {},
        };
        tagTranscript(
            "session-duplicate-reuse",
            seedTranscript,
            tagger,
            seedDb as unknown as ContextDatabase,
        );

        const reusedParts = {
            oldUse: new TestPart("tool_use", "duplicate-call", '{"turn":1}'),
            oldResult: new TestPart("tool_result", "duplicate-call", "old result"),
            newUse: new TestPart("tool_use", "duplicate-call", '{"turn":2}'),
            newResult: new TestPart("tool_result", "duplicate-call", "new result"),
        };
        const reusedTranscript: Transcript = {
            harness: "pi",
            messages: [
                {
                    info: { id: "assistant-old", role: "assistant" },
                    parts: [reusedParts.oldUse],
                },
                {
                    info: { id: "user-old", role: "user" },
                    parts: [reusedParts.oldResult],
                },
                {
                    info: { id: "assistant-new", role: "assistant" },
                    parts: [reusedParts.newUse],
                },
                {
                    info: { id: "user-new", role: "user" },
                    parts: [reusedParts.newResult],
                },
            ],
            commit() {},
        };
        const reuseDb = new FakeDb();
        const reused = tagTranscript(
            "session-duplicate-reuse",
            reusedTranscript,
            tagger,
            reuseDb as unknown as ContextDatabase,
            {
                reuseMessageIds: new Set([
                    "assistant-old",
                    "user-old",
                    "assistant-new",
                    "user-new",
                ]),
            },
        );

        const oldTag = tagger.getToolTag(
            "session-duplicate-reuse",
            "duplicate-call",
            "assistant-old",
        );
        const newTag = tagger.getToolTag(
            "session-duplicate-reuse",
            "duplicate-call",
            "assistant-new",
        );
        expect(oldTag).toBeDefined();
        expect(newTag).toBeDefined();
        expect(oldTag).not.toBe(newTag);
        expect(reused.targets.size).toBe(2);
        expect(reusedParts.oldResult.getText()).toBe(`§${oldTag}§ old result`);
        expect(reusedParts.newResult.getText()).toBe(`§${newTag}§ new result`);
        expect(reuseDb.byteSizeUpdates).toEqual([]);
    });

    it("backfills a newly appended result after reusing its stable invocation", () => {
        const tagger = new FakeTagger();
        const callId = "late-result";
        tagTranscript(
            "session-late-result",
            {
                harness: "pi",
                messages: [
                    {
                        info: { id: "assistant-owner", role: "assistant" },
                        parts: [new TestPart("tool_use", callId, '{"path":"a"}')],
                    },
                ],
                commit() {},
            },
            tagger,
            new FakeDb() as unknown as ContextDatabase,
        );

        const result = new TestPart("tool_result", callId, "late output");
        const db = new FakeDb();
        tagTranscript(
            "session-late-result",
            {
                harness: "pi",
                messages: [
                    {
                        info: { id: "assistant-owner", role: "assistant" },
                        parts: [new TestPart("tool_use", callId, '{"path":"a"}')],
                    },
                    {
                        info: { id: "user-result", role: "user" },
                        parts: [result],
                    },
                ],
                commit() {},
            },
            tagger,
            db as unknown as ContextDatabase,
            { reuseMessageIds: new Set(["assistant-owner"]) },
        );

        const tag = tagger.getToolTag("session-late-result", callId, "assistant-owner");
        expect(result.getText()).toBe(`§${tag}§ late output`);
        expect(db.byteSizeUpdates.some((update) => update.tagNumber === tag)).toBe(true);
    });

    it("accounts non-text tool_result content when ranking tool output byte size", () => {
        const tagger = new FakeTagger();
        const db = new FakeDb();
        const toolUse = new TestPart("tool_use", "read:image", "{}");
        const caption = new TestPart("tool_result", "read:image", "c");
        const image = new NonTextToolResultPart("read:image", {
            type: "image",
            data: "x".repeat(512),
            mediaType: "image/png",
        });
        const transcript: Transcript = {
            harness: "pi",
            messages: [
                { info: { id: "assistant-1", role: "assistant" }, parts: [toolUse] },
                { info: { id: "user-1", role: "user" }, parts: [caption, image] },
            ],
            commit() {},
        };

        tagTranscript("session-1", transcript, tagger, db as unknown as ContextDatabase);

        const tag = tagger.getToolTag("session-1", "read:image", "assistant-1");
        expect(tag).toBeDefined();
        // byte_size is OUTPUT-only: the tag reserves at 0 on the tool_use
        // occurrence (args live in inputByteSize), then updates to the real
        // result payload size when the tool_result (incl. the non-text image
        // block) is seen — proving non-text content is byte-accounted.
        expect(tagger.byteSizes.get(tag ?? -1)).toBe(0);
        // Two tool_result blocks (caption text + image) under one callId; the
        // tag byte_size climbs to the LARGEST output block (the image > 512B).
        const updatesForTag = db.byteSizeUpdates.filter((u) => u.tagNumber === tag);
        expect(updatesForTag.length).toBeGreaterThanOrEqual(1);
        expect(Math.max(...updatesForTag.map((u) => u.byteSize))).toBeGreaterThan(512);
    });
});

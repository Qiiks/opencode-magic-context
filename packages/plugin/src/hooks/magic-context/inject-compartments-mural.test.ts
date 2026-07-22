/// <reference types="bun-types" />

import { describe, expect, it } from "bun:test";
import { initializeDatabase } from "../../features/magic-context/storage-db";
import { getOrCreateSessionMeta } from "../../features/magic-context/storage-meta-session";
import { Database } from "../../shared/sqlite";
import { closeQuietly } from "../../shared/sqlite-helpers";
import { injectM0M1, type M0M1State } from "./inject-compartments";
import type { MessageLike } from "./tag-messages";

const SESSION_ID = "ses_mural_inject";

function makeDb(): Database {
    const db = new Database(":memory:");
    initializeDatabase(db);
    getOrCreateSessionMeta(db, SESSION_ID);
    return db;
}

// A 1x1 transparent PNG data URL, standing in for a rendered mural.
const FAKE_MURAL_DATA_URL =
    "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

function muralOption() {
    return {
        enabled: true,
        supportsVision: true,
        dataUrl: FAKE_MURAL_DATA_URL,
        contentHash: "mural-hash-1",
    };
}

describe("m[0] mural image fold (on-demand render → wire)", () => {
    it("folds the <memory-mural> block and image part when a mural is supplied, and replays it on defer", () => {
        const db = makeDb();
        try {
            const state = getOrCreateSessionMeta(db, SESSION_ID) as unknown as M0M1State;

            const hardMessages: MessageLike[] = [];
            const first = injectM0M1({
                db,
                sessionId: SESSION_ID,
                messages: hardMessages,
                state,
                projectPath: undefined,
                isCacheBustingPass: true,
                mural: muralOption(),
            });
            expect(first.injected).toBe(true);
            // m[0] carries the mural marker block.
            expect(first.m0Bytes?.toString("utf8")).toContain("<memory-mural>");
            // The prepended synthetic head message carries an image file part.
            const head = hardMessages[0];
            const imagePart = head?.parts.find(
                (part) => (part as { type?: string }).type === "file",
            ) as { type: string; mime?: string; url?: string } | undefined;
            expect(imagePart).toBeDefined();
            expect(imagePart?.mime).toBe("image/png");
            expect(imagePart?.url).toBe(FAKE_MURAL_DATA_URL);

            // A defer pass (no mural option supplied) must replay the SAME baked-in
            // data URL from state, not drop the image — the "swaps only on a HARD
            // fold" rule.
            const deferMessages: MessageLike[] = [];
            const second = injectM0M1({
                db,
                sessionId: SESSION_ID,
                messages: deferMessages,
                state,
                projectPath: undefined,
                isCacheBustingPass: false,
            });
            expect(second.m0Bytes).toEqual(first.m0Bytes);
            const deferImage = deferMessages[0]?.parts.find(
                (part) => (part as { type?: string }).type === "file",
            ) as { url?: string } | undefined;
            expect(deferImage?.url).toBe(FAKE_MURAL_DATA_URL);
        } finally {
            closeQuietly(db);
        }
    });

    it("omits the mural block entirely when no mural is supplied and the feature is off", () => {
        const db = makeDb();
        try {
            const state = getOrCreateSessionMeta(db, SESSION_ID) as unknown as M0M1State;
            const messages: MessageLike[] = [];
            const result = injectM0M1({
                db,
                sessionId: SESSION_ID,
                messages,
                state,
                projectPath: undefined,
                isCacheBustingPass: true,
                // muralEnabled defaults undefined → no image path.
            });
            expect(result.m0Bytes?.toString("utf8")).not.toContain("<memory-mural>");
            const imagePart = messages[0]?.parts.find(
                (part) => (part as { type?: string }).type === "file",
            );
            expect(imagePart).toBeUndefined();
        } finally {
            closeQuietly(db);
        }
    });
});

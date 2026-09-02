import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { __test } from "./analyze-cache-busts";

const tempDirs: string[] = [];

afterEach(() => {
    for (const dir of tempDirs.splice(0)) {
        rmSync(dir, { recursive: true, force: true });
    }
});

function writeDump(
    dir: string,
    stem: string,
    createdAt: string,
    session: string,
    text: string,
): void {
    const bodyPath = join(dir, `${stem}.body.json`);
    writeFileSync(
        bodyPath,
        JSON.stringify({
            system: [{ type: "text", text: "system", cache_control: { type: "ephemeral" } }],
            messages: [{ role: "user", content: [{ type: "text", text }] }],
        }),
    );
    writeFileSync(
        join(dir, `${stem}.meta.json`),
        JSON.stringify({
            createdAt,
            session: `${session.slice(0, 12)}…`,
            files: { body: bodyPath },
            body: { messagesCount: 1 },
        }),
    );
}

describe("analyze-cache-bust dump discovery", () => {
    test("loads and orders legacy and sequence+routing filename layouts by timestamp and sequence", () => {
        const dir = mkdtempSync(join(tmpdir(), "cache-bust-fixture-"));
        tempDirs.push(dir);
        const session = "ses_fixtureFull123";
        const legacy = `2026-09-02T08-41-39-474Z-${session}`;
        const currentEarly = `2026-09-02T08-46-03-306Z-000002-${session}-direct-sticky-yiyi`;
        const currentLate = `2026-09-02T08-46-03-306Z-000013-${session}-direct-sticky-yiyi`;
        writeDump(dir, currentLate, "2026-09-02T08:46:03.306Z", session, "late");
        writeDump(dir, legacy, "2026-09-02T08:41:39.474Z", session, "legacy");
        writeDump(dir, currentEarly, "2026-09-02T08:46:03.306Z", session, "early");

        const opts = __test.parseArgs([
            "bun",
            "analyze-cache-busts.ts",
            "--session",
            session,
            "--dir",
            dir,
        ]);
        const snapshots = __test.loadSnapshots(opts);

        expect(opts.sessionPrefix).toBe(session);
        expect(snapshots.map((snapshot) => snapshot.file)).toEqual([
            `${legacy}.meta.json`,
            `${currentEarly}.meta.json`,
            `${currentLate}.meta.json`,
        ]);
        expect(snapshots.every((snapshot) => snapshot.session === session)).toBe(true);
    });

    test("resolves relative --since durations", () => {
        expect(__test.resolveTimeBound("30m", 1_800_000)).toBe("1970-01-01T00:00:00.000Z");
        expect(__test.resolveTimeBound("2026-09-02T08:30:00Z", 0)).toBe(
            "2026-09-02T08:30:00.000Z",
        );
    });
});

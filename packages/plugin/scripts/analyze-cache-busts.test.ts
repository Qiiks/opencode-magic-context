import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { __test } from "./analyze-cache-busts";

type UsageFixture = {
    cache_read_input_tokens?: number;
    cache_creation_input_tokens?: number;
    input_tokens: number;
};

const tempDirs: string[] = [];

afterEach(() => {
    for (const dir of tempDirs.splice(0)) {
        rmSync(dir, { recursive: true, force: true });
    }
});

function responseUsage(usage: UsageFixture): unknown {
    return { status: 200, usage };
}

function streamedUsage(usage: UsageFixture): unknown {
    return {
        events: [
            { type: "message_start", message: { usage } },
            { type: "message_delta", usage },
        ],
    };
}

function writeDump(
    dir: string,
    stem: string,
    createdAt: string,
    session: string,
    body: unknown,
    response: unknown = responseUsage({ input_tokens: 1 }),
): void {
    const bodyPath = join(dir, `${stem}.body.json`);
    const responsePath = join(dir, `${stem}.response.json`);
    writeFileSync(bodyPath, JSON.stringify(body));
    writeFileSync(responsePath, JSON.stringify(response));
    writeFileSync(
        join(dir, `${stem}.meta.json`),
        JSON.stringify({
            createdAt,
            session: `${session.slice(0, 12)}…`,
            files: { body: bodyPath, response: responsePath },
            body: { messagesCount: Array.isArray((body as { messages?: unknown[] }).messages) ? (body as { messages: unknown[] }).messages.length : 0 },
        }),
    );
}

function bodyWithBreakpointMessage(text: string): unknown {
    return {
        messages: [
            {
                role: "user",
                content: [{ type: "text", text, cache_control: { type: "ephemeral" } }],
            },
        ],
    };
}

function bodyWithTail(text: string, tailBreakpoint = false): unknown {
    const tail = { type: "text", text } as { type: string; text: string; cache_control?: unknown };
    if (tailBreakpoint) tail.cache_control = { type: "ephemeral" };
    return {
        messages: [
            {
                role: "user",
                content: [{ type: "text", text: "cached prefix", cache_control: { type: "ephemeral" } }],
            },
            { role: "assistant", content: [tail] },
        ],
    };
}

function snapshotsFor(dir: string, session: string) {
    return __test.loadSnapshots(
        __test.parseArgs(["bun", "analyze-cache-busts.ts", "--session", session, "--dir", dir]),
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
        writeDump(dir, currentLate, "2026-09-02T08:46:03.306Z", session, bodyWithTail("late"));
        writeDump(dir, legacy, "2026-09-02T08:41:39.474Z", session, bodyWithTail("legacy"));
        writeDump(dir, currentEarly, "2026-09-02T08:46:03.306Z", session, bodyWithTail("early"));

        const snapshots = snapshotsFor(dir, session);

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

describe("analyze-cache-bust provider meter verdicts", () => {
    test("classifies a breakpoint rewrite as a metered bust", () => {
        const dir = mkdtempSync(join(tmpdir(), "cache-bust-meter-"));
        tempDirs.push(dir);
        const session = "ses_realBustFixture";
        writeDump(
            dir,
            `2026-09-02T08-45-00-000Z-000001-${session}`,
            "2026-09-02T08:45:00.000Z",
            session,
            bodyWithBreakpointMessage("old prompt section"),
            responseUsage({ cache_read_input_tokens: 387_595, input_tokens: 4 }),
        );
        writeDump(
            dir,
            `2026-09-02T08-46-03-000Z-000002-${session}`,
            "2026-09-02T08:46:03.000Z",
            session,
            bodyWithBreakpointMessage("rewritten prompt section"),
            responseUsage({
                cache_read_input_tokens: 267_383,
                cache_creation_input_tokens: 120_212,
                input_tokens: 4,
            }),
        );

        const row = __test.analyzeSnapshots(snapshotsFor(dir, session))[1];

        expect(row.verdict).toBe("BUST");
        expect(row.byteVerdict).toBe("BUST");
        expect(row.meterVsBytes).toBe("AGREE");
        expect(row.rewrittenTokens).toBe(120_216);
    });

    test("forgives a tail divergence when the streamed usage meter reports a hit", () => {
        const dir = mkdtempSync(join(tmpdir(), "cache-bust-meter-"));
        tempDirs.push(dir);
        const session = "ses_falsePositiveFixture";
        const priorUsage = {
            cache_read_input_tokens: 411_287,
            cache_creation_input_tokens: 1_771,
            input_tokens: 4,
        };
        writeDump(
            dir,
            `2026-09-02T09-17-40-000Z-000001-${session}`,
            "2026-09-02T09:17:40.000Z",
            session,
            bodyWithTail("older tail payload", true),
            responseUsage(priorUsage),
        );
        writeDump(
            dir,
            `2026-09-02T09-17-52-000Z-000002-${session}`,
            "2026-09-02T09:17:52.000Z",
            session,
            bodyWithTail("new tail payload"),
            streamedUsage({
                cache_read_input_tokens: 411_287,
                cache_creation_input_tokens: 1_771,
                input_tokens: 4,
            }),
        );

        const row = __test.analyzeSnapshots(snapshotsFor(dir, session))[1];

        // Replacing the meter verdict with byte geometry makes this assertion red:
        // the previous request's tail breakpoint makes the changed tail look like a bust.
        expect(row.verdict).toBe("STABLE");
        expect(row.byteVerdict).toBe("BUST");
        expect(row.meterVsBytes).toBe("BYTES-ONLY");
        expect(row.meterFloor).toBe(411_291);
        expect(row.comparableRead).toBe(411_291);
        expect(row.current.usage?.source).toBe("message_delta.usage");
    });

    test("reports an unmetered byte-attributed candidate when a response has no usage", () => {
        const dir = mkdtempSync(join(tmpdir(), "cache-bust-meter-"));
        tempDirs.push(dir);
        const session = "ses_unmeteredFixture";
        writeDump(
            dir,
            `2026-09-02T09-20-00-000Z-000001-${session}`,
            "2026-09-02T09:20:00.000Z",
            session,
            bodyWithBreakpointMessage("before"),
            responseUsage({ cache_read_input_tokens: 10, input_tokens: 1 }),
        );
        writeDump(
            dir,
            `2026-09-02T09-20-01-000Z-000002-${session}`,
            "2026-09-02T09:20:01.000Z",
            session,
            bodyWithBreakpointMessage("after"),
            { status: 200, stream_complete: false },
        );

        const row = __test.analyzeSnapshots(snapshotsFor(dir, session))[1];

        expect(row.verdict).toBe("UNMETERED");
        expect(row.byteVerdict).toBe("BUST");
        expect(row.meterVsBytes).toBe("UNMETERED");
    });
});

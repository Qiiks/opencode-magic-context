import { describe, expect, test } from "bun:test";
import { createMessagesTransformHandler } from "../../plugin/messages-transform";
import { EmergencyFailClosedError } from "./emergency-fail-closed";
import {
    buildLkgPrefix,
    captureLkgSlot,
    replayLkg,
    validateLkgEntry,
    validateLkgSeam,
} from "./lkg-replay";
import { captureSlot, getSlot, noteEntry, resetLkgSlotsForTest } from "./lkg-slot";
import { createPassOutcome } from "./pass-outcome";
import type { MessageLike } from "./transform-operations";

function user(
    id: string,
    created: number,
    model = { providerID: "test", modelID: "model" },
): MessageLike {
    return {
        info: { id, role: "user", sessionID: "session", model, time: { created } } as never,
        parts: [{ type: "text", text: id }],
    };
}

function assistant(id: string, created: number, parts: unknown[] = []): MessageLike {
    return {
        info: {
            id,
            role: "assistant",
            sessionID: "session",
            time: { created },
            finish: "stop",
        } as never,
        parts,
    };
}

describe("LKG transform replay", () => {
    test("captures only the prefix through an early anchor and serves a pristine tail", () => {
        resetLkgSlotsForTest();
        const input = [
            user("u0", 1),
            user("u1", 2),
            assistant("a1", 3, [
                {
                    type: "tool",
                    callID: "call-1",
                    state: { status: "completed", output: { nested: "original" } },
                },
            ]),
        ];
        const output = structuredClone(input) as MessageLike[];
        expect(
            captureLkgSlot({
                sessionId: "session",
                input,
                output,
                modelKey: "test/model",
                providerKey: "test",
            }),
        ).toBe(true);
        const current = [...structuredClone(input), user("u2", 4)] as MessageLike[];
        const entry = noteEntry("session", current);
        expect(entry).not.toBeNull();
        const originalTail = structuredClone(entry?.pristineTail);
        const tool = current[2]?.parts[0] as Record<string, unknown>;
        (tool.state as Record<string, unknown>).output = { nested: "mutated" };
        const replay = replayLkg({
            sessionId: "session",
            messages: current,
            modelKey: "test/model",
            providerKey: "test",
            entry,
        });
        expect(replay.ok).toBe(true);
        if (replay.ok) {
            expect(replay.messages.map((message) => message.info.id)).toEqual([
                "u0",
                "u1",
                "a1",
                "u2",
            ]);
            expect(replay.messages[2]).toEqual(originalTail?.[0]);
            expect(new Set(replay.messages.map((message) => message.info.id)).size).toBe(4);
        }
    });

    test("declines duplicate input ids instead of storing a full-output snapshot", () => {
        resetLkgSlotsForTest();
        const input = [user("u0", 1), user("u0", 2)];
        expect(buildLkgPrefix(input, input)).toBeNull();
        expect(getSlot("session")).toBeUndefined();
    });

    test("marker validation rejects shifted starts, missing anchors, and suffix-only matches", () => {
        resetLkgSlotsForTest();
        captureSlot("session", {
            jsonPrefix: JSON.stringify([user("u1", 1)]),
            inputIdSeq: ["u1", "u2"],
            lastInputMessageId: "u2",
            modelKey: "test/model",
            providerKey: "test",
            capturedAt: 1,
        });
        const slot = getSlot("session");
        expect(slot).toBeDefined();
        expect(validateLkgEntry(slot!, ["u0", "u1", "u2", "u3"])).toBe(false);
        expect(validateLkgEntry(slot!, ["u1", "u3", "u4"])).toBe(false);
        expect(validateLkgEntry(slot!, ["u1", "u2", "u3"])).toBe(true);
    });

    test("degraded passes decline capture and preserve the prior snapshot", () => {
        resetLkgSlotsForTest();
        captureSlot("session", {
            jsonPrefix: JSON.stringify([user("old", 1)]),
            inputIdSeq: ["old"],
            lastInputMessageId: "old",
            modelKey: "test/model",
            providerKey: "test",
            capturedAt: 1,
        });
        const outcome = createPassOutcome();
        outcome.record("session-meta-early-return", "fatal");
        outcome.markFinalized();
        expect(outcome.captureEligible).toBe(false);
        expect(getSlot("session")?.lastInputMessageId).toBe("old");
    });

    test("provider-visible fixture survives serializer round trip", () => {
        const fixture = [
            { role: "user", content: "inspect" },
            {
                role: "assistant",
                content: null,
                tool_calls: [
                    { id: "call-1", type: "function", function: { name: "read", arguments: "{}" } },
                ],
            },
            { role: "tool", tool_call_id: "call-1", content: "ok" },
        ];
        const roundTripped = JSON.parse(JSON.stringify(fixture));
        expect(roundTripped).toEqual(fixture);
        expect(validateLkgSeam(fixture as never, [], "openai")).toBe(true);
    });

    test("declines a seam that splits an unfinished tool run", () => {
        const prefix = [
            assistant("a1", 1, [{ type: "tool", callID: "call-1", state: { status: "running" } }]),
        ];
        const tail = [
            {
                info: { id: "tool-result", role: "tool" } as never,
                parts: [{ type: "tool_result", tool_call_id: "call-1", output: "result" }],
            } as MessageLike,
        ];
        expect(validateLkgSeam(prefix, tail, "openai")).toBe(false);
    });

    test("outermost handler rethrows emergency fail-closed errors", async () => {
        const handler = createMessagesTransformHandler({
            magicContext: {
                "experimental.chat.messages.transform": async () => {
                    throw new EmergencyFailClosedError("abort failed");
                },
            },
        });
        const output = { messages: [user("u0", 1)] } as never;
        await expect(handler({}, output)).rejects.toBeInstanceOf(EmergencyFailClosedError);
    });
});

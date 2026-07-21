import { describe, expect, test } from "bun:test";
import { type MuralSourceMemory, validateMuralManifest } from "./mural-prompt";
import { authorMuralWithRetry } from "./mural-task";

describe("mural author validation", () => {
    test("rejects an over-budget cue once and retries with the violating id", async () => {
        const source: MuralSourceMemory[] = [
            { id: 42, category: "NAMING", importance: 50, content: "Use the stable queue name." },
        ];
        const calls: string[] = [];
        const result = await authorMuralWithRetry({
            source,
            call: async (prompt) => {
                calls.push(prompt);
                return calls.length === 1
                    ? `<mural category="NAMING"><room name="Queue"><entry id="42" importance="50">${"x".repeat(51)}</entry></room></mural>`
                    : `<mural category="NAMING"><room name="Queue"><entry id="42" importance="50">stable queue name</entry></room></mural>`;
            },
        });
        expect(result[0]?.id).toBe(42);
        expect(calls).toHaveLength(2);
        expect(calls[1]).toContain("42");
    });

    test("requires a mechanism after a prohibition marker", () => {
        const source: MuralSourceMemory[] = [
            { id: 7, category: "CONSTRAINTS", importance: 80, content: "Do not write the cache." },
        ];
        expect(() =>
            validateMuralManifest(source, [
                {
                    id: 7,
                    category: "CONSTRAINTS",
                    room: "Cache",
                    importance: 80,
                    cue: "⊘cache write",
                },
            ]),
        ).toThrow(/mechanism/);
    });
});

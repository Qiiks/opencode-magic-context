import { describe, expect, it } from "bun:test";

import { buildVerifyPrompt, parseVerifyManifest, validateVerifyManifest } from "./verify-prompt";

describe("parseVerifyManifest", () => {
    it("parses verified / update / archive with attribute-order tolerance", () => {
        const text = `narration
<verify>
<verified id="1" files="a/b.ts,c/d.ts"/>
<update id="2" files="x.ts">X uses Y now</update>
<archive id="3" reason="the symbol no longer exists"/>
<verified files="z.ts" id="4"/>
</verify>`;
        const out = parseVerifyManifest(text);
        expect(out.verified).toEqual([
            { id: 1, files: ["a/b.ts", "c/d.ts"] },
            { id: 4, files: ["z.ts"] },
        ]);
        expect(out.updated).toEqual([{ id: 2, files: ["x.ts"], content: "X uses Y now" }]);
        expect(out.archived).toEqual([{ id: 3, reason: "the symbol no longer exists" }]);
    });

    it("handles a self-closing update (no content)", () => {
        const out = parseVerifyManifest(`<verify><update id="7" files="a.ts"/></verify>`);
        expect(out.updated).toEqual([{ id: 7, files: ["a.ts"], content: "" }]);
    });

    it("rejects a truncated manifest with no closing root", () => {
        expect(() => parseVerifyManifest(`<verify><archive id="9" reason="r"/>`)).toThrow(
            /closing root/,
        );
    });

    it("still accepts an empty verify body (no unrecognized children)", () => {
        expect(parseVerifyManifest(`<verify></verify>`)).toEqual({
            verified: [],
            updated: [],
            archived: [],
        });
    });

    it("rejects a well-formed root with no recognized entries", () => {
        expect(() => parseVerifyManifest(`<verify><item id="1"/></verify>`)).toThrow(
            /root <item> unrecognized; expected <verify> with <verified> entries/,
        );
        expect(() =>
            parseVerifyManifest(`<verify>[{ "id": 1, "status": "verified" }]</verify>`),
        ).toThrow(/JSON array unrecognized; expected <verify> with <verified> entries/);
    });

    it("rejects duplicate ids and invalid entries", () => {
        expect(() =>
            parseVerifyManifest(
                `<verify><verified id="9" files="a.ts"/><archive id="9" reason="r"/></verify>`,
            ),
        ).toThrow(/duplicate id/);
        expect(() =>
            parseVerifyManifest(`<verify><verified id="x" files="a.ts"/></verify>`),
        ).toThrow(/numeric id/);
    });
});

describe("validateVerifyManifest", () => {
    it("rejects an empty parse against a non-empty batch", () => {
        expect(() => validateVerifyManifest(`<verify></verify>`, new Set([1]))).toThrow(
            /parsed zero entries; expected <verify> with <verified> entries/,
        );
    });

    it("rejects missing and extra ids at retry time", () => {
        expect(() =>
            validateVerifyManifest(
                `<verify><verified id="1" files="a.ts"/></verify>`,
                new Set([1, 2]),
            ),
        ).toThrow(/missing id 2/);
        expect(() =>
            validateVerifyManifest(
                `<verify><verified id="1" files="a.ts"/><verified id="9" files="b.ts"/></verify>`,
                new Set([1]),
            ),
        ).toThrow(/unknown id 9/);
    });
});

describe("buildVerifyPrompt", () => {
    it("lists each memory with its backing files and instructs default-verified", () => {
        const prompt = buildVerifyPrompt("git:abc", [
            { id: 1, category: "ARCHITECTURE", content: "foo", mappedFiles: ["a.ts", "b.ts"] },
        ]);
        expect(prompt).toContain("[1] ARCHITECTURE");
        expect(prompt).toContain("Backing files: a.ts, b.ts");
        expect(prompt).toContain("default verified");
    });
});

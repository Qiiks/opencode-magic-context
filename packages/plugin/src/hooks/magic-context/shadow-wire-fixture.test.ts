/// <reference types="bun-types" />

import { describe, expect, it } from "bun:test";
import { readFileSync } from "node:fs";
import {
    generateShadowWireFixture,
    SHADOW_WIRE_FIXTURE_PATH,
} from "../../../scripts/generate-shadow-wire-fixture";

describe("shadow wire fixture", () => {
    it("matches the deterministic output of the real shadow payload builders", () => {
        expect(readFileSync(SHADOW_WIRE_FIXTURE_PATH, "utf8")).toBe(generateShadowWireFixture());
    });
});

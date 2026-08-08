import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import {
    ACTIVE_TOOL_IDS,
    measureAgentSurface,
    measureLightSurface,
    readLightSurface,
    PRIMARY_VARIANT_ID,
    TOKENIZER_ENCODING,
    TOKENIZER_PACKAGE,
    TOKENIZER_VERSION,
} from "./prompt-surface-measurement";

interface BudgetFixture {
    status?: string;
    policy?: { fraction?: number; expression?: string };
    tokenizer?: { package?: string; encoding?: string; version?: string; method?: string };
    primaryVariant?: { id?: string };
    activeTools?: string[];
    fullMeasurements?: {
        guidance?: Record<string, { chars: number; tokens: number }>;
        toolDescriptions?: Record<string, { chars: number; tokens: number }>;
        serializedParameterSchemas?: {
            [id: string]: { chars: number; tokens: number } | number | undefined;
            totalTokens?: number;
        };
        builtInProviderVisibleTotal?: number;
    };
    mutableProseBaseline?: number;
    integerLightCeiling?: number;
    lightMeasurement?: { status?: string };
}

export interface BudgetValidationResult {
    errors: string[];
    messages: string[];
}

function readFixture(path: string): BudgetFixture {
    return JSON.parse(readFileSync(resolve(path), "utf8")) as BudgetFixture;
}

function compareCount(
    errors: string[],
    label: string,
    expected: { chars: number; tokens: number } | undefined,
    actual: { chars: number; tokens: number } | undefined,
) {
    if (!expected || !actual || expected.chars !== actual.chars || expected.tokens !== actual.tokens) {
        errors.push(
            `${label} drifted: fixture=${expected ? `${expected.chars} chars/${expected.tokens} tokens` : "missing"}, source=${actual ? `${actual.chars} chars/${actual.tokens} tokens` : "missing"}`,
        );
    }
}

export function validateBudgetFixture(options: {
    fixturePath: string;
    lightSurfacePath?: string;
}): BudgetValidationResult {
    const fixture = readFixture(options.fixturePath);
    const surface = measureAgentSurface();
    const errors: string[] = [];
    const messages: string[] = [];

    if (fixture.tokenizer?.package !== TOKENIZER_PACKAGE) {
        errors.push(`tokenizer package must be ${TOKENIZER_PACKAGE}`);
    }
    if (fixture.tokenizer?.encoding !== TOKENIZER_ENCODING) {
        errors.push(`tokenizer encoding must be ${TOKENIZER_ENCODING}`);
    }
    if (fixture.tokenizer?.version !== TOKENIZER_VERSION) {
        errors.push(`tokenizer version must be ${TOKENIZER_VERSION}`);
    }
    if (fixture.tokenizer?.method !== surface.tokenizer.method) {
        errors.push(`tokenizer method drifted: expected ${surface.tokenizer.method}`);
    }
    if (fixture.primaryVariant?.id !== PRIMARY_VARIANT_ID) {
        errors.push(`primary variant must be ${PRIMARY_VARIANT_ID}`);
    }

    const fixtureTools = fixture.activeTools ?? [];
    if (JSON.stringify(fixtureTools) !== JSON.stringify([...ACTIVE_TOOL_IDS])) {
        errors.push(`active tool set drifted: fixture=${fixtureTools.join(",")}, source=${ACTIVE_TOOL_IDS.join(",")}`);
    }

    const guidanceMeasurements = fixture.fullMeasurements?.guidance ?? {};
    for (const row of surface.guidance) {
        compareCount(errors, `guidance ${row.id}`, guidanceMeasurements[row.id], row.full);
    }
    for (const id of Object.keys(guidanceMeasurements)) {
        if (!surface.guidance.some((row) => row.id === id)) {
            errors.push(`fixture contains unknown guidance variant ${id}`);
        }
    }

    const descriptionMeasurements = fixture.fullMeasurements?.toolDescriptions ?? {};
    for (const id of ACTIVE_TOOL_IDS) {
        compareCount(errors, `tool description ${id}`, descriptionMeasurements[id], surface.tools[id].description);
    }
    for (const id of Object.keys(descriptionMeasurements)) {
        if (!ACTIVE_TOOL_IDS.includes(id as (typeof ACTIVE_TOOL_IDS)[number])) {
            errors.push(`fixture contains unknown active tool ${id}`);
        }
    }

    const schemaMeasurements = fixture.fullMeasurements?.serializedParameterSchemas ?? {};
    let schemaTotal = 0;
    for (const id of ACTIVE_TOOL_IDS) {
        const expected = schemaMeasurements[id];
        const actual = surface.tools[id].serializedParameterSchema;
        if (typeof expected === "number") {
            errors.push(`serialized parameter schema ${id} must record chars and tokens, not only a number`);
        } else {
            compareCount(errors, `serialized parameter schema ${id}`, expected, actual);
        }
        schemaTotal += actual.tokens;
    }
    if (schemaMeasurements.totalTokens !== schemaTotal) {
        errors.push(`serialized parameter schema total drifted: fixture=${schemaMeasurements.totalTokens}, source=${schemaTotal}`);
    }

    const baseline = surface.primary.mutableProseBaseline;
    if (fixture.mutableProseBaseline !== baseline) {
        errors.push(`mutable-prose baseline drifted: fixture=${fixture.mutableProseBaseline}, source=${baseline}`);
    }
    const expectedCeiling = Math.floor(0.5 * baseline);
    if (fixture.policy?.fraction !== 0.5 || fixture.integerLightCeiling !== expectedCeiling) {
        errors.push(
            `integer light ceiling drifted: fixture=${fixture.integerLightCeiling}, expected floor(0.50 * ${baseline})=${expectedCeiling}`,
        );
    }
    const expectedProviderTotal = baseline + schemaTotal;
    if (fixture.fullMeasurements?.builtInProviderVisibleTotal !== expectedProviderTotal) {
        errors.push(
            `built-in provider-visible total drifted: fixture=${fixture.fullMeasurements?.builtInProviderVisibleTotal}, expected=${expectedProviderTotal}`,
        );
    }

    if (options.lightSurfacePath) {
        const light = measureLightSurface(readLightSurface(options.lightSurfacePath), surface);
        if (light.variant !== PRIMARY_VARIANT_ID) {
            errors.push(`light surface variant must be ${PRIMARY_VARIANT_ID}, got ${light.variant}`);
        }
        messages.push(`measured primary light mutable-prose total: ${light.mutableProseTotal} tokens`);
        messages.push(`ratified integer light ceiling: ${fixture.integerLightCeiling} tokens`);
        if (light.mutableProseTotal > (fixture.integerLightCeiling ?? -1)) {
            errors.push(
                `primary light mutable-prose total ${light.mutableProseTotal} exceeds ceiling ${fixture.integerLightCeiling}`,
            );
        } else {
            messages.push("primary light surface is at or below the ceiling");
        }
    } else if (fixture.lightMeasurement?.status === "PENDING-LIGHT-AUTHORING") {
        messages.push("light ceiling enforcement is armed; no light counts are fabricated before S3 authorship");
    } else {
        errors.push("light surface manifest is required once lightMeasurement is no longer pending");
    }

    if (errors.length === 0) {
        messages.unshift(`budget fixture matches source: baseline ${baseline}, ceiling ${expectedCeiling}`);
    }
    return { errors, messages };
}

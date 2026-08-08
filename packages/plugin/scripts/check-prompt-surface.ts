#!/usr/bin/env bun
/**
 * Validate the prompt-surface checklist and its fragment composition
 * map. Applicability is calculated from the source fragment's composedIn set and
 * sharedAcrossPresets flag; it is never accepted solely because a row declares a
 * status. The same command also validates the budget fixture when requested.
 *
 * Usage:
 *   bun packages/plugin/scripts/check-prompt-surface.ts
 *   bun packages/plugin/scripts/check-prompt-surface.ts --budget
 */
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { validateBudgetFixture } from "./prompt-surface-fixture";

const rootDir = resolve(import.meta.dir, "../../..");
const checklistPath = resolve(rootDir, "docs/specs/prompt-surface/checklist.json");
const budgetFixturePath = resolve(rootDir, "docs/specs/prompt-surface/budget-fixture.json");
const VALID_STATUSES = new Set(["compressed", "shared", "not-present"]);

type Checklist = {
    status?: string;
    mappingStatus?: string;
    variants?: Record<string, { kind?: string }>;
    fragments?: Record<string, {
        source?: { file?: string; evidence?: string };
        sharedAcrossPresets?: boolean;
        composedIn?: string[];
        statusByVariant?: Record<string, string>;
    }>;
    requiredRuleIds?: string[];
    rules?: Array<{
        id?: string;
        title?: string;
        sourceFragment?: string;
        scope?: string;
        polarity?: string;
        operativeCondition?: string;
        mechanism?: string;
        consequence?: string;
        evidence?: string;
    }>;
};

export interface ChecklistValidationResult {
    errors: string[];
    messages: string[];
}

function readChecklist(path = checklistPath): Checklist {
    return JSON.parse(readFileSync(path, "utf8")) as Checklist;
}

export function validateChecklist(path = checklistPath): ChecklistValidationResult {
    const checklist = readChecklist(path);
    const errors: string[] = [];
    const messages: string[] = [];
    const variants = checklist.variants ?? {};
    const fragments = checklist.fragments ?? {};
    const rules = checklist.rules ?? [];
    const requiredIds = checklist.requiredRuleIds ?? [];

    if (checklist.mappingStatus !== "PRE-LIGHT-AUTHORING") {
        errors.push("checklist mappingStatus must remain PRE-LIGHT-AUTHORING until S3 authors light prose");
    }
    if (new Set(requiredIds).size !== requiredIds.length) {
        errors.push("requiredRuleIds contains duplicates");
    }
    const ruleIds = rules.map((rule) => rule.id ?? "");
    if (new Set(ruleIds).size !== ruleIds.length) {
        errors.push("checklist rules contain duplicate IDs");
    }
    const missingRules = requiredIds.filter((id) => !ruleIds.includes(id));
    const unexpectedRules = ruleIds.filter((id) => !requiredIds.includes(id));
    if (missingRules.length > 0) errors.push(`checklist entries missing: ${missingRules.join(", ")}`);
    if (unexpectedRules.length > 0) errors.push(`checklist entries not designated in requiredRuleIds: ${unexpectedRules.join(", ")}`);

    const variantIds = Object.keys(variants);
    if (variantIds.length === 0) errors.push("fragment-to-variant composition map has no variants");
    const sourceTextCache = new Map<string, string>();
    for (const [fragmentId, fragment] of Object.entries(fragments)) {
        if (!fragment.source?.file || !fragment.source.evidence) {
            errors.push(`fragment ${fragmentId} is missing source file or evidence`);
            continue;
        }
        const sourcePath = resolve(rootDir, fragment.source.file);
        try {
            let source = sourceTextCache.get(sourcePath);
            if (!source) {
                source = readFileSync(sourcePath, "utf8");
                sourceTextCache.set(sourcePath, source);
            }
            if (!source.includes(fragment.source.evidence)) {
                errors.push(`fragment ${fragmentId} source evidence is stale: ${fragment.source.file}`);
            }
        } catch {
            errors.push(`fragment ${fragmentId} source file is unreadable: ${fragment.source.file}`);
        }

        const composedIn = new Set(fragment.composedIn ?? []);
        const statuses = fragment.statusByVariant ?? {};
        if (JSON.stringify(Object.keys(statuses).sort()) !== JSON.stringify([...variantIds].sort())) {
            errors.push(`fragment ${fragmentId} does not map every variant exactly once`);
        }
        for (const variantId of variantIds) {
            const expected = composedIn.has(variantId)
                ? fragment.sharedAcrossPresets === true
                    ? "shared"
                    : "compressed"
                : "not-present";
            const actual = statuses[variantId];
            if (!VALID_STATUSES.has(actual)) {
                errors.push(`fragment ${fragmentId}/${variantId} has invalid status ${actual}`);
            } else if (actual !== expected) {
                errors.push(`fragment ${fragmentId}/${variantId} disagrees with composition: declared=${actual}, derived=${expected}`);
            }
        }
        for (const variantId of composedIn) {
            if (!variants[variantId]) errors.push(`fragment ${fragmentId} references unknown variant ${variantId}`);
        }
    }

    const statuses = { compressed: 0, shared: 0, "not-present": 0 };
    for (const rule of rules) {
        const requiredFields = [
            "id",
            "title",
            "sourceFragment",
            "scope",
            "polarity",
            "operativeCondition",
            "mechanism",
            "consequence",
            "evidence",
        ] as const;
        for (const field of requiredFields) {
            if (!rule[field] || rule[field]?.trim() === "") errors.push(`rule ${rule.id ?? "<missing>"} is missing ${field}`);
        }
        const fragment = rule.sourceFragment ? fragments[rule.sourceFragment] : undefined;
        if (!fragment) {
            errors.push(`rule ${rule.id ?? "<missing>"} references unknown fragment ${rule.sourceFragment}`);
            continue;
        }
        const sourcePath = fragment.source?.file ? resolve(rootDir, fragment.source.file) : "";
        const source = sourceTextCache.get(sourcePath) ?? "";
        if (rule.evidence && !source.includes(rule.evidence)) {
            errors.push(`rule ${rule.id} evidence is stale: ${rule.evidence}`);
        }
        for (const variantId of variantIds) {
            const status = fragment.statusByVariant?.[variantId];
            if (!status || !VALID_STATUSES.has(status)) {
                errors.push(`rule ${rule.id}/${variantId} has no derived applicability status`);
            } else {
                statuses[status as keyof typeof statuses] += 1;
            }
        }
    }

    messages.push(`checked ${rules.length} checklist entries across ${variantIds.length} composed variants`);
    messages.push(`derived applicability: compressed=${statuses.compressed}, shared=${statuses.shared}, not-present=${statuses["not-present"]}`);
    messages.push("shared rows are byte-identity declarations; compressed rows await exact S3 light-line targets; not-present rows are source-derived absences");
    if (errors.length === 0) messages.unshift("checklist completeness and source mapping passed");
    return { errors, messages };
}

if (import.meta.main) {
    const budgetMode = process.argv.includes("--budget");
    try {
        const checklist = validateChecklist();
        for (const message of checklist.messages) console.log(message);
        const errors = [...checklist.errors];
        if (budgetMode) {
            const budget = validateBudgetFixture({ fixturePath: budgetFixturePath });
            for (const message of budget.messages) console.log(message);
            errors.push(...budget.errors);
        }
        for (const error of errors) console.error(`ERROR: ${error}`);
        process.exitCode = errors.length > 0 ? 1 : 0;
    } catch (error) {
        console.error(error instanceof Error ? error.message : String(error));
        process.exitCode = 1;
    }
}

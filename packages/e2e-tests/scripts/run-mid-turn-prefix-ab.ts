import { writeFile } from "node:fs/promises";
import {
    formatAbTable,
    runLiveAnthropicAbExperiment,
    runMockAbExperiment,
    type CacheTtl,
    type MidTurnExperimentOptions,
} from "../src/mid-turn-prefix-ab";

function integerEnv(name: string): number | undefined {
    const raw = process.env[name]?.trim();
    if (!raw) return undefined;
    const value = Number(raw);
    if (!Number.isInteger(value)) throw new Error(`${name} must be an integer`);
    return value;
}

const args = new Set(process.argv.slice(2));
const live = args.has("--live");
const jsonOnly = args.has("--json");
const ttl = (process.env.MC_MIDTURN_AB_TTL?.trim() || "5m") as CacheTtl;
if (ttl !== "5m" && ttl !== "1h") throw new Error("MC_MIDTURN_AB_TTL must be 5m or 1h");

const options: MidTurnExperimentOptions = {
    steps: integerEnv("MC_MIDTURN_AB_STEPS"),
    applyAtStep: integerEnv("MC_MIDTURN_AB_APPLY_AT"),
    dropChars: integerEnv("MC_MIDTURN_AB_DROP_CHARS"),
    ttl,
    model: process.env.MC_MIDTURN_AB_MODEL?.trim() || undefined,
};

const result = live
    ? await runLiveAnthropicAbExperiment(options)
    : await runMockAbExperiment(options);
const json = `${JSON.stringify(result, null, 2)}\n`;
const outputPath = process.env.MC_MIDTURN_AB_OUTPUT?.trim();
if (outputPath) await writeFile(outputPath, json, "utf8");

if (jsonOnly) {
    process.stdout.write(json);
} else {
    console.log(`mid-turn prefix A/B mode=${result.mode} ttl=${result.options.ttl}`);
    console.log(formatAbTable(result));
    if (outputPath) console.log(`raw records: ${outputPath}`);
}

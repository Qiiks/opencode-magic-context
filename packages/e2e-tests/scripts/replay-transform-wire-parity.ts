#!/usr/bin/env bun

import { resolve } from "node:path";
import { runPairedSessionReplay } from "../src/paired-session-replay";

function valueAfter(flag: string): string | undefined {
    const index = Bun.argv.indexOf(flag);
    return index === -1 ? undefined : Bun.argv[index + 1];
}

const fixturePath = valueAfter("--fixture");
const providerArm = valueAfter("--provider-arm");
const providerID = valueAfter("--provider-id");
const output = await runPairedSessionReplay({
    ...(fixturePath ? { fixturePath: resolve(fixturePath) } : {}),
    ...(providerArm ? { providerArm } : {}),
    ...(providerID ? { providerID } : {}),
});
console.log(JSON.stringify(output, null, 2));
process.exit(output.unadjudicated_divergence_count > 0 ? 1 : 0);

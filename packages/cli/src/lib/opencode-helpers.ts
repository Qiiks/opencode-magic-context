import { execFileSync, execSync } from "node:child_process";
import type { OpenCodeInstallation } from "./opencode-detect";

/**
 * Run `opencode <args>`. If a `binary` path is given (an absolute path resolved
 * for a stock `~/.opencode/bin` install or a version-manager shim that is not on
 * PATH), call that exact path via execFile; otherwise fall back to a bare
 * `opencode` on PATH.
 */
function runOpenCode(args: string[], binary?: string | null, timeoutMs?: number): string | null {
    try {
        const options = { stdio: "pipe" as const, ...(timeoutMs ? { timeout: timeoutMs } : {}) };
        if (binary) {
            return execFileSync(binary, args, options).toString().trim();
        }
        return execSync(`opencode ${args.join(" ")}`, options)
            .toString()
            .trim();
    } catch {
        return null;
    }
}

/**
 * Version probes must be bounded because a broken shim can wait forever. The
 * doctor probes every detected install, so a per-process timeout is important.
 */
export const OPENCODE_VERSION_PROBE_TIMEOUT_MS = 2_000;

export function getOpenCodeVersion(binary?: string | null): string | null {
    return runOpenCode(["--version"], binary, OPENCODE_VERSION_PROBE_TIMEOUT_MS);
}

export interface OpenCodeInstallationReport extends OpenCodeInstallation {
    version: string;
    active: boolean;
}

/** Probe versions for all detected installs, retaining the detection order. */
export function describeOpenCodeInstallations(
    installations: OpenCodeInstallation[],
): OpenCodeInstallationReport[] {
    return installations.map((installation, index) => ({
        ...installation,
        version:
            installation.kind === "cli"
                ? (getOpenCodeVersion(installation.path) ?? "unknown")
                : "unknown",
        active: index === 0,
    }));
}

export function getAvailableModels(binary?: string | null): string[] {
    const output = runOpenCode(["models"], binary);
    if (output === null) return [];
    return output
        .split("\n")
        .map((l) => l.trim())
        .filter(Boolean);
}

import { afterEach, describe, expect, it } from "bun:test";
import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { detectOpenCodeInstallations } from "./opencode-detect";
import {
    describeOpenCodeInstallations,
    getAvailableModels,
    getOpenCodeVersion,
    OPENCODE_VERSION_PROBE_TIMEOUT_MS,
} from "./opencode-helpers";

// These assert that a RESOLVED absolute binary path is actually invoked (the
// #196 follow-up: a stock CLI not on PATH must still enumerate). POSIX-only:
// the test writes an executable shell stub, which CI runs on Linux/macOS.
const isPosix = process.platform !== "win32";
const tempDirs: string[] = [];

afterEach(() => {
    for (const dir of tempDirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

function fakeOpencode(body: string): string {
    const dir = mkdtempSync(join(tmpdir(), "mc-oc-bin-"));
    tempDirs.push(dir);
    const bin = join(dir, "opencode");
    writeFileSync(bin, `#!/bin/sh\n${body}\n`);
    chmodSync(bin, 0o755);
    return bin;
}

function fakeHomeOpencode(body: string): { home: string; bin: string } {
    const home = mkdtempSync(join(tmpdir(), "mc-oc-home-"));
    tempDirs.push(home);
    const bin = join(home, ".opencode", "bin", "opencode");
    mkdirSync(dirname(bin), { recursive: true });
    writeFileSync(bin, `#!/bin/sh\n${body}\n`);
    chmodSync(bin, 0o755);
    return { home, bin };
}

describe.if(isPosix)("opencode helpers with a resolved binary path", () => {
    it("getAvailableModels invokes the given absolute binary", () => {
        const bin = fakeOpencode(
            'if [ "$1" = "models" ]; then printf "anthropic/claude-opus-4-8\\nopenai/gpt-5.5\\n"; fi',
        );
        expect(getAvailableModels(bin)).toEqual(["anthropic/claude-opus-4-8", "openai/gpt-5.5"]);
    });

    it("getOpenCodeVersion invokes the given absolute binary", () => {
        const bin = fakeOpencode('if [ "$1" = "--version" ]; then echo "1.2.3"; fi');
        expect(getOpenCodeVersion(bin)).toBe("1.2.3");
    });

    it("enumerates versions for both installs and marks PATH as active", () => {
        const pathBin = fakeOpencode('echo "1.18.0"');
        const homeInstall = fakeHomeOpencode('echo "1.15.13"');
        const installations = detectOpenCodeInstallations({
            exists: () => false,
            isExecutable: (path) => path === pathBin || path === homeInstall.bin,
            home: homeInstall.home,
            platform: "darwin",
            env: {},
            onPath: () => pathBin,
            realpath: (path) => path,
        });
        expect(describeOpenCodeInstallations(installations)).toEqual([
            { path: pathBin, source: "PATH", kind: "cli", version: "1.18.0", active: true },
            {
                path: homeInstall.bin,
                source: "home-bin",
                kind: "cli",
                version: "1.15.13",
                active: false,
            },
        ]);
    });

    it("bounds a hanging version probe", () => {
        const bin = fakeOpencode("sleep 5");
        const started = performance.now();
        expect(getOpenCodeVersion(bin)).toBeNull();
        expect(performance.now() - started).toBeLessThan(OPENCODE_VERSION_PROBE_TIMEOUT_MS + 1_500);
    });

    it("returns empty / null when the binary path does not exist", () => {
        const missing = join(tmpdir(), "definitely-not-a-real-opencode-binary-xyz");
        expect(getAvailableModels(missing)).toEqual([]);
        expect(getOpenCodeVersion(missing)).toBeNull();
    });
});

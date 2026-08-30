import { mkdtempSync, readdirSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";

const STALE_TEMP_DIR_AGE_MS = 60 * 60 * 1000;

// Keep this list narrow: startup cleanup must never turn into a shared /tmp purge.
export const TEST_TEMP_DIR_PREFIXES = [
    "mc-plugin-test-xdg-pid-",
    "mc-pi-test-xdg-",
    "mc-conflict-",
    "mc-conflict-fixer-",
    "mc-config-secret-",
    "pi-reasoning-replay-",
    "pi-session-api-test-",
    "pi-symlink-test-",
    "pi-running-vs-stale-",
    "pi-dist-metadata-",
    "pi-traversal-",
    "pi-source-mode-",
    "pi-source-missing-",
    "pi-nonprefixed-",
    "pi-default-cond-",
    "pi-array-exports-",
    "pi-extensionless-",
    "pi-tsx-source-",
    "pi-guidance-",
    "pi-system-v2-",
    "pi-no-guidance-",
    "pi-guidance-dedup-",
    "pi-noreduce-",
    "pi-language-baseline-",
    "pi-language-unset-",
    "pi-language-set-",
    "pi-a1-guidance-",
    "pi-prompt-epoch-",
    "pi-embedding-bootstrap-",
    "pi-embedding-home-",
    "magic-context-pi-latch-test-",
    "magic-context-pi-index-test-",
    "mc-test-temp-dir-helper-",
] as const;

export type TestTempDirPrefix = (typeof TEST_TEMP_DIR_PREFIXES)[number];

type AfterAll = (callback: () => void) => void;
type SweepOptions = {
    tempDir?: string;
    nowMs?: number;
    staleAgeMs?: number;
};

const registeredTempDirs = new Set<string>();
let lifecycleCleanupInstalled = false;

function hasRecognizedPrefix(name: string): boolean {
    return TEST_TEMP_DIR_PREFIXES.some((prefix) => name.startsWith(prefix));
}

function isRegisteredTempDir(directory: string): boolean {
    const resolvedDirectory = resolve(directory);
    return (
        registeredTempDirs.has(resolvedDirectory) &&
        dirname(resolvedDirectory) === resolve(tmpdir()) &&
        hasRecognizedPrefix(basename(resolvedDirectory))
    );
}

/**
 * Creates a test-owned root under the system temp directory. The returned cleanup
 * only removes roots made by this module, so an accidental path cannot delete an
 * unrelated temp directory.
 */
export function createTestTempDir(
    prefix: TestTempDirPrefix,
    nameSuffix = "",
): { dir: string; cleanup: () => void } {
    const dir = resolve(mkdtempSync(join(tmpdir(), `${prefix}${nameSuffix}`)));
    registeredTempDirs.add(dir);
    return { dir, cleanup: () => cleanupTestTempDir(dir) };
}

/** Removes one registered test root and forgets it even when best-effort removal fails. */
export function cleanupTestTempDir(directory: string): void {
    const resolvedDirectory = resolve(directory);
    if (!isRegisteredTempDir(resolvedDirectory)) return;

    registeredTempDirs.delete(resolvedDirectory);
    try {
        rmSync(resolvedDirectory, {
            recursive: true,
            force: true,
            maxRetries: 10,
            retryDelay: 100,
        });
    } catch {
        // A process can still hold a file open on Windows. The next startup sweep retries it.
    }
}

/** Removes every root registered by this process. Safe to call more than once. */
export function cleanupRegisteredTestTempDirs(): void {
    for (const directory of [...registeredTempDirs]) cleanupTestTempDir(directory);
}

/**
 * Runs a callback with a fresh root and removes that root even when the callback throws.
 * This keeps small fixture builders from forgetting a failure-path cleanup.
 */
export function withTestTempDir<T>(
    prefix: TestTempDirPrefix,
    callback: (directory: string) => T,
): T {
    const { dir, cleanup } = createTestTempDir(prefix);
    try {
        return callback(dir);
    } finally {
        cleanup();
    }
}

/**
 * Installs both Bun's normal suite cleanup and an exit fallback for aborted runs.
 * The callback is passed in so this utility remains usable from the test preload
 * without making production code import bun:test.
 */
export function installTestTempDirCleanup(afterAll: AfterAll): void {
    if (lifecycleCleanupInstalled) return;
    lifecycleCleanupInstalled = true;
    afterAll(cleanupRegisteredTestTempDirs);
    process.once("exit", cleanupRegisteredTestTempDirs);
}

/**
 * Removes only old, recognized test roots from one temp directory. The scan does
 * one directory read, ignores non-directories, and leaves fresh roots and every
 * unrecognized name alone so concurrently running tools stay isolated.
 */
export function sweepStaleTestTempDirs({
    tempDir = tmpdir(),
    nowMs = Date.now(),
    staleAgeMs = STALE_TEMP_DIR_AGE_MS,
}: SweepOptions = {}): string[] {
    let entries: Array<{ isDirectory(): boolean; name: string }>;
    try {
        entries = readdirSync(tempDir, { withFileTypes: true });
    } catch {
        return [];
    }

    const removed: string[] = [];
    for (const entry of entries) {
        if (!entry.isDirectory() || !hasRecognizedPrefix(entry.name)) continue;

        const directory = join(tempDir, entry.name);
        try {
            if (nowMs - statSync(directory).mtimeMs <= staleAgeMs) continue;
            rmSync(directory, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
            removed.push(directory);
        } catch {
            // Temp cleanup is best-effort; a later test run retries busy directories.
        }
    }
    return removed;
}

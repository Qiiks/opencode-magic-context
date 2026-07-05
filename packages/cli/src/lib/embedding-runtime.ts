import { existsSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join, sep } from "node:path";

/**
 * Detects whether the local-embedding native runtime (`onnxruntime-node`) is
 * actually present and usable in an installed plugin tree.
 *
 * Why this check exists (issue #128): the plugin's `@huggingface/transformers`
 * Node entry does a STATIC `import "onnxruntime-node"`. When that package — or
 * its platform-specific native binary — failed to install (seen on Windows when
 * the binary download is interrupted), the import throws
 * `Cannot find package 'onnxruntime-node'` on EVERY embedding attempt, and the
 * runtime can only degrade. Doctor surfaces it ahead of time with an actionable
 * fix instead of leaving users to decode the cryptic resolver error.
 */

export type LocalEmbeddingRuntimeStatus =
    | { state: "ok"; binaryPath: string }
    | { state: "package-missing"; packageDir: string }
    | { state: "binary-missing"; packageDir: string; expectedBinary: string }
    | { state: "load-failed"; packageDir: string; reason: string }
    | { state: "unknown"; reason: string };

export type BrokenLocalEmbeddingRuntimeStatus = Extract<
    LocalEmbeddingRuntimeStatus,
    { state: "package-missing" | "binary-missing" | "load-failed" }
>;

function describeError(error: unknown): string {
    const message = error instanceof Error ? error.message : String(error ?? "unknown error");
    const code = (error as { code?: unknown } | null)?.code;
    return typeof code === "string" && code.length > 0 ? `${code}: ${message}` : message;
}

function probeOnnxRuntimeNodeLoad(packageDir: string): LocalEmbeddingRuntimeStatus | null {
    try {
        const req = createRequire(join(packageDir, "package.json"));
        req(packageDir);
        return null;
    } catch (error) {
        return { state: "load-failed", packageDir, reason: describeError(error) };
    }
}

export function isLocalEmbeddingRuntimeBroken(
    status: LocalEmbeddingRuntimeStatus,
): status is BrokenLocalEmbeddingRuntimeStatus {
    return (
        status.state === "package-missing" ||
        status.state === "binary-missing" ||
        status.state === "load-failed"
    );
}

export function formatLocalEmbeddingRuntimeDoctorWarning(
    status: BrokenLocalEmbeddingRuntimeStatus,
): string {
    const cause =
        status.state === "package-missing"
            ? "package is not installed"
            : status.state === "binary-missing"
              ? "expected platform binding file is absent"
              : `binding failed to load: ${status.reason}`;
    return (
        "Embedding provider: local — onnxruntime-node native binding missing — " +
        `${cause}; its postinstall likely failed. Embeddings will not work. ` +
        "Reinstall with network access to the npm registry and GitHub releases, " +
        "or switch `embedding.provider` to an HTTP endpoint (`openai-compatible`)."
    );
}

/**
 * Maps `process.platform`/`process.arch` to onnxruntime-node's on-disk binary
 * layout: `bin/napi-v6/<platform>/<arch>/onnxruntime_binding.node`. The dir
 * names match Node's platform/arch tokens directly (linux/darwin/win32,
 * x64/arm64), so no translation is needed beyond filtering to what ships.
 */
function expectedBinaryRelPath(platform: NodeJS.Platform, arch: string): string | null {
    const supportedPlatform = platform === "linux" || platform === "darwin" || platform === "win32";
    const supportedArch = arch === "x64" || arch === "arm64";
    if (!supportedPlatform || !supportedArch) return null;
    return join("bin", "napi-v6", platform, arch, "onnxruntime_binding.node");
}

/**
 * Check a single install root (the directory that owns `node_modules`) for a
 * usable onnxruntime-node. `npm`/Bun hoist transitive deps, so the package lands
 * at `<installRoot>/node_modules/onnxruntime-node`.
 */
export function checkLocalEmbeddingRuntimeAt(
    installRoot: string,
    platform: NodeJS.Platform = process.platform,
    arch: string = process.arch,
): LocalEmbeddingRuntimeStatus {
    const packageDir = join(installRoot, "node_modules", "onnxruntime-node");
    if (!existsSync(join(packageDir, "package.json"))) {
        return { state: "package-missing", packageDir };
    }
    const rel = expectedBinaryRelPath(platform, arch);
    if (rel === null) {
        // Unknown platform/arch — the package is present, but a direct package
        // load can still prove whether its own native-loader path works.
        return probeOnnxRuntimeNodeLoad(packageDir) ?? { state: "ok", binaryPath: packageDir };
    }
    const binaryPath = join(packageDir, rel);
    if (!existsSync(binaryPath)) {
        return { state: "binary-missing", packageDir, expectedBinary: binaryPath };
    }
    return probeOnnxRuntimeNodeLoad(packageDir) ?? { state: "ok", binaryPath };
}

/**
 * Check across candidate install roots (a plugin can be cached under
 * `@pkg@latest/...` or `@pkg/...`). Returns the first `ok`; otherwise the first
 * informative failure; `unknown` only when no candidate root even exists (we
 * can't introspect the install, so we stay silent rather than false-alarm).
 */
export function checkLocalEmbeddingRuntime(
    installRoots: string[],
    platform: NodeJS.Platform = process.platform,
    arch: string = process.arch,
): LocalEmbeddingRuntimeStatus {
    const existing = installRoots.filter((root) => existsSync(root));
    if (existing.length === 0) {
        return {
            state: "unknown",
            reason: "no installed plugin tree found to inspect",
        };
    }
    let firstFailure: LocalEmbeddingRuntimeStatus | null = null;
    for (const root of existing) {
        const status = checkLocalEmbeddingRuntimeAt(root, platform, arch);
        if (status.state === "ok") return status;
        if (firstFailure === null) firstFailure = status;
    }
    return firstFailure ?? { state: "unknown", reason: "no candidate roots" };
}

/** Slice a resolved module path back to its package directory (the dir that
 *  owns `node_modules/<pkg>`), so we can locate the platform binary relative to
 *  it regardless of how deep the resolved entry (`dist/index.js`) sits. */
function packageDirFromResolved(resolvedPath: string, packageName: string): string {
    const marker = `node_modules${sep}${packageName.split("/").join(sep)}`;
    const idx = resolvedPath.indexOf(marker);
    return idx >= 0 ? resolvedPath.slice(0, idx + marker.length) : dirname(resolvedPath);
}

/**
 * Resolution-based variant for harnesses whose install layout is NOT a single
 * deterministic `<root>/node_modules/onnxruntime-node` (Pi: dev-path bun
 * workspace, npm-hoisted user/project install, or pnpm strict store — verified
 * empirically that the physical path differs across all three). Instead of
 * guessing a path, it asks Node's resolver exactly as the plugin would at
 * runtime: resolve onnxruntime-node FROM the installed plugin dir, then locate
 * the platform binary relative to the resolved package.
 *
 * Two resolution attempts, both layout-agnostic:
 *   A. resolve `onnxruntime-node` directly from the plugin (works when hoisted
 *      or visible to the plugin — npm/bun default).
 *   B. resolve `@huggingface/transformers` (a direct plugin dep that OWNS
 *      onnxruntime-node), then resolve onnxruntime-node from THERE — covers
 *      pnpm-strict where the transitive dep isn't visible to the plugin itself.
 *
 * Returns `unknown` (caller stays SILENT) when the plugin dir doesn't exist or
 * neither resolution succeeds in a way we can introspect — never a false alarm.
 */
export function checkLocalEmbeddingRuntimeByResolution(
    pluginDir: string,
    platform: NodeJS.Platform = process.platform,
    arch: string = process.arch,
): LocalEmbeddingRuntimeStatus {
    if (!existsSync(join(pluginDir, "package.json"))) {
        return { state: "unknown", reason: "plugin package dir not found" };
    }

    let onnxDir: string | null = null;
    let resolveError: string | undefined;
    try {
        const reqPlugin = createRequire(join(pluginDir, "package.json"));
        try {
            // A: direct (hoisted / bun / npm)
            onnxDir = packageDirFromResolved(
                reqPlugin.resolve("onnxruntime-node"),
                "onnxruntime-node",
            );
        } catch {
            // B: through the transformers package that owns it (pnpm strict)
            const tfResolved = reqPlugin.resolve("@huggingface/transformers");
            const tfDir = packageDirFromResolved(tfResolved, "@huggingface/transformers");
            const reqTf = createRequire(join(tfDir, "package.json"));
            onnxDir = packageDirFromResolved(reqTf.resolve("onnxruntime-node"), "onnxruntime-node");
        }
    } catch (error) {
        // Read `.code` directly off the thrown object — do NOT gate on
        // `instanceof Error`: Bun's resolver throws a `ResolveMessage` that is
        // NOT an Error instance (code "MODULE_NOT_FOUND"), Node throws
        // "ERR_MODULE_NOT_FOUND" (ESM) / "MODULE_NOT_FOUND" (CJS createRequire).
        resolveError = (error as { code?: string } | null)?.code;
        // onnxruntime-node genuinely not resolvable from the installed plugin =
        // the #128 missing-package case (only meaningful because we confirmed
        // the plugin dir exists above).
        if (resolveError === "ERR_MODULE_NOT_FOUND" || resolveError === "MODULE_NOT_FOUND") {
            return {
                state: "package-missing",
                packageDir: join(pluginDir, "node_modules", "onnxruntime-node"),
            };
        }
        return {
            state: "unknown",
            reason: `could not resolve onnxruntime-node (${resolveError ?? "unknown error"})`,
        };
    }

    if (!onnxDir) {
        return { state: "unknown", reason: "onnxruntime-node resolution produced no path" };
    }

    const rel = expectedBinaryRelPath(platform, arch);
    if (rel === null) {
        // Unknown platform/arch — package resolves, but a direct package load can
        // still prove whether its own native-loader path works.
        return probeOnnxRuntimeNodeLoad(onnxDir) ?? { state: "ok", binaryPath: onnxDir };
    }
    const binaryPath = join(onnxDir, rel);
    if (!existsSync(binaryPath)) {
        return { state: "binary-missing", packageDir: onnxDir, expectedBinary: binaryPath };
    }
    return probeOnnxRuntimeNodeLoad(onnxDir) ?? { state: "ok", binaryPath };
}

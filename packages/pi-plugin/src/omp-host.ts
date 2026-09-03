/**
 * OMP host surface loader.
 *
 * Locates the running OMP host's structured-subagent spawn API
 * (`runStructuredSubagent`) so `OmpSubagentRunner` can spawn MC subagents
 * in-process through OMP's native task machinery instead of headless
 * `--print` subprocesses (see `omp-subagent-runner.ts` and
 * cortexkit/magic-context#416).
 *
 * Dynamic imports are REQUIRED here, not style drift (same pattern as
 * `dreamer/pi-session-api.ts`'s module ladder): the module specifier is
 * genuinely runtime-selected — it is derived from the running OMP host's
 * install location, and a static import of `@oh-my-pi/pi-coding-agent`
 * would execute host-native addons at extension-bundle load time even on
 * non-OMP hosts where the package may be absent. Runtime loading also lets
 * a failure resolve to `null` (fallback to the subprocess runner) instead
 * of failing the plugin boot.
 *
 * OMP ships its `src/` tree raw and maps package subpaths via the
 * `"./*": { import: "./src/*.ts" }` exports wildcard, so
 * `@oh-my-pi/pi-coding-agent/task/structured-subagent` resolves to the same
 * source module the running CLI executes. The module must be imported
 * in-process (inside the OMP extension host) — standalone imports fail on
 * native-addon loading outside the host process, which is fine: this loader
 * only runs inside OMP where the addon is already loaded.
 *
 * Resolution order (memoized, never throws):
 *  1. Walk up from the running entry (`process.argv[1]`, then
 *     `process.execPath`) to the `@oh-my-pi/pi-coding-agent` package root
 *     (same walking rules as `subagent-runner.ts`'s `isOmpHostProcess`), then
 *     import `task/structured-subagent` directly from its `src/` tree.
 *  2. Bare ESM subpath import of
 *     `@oh-my-pi/pi-coding-agent/task/structured-subagent`.
 *
 * Any failure resolves to `null`; callers fall back to the subprocess
 * runner. This module deliberately throws nothing.
 */

import { existsSync, readFileSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const OMP_PACKAGE_NAME = "@oh-my-pi/pi-coding-agent";
const STRUCTURED_SUBAGENT_SUBPATH = "task/structured-subagent";

/** The in-process spawn surface MC needs from the OMP host. */
export interface OmpSubagentSurface {
	runStructuredSubagent: (request: Record<string, unknown>) => Promise<{
		result: {
			exitCode: number;
			output: string;
			stderr: string;
			truncated: boolean;
			error?: string;
			aborted?: boolean;
			durationMs: number;
			tokens?: number;
			usage?: { input?: number; output?: number; cacheWrite?: number; cacheRead?: number };
			resolvedModel?: string;
		};
	}>;
	/** OMP's `Settings` class — the synthetic ToolSession requires a live instance. */
	Settings: {
		init: (options?: { cwd?: string }) => Promise<{
			reloadFromDisk: () => Promise<void>;
			get: (key: string) => unknown;
		}>;
	};
	/** OMP's credential storage resolver (`discoverAuthStorage` from ./sdk). */
	discoverAuthStorage: (agentDir?: string) => Promise<unknown>;
}

export interface OmpSurfaceLoadResult {
	surface: OmpSubagentSurface | null;
	/** Human-readable failure reason when `surface` is null (for logging). */
	reason?: string;
}

let cachedResult: Promise<OmpSurfaceLoadResult> | null = null;

/** Reset the memoized loader (test seam). */
export function clearOmpSurfaceCache(): void {
	cachedResult = null;
}

function readPackageName(packageJsonPath: string): string | null {
	try {
		const manifest = JSON.parse(readFileSync(packageJsonPath, "utf8")) as {
			name?: unknown;
		};
		return typeof manifest.name === "string" ? manifest.name : null;
	} catch {
		return null;
	}
}

/**
 * Walk up from `startDir` looking for the OMP package root. Mirrors the
 * containment-safe walking in `subagent-runner.ts` (`isOmpHostProcess`).
 */
function findOmpPackageRoot(startDir: string): string | null {
	let current = startDir;
	// eslint-disable-next-line no-constant-condition
	while (true) {
		const manifestPath = join(current, "package.json");
		if (existsSync(manifestPath) && readPackageName(manifestPath) === OMP_PACKAGE_NAME) {
			return current;
		}
		const parent = dirname(current);
		if (parent === current) return null;
		current = parent;
	}
}

async function importFromFile(filePath: string): Promise<unknown> {
	return await import(pathToFileURL(filePath).href);
}

/**
 * Resolve the structured-subagent module from the running OMP entry. The
 * exports wildcard maps `./task/structured-subagent` → `./src/task/
 * structured-subagent.ts`, so importing the source file directly is the same
 * module graph edge the host itself uses.
 */
async function loadFromRunningEntry(): Promise<{ surface: OmpSubagentSurface } | { error: string }> {
	if (process.env.JITI_VIRTUAL_SCRIPT_PREFIX && process.argv[1]?.startsWith?.(process.env.JITI_VIRTUAL_SCRIPT_PREFIX)) {
		return { error: "running entry is a Jiti virtual module" };
	}
	const entryCandidates = [process.argv[1], process.execPath].filter(
		(candidate): candidate is string => typeof candidate === "string" && candidate.length > 0,
	);
	for (const candidate of entryCandidates) {
		let startDir: string;
		try {
			const stat = statSync(candidate);
			startDir = stat.isFile() ? dirname(resolve(candidate)) : resolve(candidate);
		} catch {
			continue;
		}
		const packageRoot = findOmpPackageRoot(startDir);
		if (!packageRoot) continue;
		const moduleEntry = join(packageRoot, "src", "task", "structured-subagent.ts");
		if (!existsSync(moduleEntry)) {
			return { error: `${OMP_PACKAGE_NAME} found at ${packageRoot} but src/${STRUCTURED_SUBAGENT_SUBPATH}.ts is missing` };
		}
		try {
			const mod = (await importFromFile(moduleEntry)) as Record<string, unknown>;
			return await extractSurface(mod);
		} catch (error) {
			return {
				error: `import of ${moduleEntry} failed: ${error instanceof Error ? error.message : String(error)}`,
			};
		}
	}
	return { error: `no ${OMP_PACKAGE_NAME} package root found from running entry` };
}

async function extractSurface(mod: Record<string, unknown>): Promise<{ surface: OmpSubagentSurface } | { error: string }> {
	const run = mod.runStructuredSubagent;
	if (typeof run !== "function") {
		return { error: "module loaded but runStructuredSubagent is not exported" };
	}
	// Settings lives at config/settings (re-exported from the package index);
	// discoverAuthStorage at ./sdk. The structured-subagent module itself does
	// not re-export them, so load them from the same package root the spawn
	// surface came from — they must be the same copies the host runs.
	let Settings: OmpSubagentSurface["Settings"] | undefined;
	let discoverAuthStorage: OmpSubagentSurface["discoverAuthStorage"] | undefined;
	const searchRoots: string[] = [];
	if (process.argv[1]) {
		const fromEntry = findOmpPackageRoot(dirname(resolve(process.argv[1])));
		if (fromEntry) searchRoots.push(fromEntry);
	}
	const fromModule = findOmpPackageRoot(dirname(fileURLToPath(import.meta.url)));
	if (fromModule && !searchRoots.includes(fromModule)) searchRoots.push(fromModule);
	for (const root of searchRoots) {
		try {
			const settingsMod = (await importFromFile(join(root, "src", "config", "settings.ts"))) as Record<string, unknown>;
			const sdkMod = (await importFromFile(join(root, "src", "sdk.ts"))) as Record<string, unknown>;
			const settingsClass = settingsMod.Settings as OmpSubagentSurface["Settings"] | undefined;
			const authResolver = sdkMod.discoverAuthStorage as OmpSubagentSurface["discoverAuthStorage"] | undefined;
			if (typeof settingsClass?.init === "function" && typeof authResolver === "function") {
				Settings = settingsClass;
				discoverAuthStorage = authResolver;
				break;
			}
		} catch {
			// Try the next root.
		}
	}
	if (!Settings || !discoverAuthStorage) {
		return { error: "module loaded but Settings/discoverAuthStorage could not be resolved from the OMP package" };
	}
	return { surface: { runStructuredSubagent: run as OmpSubagentSurface["runStructuredSubagent"], Settings, discoverAuthStorage } };
}

/**
 * Load the OMP structured-subagent surface. Memoized; never throws. Resolves
 * to `{ surface: null, reason }` on any failure so callers fall back to the
 * subprocess runner.
 */
export function loadOmpSubagentSurface(): Promise<OmpSurfaceLoadResult> {
	cachedResult ??= (async (): Promise<OmpSurfaceLoadResult> => {
		// 1. Running-entry walk (same OMP copy that owns the live session).
		const fromEntry = await loadFromRunningEntry();
		if ("surface" in fromEntry) return fromEntry;

		// 2. Walk up from THIS module's own install location. Under a real
		// OMP install the plugin lives at
		// `~/.omp/plugins/node_modules/@cortexkit/pi-magic-context/`, whose
		// ancestor `node_modules` also hosts `@oh-my-pi/pi-coding-agent` —
		// importing the subpath from there loads the exact copy the host runs.
		const here = fileURLToPath(import.meta.url);
		const packageRoot = findOmpPackageRoot(dirname(here));
		if (packageRoot) {
			const moduleEntry = join(packageRoot, "src", "task", "structured-subagent.ts");
			if (existsSync(moduleEntry)) {
				try {
					const mod = (await importFromFile(moduleEntry)) as Record<string, unknown>;
					const extracted = await extractSurface(mod);
					if ("surface" in extracted) return extracted;
					return { surface: null, reason: extracted.error };
				} catch (error) {
					return {
						surface: null,
						reason: `${fromEntry.error}; sibling-package import failed: ${error instanceof Error ? error.message : String(error)}`,
					};
				}
			}
		}

		// 3. Bare ESM subpath (covers non-standard launch shapes).
		try {
			const mod = (await import(
				/* @vite-ignore */ `${OMP_PACKAGE_NAME}/${STRUCTURED_SUBAGENT_SUBPATH}`
			)) as Record<string, unknown>;
			const extracted = await extractSurface(mod);
			if ("surface" in extracted) return extracted;
			return { surface: null, reason: extracted.error };
		} catch (error) {
			return {
				surface: null,
				reason: `${fromEntry.error}; bare subpath import failed: ${error instanceof Error ? error.message : String(error)}`,
			};
		}
	})();
	return cachedResult;
}

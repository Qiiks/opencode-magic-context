import { lstat, readlink, realpath } from "node:fs/promises";
import { homedir } from "node:os";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { ProviderError } from "./errors";

export interface ResolveProviderPathOptions {
    allowMissing: boolean;
    homeDirectory?: string;
    cwd?: string;
}

export async function resolveAndFenceProviderPath(
    configuredPath: string,
    options: ResolveProviderPathOptions,
): Promise<string> {
    const configuredHomePath = resolve(options.homeDirectory ?? process.env.HOME ?? homedir());
    let home: string;
    try {
        home = await realpath(configuredHomePath);
    } catch (error) {
        throw fsError(configuredHomePath, error);
    }
    const expanded = configuredPath.startsWith("~/")
        ? join(home, configuredPath.slice(2))
        : configuredPath === "~"
          ? home
          : configuredPath;
    const absolute = isAbsolute(expanded)
        ? resolve(expanded)
        : resolve(options.cwd ?? process.cwd(), expanded);
    const canonical = await canonicalPath(absolute, options.allowMissing);
    if (isFencedPath(canonical, home)) {
        throw new ProviderError("fenced_path", `Refusing fenced path: ${canonical}`);
    }
    return canonical;
}

async function canonicalPath(path: string, allowMissing: boolean): Promise<string> {
    try {
        return await realpath(path);
    } catch (error) {
        if (!allowMissing || !isMissingError(error)) {
            throw fsError(path, error);
        }

        const suffix: string[] = [];
        let candidate = path;
        while (true) {
            try {
                const metadata = await lstat(candidate);
                if (metadata.isSymbolicLink()) {
                    const target = await readlink(candidate);
                    const resolvedTarget = resolve(dirname(candidate), target);
                    return canonicalPath(join(resolvedTarget, ...suffix), true);
                }
            } catch (candidateError) {
                if (!isMissingError(candidateError)) {
                    throw fsError(path, candidateError);
                }
            }

            const parent = dirname(candidate);
            if (parent === candidate) {
                throw fsError(path, error);
            }
            suffix.unshift(basename(candidate));
            candidate = parent;
            try {
                return join(await realpath(candidate), ...suffix);
            } catch (parentError) {
                if (!isMissingError(parentError)) {
                    throw fsError(path, parentError);
                }
            }
        }
    }
}

export function isFencedPath(canonicalPath: string, homeDirectory: string): boolean {
    const home = resolve(homeDirectory);
    const cortexkitRoot = join(home, ".local", "share", "cortexkit");
    const relativeToCortexkit = relative(cortexkitRoot, canonicalPath);
    const insideCortexkit =
        relativeToCortexkit !== "" &&
        relativeToCortexkit !== ".." &&
        !relativeToCortexkit.startsWith(`..${sep}`) &&
        !isAbsolute(relativeToCortexkit);
    const parts = insideCortexkit ? relativeToCortexkit.split(sep) : [];
    const pathParts = canonicalPath.split(sep).filter(Boolean);
    const name = basename(canonicalPath);

    const catalogDirectoryCarveIn = pathParts.includes("catalog");
    const moduleBinCarveIn = parts.length >= 2 && parts[1] === "bin";
    const catalogJsonCarveIn = name.endsWith(".json") && name.includes("catalog");
    if (catalogDirectoryCarveIn || moduleBinCarveIn || catalogJsonCarveIn) {
        return false;
    }

    const inFencedRoot =
        insideCortexkit && ["plexus", "claustrum", "staging"].includes(parts[0] ?? "");
    const fencedBasename = name.includes("binding-key") || name.endsWith(".handle");
    const plexusStore = insideCortexkit && parts[0] === "plexus" && name.startsWith("store.db");
    return inFencedRoot || fencedBasename || plexusStore;
}

function fsError(path: string, error: unknown): ProviderError {
    const message = error instanceof Error ? error.message : String(error);
    return new ProviderError("unreadable_path", `Could not read ${path}: ${message}`);
}

function isMissingError(error: unknown): boolean {
    return (
        error !== null &&
        typeof error === "object" &&
        "code" in error &&
        ((error as { code?: unknown }).code === "ENOENT" ||
            (error as { code?: unknown }).code === "ENOTDIR")
    );
}

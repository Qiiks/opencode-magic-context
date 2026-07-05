import { afterEach, describe, expect, it, mock } from "bun:test";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import path, { join } from "node:path";
import {
    __clearProjectIdentityTransientCooldownForTests,
    __resetProjectIdentityForTests,
    __setProjectIdentityTestHooks,
    normalizeStoredProjectPath,
    ProjectIdentityError,
    resolveProjectIdentity,
    resolveProjectIdentityStrict,
    storedPathBelongsToIdentity,
    takeDubiousOwnershipProjectIdentityWarning,
} from "./project-identity";

const tempDirs: string[] = [];

afterEach(() => {
    __resetProjectIdentityForTests();
    for (const dir of tempDirs) {
        try {
            chmodSync(dir, 0o755);
        } catch {
            // Ignore cleanup permission restoration failures.
        }
        rmSync(dir, { recursive: true, force: true });
    }
    tempDirs.length = 0;
});

function makeTempDir(prefix: string): string {
    const dir = mkdtempSync(join(tmpdir(), prefix));
    tempDirs.push(dir);
    return dir;
}

function runGit(directory: string, args: string[]): string {
    return execFileSync("git", args, {
        cwd: directory,
        encoding: "utf8",
        env: { ...process.env, LC_ALL: "C", LANG: "C" },
        stdio: ["ignore", "pipe", "pipe"],
    });
}

function makeGitRepo(): string {
    const dir = makeTempDir("project-identity-git-");
    runGit(dir, ["init"]);
    writeFileSync(join(dir, "README.md"), "# test\n", "utf8");
    runGit(dir, ["add", "README.md"]);
    runGit(dir, [
        "-c",
        "user.email=test@example.com",
        "-c",
        "user.name=Test User",
        "-c",
        "commit.gpgsign=false",
        "commit",
        "-m",
        "initial commit",
    ]);
    return dir;
}

function rootCommit(directory: string): string {
    return runGit(directory, ["rev-list", "--max-parents=0", "HEAD"]).split("\n")[0]!.trim();
}

function expectedDirIdentity(directory: string): string {
    return `dir:${createHash("md5")
        .update(path.resolve(directory), "utf8")
        .digest("hex")
        .slice(0, 12)}`;
}

function expectProjectIdentityError(fn: () => void): ProjectIdentityError {
    let caught: unknown;
    try {
        fn();
    } catch (error) {
        caught = error;
    }

    expect(caught).toBeInstanceOf(ProjectIdentityError);
    if (!(caught instanceof ProjectIdentityError)) {
        throw new Error("Expected ProjectIdentityError");
    }
    return caught;
}

function makeGitFailure(fields: {
    stderr?: string;
    code?: string;
    signal?: string;
    killed?: boolean;
}) {
    const error = new Error("git failed") as Error & {
        stderr?: Buffer;
        code?: string;
        signal?: string;
        killed?: boolean;
    };
    if (fields.stderr !== undefined) error.stderr = Buffer.from(fields.stderr, "utf8");
    if (fields.code !== undefined) error.code = fields.code;
    if (fields.signal !== undefined) error.signal = fields.signal;
    if (fields.killed !== undefined) error.killed = fields.killed;
    return error;
}

describe("project identity", () => {
    it("resolveProjectIdentityStrict returns the git root commit identity", () => {
        const repo = makeGitRepo();
        const commit = rootCommit(repo);

        const identity = resolveProjectIdentityStrict(repo);

        expect(identity).toBe(`git:${commit}`);
        expect(identity.slice("git:".length).length).toBeGreaterThanOrEqual(7);
    });

    it("resolveProjectIdentityStrict throws not_git_repo for non-git directories", () => {
        const directory = makeTempDir("project-identity-non-git-");

        const error = expectProjectIdentityError(() => resolveProjectIdentityStrict(directory));

        expect(error.errorClass).toBe("not_git_repo");
        expect(error.rawDirectory).toBe(directory);
    });

    it("resolveProjectIdentityStrict classifies missing directories without falling back", () => {
        const directory = makeTempDir("project-identity-missing-");
        rmSync(directory, { recursive: true, force: true });

        const error = expectProjectIdentityError(() => resolveProjectIdentityStrict(directory));

        expect(["permission_denied", "unknown"]).toContain(error.errorClass);
        expect(error.rawDirectory).toBe(directory);
    });

    it("resolveProjectIdentityStrict caches git identities across calls", () => {
        const repo = makeGitRepo();
        const first = resolveProjectIdentityStrict(repo);

        chmodSync(repo, 0o000);
        try {
            expect(resolveProjectIdentityStrict(repo)).toBe(first);
        } finally {
            chmodSync(repo, 0o755);
        }
    });

    it("resolveProjectIdentity falls back to dir identity for non-git directories", () => {
        const directory = makeTempDir("project-identity-wrapper-");

        expect(resolveProjectIdentity(directory)).toBe(expectedDirIdentity(directory));
    });

    it("uses the no-git fast path without spawning git", () => {
        const directory = makeTempDir("project-identity-no-git-fast-");
        const execMock = mock(() => {
            throw new Error("git should not be spawned for a directory with no .git ancestor");
        });
        __setProjectIdentityTestHooks({ execFileSync: execMock as unknown as typeof execFileSync });

        expect(resolveProjectIdentity(directory)).toBe(expectedDirIdentity(directory));
        expect(execMock).not.toHaveBeenCalled();
    });

    it("detects git metadata in ancestors for subdirectory sessions", () => {
        const repo = makeGitRepo();
        const subdir = join(repo, "nested", "session");
        mkdirSync(subdir, { recursive: true });

        expect(resolveProjectIdentity(subdir)).toBe(`git:${rootCommit(repo)}`);
    });

    it("classifies dubious ownership and falls back until the cooldown expires", () => {
        const directory = makeTempDir("project-identity-dubious-");
        mkdirSync(join(directory, ".git"));
        let now = 1_000;
        let recovered = false;
        const execMock = mock(() => {
            if (recovered) return "abcdef1234567890\n";
            throw makeGitFailure({
                stderr:
                    "fatal: detected dubious ownership in repository at '/repo'\n" +
                    "To add an exception for this directory, call:\n",
            });
        });
        __setProjectIdentityTestHooks({
            execFileSync: execMock as unknown as typeof execFileSync,
            nowMs: () => now,
        });

        const strictError = expectProjectIdentityError(() =>
            resolveProjectIdentityStrict(directory),
        );
        expect(strictError.errorClass).toBe("dubious_ownership");

        expect(resolveProjectIdentity(directory)).toBe(expectedDirIdentity(directory));
        expect(takeDubiousOwnershipProjectIdentityWarning(directory)).toContain(
            "git config --global --add safe.directory",
        );
        expect(takeDubiousOwnershipProjectIdentityWarning(directory)).toBeNull();

        recovered = true;
        expect(resolveProjectIdentity(directory)).toBe(expectedDirIdentity(directory));
        expect(execMock).toHaveBeenCalledTimes(2);

        now += 5 * 60 * 1000 + 1;
        expect(resolveProjectIdentity(directory)).toBe("git:abcdef1234567890");
        expect(execMock).toHaveBeenCalledTimes(3);
    });

    it("cools down git timeouts so immediate retries do not respawn git", () => {
        const directory = makeTempDir("project-identity-timeout-");
        mkdirSync(join(directory, ".git"));
        const execMock = mock(() => {
            throw makeGitFailure({ code: "ETIMEDOUT" });
        });
        __setProjectIdentityTestHooks({ execFileSync: execMock as unknown as typeof execFileSync });

        expect(resolveProjectIdentity(directory)).toBe(expectedDirIdentity(directory));
        expect(resolveProjectIdentity(directory)).toBe(expectedDirIdentity(directory));
        expect(execMock).toHaveBeenCalledTimes(1);
    });

    it("re-probes after a transient cooldown is cleared", () => {
        const directory = makeTempDir("project-identity-clear-cooldown-");
        mkdirSync(join(directory, ".git"));
        let recovered = false;
        const execMock = mock(() => {
            if (recovered) return "abcdef1234567890\n";
            throw makeGitFailure({ code: "ETIMEDOUT" });
        });
        __setProjectIdentityTestHooks({ execFileSync: execMock as unknown as typeof execFileSync });

        expect(resolveProjectIdentity(directory)).toBe(expectedDirIdentity(directory));
        recovered = true;
        __clearProjectIdentityTransientCooldownForTests(directory);

        expect(resolveProjectIdentity(directory)).toBe("git:abcdef1234567890");
        expect(execMock).toHaveBeenCalledTimes(2);
    });

    it("still propagates permission_denied git failures", () => {
        const directory = makeTempDir("project-identity-permission-");
        mkdirSync(join(directory, ".git"));
        __setProjectIdentityTestHooks({
            execFileSync: mock(() => {
                throw makeGitFailure({ code: "EACCES" });
            }) as unknown as typeof execFileSync,
        });

        const error = expectProjectIdentityError(() => resolveProjectIdentity(directory));

        expect(error.errorClass).toBe("permission_denied");
    });

    it("normalizeStoredProjectPath returns stored identities unchanged", () => {
        expect(normalizeStoredProjectPath("git:not-a-filesystem-path")).toBe(
            "git:not-a-filesystem-path",
        );
        expect(normalizeStoredProjectPath("dir:abcdef123456")).toBe("dir:abcdef123456");
    });

    it("normalizeStoredProjectPath resolves raw filesystem paths through the wrapper", () => {
        const directory = makeTempDir("project-identity-normalize-");

        expect(normalizeStoredProjectPath(directory)).toBe(expectedDirIdentity(directory));
    });

    it("storedPathBelongsToIdentity matches on exact identity and on normalized raw path", () => {
        // Exact stored-identity match.
        expect(storedPathBelongsToIdentity("git:abc123", "git:abc123")).toBe(true);
        expect(storedPathBelongsToIdentity("dir:deadbeef", "dir:deadbeef")).toBe(true);
        // Mismatched identity.
        expect(storedPathBelongsToIdentity("git:abc123", "git:other")).toBe(false);
        // A raw filesystem path stored before normalization must still match the
        // identity it normalizes to (the #11 case Pi previously rejected).
        const directory = makeTempDir("project-identity-belongs-");
        const identity = expectedDirIdentity(directory);
        expect(storedPathBelongsToIdentity(directory, identity)).toBe(true);
        expect(storedPathBelongsToIdentity(directory, "dir:not-this-one")).toBe(false);
    });

    it("ProjectIdentityError carries classification and raw directory fields", () => {
        const cause = new Error("inner");
        const error = new ProjectIdentityError("git_timeout", "/raw/path", "timed out", cause);

        expect(error.name).toBe("ProjectIdentityError");
        expect(error.errorClass).toBe("git_timeout");
        expect(error.rawDirectory).toBe("/raw/path");
        expect(error.cause).toBe(cause);
    });
});

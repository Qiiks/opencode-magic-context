import { afterEach, describe, expect, test } from "bun:test";
import type { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, realpathSync, rmSync } from "node:fs";
import { homedir, tmpdir } from "node:os";
import { join } from "node:path";
import {
    __resetProjectIdentityForTests,
    __setProjectIdentityTestHooks,
    resolveProjectIdentity,
    resolveProjectIdentityForSession,
} from "./project-identity";

function tempDir(): string {
    return mkdtempSync(join(tmpdir(), "mc-identity-"));
}

function returningRootCommit(rootCommit: string): typeof execFileSync {
    return (() => `${rootCommit}\n`) as typeof execFileSync;
}

afterEach(() => {
    __resetProjectIdentityForTests();
});

describe("resolveProjectIdentity directory fallback", () => {
    test("refuses the exact canonical home directory unless the user opts in", () => {
        expect(resolveProjectIdentityForSession(homedir())).toBeUndefined();
        expect(resolveProjectIdentityForSession(join(homedir(), "a-project"))).not.toBeUndefined();
    });

    test("uses the canonical home directory's stable dir identity when opted in", () => {
        const canonicalHome = realpathSync.native(homedir());
        const expected = `dir:${createHash("md5").update(canonicalHome, "utf8").digest("hex").slice(0, 12)}`;

        expect(resolveProjectIdentityForSession(homedir(), true)).toBe(expected);
    });

    test("never lets a home identity match a contained directory", () => {
        const contained = mkdtempSync(join(homedir(), "mc-home-identity-"));
        try {
            const homeIdentity = resolveProjectIdentityForSession(homedir(), true);
            const containedIdentity = resolveProjectIdentityForSession(contained, true);

            expect(homeIdentity).toBeDefined();
            expect(containedIdentity).toBeDefined();
            expect(containedIdentity).not.toBe(homeIdentity);
        } finally {
            rmSync(contained, { recursive: true, force: true });
        }
    });
    test("flips dir: fallback to git: once a repo gains its first commit (no stale cache)", () => {
        const dir = tempDir();
        try {
            const first = resolveProjectIdentity(dir);
            expect(first).toMatch(/^dir:[0-9a-f]{12}$/);
            expect(resolveProjectIdentity(dir)).toBe(first);

            mkdirSync(join(dir, ".git"));
            __setProjectIdentityTestHooks({ execFileSync: returningRootCommit("abc1234") });

            const second = resolveProjectIdentity(dir);
            expect(second).toBe("git:abc1234");
            expect(second).not.toBe(first);
            expect(resolveProjectIdentity(dir)).toBe(second);
        } finally {
            rmSync(dir, { recursive: true, force: true });
        }
    });

    test("derives a deterministic identity from grafted-history repos (multiple root commits)", () => {
        const dir = tempDir();
        try {
            mkdirSync(join(dir, ".git"));
            // Repos merged with --allow-unrelated-histories keep several live root
            // commits, and git's enumeration order varies by traversal. The identity
            // must be the lexicographic minimum of the SET, not the first line.
            __setProjectIdentityTestHooks({
                execFileSync: (() => "7e96b9e\n1e394c2\n4058752\n") as typeof execFileSync,
            });
            expect(resolveProjectIdentity(dir)).toBe("git:1e394c2");
        } finally {
            rmSync(dir, { recursive: true, force: true });
        }
    });

    test("reuses a parent repository identity for subdirectory transient git failures", () => {
        const dir = tempDir();
        try {
            mkdirSync(join(dir, ".git"));
            __setProjectIdentityTestHooks({ execFileSync: returningRootCommit("def5678") });
            const parentIdentity = resolveProjectIdentity(dir);
            const subdir = join(dir, "nested", "child");
            mkdirSync(subdir, { recursive: true });

            __setProjectIdentityTestHooks({
                execFileSync: (() => {
                    throw new Error("temporary git failure");
                }) as typeof execFileSync,
            });

            expect(resolveProjectIdentity(subdir)).toBe(parentIdentity);
        } finally {
            rmSync(dir, { recursive: true, force: true });
        }
    });
});

import { describe, expect, it } from "bun:test";
import { summarizeChildStderr } from "./summarize-child-stderr";

function bundledCrashStderr(): { sourceLine: string; stderr: string } {
    const sourceLine = `263 | ${"FROM cacheInterceptorV${VERSION2} ".repeat(100)}`.slice(0, 3_000);
    return {
        sourceLine,
        stderr: [
            sourceLine,
            "^",
            "SqliteError: database is locked",
            "    at openDatabase (bundle.js:100:20)",
            "    at runHistorian (runner.js:12:4)",
            "    at main (cli.js:2:1)",
        ].join("\n"),
    };
}

describe("summarizeChildStderr", () => {
    it("keeps an error line and the first two stack frames after bundled source context", () => {
        const { sourceLine, stderr } = bundledCrashStderr();

        const excerpt = summarizeChildStderr(stderr);

        expect(excerpt).toContain("SqliteError: database is locked");
        expect(excerpt).toContain("at openDatabase (bundle.js:100:20)");
        expect(excerpt).toContain("at runHistorian (runner.js:12:4)");
        expect(excerpt).not.toContain("at main (cli.js:2:1)");
        expect(excerpt.startsWith(sourceLine)).toBe(false);
    });

    it("passes through short stderr without an error-shaped line unchanged", () => {
        const stderr = "auth failure: missing API key";

        expect(summarizeChildStderr(stderr)).toBe(stderr);
    });

    it("keeps the tail of long chatty stderr when no error line is present", () => {
        const stderr = `${"head-only\n".repeat(2_000)}${"tail-only\n".repeat(2_000)}`;
        const excerpt = summarizeChildStderr(stderr);

        expect(stderr.length).toBeGreaterThan(16_000);
        expect(excerpt).toBe(stderr.slice(-500));
        expect(excerpt).toContain("tail-only");
        expect(excerpt).not.toContain("head-only");
    });

    it("recognizes Node's bracketed error code form", () => {
        expect(
            summarizeChildStderr(
                "Error [ERR_MODULE_NOT_FOUND]: Cannot find package\n    at loader (node:internal/modules:1:1)",
            ),
        ).toContain("Error [ERR_MODULE_NOT_FOUND]: Cannot find package");
    });
});

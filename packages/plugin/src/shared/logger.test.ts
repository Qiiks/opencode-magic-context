import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

type LoggerScenarioResult = {
    exists: boolean;
    content: string;
    healthyDiagnostics: {
        swallowedWriteCount: number;
        lastErrorMessage: string | null;
        lastErrorTime: string | null;
    };
    failedDiagnostics: {
        swallowedWriteCount: number;
        lastErrorMessage: string | null;
        lastErrorTime: string | null;
    };
};

type RotationScenarioResult = {
    firstCurrent: string;
    firstPredecessorFirstByte: number;
    current: string;
    predecessorFirstByte: number;
    predecessorSize: number;
    currentSize: number;
    currentMode: number;
    predecessorMode: number;
    files: string[];
};

type HotPathScenarioResult = {
    statCalls: number;
};

const loggerScenario = `
import fs from "node:fs";
import * as path from "node:path";
import { mock } from "bun:test";

const root = process.env.LOGGER_SCENARIO_ROOT;
const loggerModuleUrl = process.env.LOGGER_MODULE_URL;
const scenario = process.env.LOGGER_SCENARIO;
if (!root || !loggerModuleUrl || !scenario) {
    throw new Error("logger scenario environment is incomplete");
}

const logPath = path.join(root, "nested", "magic-context.log");
process.env.MAGIC_CONTEXT_LOG_PATH = logPath;
let statCalls = 0;
if (scenario === "hot-path") {
    mock.module("node:fs", () => ({
        ...fs,
        statSync: (...args) => {
            statCalls++;
            return fs.statSync(...args);
        },
    }));
}

const logger = await import(loggerModuleUrl);

if (scenario === "rotation") {
    const maxLogBytes = 32 * 1024 * 1024;
    fs.mkdirSync(path.dirname(logPath), { recursive: true });
    fs.writeFileSync(logPath, Buffer.alloc(maxLogBytes, "o"));
    fs.chmodSync(logPath, 0o600);
    fs.writeFileSync(logPath + ".1", "prior predecessor");

    logger.log("after first rotation");
    logger.flushLogger();
    const firstCurrent = fs.readFileSync(logPath, "utf8");
    const firstPredecessorFirstByte = fs.readFileSync(logPath + ".1")[0];

    // Switch paths once so the logger rechecks the primary path after this
    // external replacement, as a different plugin process would require.
    fs.writeFileSync(logPath, Buffer.alloc(maxLogBytes, "n"));
    process.env.MAGIC_CONTEXT_LOG_PATH = path.join(root, "alternate.log");
    logger.log("alternate path");
    logger.flushLogger();
    process.env.MAGIC_CONTEXT_LOG_PATH = logPath;
    logger.log("after second rotation");
    logger.flushLogger();

    const predecessorPath = logPath + ".1";
    console.log(JSON.stringify({
        firstCurrent,
        firstPredecessorFirstByte,
        current: fs.readFileSync(logPath, "utf8"),
        predecessorFirstByte: fs.readFileSync(predecessorPath)[0],
        predecessorSize: fs.statSync(predecessorPath).size,
        currentSize: fs.statSync(logPath).size,
        currentMode: fs.statSync(logPath).mode & 0o777,
        predecessorMode: fs.statSync(predecessorPath).mode & 0o777,
        files: fs.readdirSync(path.dirname(logPath)).filter((name) => name.startsWith("magic-context.log")).sort(),
    }));
} else if (scenario === "hot-path") {
    for (let index = 0; index < 20; index++) {
        logger.log("hot-path-" + index);
        logger.flushLogger();
    }
    console.log(JSON.stringify({ statCalls }));
} else {
    logger.log("first");
    logger.log(
        "tokens.input=45000 api_key=sk-proj-abcdefghijklmnopqrstuvwxyzABCDEFGH",
        { authorization: "Bearer abcdefghijklmnop", totalInputTokens: 132000 },
    );
    logger.flushLogger();
    const healthyDiagnostics = logger.getLoggerDiagnostics();

    const logDirectory = path.dirname(logPath);
    if (scenario === "recovery") {
        fs.rmSync(logDirectory, { recursive: true, force: true });
        logger.log("second");
        logger.flushLogger();
    }

    // A directory used as the file target makes append fail deterministically on every platform.
    const failedPath = path.join(root, "unwritable-log-target");
    fs.mkdirSync(failedPath, { recursive: true });
    process.env.MAGIC_CONTEXT_LOG_PATH = failedPath;
    logger.log("failed write");
    logger.flushLogger();
    const failedDiagnostics = logger.getLoggerDiagnostics();

    const content = fs.existsSync(logPath) ? fs.readFileSync(logPath, "utf8") : "";
    console.log(JSON.stringify({
        exists: fs.existsSync(logPath),
        content,
        healthyDiagnostics,
        failedDiagnostics,
    }));
}
`;

const scenarioRoots: string[] = [];

afterEach(() => {
    for (const root of scenarioRoots.splice(0)) {
        rmSync(root, { recursive: true, force: true });
    }
});

async function runLoggerScenario<T>(
    scenario: "recovery" | "diagnostics" | "rotation" | "hot-path",
): Promise<T> {
    const root = mkdtempSync(path.join(os.tmpdir(), "magic-context-logger-test-"));
    scenarioRoots.push(root);
    const child = Bun.spawn({
        cmd: ["bun", "--eval", loggerScenario],
        cwd: import.meta.dir,
        env: {
            ...process.env,
            NODE_ENV: "production",
            LOGGER_MODULE_URL: new URL("./logger.ts", import.meta.url).href,
            LOGGER_SCENARIO: scenario,
            LOGGER_SCENARIO_ROOT: root,
        },
        stdout: "pipe",
        stderr: "pipe",
    });
    const [exitCode, stdout, stderr] = await Promise.all([
        child.exited,
        new Response(child.stdout).text(),
        new Response(child.stderr).text(),
    ]);
    expect(exitCode, stderr).toBe(0);
    return JSON.parse(stdout.trim()) as T;
}

describe("logger", () => {
    test("recreates a log directory removed while the process is running", async () => {
        const result = await runLoggerScenario<LoggerScenarioResult>("recovery");

        expect(result.exists).toBe(true);
        expect(result.content).toContain("second");
    });

    test("redacts secrets while preserving numeric diagnostics", async () => {
        const result = await runLoggerScenario<LoggerScenarioResult>("diagnostics");

        expect(result.content).toContain("tokens.input=45000");
        expect(result.content).toContain('"totalInputTokens":132000');
        expect(result.content).toContain("<REDACTED:api_key>");
        expect(result.content).toContain("<REDACTED:authorization>");
        expect(result.content).not.toContain("abcdefghijklmnopqrstuvwxyzABCDEFGH");
        expect(result.content).not.toContain("abcdefghijklmnop");
    });

    test("rotates at the 32 MiB bound with one private predecessor", async () => {
        const result = await runLoggerScenario<RotationScenarioResult>("rotation");
        const maxLogBytes = 32 * 1024 * 1024;

        expect(result.firstCurrent).toContain("after first rotation");
        expect(result.firstPredecessorFirstByte).toBe("o".charCodeAt(0));
        expect(result.current).toContain("after second rotation");
        expect(result.predecessorFirstByte).toBe("n".charCodeAt(0));
        expect(result.predecessorSize).toBeLessThanOrEqual(maxLogBytes);
        expect(result.currentSize).toBeLessThanOrEqual(maxLogBytes);
        expect(result.files).toEqual(["magic-context.log", "magic-context.log.1"]);
        if (process.platform !== "win32") {
            expect(result.currentMode).toBe(0o600);
            expect(result.predecessorMode).toBe(0o600);
        }
    });

    test("uses cached size between periodic rotation checks", async () => {
        const result = await runLoggerScenario<HotPathScenarioResult>("hot-path");

        expect(result.statCalls).toBeLessThanOrEqual(1);
    });

    test("reports swallowed writes while healthy writes leave the counter at zero", async () => {
        const result = await runLoggerScenario<LoggerScenarioResult>("diagnostics");

        expect(result.healthyDiagnostics).toEqual({
            swallowedWriteCount: 0,
            lastErrorMessage: null,
            lastErrorTime: null,
        });
        expect(result.failedDiagnostics.swallowedWriteCount).toBe(1);
        expect(result.failedDiagnostics.lastErrorMessage).toBeTruthy();
        expect(result.failedDiagnostics.lastErrorTime).toMatch(
            /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/,
        );
    });
});

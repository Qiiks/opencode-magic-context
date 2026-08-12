/// <reference types="bun-types" />

import { afterEach, describe, expect, test } from "bun:test";
import type { execFileSync } from "node:child_process";
import type { readFileSync } from "node:fs";
import {
    __resetRpcIdentityTestHooks,
    __setRpcIdentityTestHooks,
    discoverLivePiProcessIds,
    inspectLivePiProcesses,
    isPidAlive,
    isPidIdentityPlausible,
    type RpcPortFileRecord,
} from "./rpc-utils";

const PID = 1234;
const NOW_MS = 2_000_000;
const UPTIME_SECONDS = 1_000;

function record(startedAt: number): RpcPortFileRecord {
    return { port: 43123, pid: PID, started_at: startedAt };
}

function procStat(startTimeTicks: number): string {
    // After the closing command-name parenthesis, field 3 is `state` and field
    // 22 is the twentieth value in the suffix.
    return `${PID} (opencode) S ${Array.from({ length: 18 }, () => "0").join(" ")} ${startTimeTicks}`;
}

function linuxFiles(files: Record<string, string | Error>): typeof readFileSync {
    return ((path: string | URL) => {
        const value = files[String(path)];
        if (value instanceof Error) throw value;
        if (value === undefined) throw new Error(`unexpected read: ${String(path)}`);
        return value;
    }) as typeof readFileSync;
}

function psOutput(output: string | Error): typeof execFileSync {
    return (() => {
        if (output instanceof Error) throw output;
        return output;
    }) as typeof execFileSync;
}

afterEach(() => {
    __resetRpcIdentityTestHooks();
});

describe("discoverLivePiProcessIds", () => {
    test("finds Pi-family harness commands while excluding the current process", () => {
        __setRpcIdentityTestHooks({
            processListExecFileSync: (() =>
                [
                    ` ${process.pid} /usr/local/bin/pi`,
                    " 41001 /usr/local/bin/pi --model test",
                    " 41002 node /opt/node_modules/@mariozechner/pi-coding-agent/dist/cli.js",
                    " 41003 bun /opt/node_modules/@oh-my-pi/pi-coding-agent/dist/cli.js",
                    " 41004 /Applications/OpenCode.app/Contents/MacOS/opencode",
                    " 41005 node /workspace/pi-plugin/src/index.ts",
                    " 41006 npm install @earendil-works/pi-coding-agent",
                    " 41007 /usr/local/bin/omp --model test",
                ].join("\n")) as typeof execFileSync,
        });

        expect(discoverLivePiProcessIds()).toEqual([41001, 41002, 41003, 41007]);
    });

    test("reports uncertainty instead of treating an unavailable process list as empty", () => {
        __setRpcIdentityTestHooks({
            processListExecFileSync: (() => {
                throw new Error("ps unavailable");
            }) as typeof execFileSync,
        });

        expect(inspectLivePiProcesses()).toEqual({
            state: "unreadable",
            processIds: [],
            error: "ps unavailable",
        });
    });
});

describe("isPidAlive", () => {
    test("distinguishes confirmed, dead, and inaccessible PID probes", () => {
        const probeFailure = (code: string): NodeJS.ErrnoException => {
            const error = new Error(`kill failed: ${code}`) as NodeJS.ErrnoException;
            error.code = code;
            return error;
        };

        __setRpcIdentityTestHooks({
            processKill: (() => true) as typeof process.kill,
        });
        expect(isPidAlive(PID)).toBe("alive");

        __setRpcIdentityTestHooks({
            processKill: (() => {
                throw probeFailure("ESRCH");
            }) as typeof process.kill,
        });
        expect(isPidAlive(PID)).toBe("dead");

        __setRpcIdentityTestHooks({
            processKill: (() => {
                throw probeFailure("EPERM");
            }) as typeof process.kill,
        });
        expect(isPidAlive(PID)).toBe("inconclusive");
    });
});

describe("isPidIdentityPlausible", () => {
    test("rejects a reused Linux PID when proc start time is substantially newer", () => {
        const readPaths: string[] = [];
        __setRpcIdentityTestHooks({
            platform: "linux",
            nowMs: () => NOW_MS,
            readFileSync: ((path: string | URL) => {
                readPaths.push(String(path));
                const files = {
                    [`/proc/${PID}/stat`]: procStat(10_000),
                    "/proc/uptime": `${UPTIME_SECONDS}.0 0.0`,
                };
                return files[String(path) as keyof typeof files];
            }) as typeof readFileSync,
            execFileSync: (() => {
                throw new Error("ps must not run on Linux");
            }) as typeof execFileSync,
        });

        expect(isPidIdentityPlausible(record(500_000))).toBe("implausible");
        expect(readPaths).toEqual([`/proc/${PID}/stat`, "/proc/uptime"]);
    });

    test("accepts a genuine Linux record when the process started no later than the record", () => {
        __setRpcIdentityTestHooks({
            platform: "linux",
            nowMs: () => NOW_MS,
            readFileSync: linuxFiles({
                [`/proc/${PID}/stat`]: procStat(10_000),
                "/proc/uptime": `${UPTIME_SECONDS}.0 0.0`,
            }),
        });

        // The mocked process start is 1,100,000ms. The 120s tolerance is part of
        // the contract because port-file creation follows process startup.
        expect(isPidIdentityPlausible(record(1_000_000))).toBe("plausible");
    });

    test("reports an unreadable Linux start-time probe as inconclusive", () => {
        __setRpcIdentityTestHooks({
            platform: "linux",
            readFileSync: linuxFiles({
                [`/proc/${PID}/stat`]: new Error("procfs unavailable"),
            }),
        });

        expect(isPidIdentityPlausible(record(500_000))).toBe("inconclusive");
    });

    test("uses the legacy Linux command fallback and distinguishes probe errors", () => {
        __setRpcIdentityTestHooks({
            platform: "linux",
            readFileSync: linuxFiles({
                [`/proc/${PID}/cmdline`]: "/usr/sbin/opendkim --config /etc/opendkim.conf",
            }),
        });
        expect(isPidIdentityPlausible(record(0))).toBe("implausible");

        __setRpcIdentityTestHooks({
            platform: "linux",
            readFileSync: linuxFiles({
                [`/proc/${PID}/cmdline`]: "/usr/local/bin/opencode serve",
            }),
        });
        expect(isPidIdentityPlausible(record(0))).toBe("plausible");

        __setRpcIdentityTestHooks({
            platform: "linux",
            readFileSync: linuxFiles({
                [`/proc/${PID}/cmdline`]: new Error("procfs unavailable"),
            }),
        });
        expect(isPidIdentityPlausible(record(0))).toBe("inconclusive");
    });

    test("uses ps start time and command probes on non-Linux platforms", () => {
        __setRpcIdentityTestHooks({
            platform: "darwin",
            execFileSync: psOutput("Mon Aug  7 00:00:00 1970"),
        });
        expect(
            isPidIdentityPlausible(record(Date.parse("Mon Aug  7 00:00:00 1970") - 121_000)),
        ).toBe("implausible");

        __setRpcIdentityTestHooks({
            platform: "darwin",
            execFileSync: psOutput("Mon Aug  7 00:00:00 1970"),
        });
        expect(
            isPidIdentityPlausible(record(Date.parse("Mon Aug  7 00:00:00 1970") - 120_000)),
        ).toBe("plausible");

        __setRpcIdentityTestHooks({
            platform: "darwin",
            execFileSync: psOutput("/usr/sbin/opendkim -f"),
        });
        expect(isPidIdentityPlausible(record(0))).toBe("implausible");

        __setRpcIdentityTestHooks({
            platform: "darwin",
            execFileSync: psOutput("/Applications/OpenCode.app/Contents/MacOS/opencode"),
        });
        expect(isPidIdentityPlausible(record(0))).toBe("plausible");

        __setRpcIdentityTestHooks({
            platform: "darwin",
            execFileSync: psOutput(new Error("ps unavailable")),
        });
        expect(isPidIdentityPlausible(record(0))).toBe("inconclusive");
    });
});

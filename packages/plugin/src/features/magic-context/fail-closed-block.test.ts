/// <reference types="bun-types" />

import { describe, expect, it } from "bun:test";

import {
    createFailClosedBlockingError,
    createFailClosedController,
    FAIL_CLOSED_DOCTOR_COMMAND,
    type FailClosedReason,
    formatFailClosedBlockingMessage,
    formatFailClosedBlockingProcesses,
    isFailClosedBlockingError,
    resolveAgentNameFromMessages,
    shouldBypassFailClosedBlock,
} from "./fail-closed-block";

const fenceReason: FailClosedReason = {
    kind: "schema_fence",
    persistedVersion: 65,
    supportedVersion: 64,
};

const migrationReason: FailClosedReason = {
    kind: "migration_guard",
    persistedVersion: 73,
    supportedVersion: 74,
    blockingProcesses: [
        { harness: "OpenCode server", pid: 5736 },
        { harness: "OpenCode server", pid: 5736 },
        { harness: "OpenCode server", pid: 5737 },
    ],
};

const storageReason: FailClosedReason = {
    kind: "storage_failure",
    cause: "disk full",
};

describe("formatFailClosedBlockingMessage", () => {
    it("explains that this build is older for a newer database", () => {
        const message = formatFailClosedBlockingMessage(fenceReason);
        expect(message).toContain(
            "this Magic Context build is older than the database; upgrade/restart this harness",
        );
        expect(message).toContain("v65");
        expect(message).toContain("v64");
        expect(message).toContain(FAIL_CLOSED_DOCTOR_COMMAND);
    });

    it("names the blocking processes and gives ordered recovery actions", () => {
        const message = formatFailClosedBlockingMessage(migrationReason);
        expect(message).toContain("OpenCode server (PID 5736)");
        expect(message).toContain("OpenCode server (PID 5737)");
        expect(message).toContain("an older Magic Context build");
        expect(message).toContain("would fail against the migrated database");
        expect(message).toContain(
            "Restart the blocking process (it will pick up the new build and migrate on start), or shut it down and retry.",
        );
        expect(message).not.toContain("fence");
        expect(message).toContain(FAIL_CLOSED_DOCTOR_COMMAND);
    });

    it("names an uncertain discovery file and gives safe I/O recovery guidance", () => {
        const file = "/home/user/.local/share/cortexkit/magic-context/rpc/project/port";
        const message = formatFailClosedBlockingMessage({
            kind: "migration_guard",
            persistedVersion: 73,
            supportedVersion: 74,
            blockingProcesses: [],
            unreadableFile: file,
            unreadableArm: "io",
        });
        expect(message).toContain(file);
        expect(message).toContain("io arm");
        expect(message).toContain("safe to delete");
        expect(message).toContain(FAIL_CLOSED_DOCTOR_COMMAND);
    });

    it("names a fresh parse-invalid discovery file separately from I/O uncertainty", () => {
        const file = "/home/user/.local/share/cortexkit/magic-context/rpc/project/port";
        const message = formatFailClosedBlockingMessage({
            kind: "migration_guard",
            persistedVersion: 73,
            supportedVersion: 74,
            blockingProcesses: [],
            unreadableFile: file,
            unreadableArm: "parse",
        });
        expect(message).toContain(file);
        expect(message).toContain("parse arm");
        expect(message).not.toContain("safe to delete");
    });

    it("deduplicates and bounds the process list", () => {
        const processes = [
            ...Array.from({ length: 10 }, (_, index) => ({
                harness: "OpenCode server",
                pid: index + 1,
            })),
            { harness: "OpenCode server", pid: 1 },
        ];
        const message = formatFailClosedBlockingProcesses(processes);
        expect(message).toContain("OpenCode server (PID 1)");
        expect(message).not.toContain("OpenCode server (PID 9)");
        expect(message).toContain("2 more blocking process(es)");
        expect(message.match(/OpenCode server \(PID 1\)/g)).toHaveLength(1);
    });

    it("includes the storage cause and recovery command", () => {
        const message = formatFailClosedBlockingMessage(storageReason);
        expect(message).toContain("disk full");
        expect(message).toContain(FAIL_CLOSED_DOCTOR_COMMAND);
    });
});

describe("shouldBypassFailClosedBlock", () => {
    it("bypasses OpenCode internal agents and Magic Context hidden children", () => {
        expect(shouldBypassFailClosedBlock({ agent: "title" })).toBe(true);
        expect(shouldBypassFailClosedBlock({ agent: "summary" })).toBe(true);
        expect(shouldBypassFailClosedBlock({ agent: "compaction" })).toBe(true);
        expect(shouldBypassFailClosedBlock({ agent: "historian" })).toBe(true);
        expect(shouldBypassFailClosedBlock({ agent: "dreamer-docs" })).toBe(true);
        expect(shouldBypassFailClosedBlock({ agent: "sidekick" })).toBe(true);
        expect(shouldBypassFailClosedBlock({ isInternalChildSession: true })).toBe(true);
        expect(shouldBypassFailClosedBlock({ isPiSubagentEnv: true })).toBe(true);
    });

    it("does not bypass primary build agents", () => {
        expect(shouldBypassFailClosedBlock({ agent: "build" })).toBe(false);
        expect(shouldBypassFailClosedBlock({})).toBe(false);
    });
});

describe("createFailClosedController", () => {
    it("throws FailClosedBlockingError with both versions when armed", async () => {
        const gate = createFailClosedController({ reprobeEveryN: 5 });
        gate.arm(fenceReason);
        let thrown: unknown;
        try {
            await gate.enforce({ blockingEnabled: true, exempt: false });
        } catch (error) {
            thrown = error;
        }
        expect(isFailClosedBlockingError(thrown)).toBe(true);
        const message = thrown instanceof Error ? thrown.message : String(thrown);
        expect(message).toContain("v65");
        expect(message).toContain("v64");
        expect(message).toContain(FAIL_CLOSED_DOCTOR_COMMAND);
    });

    it("no-ops when blocking is disabled (degrade-silently escape hatch)", async () => {
        const gate = createFailClosedController();
        gate.arm(fenceReason);
        await expect(
            gate.enforce({ blockingEnabled: false, exempt: false }),
        ).resolves.toBeUndefined();
    });

    it("no-ops for exempt child sessions", async () => {
        const gate = createFailClosedController();
        gate.arm(fenceReason);
        await expect(
            gate.enforce({ blockingEnabled: true, exempt: true }),
        ).resolves.toBeUndefined();
    });

    it("re-probes and clears when storage heals without restart", async () => {
        const gate = createFailClosedController({ reprobeEveryN: 2 });
        gate.arm(storageReason);
        let opens = 0;
        const tryReopen = async () => {
            opens += 1;
            return opens >= 2;
        };

        await expect(
            gate.enforce({ blockingEnabled: true, exempt: false, tryReopen }),
        ).rejects.toBeInstanceOf(Error);
        expect(gate.isArmed()).toBe(true);

        // Second blocked pass hits reprobeEveryN=2 and heals.
        await expect(
            gate.enforce({ blockingEnabled: true, exempt: false, tryReopen }),
        ).resolves.toBeUndefined();
        expect(gate.isArmed()).toBe(false);
        expect(opens).toBe(2);

        // Subsequent passes stay unblocked without another reopen.
        await expect(
            gate.enforce({ blockingEnabled: true, exempt: false, tryReopen }),
        ).resolves.toBeUndefined();
        expect(opens).toBe(2);
    });
});

describe("resolveAgentNameFromMessages", () => {
    it("reads the newest message agent field", () => {
        expect(
            resolveAgentNameFromMessages([
                { info: { agent: "build" } },
                { info: { agent: "title" } },
            ]),
        ).toBe("title");
    });
});

describe("createFailClosedBlockingError", () => {
    it("sets a stable name and code for wrapper instanceof checks", () => {
        const error = createFailClosedBlockingError(fenceReason);
        expect(error.name).toBe("FailClosedBlockingError");
        expect(error.code).toBe("FAIL_CLOSED_BLOCKING");
        expect(isFailClosedBlockingError(error)).toBe(true);
    });
});

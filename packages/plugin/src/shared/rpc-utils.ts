import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { join } from "node:path";

export interface RpcPortFileRecord {
    port: number;
    pid: number;
    started_at: number;
    /**
     * Per-process bearer token. The server requires it on all non-health RPC
     * calls so a random local process or browser-origin script that merely
     * discovers/guesses the port cannot drive side-effecting endpoints
     * (recomp/upgrade/dismiss). Optional in the type for forward/backward
     * compatibility with port files written by older builds (treated as "no
     * auth required" only when the server itself didn't set one).
     */
    token?: string;
    /** Per-server filename nonce; prevents same-process instances from sharing one file. */
    instance_id?: string;
}

/**
 * Stable hash for a project directory — scopes RPC port files per-project
 * so multiple OpenCode instances don't collide.
 */
export function projectHash(directory: string): string {
    const normalized = directory.replace(/\/+$/, "");
    return createHash("sha256").update(normalized).digest("hex").slice(0, 16);
}

/** Directory containing per-process RPC discovery files for a project. */
export function rpcPortDir(storageDir: string, directory: string): string {
    return join(storageDir, "rpc", projectHash(directory));
}

/** Per-process RPC port file path. */
export function rpcPortFilePath(
    storageDir: string,
    directory: string,
    pid = process.pid,
    instanceId?: string,
): string {
    const suffix = instanceId ? `-${instanceId}` : "";
    return join(rpcPortDir(storageDir, directory), `port-${pid}${suffix}.json`);
}

/** Legacy single-port file used by v0.18.0 and earlier. */
export function legacyRpcPortFilePath(storageDir: string, directory: string): string {
    return join(rpcPortDir(storageDir, directory), "port");
}

export function isPidAlive(pid: number): boolean {
    if (!Number.isInteger(pid) || pid <= 0) return false;
    try {
        process.kill(pid, 0);
        return true;
    } catch (err) {
        return (err as NodeJS.ErrnoException).code === "EPERM";
    }
}

const RPC_IDENTITY_SKEW_TOLERANCE_MS = 120_000;
const LINUX_CLOCK_TICKS_PER_SECOND = 100;
const PS_PROBE_TIMEOUT_MS = 1_000;
const OPEN_CODE_COMMAND_MARKERS = ["opencode", "node", "bun", "electron"];

let rpcIdentityReadFileSync: typeof readFileSync = readFileSync;
let rpcIdentityExecFileSync: typeof execFileSync = execFileSync;
let rpcProcessListExecFileSync: typeof execFileSync = execFileSync;
let rpcProcessListTestOverride = false;
let rpcIdentityPlatform: NodeJS.Platform = process.platform;
let rpcIdentityNowMs: () => number = () => Date.now();

function parseLinuxProcessStartTime(statContent: string, uptimeContent: string): number | null {
    const closingCommandName = statContent.lastIndexOf(")");
    if (closingCommandName < 0) return null;

    // The fields after the command name begin at field 3 (`state`), so field 22
    // (`starttime`) is index 19 in this suffix. The command name can contain ')',
    // hence the last closing parenthesis rather than the first one is significant.
    const statFields = statContent
        .slice(closingCommandName + 1)
        .trim()
        .split(/\s+/);
    const startTimeTicks = Number(statFields[19]);
    const uptimeSeconds = Number(uptimeContent.trim().split(/\s+/)[0]);
    if (
        !Number.isFinite(startTimeTicks) ||
        startTimeTicks < 0 ||
        !Number.isFinite(uptimeSeconds) ||
        uptimeSeconds < 0
    ) {
        return null;
    }

    const processStartTime =
        rpcIdentityNowMs() -
        uptimeSeconds * 1_000 +
        (startTimeTicks / LINUX_CLOCK_TICKS_PER_SECOND) * 1_000;
    return Number.isFinite(processStartTime) ? processStartTime : null;
}

function readLinuxProcessStartTime(pid: number): number | null {
    try {
        const statContent = String(rpcIdentityReadFileSync(`/proc/${pid}/stat`, "utf8"));
        const uptimeContent = String(rpcIdentityReadFileSync("/proc/uptime", "utf8"));
        return parseLinuxProcessStartTime(statContent, uptimeContent);
    } catch {
        return null;
    }
}

function readPsProcessStartTime(pid: number): number | null {
    try {
        const output = rpcIdentityExecFileSync("ps", ["-p", String(pid), "-o", "lstart="], {
            encoding: "utf8",
            timeout: PS_PROBE_TIMEOUT_MS,
        });
        const processStartTime = Date.parse(String(output).trim());
        return Number.isFinite(processStartTime) ? processStartTime : null;
    } catch {
        return null;
    }
}

function readLinuxProcessCommand(pid: number): string | null {
    try {
        return String(rpcIdentityReadFileSync(`/proc/${pid}/cmdline`, "utf8"));
    } catch {
        return null;
    }
}

function readPsProcessCommand(pid: number): string | null {
    try {
        const output = rpcIdentityExecFileSync("ps", ["-p", String(pid), "-o", "command="], {
            encoding: "utf8",
            timeout: PS_PROBE_TIMEOUT_MS,
        });
        return String(output);
    } catch {
        return null;
    }
}

function commandLooksLikeOpenCode(command: string): boolean {
    const normalized = command.toLowerCase();
    return OPEN_CODE_COMMAND_MARKERS.some((marker) => normalized.includes(marker));
}

/**
 * Verify that a live PID still belongs to the process that wrote a port record.
 *
 * A PID can be reused after its original process exits. On Linux, procfs gives
 * us a process start time without spawning a helper; other platforms use `ps`
 * only on this cold database-open guard path. Legacy records without a start
 * time use a weaker command-name check, and every probe failure stays live so
 * this identity check cannot weaken the migration guard on uncertainty.
 */
export function isPidIdentityPlausible(record: RpcPortFileRecord): boolean {
    if (!Number.isInteger(record.pid) || record.pid <= 0) return false;

    if (Number.isFinite(record.started_at) && record.started_at > 0) {
        const processStartTime =
            rpcIdentityPlatform === "linux"
                ? readLinuxProcessStartTime(record.pid)
                : readPsProcessStartTime(record.pid);
        if (processStartTime === null) return true;
        return processStartTime <= record.started_at + RPC_IDENTITY_SKEW_TOLERANCE_MS;
    }

    const command =
        rpcIdentityPlatform === "linux"
            ? readLinuxProcessCommand(record.pid)
            : readPsProcessCommand(record.pid);
    if (command === null) return true;
    return commandLooksLikeOpenCode(command);
}

export function __setRpcIdentityTestHooks(hooks: {
    readFileSync?: typeof readFileSync;
    execFileSync?: typeof execFileSync;
    processListExecFileSync?: typeof execFileSync;
    platform?: NodeJS.Platform;
    nowMs?: () => number;
}): void {
    rpcIdentityReadFileSync = hooks.readFileSync ?? readFileSync;
    rpcIdentityExecFileSync = hooks.execFileSync ?? execFileSync;
    rpcProcessListExecFileSync = hooks.processListExecFileSync ?? execFileSync;
    rpcProcessListTestOverride = hooks.processListExecFileSync !== undefined;
    rpcIdentityPlatform = hooks.platform ?? process.platform;
    rpcIdentityNowMs = hooks.nowMs ?? (() => Date.now());
}

export function __resetRpcIdentityTestHooks(): void {
    rpcIdentityReadFileSync = readFileSync;
    rpcIdentityExecFileSync = execFileSync;
    rpcProcessListExecFileSync = execFileSync;
    rpcProcessListTestOverride = false;
    rpcIdentityPlatform = process.platform;
    rpcIdentityNowMs = () => Date.now();
}

function commandLooksLikePi(command: string): boolean {
    const normalized = command.trim().toLowerCase().replaceAll("\\", "/");
    const tokens = normalized.split(/\s+/).filter(Boolean);
    const executableName = (token: string | undefined): string =>
        (token ?? "").split("/").at(-1) ?? "";
    const first = executableName(tokens[0]).replace(/\.exe$/, "");
    if (["pi", "pi.cmd", "omp", "oh-my-pi"].includes(first)) return true;
    if (["node", "bun", "deno"].includes(first)) {
        const script = executableName(tokens[1]);
        return (
            ["pi", "pi.js", "pi.mjs", "pi.cjs"].includes(script) ||
            normalized.includes("pi-coding-agent")
        );
    }
    return false;
}

/** Result of checking whether Pi/OMP processes may currently hold the shared database. */
export interface PiProcessDiscovery {
    state: "known" | "unreadable";
    processIds: number[];
    error?: string;
}

/**
 * Inspect Pi/OMP processes without converting a failed process-list probe into
 * false evidence that no harness is running. Destructive maintenance callers
 * use the unreadable state to fail closed; ordinary migration guards retain
 * their historical best-effort process list through discoverLivePiProcessIds().
 */
export function inspectLivePiProcesses(): PiProcessDiscovery {
    if (process.env.NODE_ENV === "test" && !rpcProcessListTestOverride) {
        return { state: "known", processIds: [] };
    }
    try {
        const output = String(
            rpcProcessListExecFileSync("ps", ["-axo", "pid=,command="], {
                encoding: "utf8",
                timeout: PS_PROBE_TIMEOUT_MS,
            }),
        );
        const pids = new Set<number>();
        for (const line of output.split(/\r?\n/)) {
            const match = /^\s*(\d+)\s+(.+)$/.exec(line);
            if (!match) continue;
            const pid = Number(match[1]);
            if (!Number.isInteger(pid) || pid <= 0 || pid === process.pid) continue;
            if (commandLooksLikePi(match[2])) pids.add(pid);
        }
        return { state: "known", processIds: [...pids].sort((left, right) => left - right) };
    } catch (error) {
        return {
            state: "unreadable",
            processIds: [],
            error: error instanceof Error ? error.message : String(error),
        };
    }
}

/** Enumerate live Pi/OMP harness processes before deciding whether migration can proceed. */
export function discoverLivePiProcessIds(): number[] {
    return inspectLivePiProcesses().processIds;
}

export function parseRpcPortFile(content: string, fallbackPid = 0): RpcPortFileRecord | null {
    const trimmed = content.trim();
    if (!trimmed) return null;

    if (trimmed.startsWith("{")) {
        try {
            const parsed = JSON.parse(trimmed) as Partial<RpcPortFileRecord>;
            const port = Number(parsed.port);
            const pid = Number(parsed.pid);
            const startedAt = Number(parsed.started_at);
            if (!isValidPort(port) || !Number.isInteger(pid) || pid <= 0) return null;
            return {
                port,
                pid,
                started_at: Number.isFinite(startedAt) ? startedAt : 0,
                token: typeof parsed.token === "string" ? parsed.token : undefined,
                instance_id:
                    typeof parsed.instance_id === "string" ? parsed.instance_id : undefined,
            };
        } catch {
            return null;
        }
    }

    const port = Number.parseInt(trimmed, 10);
    if (!isValidPort(port)) return null;
    return { port, pid: fallbackPid, started_at: 0 };
}

function isValidPort(port: number): boolean {
    return Number.isInteger(port) && port > 0 && port <= 65535;
}

import * as fs from "node:fs";
import * as path from "node:path";
import { getMagicContextLogPath } from "./data-path";
import { sanitizeConfigValue, sanitizeDiagnosticText } from "./redaction";

const isTestEnv = process.env.NODE_ENV === "test";

let buffer: string[] = [];
let flushTimer: ReturnType<typeof setTimeout> | null = null;
const FLUSH_INTERVAL_MS = 500;
const BUFFER_SIZE_LIMIT = 50;
const MAX_LOG_FILE_BYTES = 32 * 1024 * 1024;
const SIZE_CHECK_INTERVAL_FLUSHES = 64;

let activeLogFile: string | null = null;
let activeLogSize: number | null = null;
let flushesSinceSizeCheck = 0;

export interface LoggerDiagnostics {
    swallowedWriteCount: number;
    lastErrorMessage: string | null;
    lastErrorTime: string | null;
}

let swallowedWriteCount = 0;
let lastErrorMessage: string | null = null;
let lastErrorTime: string | null = null;

function recordSwallowedWrite(error: unknown): void {
    try {
        swallowedWriteCount++;
        lastErrorMessage = sanitizeDiagnosticText(
            error instanceof Error ? error.message : String(error),
        );
        lastErrorTime = new Date().toISOString();
    } catch {
        // Diagnostics must not make the logger throw either.
    }
}

function ensureDir(filePath: string): void {
    fs.mkdirSync(path.dirname(filePath), { recursive: true });
}

function isMissingFile(error: unknown): boolean {
    return (
        typeof error === "object" &&
        error !== null &&
        "code" in error &&
        (error as { code?: unknown }).code === "ENOENT"
    );
}

function getCurrentLogSize(logFile: string): number {
    if (
        activeLogFile === logFile &&
        activeLogSize !== null &&
        flushesSinceSizeCheck < SIZE_CHECK_INTERVAL_FLUSHES
    ) {
        return activeLogSize;
    }

    try {
        const stat = fs.statSync(logFile);
        if (!stat.isFile()) {
            throw new Error(`Magic Context log path is not a regular file: ${logFile}`);
        }
        // Logs can contain diagnostic context, so tighten files created by older
        // versions before copying or appending any new content.
        fs.chmodSync(logFile, 0o600);
        activeLogFile = logFile;
        activeLogSize = stat.size;
        flushesSinceSizeCheck = 0;
        return stat.size;
    } catch (error) {
        if (!isMissingFile(error)) throw error;
        activeLogFile = logFile;
        activeLogSize = 0;
        flushesSinceSizeCheck = 0;
        return 0;
    }
}

function capLogData(data: string): string {
    if (Buffer.byteLength(data) <= MAX_LOG_FILE_BYTES) return data;

    // A single diagnostic payload must not defeat the on-disk bound. Cutting a
    // Buffer can split UTF-8, so trim any replacement character expansion too.
    let bounded = Buffer.from(data).subarray(0, MAX_LOG_FILE_BYTES).toString("utf8");
    while (Buffer.byteLength(bounded) > MAX_LOG_FILE_BYTES) {
        bounded = bounded.slice(0, -1);
    }
    return bounded;
}

function writeBoundedPredecessor(logFile: string, predecessorPath: string, size: number): void {
    const predecessorFd = fs.openSync(predecessorPath, "w", 0o600);
    try {
        // `openSync(..., "w")` preserves an existing file's mode, so set it
        // before any copied diagnostic text reaches the predecessor.
        fs.fchmodSync(predecessorFd, 0o600);
        const bytesToCopy = Math.min(size, MAX_LOG_FILE_BYTES);
        const sourceFd = fs.openSync(logFile, "r");
        try {
            const chunk = Buffer.allocUnsafe(Math.min(64 * 1024, bytesToCopy));
            let remaining = bytesToCopy;
            let position = Math.max(0, size - bytesToCopy);
            while (remaining > 0) {
                const bytesRead = fs.readSync(
                    sourceFd,
                    chunk,
                    0,
                    Math.min(chunk.length, remaining),
                    position,
                );
                if (bytesRead === 0) break;
                fs.writeSync(predecessorFd, chunk, 0, bytesRead);
                remaining -= bytesRead;
                position += bytesRead;
            }
        } finally {
            fs.closeSync(sourceFd);
        }
    } finally {
        fs.closeSync(predecessorFd);
    }
}

function rotateLogFile(logFile: string, size: number): void {
    const predecessorPath = `${logFile}.1`;
    writeBoundedPredecessor(logFile, predecessorPath, size);
    // Keep the current inode in place so diagnostic readers can observe a
    // shorter log during rotation instead of seeing the path disappear.
    fs.truncateSync(logFile, 0);
    activeLogSize = 0;
    flushesSinceSizeCheck = 0;
}

function flush(): void {
    if (flushTimer) {
        clearTimeout(flushTimer);
        flushTimer = null;
    }
    if (buffer.length === 0) return;
    const bufferedData = buffer.join("");
    buffer = [];
    try {
        const data = capLogData(bufferedData);
        const logFile = getMagicContextLogPath();
        ensureDir(logFile);
        let currentSize = getCurrentLogSize(logFile);
        const dataSize = Buffer.byteLength(data);
        if (currentSize > 0 && currentSize + dataSize > MAX_LOG_FILE_BYTES) {
            rotateLogFile(logFile, currentSize);
            currentSize = 0;
        }
        fs.appendFileSync(logFile, data, { encoding: "utf8", mode: 0o600 });
        activeLogFile = logFile;
        activeLogSize = currentSize + dataSize;
        flushesSinceSizeCheck++;
    } catch (error) {
        activeLogFile = null;
        activeLogSize = null;
        flushesSinceSizeCheck = 0;
        recordSwallowedWrite(error);
    }
}

function scheduleFlush(): void {
    if (flushTimer) return;
    flushTimer = setTimeout(() => {
        flushTimer = null;
        flush();
    }, FLUSH_INTERVAL_MS);
}

export function log(message: string, data?: unknown): void {
    if (isTestEnv) return;
    try {
        const timestamp = new Date().toISOString();
        const serialized =
            data === undefined
                ? ""
                : data instanceof Error
                  ? ` ${sanitizeDiagnosticText(
                        `${data.message}${data.stack ? `\n${data.stack}` : ""}`,
                    )}`
                  : ` ${JSON.stringify(sanitizeConfigValue(data))}`;
        buffer.push(`[${timestamp}] ${sanitizeDiagnosticText(message)}${serialized}\n`);
        if (buffer.length >= BUFFER_SIZE_LIMIT) {
            flush();
        } else {
            scheduleFlush();
        }
    } catch {
        // Intentional: logging must never throw
    }
}

export function sessionLog(sessionId: string, message: string, data?: unknown): void {
    log(`[magic-context][${sessionId}] ${message}`, data);
}

export function getLoggerDiagnostics(): LoggerDiagnostics {
    return {
        swallowedWriteCount,
        lastErrorMessage,
        lastErrorTime,
    };
}

/** Flush buffered log entries immediately. Primarily useful to diagnostic readers and tests. */
export function flushLogger(): void {
    flush();
}

/**
 * Resolve the current log file path. The path is harness-aware (see
 * {@link getMagicContextLogPath}) and re-evaluated on every call, so callers
 * who format diagnostic output with this value always see the path the next
 * flush will actually use.
 */
export function getLogFilePath(): string {
    return getMagicContextLogPath();
}

// Flush remaining buffer on process exit
if (!isTestEnv) {
    process.on("exit", flush);
}

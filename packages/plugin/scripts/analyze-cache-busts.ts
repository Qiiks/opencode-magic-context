#!/usr/bin/env bun
/**
 * analyze-cache-busts.ts — walk a session's anthropic-auth request dumps in
 * order and attribute prompt changes while using the provider's usage meter
 * as the cache-hit verdict.
 *
 * The opencode-anthropic-auth plugin dumps every outbound request body to a
 * temp dir (`<tmpdir>/opencode-anthropic-auth-dumps/*.body.json`) alongside a
 * `.meta.json` and, when available, a `.response.json`. This tool reconstructs
 * the wire-order segment list for each request and finds the first segment
 * whose content changed versus the preceding same-session request. That byte
 * comparison is attribution only: Anthropic's cache usage meter determines
 * whether the request actually busted the cache.
 *
 * Usage:
 *   bun scripts/analyze-cache-busts.ts <sessionIdPrefix> [options]
 *   bun scripts/analyze-cache-busts.ts --session <sessionIdPrefix> [options]
 * Options:
 *   --session <id>   session id or prefix (positional form is also supported)
 *   --dir <path>     dump dir (default: <tmpdir>/opencode-anthropic-auth-dumps)
 *   --since <time>   created at/after ISO time or duration ago (for example 30m)
 *   --until <time>   created at/before ISO time or duration ago
 *   --limit <N>      only the last N requests in range
 *   --show-diff      print before/after snippet of the first-diverging segment
 *   --all-busts      list every diverging segment, not just the first
 *   --all-rows       also print STABLE and UNMETERED rows
 */
import { createHash } from "node:crypto";
import { existsSync, readdirSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

type Json = Record<string, unknown>;
type ByteVerdict = "BUST" | "STABLE";
type MeterVerdict = ByteVerdict | "UNMETERED";
type MeterVsBytes = "AGREE" | "BYTES-ONLY" | "METER-ONLY" | "UNMETERED";

interface Segment {
    id: string;
    hash: string;
    bytes: number;
    breakpoint: boolean;
}

interface MeterUsage {
    cacheRead: number;
    cacheCreation: number;
    input: number;
    total: number;
    source: string;
}

interface Snapshot {
    file: string;
    bodyPath: string;
    createdAt: string;
    session: string;
    messagesCount: number;
    segments: Segment[];
    usage?: MeterUsage;
    orderCreatedAt: string;
    sequence: number;
}

interface AnalysisRow {
    current: Snapshot;
    previous?: Snapshot;
    divergenceIndex: number;
    byteVerdict?: ByteVerdict;
    verdict: MeterVerdict | "BASE";
    meterVsBytes?: MeterVsBytes;
    prevTotal?: number;
    writeSlack?: number;
    meterFloor?: number;
    comparableRead?: number;
    rewrittenTokens?: number;
}

function sha(s: string): string {
    return createHash("sha256").update(s).digest("hex").slice(0, 10);
}

function parseArgs(argv: string[]): {
    sessionPrefix: string;
    dir: string;
    since?: string;
    until?: string;
    limit?: number;
    showDiff: boolean;
    allBusts: boolean;
    allRows: boolean;
} {
    const args = argv.slice(2);
    const getOpt = (name: string): string | undefined => {
        const i = args.indexOf(name);
        return i >= 0 && i + 1 < args.length ? args[i + 1] : undefined;
    };
    const valueOptions = new Set(["--session", "--dir", "--since", "--until", "--limit"]);
    let positionalSession = "";
    for (let index = 0; index < args.length; index += 1) {
        const arg = args[index];
        if (valueOptions.has(arg)) {
            index += 1;
            continue;
        }
        if (!arg.startsWith("--")) {
            positionalSession = arg;
            break;
        }
    }
    const limitRaw = getOpt("--limit");
    return {
        sessionPrefix: getOpt("--session") ?? positionalSession,
        dir: getOpt("--dir") ?? join(tmpdir(), "opencode-anthropic-auth-dumps"),
        since: getOpt("--since"),
        until: getOpt("--until"),
        limit: limitRaw ? Number.parseInt(limitRaw, 10) : undefined,
        showDiff: args.includes("--show-diff"),
        allBusts: args.includes("--all-busts"),
        allRows: args.includes("--all-rows"),
    };
}

function resolveTimeBound(value: string | undefined, nowMs = Date.now()): string | undefined {
    if (!value) return undefined;
    const duration = /^(\d+)(ms|s|m|h|d)$/.exec(value);
    if (duration) {
        const unitMs = { ms: 1, s: 1_000, m: 60_000, h: 3_600_000, d: 86_400_000 }[
            duration[2] as "ms" | "s" | "m" | "h" | "d"
        ];
        return new Date(nowMs - Number.parseInt(duration[1], 10) * unitMs).toISOString();
    }
    const parsed = Date.parse(value);
    if (!Number.isFinite(parsed)) throw new Error(`Invalid time bound: ${value}`);
    return new Date(parsed).toISOString();
}

function parseDumpFilename(file: string): {
    createdAt: string;
    sequence: number;
    session: string;
} | null {
    const timestamp = /(\d{4}-\d{2}-\d{2}T\d{2})-(\d{2})-(\d{2})-(\d{3})Z/.exec(file);
    const session = /(?:^|-)(ses_[A-Za-z0-9]+)(?=-|\.meta\.json$)/.exec(file);
    if (!timestamp || !session) return null;
    const rest = file.slice(timestamp.index + timestamp[0].length);
    const sequence = /^-(\d+)(?=-ses_)/.exec(rest);
    return {
        createdAt: `${timestamp[1]}:${timestamp[2]}:${timestamp[3]}.${timestamp[4]}Z`,
        sequence: sequence ? Number.parseInt(sequence[1], 10) : 0,
        session: session[1],
    };
}

function sessionMatches(candidate: string, prefix: string): boolean {
    const visibleHead = candidate.replace(/[….]+$/, "");
    return candidate.startsWith(prefix) || prefix.startsWith(visibleHead);
}

/** Recursively strip `cache_control` fields because marker movement is not content. */
function stripCacheControl(value: unknown): unknown {
    if (Array.isArray(value)) return value.map(stripCacheControl);
    if (value && typeof value === "object") {
        const out: Json = {};
        for (const [k, v] of Object.entries(value as Json)) {
            if (k === "cache_control") continue;
            out[k] = stripCacheControl(v);
        }
        return out;
    }
    return value;
}

function hasCacheControl(block: unknown): boolean {
    return !!block && typeof block === "object" && "cache_control" in (block as Json);
}

function messageHasBreakpoint(msg: Json): boolean {
    const content = msg.content;
    if (Array.isArray(content)) return content.some((part) => hasCacheControl(part));
    return hasCacheControl(msg);
}

/** Normalize the per-request billing nonce so it isn't seen as a content change. */
function normalizeSystemText(text: string): string {
    return text.replace(/cch=[^;]*;/g, "cch=<NONCE>;");
}

function blockText(block: unknown): string {
    if (block && typeof block === "object" && typeof (block as Json).text === "string") {
        return (block as Json).text as string;
    }
    return JSON.stringify(stripCacheControl(block));
}

function normalizedSystemSegment(block: unknown): string {
    return normalizeSystemText(blockText(block));
}

function normalizedMessageSegment(message: Json): string {
    return JSON.stringify({ role: message.role, content: stripCacheControl(message.content) });
}

function buildSegments(body: Json): Segment[] {
    const segs: Segment[] = [];
    const system = body.system;
    const sysBlocks = Array.isArray(system) ? system : system != null ? [system] : [];
    sysBlocks.forEach((block, index) => {
        const raw = blockText(block);
        segs.push({
            id: `system[${index}]`,
            hash: sha(normalizedSystemSegment(block)),
            bytes: Buffer.byteLength(raw),
            breakpoint: hasCacheControl(block),
        });
    });
    const messages = Array.isArray(body.messages) ? (body.messages as Json[]) : [];
    messages.forEach((message, index) => {
        segs.push({
            id: `message[${index}](${String(message.role)})`,
            hash: sha(normalizedMessageSegment(message)),
            bytes: Buffer.byteLength(JSON.stringify(message)),
            breakpoint: messageHasBreakpoint(message),
        });
    });
    return segs;
}

function asJson(value: unknown): Json | undefined {
    return value && typeof value === "object" && !Array.isArray(value) ? (value as Json) : undefined;
}

function meterUsage(value: unknown, source: string): MeterUsage | undefined {
    const usage = asJson(value);
    if (!usage || typeof usage.input_tokens !== "number" || !Number.isFinite(usage.input_tokens)) {
        return undefined;
    }
    const cacheRead = usage.cache_read_input_tokens;
    const cacheCreation = usage.cache_creation_input_tokens;
    if (
        (cacheRead !== undefined && (typeof cacheRead !== "number" || !Number.isFinite(cacheRead))) ||
        (cacheCreation !== undefined &&
            (typeof cacheCreation !== "number" || !Number.isFinite(cacheCreation)))
    ) {
        return undefined;
    }
    return {
        cacheRead: cacheRead ?? 0,
        cacheCreation: cacheCreation ?? 0,
        input: usage.input_tokens,
        total: (cacheRead ?? 0) + (cacheCreation ?? 0) + usage.input_tokens,
        source,
    };
}

/** Collect usage from completed JSON responses and from message_start/message_delta stream events. */
function collectUsageCandidates(value: unknown, source: string, candidates: MeterUsage[]): void {
    if (Array.isArray(value)) {
        value.forEach((entry, index) => collectUsageCandidates(entry, `${source}[${index}]`, candidates));
        return;
    }
    const object = asJson(value);
    if (!object) return;

    const eventType = typeof object.type === "string" ? object.type : source;
    const direct = meterUsage(object.usage, `${eventType}.usage`);
    if (direct) candidates.push(direct);
    const message = asJson(object.message);
    const messageUsage = meterUsage(message?.usage, `${eventType}.message.usage`);
    if (messageUsage) candidates.push(messageUsage);

    for (const [key, child] of Object.entries(object)) {
        if (key === "usage" || key === "message") continue;
        if (typeof child === "string" && key === "data") {
            try {
                collectUsageCandidates(JSON.parse(child), `${source}.data`, candidates);
            } catch {
                // A non-JSON SSE data line cannot contain the usage meter.
            }
        } else if (child && typeof child === "object") {
            collectUsageCandidates(child, `${source}.${key}`, candidates);
        }
    }
}

function parseResponsePayloads(raw: string): unknown[] {
    try {
        return [JSON.parse(raw)];
    } catch {
        const payloads: unknown[] = [];
        for (const line of raw.split(/\r?\n/)) {
            const data = line.startsWith("data:") ? line.slice("data:".length).trim() : line.trim();
            if (!data || data === "[DONE]") continue;
            try {
                payloads.push(JSON.parse(data));
            } catch {
                // Ignore SSE event labels and incomplete/non-JSON lines.
            }
        }
        return payloads;
    }
}

function loadMeterUsage(responsePath: string | undefined): MeterUsage | undefined {
    if (!responsePath || !existsSync(responsePath)) return undefined;
    try {
        const candidates: MeterUsage[] = [];
        for (const payload of parseResponsePayloads(readFileSync(responsePath, "utf8"))) {
            collectUsageCandidates(payload, "response", candidates);
        }
        return candidates.at(-1);
    } catch {
        return undefined;
    }
}

function loadSnapshots(opts: ReturnType<typeof parseArgs>): Snapshot[] {
    const since = resolveTimeBound(opts.since);
    const until = resolveTimeBound(opts.until);
    const metas = readdirSync(opts.dir).filter((file) => file.endsWith(".meta.json"));
    const snaps: Snapshot[] = [];
    for (const metaFile of metas) {
        const dumpName = parseDumpFilename(metaFile);
        if (dumpName && !sessionMatches(dumpName.session, opts.sessionPrefix)) continue;

        let meta: Json;
        try {
            meta = JSON.parse(readFileSync(join(opts.dir, metaFile), "utf8")) as Json;
        } catch {
            continue;
        }
        const metadataSession = String(meta.session ?? "");
        if (!dumpName && !sessionMatches(metadataSession, opts.sessionPrefix)) continue;

        const session = dumpName?.session ?? metadataSession;
        const createdAt = dumpName?.createdAt ?? String(meta.createdAt ?? "");
        if (since && createdAt < since) continue;
        if (until && createdAt > until) continue;

        const files = asJson(meta.files);
        const referencedBodyPath = typeof files?.body === "string" ? files.body : undefined;
        const adjacentBodyPath = join(opts.dir, metaFile.replace(/\.meta\.json$/, ".body.json"));
        const bodyPath =
            referencedBodyPath && existsSync(referencedBodyPath)
                ? referencedBodyPath
                : existsSync(adjacentBodyPath)
                  ? adjacentBodyPath
                  : undefined;
        if (!bodyPath) continue;

        let body: Json;
        try {
            body = JSON.parse(readFileSync(bodyPath, "utf8")) as Json;
        } catch {
            continue;
        }
        const referencedResponsePath = typeof files?.response === "string" ? files.response : undefined;
        const adjacentResponsePath = join(opts.dir, metaFile.replace(/\.meta\.json$/, ".response.json"));
        const responsePath =
            referencedResponsePath && existsSync(referencedResponsePath)
                ? referencedResponsePath
                : existsSync(adjacentResponsePath)
                  ? adjacentResponsePath
                  : undefined;
        const bodyMeta = asJson(meta.body);
        snaps.push({
            file: metaFile,
            bodyPath,
            createdAt,
            session,
            messagesCount: typeof bodyMeta?.messagesCount === "number" ? bodyMeta.messagesCount : -1,
            segments: buildSegments(body),
            usage: loadMeterUsage(responsePath),
            orderCreatedAt: dumpName?.createdAt ?? createdAt,
            sequence: dumpName?.sequence ?? 0,
        });
    }
    snaps.sort(
        (a, b) =>
            a.orderCreatedAt.localeCompare(b.orderCreatedAt) ||
            a.sequence - b.sequence ||
            a.file.localeCompare(b.file),
    );
    return opts.limit && snaps.length > opts.limit ? snaps.slice(snaps.length - opts.limit) : snaps;
}

/** First wire-order segment index where prev/cur diverge (added/removed/changed). */
function firstDivergence(prev: Segment[], cur: Segment[]): number {
    const n = Math.min(prev.length, cur.length);
    for (let index = 0; index < n; index += 1) {
        if (prev[index].hash !== cur[index].hash || prev[index].id !== cur[index].id) return index;
    }
    return prev.length === cur.length ? -1 : n;
}

/** Effective cached prefix = bytes up to the last breakpoint strictly before divergence. */
function cachedPrefixBytes(segs: Segment[], divergeIdx: number): { bytes: number; at: string } {
    let bytes = 0;
    let lastBreakpointBytes = 0;
    let lastBreakpointId = "(none)";
    const limit = divergeIdx < 0 ? segs.length : divergeIdx;
    for (let index = 0; index < segs.length; index += 1) {
        if (index < limit && segs[index].breakpoint) {
            lastBreakpointBytes = bytes + segs[index].bytes;
            lastBreakpointId = segs[index].id;
        }
        bytes += segs[index].bytes;
    }
    return { bytes: lastBreakpointBytes, at: lastBreakpointId };
}

function lastBreakpointIndex(segs: Segment[]): number {
    let last = -1;
    for (let index = 0; index < segs.length; index += 1) {
        if (segs[index].breakpoint) last = index;
    }
    return last;
}

function analyzeSnapshots(snaps: Snapshot[]): AnalysisRow[] {
    return snaps.map((current, index) => {
        if (index === 0) return { current, divergenceIndex: -1, verdict: "BASE" };
        const previous = snaps[index - 1];
        const divergenceIndex = firstDivergence(previous.segments, current.segments);
        const byteVerdict: ByteVerdict =
            divergenceIndex !== -1 && divergenceIndex <= lastBreakpointIndex(previous.segments)
                ? "BUST"
                : "STABLE";
        if (!current.usage || !previous.usage) {
            return {
                current,
                previous,
                divergenceIndex,
                byteVerdict,
                verdict: "UNMETERED",
                meterVsBytes: "UNMETERED",
            };
        }
        // The preceding request's newest cache write can still overlap this stream,
        // so it cannot be required to appear in this request's cache-read meter.
        const prevTotal = previous.usage.total;
        const writeSlack = previous.usage.cacheCreation;
        const meterFloor = prevTotal - writeSlack;
        // input_tokens are direct, non-cacheable tokens. Add the current direct input
        // back to cache_read before comparing it with a previous total that includes it.
        const comparableRead = current.usage.cacheRead + current.usage.input;
        const verdict: ByteVerdict = comparableRead < meterFloor ? "BUST" : "STABLE";
        const meterVsBytes: MeterVsBytes =
            verdict === byteVerdict ? "AGREE" : verdict === "BUST" ? "METER-ONLY" : "BYTES-ONLY";
        return {
            current,
            previous,
            divergenceIndex,
            byteVerdict,
            verdict,
            meterVsBytes,
            prevTotal,
            writeSlack,
            meterFloor,
            comparableRead,
            rewrittenTokens: verdict === "BUST" ? prevTotal - current.usage.cacheRead : undefined,
        };
    });
}

function fmtTime(iso: string): string {
    const date = new Date(iso);
    if (Number.isNaN(date.getTime())) return iso;
    const mm = String(date.getUTCMonth() + 1).padStart(2, "0");
    const dd = String(date.getUTCDate()).padStart(2, "0");
    const hh = String(date.getUTCHours()).padStart(2, "0");
    const mi = String(date.getUTCMinutes()).padStart(2, "0");
    const ss = String(date.getUTCSeconds()).padStart(2, "0");
    return `${mm}-${dd} ${hh}:${mi}:${ss} UTC`;
}

function segmentText(snapshot: Snapshot, index: number): string | undefined {
    if (index < 0) return undefined;
    try {
        const body = JSON.parse(readFileSync(snapshot.bodyPath, "utf8")) as Json;
        const system = body.system;
        const systemBlocks = Array.isArray(system) ? system : system != null ? [system] : [];
        if (index < systemBlocks.length) return normalizedSystemSegment(systemBlocks[index]);
        const messages = Array.isArray(body.messages) ? (body.messages as Json[]) : [];
        const message = messages[index - systemBlocks.length];
        return message ? normalizedMessageSegment(message) : undefined;
    } catch {
        return undefined;
    }
}

function clippedDiff(text: string, start: number, end: number): string {
    const before = text.slice(Math.max(0, start - 120), start);
    const changed = text.slice(start, Math.min(end, start + 240));
    const after = text.slice(end, end + 120);
    return `${start > 120 ? "…" : ""}${before}[${changed}${end - start > 240 ? "…" : ""}]${after}${end + 120 < text.length ? "…" : ""}`;
}

function printSegmentDiff(previous: Snapshot, current: Snapshot, index: number): void {
    const prevText = segmentText(previous, index) ?? "(segment absent)";
    const curText = segmentText(current, index) ?? "(segment absent)";
    let start = 0;
    while (start < prevText.length && start < curText.length && prevText[start] === curText[start]) start += 1;
    let prevEnd = prevText.length;
    let curEnd = curText.length;
    while (prevEnd > start && curEnd > start && prevText[prevEnd - 1] === curText[curEnd - 1]) {
        prevEnd -= 1;
        curEnd -= 1;
    }
    console.log(`          └─ segment diff @char ${start}:`);
    console.log(`             prev: ${clippedDiff(prevText, start, prevEnd)}`);
    console.log(`             cur:  ${clippedDiff(curText, start, curEnd)}`);
}

function meterCell(row: AnalysisRow): string {
    if (row.verdict === "UNMETERED") return `unavailable; bytes=${row.byteVerdict}`;
    const read = row.current.usage?.cacheRead ?? 0;
    const rewritten = row.rewrittenTokens === undefined ? "" : `; rewritten≈${row.rewrittenTokens.toLocaleString()}`;
    const directInput = row.current.usage?.input ?? 0;
    return `read=${read.toLocaleString()} + input=${directInput.toLocaleString()} = ${row.comparableRead?.toLocaleString()}; floor=${row.meterFloor?.toLocaleString()} (prevTotal=${row.prevTotal?.toLocaleString()}, writeSlack=${row.writeSlack?.toLocaleString()})${rewritten}`;
}

function main(): void {
    const opts = parseArgs(process.argv);
    if (!opts.sessionPrefix) {
        console.error(
            "usage: bun scripts/analyze-cache-busts.ts <sessionIdPrefix> | --session <sessionIdPrefix> [--dir <path>] [--since ISO|duration] [--until ISO|duration] [--limit N] [--show-diff] [--all-busts]",
        );
        process.exit(1);
    }
    const snaps = loadSnapshots(opts);
    if (snaps.length === 0) {
        console.error(`No dumps found for session prefix "${opts.sessionPrefix}" in ${opts.dir}`);
        process.exit(1);
    }
    const rows = analyzeSnapshots(snaps);
    console.log(`Session: ${snaps[0].session}`);
    console.log(`Dumps:   ${snaps.length}  (dir: ${opts.dir})`);
    console.log("");
    console.log("Dashboard times are local (UTC+2); table times are UTC.");
    console.log("Meter rule: BUST when cacheRead + current input < prevTotal - writeSlack; prevTotal=previous read+creation+input, writeSlack=previous creation.");
    console.log(
        "time(UTC)          | segs | verdict          | meter                                                  | meterVsBytes | first-divergence                | prevBytes → curBytes        | cachedPrefix@breakpoint",
    );
    console.log(
        "-------------------|------|------------------|--------------------------------------------------------|--------------|---------------------------------|-----------------------------|------------------------",
    );

    let bustCount = 0;
    let unmeteredBustCount = 0;
    for (const row of rows) {
        if (row.verdict === "BASE") {
            if (opts.allRows) {
                console.log(`${fmtTime(row.current.createdAt)} | ${String(row.current.segments.length).padStart(4)} | BASE             |                                                        |              | (first request)                 |                             |`);
            }
            continue;
        }
        const shouldPrint =
            opts.allRows ||
            row.verdict === "BUST" ||
            (row.verdict === "UNMETERED" && row.byteVerdict === "BUST");
        if (!shouldPrint) continue;
        if (row.verdict === "BUST") bustCount += 1;
        if (row.verdict === "UNMETERED" && row.byteVerdict === "BUST") unmeteredBustCount += 1;

        const previous = row.previous as Snapshot;
        const index = row.divergenceIndex;
        const segment = index < 0 ? undefined : row.current.segments[index] ?? previous.segments[index];
        const attribution = segment
            ? `${segment.id} (bytes ${row.byteVerdict})`
            : `(identical; bytes ${row.byteVerdict})`;
        const previousPrefix = cachedPrefixBytes(previous.segments, previous.segments.length);
        const currentPrefix = cachedPrefixBytes(row.current.segments, index);
        const byteDelta = `${previousPrefix.bytes.toLocaleString()}B → ${currentPrefix.bytes.toLocaleString()}B`;
        const verdictLabel =
            row.verdict === "UNMETERED"
                ? `UNMETERED (bytes ${row.byteVerdict})`
                : `${row.verdict} (meter)`;
        console.log(
            `${fmtTime(row.current.createdAt)} | ${String(row.current.segments.length).padStart(4)} | ${verdictLabel.padEnd(16)} | ${meterCell(row).padEnd(54)} | ${(row.meterVsBytes ?? "").padEnd(12)} | ${attribution.padEnd(31)} | ${byteDelta.padEnd(27)} | ${currentPrefix.at} (${currentPrefix.bytes.toLocaleString()}B)`,
        );

        if ((opts.showDiff || opts.allBusts) && index >= 0 && (row.verdict === "BUST" || opts.allRows)) {
            if (opts.allBusts) {
                const diffs: number[] = [];
                const count = Math.max(previous.segments.length, row.current.segments.length);
                for (let diffIndex = index; diffIndex < count; diffIndex += 1) {
                    if (
                        previous.segments[diffIndex]?.hash !== row.current.segments[diffIndex]?.hash ||
                        previous.segments[diffIndex]?.id !== row.current.segments[diffIndex]?.id
                    ) {
                        diffs.push(diffIndex);
                    }
                }
                for (const diffIndex of diffs) {
                    console.log(`          └─ diverge @${diffIndex}: prev=${previous.segments[diffIndex]?.id ?? "—"}/${previous.segments[diffIndex]?.hash ?? "—"}  cur=${row.current.segments[diffIndex]?.id ?? "—"}/${row.current.segments[diffIndex]?.hash ?? "—"}`);
                }
            }
            if (opts.showDiff) printSegmentDiff(previous, row.current, index);
        }
    }

    console.log("");
    if (bustCount === 0) {
        console.log(`No metered busts across ${snaps.length} request(s).`);
    } else {
        console.log(`${bustCount} metered bust(s) across ${snaps.length} request(s).${opts.allRows ? "" : " (STABLE rows hidden; pass --all-rows to show them.)"}`);
    }
    if (unmeteredBustCount > 0) {
        console.log(`${unmeteredBustCount} unmetered byte-attributed bust candidate(s); response usage was unavailable.`);
    }
}

export const __test = {
    analyzeSnapshots,
    loadMeterUsage,
    loadSnapshots,
    parseArgs,
    parseDumpFilename,
    resolveTimeBound,
};

if (import.meta.main) main();

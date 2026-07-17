import * as crypto from "node:crypto";
import {
    getRawSessionStoredMessageCount,
    readRawSessionMessageOrdinalPage,
} from "./read-session-chunk";
import { isRawCompactionSummaryInfo, type RawMessageOrdinalAnchor } from "./read-session-raw";
import type { MessageLike } from "./transform-operations";

/** The maximum request page size accepted by the module facade. */
export const MODULE_PAGE_MAX_BYTES = 512 * 1024;
/** Large individual values are split so one message cannot exceed a page. */
export const MODULE_ITEM_CONTINUATION_CHUNK_BYTES = 64 * 1024;
// The module-side reassembler already recognizes this wire key; authority and
// shadow requests must use the same continuation envelope.
export const MODULE_ITEM_CONTINUATION_KEY = "__shadow_item_continuation";
export const MODULE_ORDINAL_PAGE_SIZE = 500;

export interface ModuleNormalizationRecord {
    kind: "tag_prefix" | "ctx_search_hint" | "summary_message";
    message_id: string | null;
    part_index: number;
    field: string;
    tag_number?: number;
    removed: string;
}

function yieldToEventLoop(): Promise<void> {
    return new Promise((resolve) => setTimeout(resolve, 0));
}

function canonicalJson(value: unknown): string {
    if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
    if (value !== null && typeof value === "object") {
        const record = value as Record<string, unknown>;
        return `{${Object.keys(record)
            .sort()
            .map((key) => `${JSON.stringify(key)}:${canonicalJson(record[key])}`)
            .join(",")}}`;
    }
    const encoded = JSON.stringify(value);
    return encoded === undefined ? "null" : encoded;
}

function transformPageDigest(arrays: Record<string, unknown[]>): string {
    const wireArrays = JSON.parse(JSON.stringify(arrays)) as Record<string, unknown[]>;
    return crypto.createHash("sha256").update(canonicalJson(wireArrays)).digest("hex");
}

function getMessageId(message: MessageLike): string | null {
    return typeof message.info.id === "string" && message.info.id.length > 0
        ? message.info.id
        : null;
}

/**
 * Resolve OpenCode message ids to the absolute ordinals used by the module.
 * The module and shadow lanes must see the same provisional suffix behavior, so
 * this is shared rather than reimplemented by the authority adapter.
 */
export async function resolveOrdinalsForModule(args: {
    sessionId: string;
    messages: MessageLike[];
    generation: number;
    memoGeneration: number;
    memo: Map<string, number>;
    memoAnchor?: RawMessageOrdinalAnchor | null;
    memoStoredCount?: number | null;
    memoCanonicalCount?: number;
}): Promise<
    | {
          ok: true;
          annotatedInput: unknown[];
          memoGeneration: number;
          memoAnchor: RawMessageOrdinalAnchor | null;
          memoStoredCount: number;
          memoCanonicalCount: number;
          normalizations: ModuleNormalizationRecord[];
      }
    | { ok: false; reason: "unresolved" | "mismatch"; messageId?: string }
> {
    const memo = args.memo;
    const generationChanged = args.memoGeneration !== args.generation;
    if (generationChanged) memo.clear();

    let anchor = generationChanged ? null : (args.memoAnchor ?? null);
    let storedCount = generationChanged ? null : (args.memoStoredCount ?? null);
    let canonicalCount = generationChanged ? 0 : (args.memoCanonicalCount ?? 0);
    const priming = storedCount === null;
    if (priming) {
        memo.clear();
        anchor = null;
        canonicalCount = 0;
    }

    const newEntries: Array<ReturnType<typeof readRawSessionMessageOrdinalPage>[number]> = [];
    let pageAnchor = anchor;
    while (true) {
        const page = readRawSessionMessageOrdinalPage(
            args.sessionId,
            pageAnchor,
            MODULE_ORDINAL_PAGE_SIZE,
        );
        if (page.length === 0) break;
        newEntries.push(...page);
        const last = page[page.length - 1];
        pageAnchor = { timeCreated: last.timeCreated, id: last.id };
        if (page.length < MODULE_ORDINAL_PAGE_SIZE) break;
        await yieldToEventLoop();
    }

    const currentStoredCount = getRawSessionStoredMessageCount(args.sessionId);
    const expectedStoredCount = (storedCount ?? 0) + newEntries.length;
    if (currentStoredCount !== expectedStoredCount) {
        memo.clear();
        return { ok: false, reason: "mismatch" };
    }

    for (const entry of newEntries) {
        if (!entry.contributesOrdinal) continue;
        canonicalCount += 1;
        const prior = memo.get(entry.id);
        if (prior !== undefined && prior !== canonicalCount) {
            memo.clear();
            return { ok: false, reason: "mismatch", messageId: entry.id };
        }
        memo.set(entry.id, canonicalCount);
    }
    anchor = pageAnchor;
    storedCount = currentStoredCount;

    const normalizations: ModuleNormalizationRecord[] = [];
    const visibleMessages = args.messages.filter((message) => {
        if (!isRawCompactionSummaryInfo(message.info)) return true;
        normalizations.push({
            kind: "summary_message",
            message_id: getMessageId(message),
            part_index: -1,
            field: "input",
            removed: JSON.stringify(message),
        });
        return false;
    });

    const annotated = visibleMessages as unknown as Array<Record<string, unknown>>;
    const resolved: Array<number | undefined> = new Array(annotated.length);
    let firstUnresolvedId: string | undefined;
    for (let index = 0; index < annotated.length; index += 1) {
        const messageId = getMessageId(visibleMessages[index]);
        if (!messageId) return { ok: false, reason: "unresolved" };
        const ordinal = memo.get(messageId);
        if (ordinal === undefined && firstUnresolvedId === undefined) firstUnresolvedId = messageId;
        resolved[index] = ordinal;
    }

    let suffixStart = annotated.length;
    while (suffixStart > 0 && resolved[suffixStart - 1] === undefined) suffixStart -= 1;
    for (let index = 0; index < suffixStart; index += 1) {
        if (resolved[index] === undefined) {
            return { ok: false, reason: "unresolved", messageId: firstUnresolvedId };
        }
    }
    if (suffixStart < annotated.length) {
        const base = suffixStart > 0 ? (resolved[suffixStart - 1] as number) : -1;
        for (let index = suffixStart; index < annotated.length; index += 1) {
            resolved[index] = base + (index - suffixStart) + 1;
        }
    }

    for (let index = 0; index < annotated.length; index += 1) {
        const messageId = getMessageId(visibleMessages[index]) as string;
        const ordinal = resolved[index] as number;
        const prior = memo.get(messageId);
        if (prior !== undefined && prior !== ordinal) {
            return { ok: false, reason: "mismatch", messageId };
        }
        memo.set(messageId, ordinal);
        annotated[index].absolute_ordinal = ordinal;
    }

    return {
        ok: true,
        annotatedInput: annotated,
        memoGeneration: args.generation,
        memoAnchor: anchor,
        memoStoredCount: storedCount,
        memoCanonicalCount: canonicalCount,
        normalizations,
    };
}

/** Flatten the typed builder shape to the module's top-level wire envelope. */
export function toFlatModuleWireBody(payload: {
    method: string;
    params: Record<string, unknown>;
}): Record<string, unknown> {
    return { method: payload.method, ...payload.params };
}

export function moduleWireBodyBytes(payload: {
    method: string;
    params: Record<string, unknown>;
}): number {
    return Buffer.byteLength(JSON.stringify(toFlatModuleWireBody(payload)));
}

/**
 * Page a transform request without changing any message value. Continuation
 * markers are understood by the module and are only used when a single item is
 * larger than the normal page envelope.
 */
export function buildPagedModuleTransformPayloads(
    body: Record<string, unknown>,
): Record<string, unknown>[] {
    if (Buffer.byteLength(JSON.stringify(body)) <= MODULE_PAGE_MAX_BYTES) return [body];

    const arrayFields = [
        "input",
        "messages",
        "native_messages",
        "ts_output",
        "ts_ck_messages",
        "normalizations",
    ].filter((field) => Array.isArray(body[field]));
    if (arrayFields.length === 0) {
        throw new Error("module transform body has no pageable message arrays");
    }
    const scalarFields = { ...body };
    for (const field of arrayFields) delete scalarFields[field];
    const transformPageId = crypto.randomUUID();
    const items = arrayFields.flatMap((field) =>
        (body[field] as unknown[]).map((value, itemIndex) => ({ field, value, itemIndex })),
    );
    const emptyArrays = (): Record<string, unknown[]> =>
        Object.fromEntries(arrayFields.map((field) => [field, []]));
    const makePage = (args: {
        index: number;
        total: number;
        complete: boolean;
        arrays: Record<string, unknown[]>;
    }): Record<string, unknown> => {
        const pageArrays = Object.fromEntries(
            arrayFields.map((field) => [field, args.arrays[field] ?? []]),
        );
        const page: Record<string, unknown> = {
            method: body.method,
            session_id: body.session_id,
            shadow_generation: body.shadow_generation,
            transform_page_id: transformPageId,
            // Authority transforms do not carry a shadow generation. A stable
            // transform generation still belongs to the page envelope so both
            // lanes use the same all-or-none paging contract.
            transform_generation: body.shadow_generation ?? 0,
            transform_page_index: args.index,
            transform_page_total: args.total,
            transform_page_complete: args.complete,
            transform_page_digest: transformPageDigest(pageArrays),
            ...pageArrays,
        };
        if (args.complete) Object.assign(page, scalarFields);
        return page;
    };
    const hasItems = (arrays: Record<string, unknown[]>): boolean =>
        Object.values(arrays).some((values) => values.length > 0);

    let assumedTotal = 1;
    for (let attempt = 0; attempt < 10; attempt += 1) {
        const pages: Record<string, unknown>[] = [];
        let current = emptyArrays();
        const appendUnit = (field: string, value: unknown): boolean => {
            const candidate = { ...current, [field]: [...current[field], value] };
            const candidatePage = makePage({
                index: pages.length,
                total: assumedTotal,
                complete: false,
                arrays: candidate,
            });
            if (Buffer.byteLength(JSON.stringify(candidatePage)) <= MODULE_PAGE_MAX_BYTES) {
                current = candidate;
                return true;
            }
            if (hasItems(current)) {
                pages.push(
                    makePage({
                        index: pages.length,
                        total: assumedTotal,
                        complete: false,
                        arrays: current,
                    }),
                );
                current = emptyArrays();
            }
            const itemOnlyPage = makePage({
                index: pages.length,
                total: assumedTotal,
                complete: false,
                arrays: { ...current, [field]: [...current[field], value] },
            });
            if (Buffer.byteLength(JSON.stringify(itemOnlyPage)) > MODULE_PAGE_MAX_BYTES)
                return false;
            current[field].push(value);
            return true;
        };

        for (const item of items) {
            if (appendUnit(item.field, item.value)) continue;
            const serialized = JSON.stringify(item.value) ?? "null";
            const bytes = Buffer.from(serialized, "utf8");
            const chunks: string[] = [];
            for (let start = 0; start < bytes.length; ) {
                let end = Math.min(start + MODULE_ITEM_CONTINUATION_CHUNK_BYTES, bytes.length);
                while (end < bytes.length && (bytes[end] & 0xc0) === 0x80) end -= 1;
                chunks.push(bytes.subarray(start, end).toString("utf8"));
                start = end;
            }
            const chunkTotal = chunks.length;
            for (const [chunkIndex, chunk] of chunks.entries()) {
                const marker = {
                    [MODULE_ITEM_CONTINUATION_KEY]: {
                        field: item.field,
                        item_index: item.itemIndex,
                        chunk_index: chunkIndex,
                        chunk_total: chunkTotal,
                    },
                    chunk,
                };
                if (!appendUnit(item.field, marker)) {
                    throw new Error("module transform continuation exceeds the 512 KiB page limit");
                }
            }
        }

        let finalPage = makePage({
            index: pages.length,
            total: assumedTotal,
            complete: true,
            arrays: current,
        });
        if (Buffer.byteLength(JSON.stringify(finalPage)) > MODULE_PAGE_MAX_BYTES) {
            if (!hasItems(current)) {
                throw new Error("module transform scalar tail exceeds the 512 KiB page limit");
            }
            pages.push(
                makePage({
                    index: pages.length,
                    total: assumedTotal,
                    complete: false,
                    arrays: current,
                }),
            );
            finalPage = makePage({
                index: pages.length,
                total: assumedTotal,
                complete: true,
                arrays: emptyArrays(),
            });
            if (Buffer.byteLength(JSON.stringify(finalPage)) > MODULE_PAGE_MAX_BYTES) {
                throw new Error("module transform scalar tail exceeds the 512 KiB page limit");
            }
        }
        pages.push(finalPage);
        if (pages.length === assumedTotal) return pages;
        assumedTotal = pages.length;
    }
    throw new Error("module transform page count did not stabilize");
}

export const __moduleWireTest = {
    buildPagedModuleTransformPayloads,
    encodeOpenCodeMessagesToCk,
    moduleWireBodyBytes,
    resolveOrdinalsForModule,
    toFlatModuleWireBody,
};

export function encodeOpenCodeMessagesToCk(messages: unknown[]): Array<{
    mid: string;
    ordinal: number;
    ck: Record<string, unknown>;
}> {
    return messages.map((message, index) => {
        const raw =
            message !== null && typeof message === "object"
                ? (message as Record<string, unknown>)
                : {};
        const info =
            raw.info !== null && typeof raw.info === "object"
                ? (raw.info as Record<string, unknown>)
                : raw;
        const id =
            (typeof info.id === "string" && info.id.length > 0 && info.id) ||
            `opencode-${crypto.createHash("sha256").update(JSON.stringify(message)).digest("hex").slice(0, 24)}`;
        const ordinal =
            (typeof raw.absolute_ordinal === "number" && raw.absolute_ordinal) ||
            (typeof info.absolute_ordinal === "number" && info.absolute_ordinal) ||
            index + 1;
        const role = typeof info.role === "string" ? info.role : "user";
        const parts = Array.isArray(raw.parts) ? raw.parts : [];
        const content: Record<string, unknown>[] = [];
        for (const partValue of parts) {
            if (partValue === null || typeof partValue !== "object") continue;
            const part = partValue as Record<string, unknown>;
            const type = typeof part.type === "string" ? part.type : "unknown";
            if (type === "text" && part.ignored !== true) {
                content.push({
                    kind: { type: "text", text: typeof part.text === "string" ? part.text : "" },
                });
            } else if (type === "reasoning") {
                content.push({
                    kind: {
                        type: "reasoning",
                        text:
                            typeof part.text === "string"
                                ? part.text
                                : typeof part.thinking === "string"
                                  ? part.thinking
                                  : "",
                    },
                });
            } else if (type === "tool") {
                const state =
                    part.state !== null && typeof part.state === "object"
                        ? (part.state as Record<string, unknown>)
                        : {};
                const callId =
                    (typeof part.callID === "string" && part.callID) ||
                    (typeof part.callId === "string" && part.callId) ||
                    (typeof part.id === "string" && part.id) ||
                    `${id}#${content.length}`;
                const toolName = typeof part.tool === "string" ? part.tool : "unknown";
                const input = state.input ?? part.input ?? part.args ?? {};
                content.push({ kind: { type: "tool_call", id: callId, name: toolName, input } });
                if (state.status === "completed" || state.status === "error") {
                    const output =
                        typeof state.output === "string"
                            ? state.output
                            : typeof state.error === "string"
                              ? state.error
                              : "";
                    content.push({
                        kind: {
                            type: "tool_result",
                            id: callId,
                            tool_name: toolName,
                            output: {
                                kind: {
                                    type: state.status === "error" ? "error_text" : "text",
                                    text: output,
                                },
                            },
                        },
                    });
                }
            } else if (
                !["compaction", "step-finish", "snapshot", "patch", "agent", "retry"].includes(type)
            ) {
                content.push({
                    kind: {
                        type: "opaque",
                        source: "opencode",
                        kind: type,
                        raw: part,
                    },
                });
            }
        }
        return {
            mid: id,
            ordinal,
            ck: {
                role,
                content,
                meta: { harness_id: id, ordinal },
            },
        };
    });
}

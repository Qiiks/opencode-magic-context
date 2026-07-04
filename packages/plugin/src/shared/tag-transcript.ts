/**
 * Harness-agnostic tagging over the Transcript interface.
 *
 * This is a deliberately minimal alternative to the OpenCode-specific
 * `tag-messages.ts` that operates on `MessageLike[]`. The OpenCode flow
 * carries 380+ lines of accumulated complexity:
 *
 *   - source-content persistence (for cross-pass detag/restore behavior),
 *   - tool-call indexing across separate "tool" and "tool_result" parts,
 *   - reasoning-byte tracking for historian projection,
 *   - file-part stable IDs,
 *   - existing-tag resolver with content-id fallback.
 *
 * Most of that is OpenCode-specific (cache stability across multi-pass
 * transforms, AI SDK part-id semantics, file part shapes). Pi's
 * `pi.on("context", ...)` fires once per LLM call with a complete
 * `AgentMessage[]`, so we can use a simpler tagging contract:
 *
 *   1. Walk the transcript in order.
 *   2. For each tag-eligible part (text, tool_use, tool_result), assign
 *      a tag number via the shared `Tagger`.
 *   3. Inject `§N§ ` prefix into the visible text (unless skipped).
 *   4. Build a `TagTarget` so `applyPendingOperations` from
 *      `apply-operations.ts` can replace this part with a sentinel when
 *      a queued drop fires.
 *
 * Tool drops aggregate by call_id across both invocation and result
 * occurrences (mirrors OpenCode tag-messages.ts:196-220). When a drop
 * fires for a tool tag, BOTH the assistant `toolCall`/`tool_use` part
 * and the user `toolResult`/`tool_result` part are mutated together so
 * the LLM sees consistent dropped state. Without this aggregation:
 *
 *   - Tool tag byte_size reflects only the args (~58 bytes for a `read`)
 *     because the FIRST occurrence (invocation) is tagged first and
 *     `assignTag` short-circuits the SECOND occurrence (result, ~4KB)
 *     to the same tag without updating byte_size.
 *   - Drops touch only the second occurrence (last write wins on
 *     `targets.set`), leaving the first in original form.
 *
 * Reuses unchanged from the OpenCode path:
 *
 *   - `Tagger` (DB-backed counter + assignment store).
 *   - `applyPendingOperations` (operates on `Map<number, TagTarget>`).
 *   - `applyFlushedStatuses` (same).
 *   - Tag prefix primitives (`prependTag`, `stripTagPrefix`, `byteSize`).
 */

import type { ContextDatabase } from "../features/magic-context/storage";
import { saveSourceContent } from "../features/magic-context/storage-source";
import {
    updateTagByteSize,
    updateTagInputByteSize,
    updateTagInputTokenCount,
    updateTagTokenCount,
} from "../features/magic-context/storage-tags";
import { makeToolCompositeKey, type Tagger } from "../features/magic-context/tagger";
import { applyEditMarkerToInput } from "../hooks/magic-context/edit-marker";
import { estimateImageTokensFromDataUrl } from "../hooks/magic-context/image-token-estimate";
import { estimateTokens } from "../hooks/magic-context/read-session-formatting";
import {
    byteSize,
    prependTag,
    stripTagPrefix,
} from "../hooks/magic-context/tag-content-primitives";
import type { TagTarget } from "../hooks/magic-context/tag-messages";
import type { Transcript, TranscriptPart } from "./transcript";

export interface TagTranscriptOptions {
    /**
     * When true, skip injecting `§N§` prefix into visible text. Tags
     * still get assigned in the DB so historian/drops can reference
     * them; the agent just doesn't see the markers. Used when the session's
     * tool surface has no `ctx_reduce` tool to act on the markers. Cache-safe
     * because the availability verdict is frozen per session.
     */
    skipPrefixInjection?: boolean;
    /**
     * Pi-only: map of messageId → raw-message fingerprint. When a NEW message
     * text tag is created, its fingerprint is persisted on the tag row so a
     * later pass can adopt the fallback-id tag onto the real SessionEntry id
     * (keeping tag_number/§N§ stable). OpenCode omits this → tags store NULL
     * → adoption never fires. Keyed by the bare messageId (not the `:pN`
     * contentId) since all parts of a message share one fingerprint.
     */
    entryFingerprintByMessageId?: ReadonlyMap<string, string>;
}

export interface TagTranscriptResult {
    targets: Map<number, TagTarget>;
}

/**
 * Tag eligible parts of a transcript and build TagTargets for them.
 *
 * "Eligible" means: parts that contribute meaningfully to the LLM input
 * and whose content can be replaced when dropped. Specifically:
 *
 *   - text parts (user or assistant): tagged as type "message", inject
 *     prefix into the visible text, target supports setContent.
 *   - thinking parts: NOT tagged. Reasoning content has provider-
 *     specific signed-content semantics (Anthropic redacted_thinking,
 *     etc.) and replacing them mid-conversation breaks signature
 *     verification. The historian's clear-reasoning pass handles them
 *     separately if needed.
 *   - tool_use parts (assistant tool invocations): tagged as type
 *     "tool", target supports drop/truncate via the tag-content
 *     primitives.
 *   - tool_result parts (folded into user messages by the Pi adapter):
 *     tagged as type "tool", paired with the corresponding invocation
 *     for full-pair drops.
 *   - image, file, structural, unknown: skipped.
 *
 * The contentId we pass to the tagger uses the part's stable id when
 * available, otherwise a synthetic locator. Pi's adapter exposes:
 *   - tool_use parts: id = ToolCall.id (from pi-ai)
 *   - tool_result parts: id = ToolResultMessage.toolCallId
 *   - text parts: id = undefined → we synthesize from message+ordinal
 */
/**
 * Per-callId aggregation of tool occurrences across the transcript.
 * Built up during the walk and used to:
 *   1. Assign one tag per call_id with byte_size = the tool_RESULT (output)
 *      size, and inputByteSize = the tool_use (args) size, tracked SEPARATELY
 *      (mirrors OpenCode tag-messages.ts). Reclaim accounting sums them
 *      (byteSize + inputByteSize + reasoning); folding args into byte_size too
 *      would double-count the args for a large-input/small-output tool.
 *   2. Build a single aggregate TagTarget that mutates BOTH the
 *      invocation and result occurrences atomically, so a queued drop
 *      replaces both halves with a sentinel instead of last-write-wins.
 */
interface ToolOccurrence {
    message: { info: { id?: string; role: string } };
    part: TranscriptPart;
    kind: "tool_use" | "tool_result";
}

interface ToolAggregate {
    callId: string;
    occurrences: ToolOccurrence[];
    /** Largest byteSize seen across occurrences — used as the tag size. */
    maxByteSize: number;
    /** Token count paired with maxByteSize (the same output occurrence). */
    maxTokenCount: number;
    /** Tool name from the first occurrence we see one on. */
    toolName: string | null;
    /** Input byte size from the invocation occurrence (for storage projection). */
    inputByteSize: number;
    /** Whether input_token_count has been persisted (from the tool_use occurrence). */
    inputTokenStored: boolean;
}

export function tagTranscript(
    sessionId: string,
    transcript: Transcript,
    tagger: Tagger,
    db: ContextDatabase,
    options: TagTranscriptOptions = {},
): TagTranscriptResult {
    const skipPrefixInjection = options.skipPrefixInjection === true;
    const targets = new Map<number, TagTarget>();

    // Tool aggregation is keyed by the same owner+callId identity used by
    // assignToolTag. OpenCode/Pi callId counters can repeat across turns, so
    // a bare callId key can merge distinct invocations and replay drops/status
    // changes against the wrong tool pair.
    const toolAggregates = new Map<string, ToolAggregate & { tagId: number }>();
    const openToolAggregateKeysByCallId = new Map<string, string[]>();
    let activeToolResultRun: { callId: string; aggregateKey: string } | undefined;

    // v3.3.1 Layer C (plan v3.3.1 Finding #16): the previous outer
    // db.transaction() wrapper rolled back EVERY tag insert + savedSource
    // when a single UNIQUE collision fired late in the walk. Per-tag
    // SAVEPOINTs inside `assignToolTag` / `assignTag` already give us the
    // atomicity we need. Removing the wrapper matches OpenCode's
    // tag-messages.ts design — see the long comment there for the
    // rationale (cache-bust amplifier story).
    for (let msgIndex = 0; msgIndex < transcript.messages.length; msgIndex += 1) {
        const message = transcript.messages[msgIndex];
        if (message === undefined) continue;
        activeToolResultRun = undefined;
        const messageId = message.info.id;

        let textOrdinal = 0;
        const parts = message.parts;

        for (let partIndex = 0; partIndex < parts.length; partIndex += 1) {
            const part = parts[partIndex];
            if (part === undefined) continue;

            if (part.kind !== "tool_result") {
                activeToolResultRun = undefined;
            }

            if (part.kind === "text") {
                // Synthetic message ids (Pi tail synthetic user with
                // no id) cannot be tagged — there's no stable handle
                // to bind a tag to across passes. Pass through
                // untagged; this is rare (only happens for the
                // dangling tool-result tail case in Pi).
                if (messageId === undefined) {
                    textOrdinal += 1;
                    continue;
                }
                tagTextPart({
                    sessionId,
                    message,
                    messageId,
                    msgIndex,
                    textOrdinal,
                    part,
                    tagger,
                    db,
                    targets,
                    skipPrefixInjection,
                    entryFingerprint: options.entryFingerprintByMessageId?.get(messageId) ?? null,
                });
                textOrdinal += 1;
                continue;
            }

            if (part.kind === "tool_use" || part.kind === "tool_result") {
                if (messageId === undefined) {
                    activeToolResultRun = undefined;
                    continue;
                }

                const callId = part.id;
                const text = part.getText() ?? "";
                const toolByteSize = getToolPartByteSize(part, text);
                const toolTokenCount = getToolPartTokenCount(part, text);
                const meta = part.getToolMetadata();
                const inputTokenCount = meta.inputTokenCount;

                if (typeof callId !== "string" || callId.length === 0) {
                    activeToolResultRun = undefined;
                    // No stable callId to aggregate on. Tag independently.
                    tagToolPart({
                        sessionId,
                        message,
                        messageId,
                        msgIndex,
                        partIndex,
                        part,
                        tagger,
                        db,
                        targets,
                        skipPrefixInjection,
                    });
                    continue;
                }

                const pendingKeys = openToolAggregateKeysByCallId.get(callId) ?? [];
                let existingKey: string | undefined;
                if (part.kind === "tool_result") {
                    if (
                        activeToolResultRun !== undefined &&
                        activeToolResultRun.callId === callId
                    ) {
                        existingKey = activeToolResultRun.aggregateKey;
                    } else {
                        existingKey = findLastUnresolvedToolAggregateKey(
                            pendingKeys,
                            toolAggregates,
                        );
                    }
                }
                const aggregateKey: string = existingKey ?? makeToolCompositeKey(messageId, callId);
                const existing = toolAggregates.get(aggregateKey);
                if (existing) {
                    // Later occurrence for this owner+callId pair. Merge into the
                    // aggregate, update byte accounting if larger, and rebuild the
                    // TagTarget so drops mutate both invocation and result.
                    existing.occurrences.push({
                        message,
                        part,
                        kind: part.kind,
                    });
                    // byte_size tracks OUTPUT bytes only (the tool_result
                    // occurrence). The invocation args are captured separately in
                    // inputByteSize below; counting tool_use bytes here too would
                    // double-count args in the emergency-drop reclaim formula
                    // (byteSize + inputByteSize + reasoning). Mirrors OpenCode,
                    // which assigns the tool tag on the result path.
                    if (part.kind === "tool_result" && toolByteSize > existing.maxByteSize) {
                        existing.maxByteSize = toolByteSize;
                        existing.maxTokenCount = toolTokenCount;
                        updateTagByteSize(db, sessionId, existing.tagId, toolByteSize);
                        // Keep token_count in lockstep with the byte bump so the
                        // grown output's tokens aren't undercounted by readers.
                        updateTagTokenCount(db, sessionId, existing.tagId, toolTokenCount);
                    }
                    if (existing.toolName === null && meta.toolName) {
                        existing.toolName = meta.toolName;
                    }
                    if (
                        existing.inputByteSize === 0 &&
                        part.kind === "tool_use" &&
                        meta.inputByteSize > 0
                    ) {
                        existing.inputByteSize = meta.inputByteSize;
                        updateTagInputByteSize(db, sessionId, existing.tagId, meta.inputByteSize);
                    }
                    if (
                        !existing.inputTokenStored &&
                        part.kind === "tool_use" &&
                        inputTokenCount > 0
                    ) {
                        existing.inputTokenStored = true;
                        updateTagInputTokenCount(db, sessionId, existing.tagId, inputTokenCount);
                    }
                    // Inject §N§ prefix into this tool_result occurrence
                    // (matches OpenCode behavior — only result gets the prefix).
                    if (!skipPrefixInjection && part.kind === "tool_result") {
                        part.setText(prependTag(existing.tagId, text));
                    }
                    // Rebuild the aggregate target so it walks the now-
                    // longer occurrences list.
                    targets.set(
                        existing.tagId,
                        buildAggregateTarget(existing.tagId, existing.occurrences),
                    );
                    if (part.kind === "tool_result") {
                        markToolAggregateResolved(
                            callId,
                            aggregateKey,
                            openToolAggregateKeysByCallId,
                        );
                        activeToolResultRun = { callId, aggregateKey };
                    }
                } else {
                    // First occurrence for this owner+callId identity — reserve
                    // the tag number. Owner stays stable across passes because
                    // transcript message ids are durable.
                    // byte_size is OUTPUT-only (0 until the tool_result occurrence
                    // is seen); the invocation args live in inputByteSize. This
                    // keeps the emergency-drop reclaim formula (byteSize +
                    // inputByteSize + reasoning) from double-counting args when the
                    // first occurrence is a large tool_use. Mirrors OpenCode.
                    const outputByteSize = part.kind === "tool_result" ? toolByteSize : 0;
                    const outputTokenCount = part.kind === "tool_result" ? toolTokenCount : 0;
                    const firstInputTokenCount = part.kind === "tool_use" ? inputTokenCount : 0;
                    const tagId = tagger.assignToolTag(
                        sessionId,
                        callId,
                        messageId,
                        outputByteSize,
                        db,
                        0,
                        meta.toolName ?? null,
                        meta.inputByteSize,
                        () => ({
                            tokenCount: outputTokenCount,
                            inputTokenCount: firstInputTokenCount,
                            reasoningTokenCount: null,
                        }),
                    );
                    const aggregate = {
                        callId,
                        tagId,
                        occurrences: [
                            {
                                message,
                                part,
                                kind: part.kind,
                            },
                        ],
                        maxByteSize: outputByteSize,
                        maxTokenCount: outputTokenCount,
                        toolName: meta.toolName ?? null,
                        inputByteSize: part.kind === "tool_use" ? meta.inputByteSize : 0,
                        inputTokenStored: part.kind === "tool_use" && firstInputTokenCount > 0,
                    };
                    toolAggregates.set(aggregateKey, aggregate);
                    if (part.kind === "tool_use") {
                        openToolAggregateKeysByCallId.set(callId, [...pendingKeys, aggregateKey]);
                    }
                    // Inject §N§ prefix into this occurrence's visible text
                    // when it's a tool_result. (OpenCode parity: prefix
                    // only goes on the result, not the invocation.)
                    if (!skipPrefixInjection && part.kind === "tool_result") {
                        part.setText(prependTag(tagId, text));
                    }
                    targets.set(tagId, buildAggregateTarget(tagId, aggregate.occurrences));
                    if (part.kind === "tool_result") {
                        markToolAggregateResolved(
                            callId,
                            aggregateKey,
                            openToolAggregateKeysByCallId,
                        );
                        activeToolResultRun = { callId, aggregateKey };
                    }
                }
            }
            // thinking, image, file, structural, unknown → skip.
        }
    }

    return { targets };
}

function findLastUnresolvedToolAggregateKey(
    pendingKeys: string[],
    toolAggregates: Map<string, ToolAggregate & { tagId: number }>,
): string | undefined {
    for (let i = pendingKeys.length - 1; i >= 0; i -= 1) {
        const key = pendingKeys[i];
        if (key === undefined) continue;
        const aggregate = toolAggregates.get(key);
        if (aggregate === undefined) continue;
        if (!aggregate.occurrences.some((occ) => occ.kind === "tool_result")) {
            return key;
        }
    }
    return undefined;
}

function markToolAggregateResolved(
    callId: string,
    aggregateKey: string,
    openToolAggregateKeysByCallId: Map<string, string[]>,
): void {
    const pendingKeys = openToolAggregateKeysByCallId.get(callId);
    if (pendingKeys === undefined) return;
    const nextPendingKeys = pendingKeys.filter((key) => key !== aggregateKey);
    if (nextPendingKeys.length === 0) {
        openToolAggregateKeysByCallId.delete(callId);
        return;
    }
    openToolAggregateKeysByCallId.set(callId, nextPendingKeys);
}

/** Real-tokenizer count for tagged text (images bill by visual tokens). */
function estimateTagTextTokens(text: string): number {
    if (!text) return 0;
    if (text.startsWith("data:image/")) return estimateImageTokensFromDataUrl(text);
    return estimateTokens(text);
}

function getToolPartByteSize(part: TranscriptPart, text: string): number {
    const textByteSize = byteSize(text);
    if (textByteSize > 0 || part.kind !== "tool_result") return textByteSize;
    return getNonTextToolResultByteSize(part);
}

/**
 * Real-tokenizer mirror of {@link getToolPartByteSize}: token count of a tool
 * part's output text (falling back to the raw payload for non-text results,
 * matching the byte path so token_count stays consistent with byte_size).
 */
function getToolPartTokenCount(part: TranscriptPart, text: string): number {
    if (text.length > 0 || part.kind !== "tool_result") return estimateTokens(text);
    const raw = part.rawByteSize?.();
    if (typeof raw === "number" && raw > 0) {
        const record = isRecord(part) ? part : undefined;
        const content =
            record?.content ??
            record?.rawContent ??
            record?.rawPart ??
            record?.part ??
            record?.data ??
            record?.image ??
            record?.source;
        const serialized = safeJsonStringify(content ?? part);
        return serialized === undefined ? 0 : estimateTokens(serialized);
    }
    return 0;
}

function getNonTextToolResultByteSize(part: TranscriptPart): number {
    // Prefer the adapter's exact raw-payload size when available (Pi's
    // tool_result proxy can serialize the real content array, incl. images).
    const raw = part.rawByteSize?.();
    if (typeof raw === "number" && raw > 0) return raw;
    const record = isRecord(part) ? part : undefined;
    const content =
        record?.content ??
        record?.rawContent ??
        record?.rawPart ??
        record?.part ??
        record?.data ??
        record?.image ??
        record?.source;
    const serialized = safeJsonStringify(content ?? part);
    return serialized === undefined ? 0 : byteSize(serialized);
}

function safeJsonStringify(value: unknown): string | undefined {
    try {
        return JSON.stringify(value);
    } catch {
        return undefined;
    }
}

function isRecord(value: unknown): value is Record<string, unknown> {
    return typeof value === "object" && value !== null;
}

interface TagTextPartArgs {
    sessionId: string;
    message: { info: { id?: string; role: string } };
    messageId: string;
    msgIndex: number;
    textOrdinal: number;
    part: TranscriptPart;
    tagger: Tagger;
    db: ContextDatabase;
    targets: Map<number, TagTarget>;
    skipPrefixInjection: boolean;
    entryFingerprint: string | null;
}

function tagTextPart(args: TagTextPartArgs): void {
    const text = args.part.getText() ?? "";
    const contentId = `${args.messageId}:p${args.textOrdinal}`;
    const tagId = args.tagger.assignTag(
        args.sessionId,
        contentId,
        "message",
        byteSize(text),
        args.db,
        0,
        null,
        0,
        args.entryFingerprint,
        // Lazy: fires only on fresh insert. Strip any §N§ prefix so a re-tag
        // from already-prefixed text still tokenizes the pristine content.
        () => ({
            tokenCount: estimateTagTextTokens(stripTagPrefix(text)),
            inputTokenCount: null,
            reasoningTokenCount: null,
        }),
    );

    // Persist the original (pre-tagged) source content so caveman
    // compression and other "compress from original" heuristics have
    // pristine text to read on later passes. saveSourceContent uses
    // INSERT OR IGNORE — first write wins; later passes that re-tag
    // the same (sessionId, tagId) pair from already-prefixed text won't
    // overwrite the original. Cache-stable.
    //
    // We strip any existing §N§ prefix before saving in case a previous
    // pass already injected one and the persisted source got lost
    // (e.g. legacy session created before this code shipped). For new
    // sessions stripTagPrefix is a no-op on the very first pass.
    const sourceContent = stripTagPrefix(text);
    if (sourceContent.trim().length > 0) {
        saveSourceContent(args.db, args.sessionId, tagId, sourceContent);
    }

    if (!args.skipPrefixInjection) {
        args.part.setText(prependTag(tagId, text));
    }

    args.targets.set(tagId, buildTextTarget(args.part, args.message));
}

interface TagToolPartArgs {
    sessionId: string;
    message: { info: { id?: string; role: string } };
    messageId: string;
    msgIndex: number;
    partIndex: number;
    part: TranscriptPart;
    tagger: Tagger;
    db: ContextDatabase;
    targets: Map<number, TagTarget>;
    skipPrefixInjection: boolean;
}

function tagToolPart(args: TagToolPartArgs): void {
    // Prefer the part's stable id (tool call id from Pi/OpenCode); fall
    // back to a synthetic locator. Tool calls and their results MAY
    // share an id (Pi sets toolCallId on ToolResultMessage to match the
    // originating ToolCall.id); when that happens, both tag operations
    // resolve to the same tag number — desired behavior, since drops
    // target the call-id pair as a unit.
    const stableId = args.part.id;
    const contentId = stableId ?? `${args.messageId}:t${args.partIndex}`;
    const text = args.part.getText() ?? "";
    const toolByteSize = getToolPartByteSize(args.part, text);
    const toolTokenCount = getToolPartTokenCount(args.part, text);
    const meta = args.part.getToolMetadata();
    // v3.3.1 Layer C: synthetic ownership for the no-callId Pi
    // fallback. Owner == callId == contentId. The composite key
    // collapses to a unique synthetic identifier per part, preserving
    // the legacy "each part gets its own tag" behavior while
    // satisfying the composite-identity contract (TagEntry.tool_owner_message_id
    // is non-null, lazy-adoption path is correctly bypassed).
    const tagId = args.tagger.assignToolTag(
        args.sessionId,
        contentId,
        contentId,
        toolByteSize,
        args.db,
        0,
        meta.toolName ?? null,
        meta.inputByteSize,
        () => ({
            tokenCount: toolTokenCount,
            inputTokenCount: meta.inputTokenCount,
            reasoningTokenCount: null,
        }),
    );

    // For tool parts, the visible payload is the tool result text. We
    // can inject the tag prefix into it for in-text references; this
    // matches the OpenCode behavior of tagging tool outputs.
    if (!args.skipPrefixInjection && args.part.kind === "tool_result") {
        const tagged = prependTag(tagId, text);
        args.part.setText(tagged);
    }

    args.targets.set(tagId, buildToolTarget(args.part, args.message, tagId));
}

function setToolContentOrText(part: TranscriptPart, content: string): boolean {
    try {
        if (part.setToolOutput(content)) return true;
    } catch {
        // Pi assistant tool_use parts deliberately assert if callers try
        // to write a nonexistent output slot. Truncated-mode drops still
        // need to shrink the invocation, so fall back to visible text/args
        // replacement while preserving the adapter-level invariant.
    }
    return part.setText(content);
}

/**
 * Build a TagTarget that walks ALL occurrences of a tool call (invocation
 * + result) when mutating. This is the per-callId aggregate target used
 * by `tagTranscript` so a single drop replaces both halves.
 *
 * The closures hold a reference to the same `occurrences` array stored
 * on the aggregate, so when the array gets mutated (a second occurrence
 * is pushed mid-walk), the next call to setContent/drop/truncate sees
 * all occurrences automatically. Callers MUST rebuild the target after
 * pushing a new occurrence so the targets map points to a fresh closure
 * over the updated array — otherwise consumers that captured the target
 * before the push won't see the new occurrence.
 *
 * Mirrors OpenCode's createToolDropTarget semantics in tool-drop-target.ts.
 */
function buildAggregateTarget(tagId: number, occurrences: ToolOccurrence[]): TagTarget {
    const role = occurrences[0]?.message.info.role ?? "user";
    const messageId = occurrences[0]?.message.info.id;

    return {
        setContent(content: string): boolean {
            // Walk all occurrences; mutate every one. Return true if at
            // least one occurrence's content actually changed (used to
            // gate sentinel-replay re-writes).
            let changed = false;
            for (const occ of occurrences) {
                // Try setToolOutput first (works on tool_result-shaped parts);
                // fall back to setText so tool_use parts also get sentinelized.
                if (setToolContentOrText(occ.part, content)) {
                    changed = true;
                }
            }
            return changed;
        },
        getContent(): string | null {
            // Prefer the result occurrence's content (the bulky payload).
            for (const occ of occurrences) {
                if (occ.kind === "tool_result") {
                    return occ.part.getText() ?? null;
                }
            }
            return occurrences[0]?.part.getText() ?? null;
        },
        drop(): "removed" | "absent" {
            // Replace BOTH halves with the dropped sentinel.
            const sentinel = `[dropped \u00a7${tagId}\u00a7]`;
            let any = false;
            for (const occ of occurrences) {
                if (occ.part.replaceWithSentinel(sentinel)) any = true;
            }
            return any ? "removed" : "absent";
        },
        truncate(): "truncated" | "absent" {
            // Skeleton-drop: replace BOTH halves' content with the one
            // canonical `[dropped §N§]` placeholder (byte-identical to a full
            // drop and to OpenCode). Frozen by the dropMode column → replays
            // the same string every pass. The tool_use call survives intact.
            const sentinel = `[dropped \u00a7${tagId}\u00a7]`;
            let any = false;
            for (const occ of occurrences) {
                if (setToolContentOrText(occ.part, sentinel)) {
                    any = true;
                }
            }
            return any ? "truncated" : "absent";
        },
        editMarker(): "truncated" | "absent" {
            // Edit-marker: preserve the tool_use input's filePath + a region
            // hint of the diff, sentinelize the result half. Separate from
            // truncate() so the existing skeleton bytes are never touched.
            // Deterministic + idempotent (re-derived from source each pass; the
            // region-hint clamp self-guards via ...[truncated]).
            const sentinel = `[dropped \u00a7${tagId}\u00a7]`;
            let any = false;
            for (const occ of occurrences) {
                if (occ.kind === "tool_use") {
                    const input = occ.part.getToolInput?.();
                    if (input) {
                        const next = { ...input };
                        applyEditMarkerToInput(next);
                        if (occ.part.setToolInput?.(next)) any = true;
                    }
                } else if (setToolContentOrText(occ.part, sentinel)) {
                    any = true;
                }
            }
            return any ? "truncated" : "absent";
        },
        // Non-mutating reclaim predicate (Pi parity with OpenCode's canDrop).
        // Pi sentinelizes BOTH halves, so unlike OpenCode there's no
        // result-part requirement — a target reclaims as long as it still has
        // at least one live occurrence to sentinelize.
        canDrop(): boolean {
            return occurrences.length > 0;
        },
        // Non-mutating read of the invocation input (the tool_use occurrence
        // carries the arguments). Used by smart-drops supersession selection.
        readInput(): Record<string, unknown> | null {
            for (const occ of occurrences) {
                const input = occ.part.getToolInput?.();
                if (input) return input;
            }
            return null;
        },
        message: {
            info: { id: messageId, role },
            parts: [],
        },
    };
}

/**
 * TagTarget for a tag-eligible text part. The shared
 * `applyPendingOperations` flow calls `setContent` to swap in a
 * sentinel like `[dropped §N§]` when a queued drop fires; `getContent`
 * returns the current visible text so the truncated-preview path can
 * compute its before/after.
 *
 * The `message.info.role` is used by `buildReplacementContent` in
 * `apply-operations.ts` to differentiate user-message drops (which
 * preserve a truncated preview) from assistant drops (full sentinel).
 */
function buildTextTarget(
    part: TranscriptPart,
    message: { info: { id?: string; role: string } },
): TagTarget {
    return {
        setContent(content: string): boolean {
            return part.setText(content);
        },
        getContent(): string | null {
            return part.getText() ?? null;
        },
        // `message` is typed as MessageLike, which has parts: unknown[].
        // We don't carry parts here (the apply-operations flow only
        // reads `info.role` on this field), so a minimal stub is
        // sufficient.
        message: {
            info: { id: message.info.id, role: message.info.role },
            parts: [],
        },
    };
}

/**
 * TagTarget for a tag-eligible tool part. Tool parts get full-drop or
 * skeleton-drop treatment from `applyFlushedStatuses` based on the stored
 * `drop_mode` column. Both render the SAME canonical `[dropped §N§]`
 * placeholder — full-drop replaces the whole pair, skeleton-drop keeps the
 * tool_use call and replaces only its output. One placeholder string,
 * byte-identical across passes and across harnesses.
 */
function buildToolTarget(
    part: TranscriptPart,
    message: { info: { id?: string; role: string } },
    tagId: number,
): TagTarget {
    return {
        setContent(content: string): boolean {
            return setToolContentOrText(part, content);
        },
        getContent(): string | null {
            return part.getText() ?? null;
        },
        drop(): "removed" | "absent" {
            // Replace the tool part's visible content with a "[dropped]"
            // shell. We can't physically remove the part because Pi
            // requires tool_use ↔ tool_result pairing for the LLM call
            // to validate; instead we shrink the content to a sentinel.
            // For Pi the current Transcript contract treats both
            // invocation and result parts symmetrically — both expose
            // setText / setToolOutput.
            const replaced = part.replaceWithSentinel(`[dropped \u00a7${tagId}\u00a7]`);
            return replaced ? "removed" : "absent";
        },
        truncate(): "truncated" | "absent" {
            // Skeleton-drop: replace the tool output with the one canonical
            // `[dropped §N§]` placeholder (byte-identical to a full drop and to
            // OpenCode). Frozen by the dropMode column, so it replays the same
            // string every pass. The tool_use call itself survives intact.
            const ok = setToolContentOrText(part, `[dropped \u00a7${tagId}\u00a7]`);
            return ok ? "truncated" : "absent";
        },
        message: {
            info: { id: message.info.id, role: message.info.role },
            parts: [],
        },
    };
}

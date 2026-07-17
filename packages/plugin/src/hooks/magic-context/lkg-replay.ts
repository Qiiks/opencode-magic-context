import {
    captureSlot,
    dropSlot,
    getSlot,
    type LkgEntryNote,
    type LkgSlot,
    noteEntry,
} from "./lkg-slot";
import { assertOpenAiCompatAdjacency } from "./openai-compat-adjacency";
import type { MessageLike } from "./transform-operations";

export interface LkgModelKeys {
    modelKey: string | null;
    providerKey: string | null;
}

export function resolveLkgModelKeys(messages: MessageLike[]): LkgModelKeys {
    for (let index = messages.length - 1; index >= 0; index -= 1) {
        const info = messages[index]?.info as Record<string, unknown> | undefined;
        const nested = info?.model;
        if (nested && typeof nested === "object") {
            const provider = (nested as Record<string, unknown>).providerID;
            const model = (nested as Record<string, unknown>).modelID;
            if (typeof provider === "string" && typeof model === "string") {
                return { modelKey: `${provider}/${model}`, providerKey: provider };
            }
        }
        const provider = info?.providerID;
        const model = info?.modelID;
        if (
            info?.role === "assistant" &&
            typeof provider === "string" &&
            typeof model === "string"
        ) {
            return { modelKey: `${provider}/${model}`, providerKey: provider };
        }
    }
    return { modelKey: null, providerKey: null };
}

export interface LkgCaptureInput {
    sessionId: string;
    input: MessageLike[];
    output: MessageLike[];
    modelKey: string | null;
    providerKey: string | null;
    capturedAt?: number;
}

export type LkgValidationFailure =
    | "lkg_model_mismatch"
    | "lkg_invalidated_reshape"
    | "lkg_unsafe_seam"
    | "lkg_seam_invalid";

function recordValue(info: unknown, key: string): unknown {
    return info && typeof info === "object" ? (info as Record<string, unknown>)[key] : undefined;
}

function messageInfo(message: MessageLike): Record<string, unknown> {
    if (message.info && typeof message.info === "object")
        return message.info as Record<string, unknown>;
    return message as unknown as Record<string, unknown>;
}

function messageParts(message: MessageLike): unknown[] {
    return Array.isArray(message.parts) ? message.parts : [];
}

function messageRole(message: MessageLike): string | undefined {
    const infoRole = recordValue(messageInfo(message), "role");
    if (typeof infoRole === "string") return infoRole;
    const role = recordValue(message, "role");
    return typeof role === "string" ? role : undefined;
}

function messageId(message: MessageLike): string | null {
    const id = recordValue(messageInfo(message), "id") ?? recordValue(message, "id");
    return typeof id === "string" && id.length > 0 ? id : null;
}

function messageTime(message: MessageLike): number | null {
    const info = messageInfo(message);
    const candidates = [
        info.timeCreated,
        info.time_created,
        info.createdAt,
        info.created_at,
        (info.time as Record<string, unknown> | undefined)?.created,
    ];
    for (const value of candidates) {
        if (typeof value === "number" && Number.isFinite(value)) return value;
    }
    return null;
}

function isSynthetic(message: MessageLike): boolean {
    return recordValue(messageInfo(message), "synthetic") === true;
}

function hasSyntheticParts(message: MessageLike): boolean {
    const parts = messageParts(message);
    return (
        parts.length > 0 &&
        parts.every((part) => {
            return Boolean(
                part &&
                    typeof part === "object" &&
                    (part as Record<string, unknown>).synthetic === true,
            );
        })
    );
}

function isSyntheticOutput(message: MessageLike): boolean {
    return isSynthetic(message) || hasSyntheticParts(message);
}

function isRealUser(message: MessageLike): boolean {
    const synthetic = recordValue(messageInfo(message), "synthetic");
    if (synthetic !== undefined && typeof synthetic !== "boolean") return false;
    return messageRole(message) === "user" && synthetic !== true && messageId(message) !== null;
}

function latestAssistant(messages: MessageLike[]): MessageLike | null {
    for (let index = messages.length - 1; index >= 0; index -= 1) {
        if (messageRole(messages[index]) === "assistant") return messages[index];
    }
    return null;
}

function hasUnexecutedTool(message: MessageLike): boolean {
    for (const rawPart of messageParts(message)) {
        if (!rawPart || typeof rawPart !== "object") continue;
        const part = rawPart as Record<string, unknown>;
        if (part.type !== "tool") continue;
        if (part.providerExecuted !== true) {
            const state = part.state;
            if (!state || typeof state !== "object") return true;
            if ((state as Record<string, unknown>).status !== "completed") return true;
        }
    }
    return false;
}

function assistantIsActive(message: MessageLike): boolean {
    const finish = recordValue(message.info, "finish");
    return finish === "tool-calls" || hasUnexecutedTool(message);
}

export function findLkgAnchor(messages: MessageLike[]): number | null {
    const assistant = latestAssistant(messages);
    const assistantTime = assistant ? messageTime(assistant) : null;
    if (assistant && assistantTime === null) return null;
    let anchor = -1;
    for (let index = messages.length - 1; index >= 0; index -= 1) {
        const message = messages[index];
        if (!isRealUser(message)) continue;
        if (assistant && assistantIsActive(assistant)) {
            const userTime = messageTime(message);
            if (assistantTime === null || userTime === null || userTime <= assistantTime) continue;
        }
        anchor = index;
        break;
    }
    return anchor >= 0 ? anchor : null;
}

function outputMessageIsPostAnchor(
    message: MessageLike,
    inputIndexById: Map<string, number>,
    anchorIndex: number,
): boolean | null {
    const id = messageId(message);
    if (id !== null) {
        const inputIndex = inputIndexById.get(id);
        if (inputIndex !== undefined) return inputIndex > anchorIndex;
        if (!isSyntheticOutput(message)) return null;
        const linked = ["sourceMessageId", "ownerMessageId", "anchorMessageId", "messageId"]
            .map((key) => recordValue(messageInfo(message), key))
            .find((value) => typeof value === "string");
        if (typeof linked === "string") {
            const linkedIndex = inputIndexById.get(linked);
            if (linkedIndex === undefined) return null;
            return linkedIndex > anchorIndex;
        }
        return false;
    }
    if (!isSyntheticOutput(message)) return null;
    const linked = ["sourceMessageId", "ownerMessageId", "anchorMessageId"]
        .map((key) => recordValue(message.info, key))
        .find((value) => typeof value === "string");
    if (typeof linked !== "string") return false;
    const linkedIndex = inputIndexById.get(linked);
    return linkedIndex === undefined ? null : linkedIndex > anchorIndex;
}

export function buildLkgPrefix(
    input: MessageLike[],
    output: MessageLike[],
): {
    anchorIndex: number;
    anchorMessageId: string;
    inputIdSeq: string[];
    prefix: MessageLike[];
} | null {
    const anchorIndex = findLkgAnchor(input);
    if (anchorIndex === null) return null;
    const inputIds = input.map(messageId);
    if (inputIds.some((id) => id === null)) return null;
    const ids = inputIds as string[];
    if (new Set(ids).size !== ids.length) return null;
    const anchorMessageId = ids[anchorIndex];
    const inputIndexById = new Map(ids.map((id, index) => [id, index]));
    const prefix: MessageLike[] = [];
    for (const message of output) {
        const postAnchor = outputMessageIsPostAnchor(message, inputIndexById, anchorIndex);
        if (postAnchor === null) return null;
        if (!postAnchor) prefix.push(message);
    }
    let json: string;
    try {
        json = JSON.stringify(prefix);
        if (typeof json !== "string") return null;
        const parsed = JSON.parse(json);
        if (!Array.isArray(parsed) || JSON.stringify(parsed) !== json) return null;
    } catch {
        return null;
    }
    return {
        anchorIndex,
        anchorMessageId,
        inputIdSeq: ids.slice(0, anchorIndex + 1),
        prefix,
    };
}

export function captureLkgSlot(args: LkgCaptureInput): boolean {
    const built = buildLkgPrefix(args.input, args.output);
    if (!built) return false;
    let jsonPrefix: string;
    try {
        jsonPrefix = JSON.stringify(built.prefix);
        if (typeof jsonPrefix !== "string") return false;
    } catch {
        return false;
    }
    return captureSlot(args.sessionId, {
        jsonPrefix,
        inputIdSeq: built.inputIdSeq,
        lastInputMessageId: built.anchorMessageId,
        modelKey: args.modelKey,
        providerKey: args.providerKey,
        capturedAt: args.capturedAt ?? Date.now(),
    });
}

function entryIdsAreValid(slot: LkgSlot, entryIds: string[]): boolean {
    if (slot.inputIdSeq.length === 0 || entryIds.length < slot.inputIdSeq.length) return false;
    if (slot.inputIdSeq[slot.inputIdSeq.length - 1] !== slot.lastInputMessageId) return false;
    const seen = new Set<string>();
    for (const id of entryIds) {
        if (!id || seen.has(id)) return false;
        seen.add(id);
    }
    if (entryIds.indexOf(slot.lastInputMessageId) !== slot.inputIdSeq.length - 1) return false;
    for (let index = 0; index < slot.inputIdSeq.length; index += 1) {
        if (entryIds[index] !== slot.inputIdSeq[index]) return false;
    }
    return true;
}

function partCallIds(message: MessageLike): string[] {
    const ids: string[] = [];
    for (const rawPart of messageParts(message)) {
        if (!rawPart || typeof rawPart !== "object") continue;
        const part = rawPart as Record<string, unknown>;
        if (part.type !== "tool" && part.type !== "tool_use") continue;
        const callId = part.callID ?? part.callId ?? part.id;
        if (typeof callId === "string" && callId.length > 0) ids.push(callId);
    }
    return ids;
}

function partResultIds(message: MessageLike): string[] {
    const ids: string[] = [];
    for (const rawPart of messageParts(message)) {
        if (!rawPart || typeof rawPart !== "object") continue;
        const part = rawPart as Record<string, unknown>;
        if (part.type !== "tool_result" && part.type !== "tool-result") continue;
        const callId = part.tool_call_id ?? part.tool_use_id ?? part.callID ?? part.callId;
        if (typeof callId === "string" && callId.length > 0) ids.push(callId);
    }
    return ids;
}

function partIsReasoning(part: unknown): boolean {
    return Boolean(
        part && typeof part === "object" && (part as Record<string, unknown>).type === "reasoning",
    );
}

export function validateLkgSeamBoundary(prefix: MessageLike[], tail: MessageLike[]): boolean {
    const last = prefix[prefix.length - 1];
    const first = tail[0];
    if (!last || !first) return true;
    const lastCalls = partCallIds(last);
    if (lastCalls.length === 0) return true;
    const firstCalls = new Set([...partCallIds(first), ...partResultIds(first)]);
    if (messageRole(first) === "tool" || lastCalls.some((callId) => firstCalls.has(callId)))
        return false;
    return !messageParts(last).some((part) => {
        if (!part || typeof part !== "object") return false;
        const value = part as Record<string, unknown>;
        if (value.type !== "tool") return false;
        const state = value.state;
        return (
            !state ||
            typeof state !== "object" ||
            (state as Record<string, unknown>).status !== "completed"
        );
    });
}

export function validateLkgSeam(
    prefix: MessageLike[],
    tail: MessageLike[],
    providerKey: string | null,
): boolean {
    const all = [...prefix, ...tail];
    const ids = new Set<string>();
    const calls = new Set<string>();
    const results = new Set<string>();
    for (const message of all) {
        const id = messageId(message);
        if (id !== null) {
            if (ids.has(id)) return false;
            ids.add(id);
        }
        for (const callId of partCallIds(message)) {
            if (calls.has(callId)) return false;
            calls.add(callId);
        }
        for (const callId of partResultIds(message)) {
            if (results.has(callId)) return false;
            results.add(callId);
        }
        if (messageRole(message) !== "assistant" && messageParts(message).some(partIsReasoning))
            return false;
        if (
            providerKey !== "anthropic" &&
            messageParts(message).some((part) => {
                if (!part || typeof part !== "object") return false;
                const value = part as Record<string, unknown>;
                return (value.type === "text" || value.type === "reasoning") && value.text === "";
            })
        )
            return false;
    }
    if (!validateLkgSeamBoundary(prefix, tail)) return false;
    const wireCandidates = all.map((message) => message as unknown as { role: string });
    if (wireCandidates.every((message) => typeof message.role === "string")) {
        const adjacency = assertOpenAiCompatAdjacency(wireCandidates);
        if (!adjacency.ok) return false;
        const wireCallIds = new Set<string>();
        for (const wireMessage of wireCandidates) {
            for (const call of (wireMessage as { tool_calls?: Array<{ id: string }> }).tool_calls ??
                []) {
                if (wireCallIds.has(call.id)) return false;
                wireCallIds.add(call.id);
            }
        }
    }
    return true;
}

export function replayLkg(args: {
    sessionId: string;
    messages: MessageLike[];
    modelKey: string | null;
    providerKey: string | null;
    entry?: LkgEntryNote | null;
    skipSeamValidation?: boolean;
}): { ok: true; messages: MessageLike[] } | { ok: false; reason: LkgValidationFailure } {
    const slot = getSlot(args.sessionId);
    if (!slot) return { ok: false, reason: "lkg_invalidated_reshape" };
    if (slot.modelKey !== args.modelKey || slot.providerKey !== args.providerKey) {
        dropSlot(args.sessionId, "lkg_model_mismatch");
        return { ok: false, reason: "lkg_model_mismatch" };
    }
    const entry = args.entry ?? noteEntry(args.sessionId, args.messages);
    if (
        !entry ||
        entry.anchorIndex !== slot.inputIdSeq.length - 1 ||
        !entryIdsAreValid(slot, entry.entryInputIds)
    ) {
        dropSlot(args.sessionId, "lkg_invalidated_reshape");
        return { ok: false, reason: "lkg_invalidated_reshape" };
    }
    let prefix: MessageLike[];
    try {
        const parsed = JSON.parse(slot.jsonPrefix) as unknown;
        if (!Array.isArray(parsed)) throw new Error("prefix is not an array");
        prefix = parsed as MessageLike[];
    } catch {
        dropSlot(args.sessionId, "lkg_seam_invalid");
        return { ok: false, reason: "lkg_seam_invalid" };
    }
    if (!args.skipSeamValidation) {
        if (!validateLkgSeamBoundary(prefix, entry.pristineTail)) {
            dropSlot(args.sessionId, "lkg_unsafe_seam");
            return { ok: false, reason: "lkg_unsafe_seam" };
        }
        if (!validateLkgSeam(prefix, entry.pristineTail, args.providerKey)) {
            dropSlot(args.sessionId, "lkg_seam_invalid");
            return { ok: false, reason: "lkg_seam_invalid" };
        }
    }
    return { ok: true, messages: [...prefix, ...entry.pristineTail] };
}

export function validateLkgEntry(slot: LkgSlot, entryIds: string[]): boolean {
    return entryIdsAreValid(slot, entryIds);
}

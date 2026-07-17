import type { MessageLike } from "./transform-operations";

export interface LkgSlot {
    jsonPrefix: string;
    inputIdSeq: string[];
    lastInputMessageId: string;
    modelKey: string | null;
    providerKey: string | null;
    capturedAt: number;
}

export interface LkgEntryNote {
    pristineTail: MessageLike[];
    entryInputIds: string[];
    anchorIndex: number;
}

const LKG_TOTAL_BYTES = 64 * 1024 * 1024;
const LKG_SINGLE_SLOT_BYTES = 24 * 1024 * 1024;
const LKG_METADATA_BYTES = 256;

const slots = new Map<string, { slot: LkgSlot; bytes: number }>();
let totalBytes = 0;

function slotBytes(slot: LkgSlot): number {
    return 2 * slot.jsonPrefix.length + LKG_METADATA_BYTES;
}

function touch(sessionId: string, entry: { slot: LkgSlot; bytes: number }): void {
    slots.delete(sessionId);
    slots.set(sessionId, entry);
}

export function captureSlot(sessionId: string, slot: LkgSlot): boolean {
    const bytes = slotBytes(slot);
    if (bytes > LKG_SINGLE_SLOT_BYTES) return false;
    const prior = slots.get(sessionId);
    if (prior) totalBytes -= prior.bytes;
    slots.delete(sessionId);
    while (totalBytes + bytes > LKG_TOTAL_BYTES) {
        const oldest = slots.keys().next().value as string | undefined;
        if (oldest === undefined) break;
        const evicted = slots.get(oldest);
        slots.delete(oldest);
        if (evicted) totalBytes -= evicted.bytes;
    }
    if (totalBytes + bytes > LKG_TOTAL_BYTES) {
        if (prior) {
            slots.set(sessionId, prior);
            totalBytes += prior.bytes;
        }
        return false;
    }
    const entry = { slot: { ...slot, inputIdSeq: [...slot.inputIdSeq] }, bytes };
    slots.set(sessionId, entry);
    totalBytes += bytes;
    return true;
}

export function getSlot(sessionId: string): LkgSlot | undefined {
    const entry = slots.get(sessionId);
    if (!entry) return undefined;
    touch(sessionId, entry);
    return { ...entry.slot, inputIdSeq: [...entry.slot.inputIdSeq] };
}

export function dropSlot(sessionId: string, _reason?: string): void {
    const entry = slots.get(sessionId);
    if (!entry) return;
    slots.delete(sessionId);
    totalBytes -= entry.bytes;
}

export function noteEntry(sessionId: string, messages: MessageLike[]): LkgEntryNote | null {
    const slot = getSlot(sessionId);
    if (!slot) return null;
    const entryInputIds = messages.map((message) => {
        const id = (message.info as { id?: unknown } | undefined)?.id;
        return typeof id === "string" ? id : "";
    });
    const anchorIndex = entryInputIds.indexOf(slot.lastInputMessageId);
    if (anchorIndex < 0) return null;
    const pristineTail = structuredClone(messages.slice(anchorIndex + 1)) as MessageLike[];
    return { pristineTail, entryInputIds, anchorIndex };
}

export function resetLkgSlotsForTest(): void {
    slots.clear();
    totalBytes = 0;
}

export function getLkgSlotStatsForTest(): { totalBytes: number; count: number } {
    return { totalBytes, count: slots.size };
}

export const __resetLkgSlotStoreForTest = resetLkgSlotsForTest;

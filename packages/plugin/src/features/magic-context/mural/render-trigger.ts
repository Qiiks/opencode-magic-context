import { createHash } from "node:crypto";

import { modelSupportsVision } from "../../../shared/models-dev-cache";
import type { Database } from "../../../shared/sqlite";
import { DEFAULT_MURAL_MEMORY_BUDGET } from "./mural-selection";
import { MURAL_HEIGHT, MURAL_WIDTH, renderMural } from "./render-mural";
import type { MuralWireOptions } from "./resolve-mural";
import { resolveMural } from "./resolve-mural";
import { getMural, upsertMural } from "./storage-mural";

/**
 * On-demand deterministic mural render. The weekly author task is gone: the
 * mural is now a pure function of the compressed cue pool, rendered when the m0
 * injection path needs it. Change detection is CHEAP — resolveMural plus a text
 * assembly hash, no PNG work — so an unchanged cue pool costs one resolve and a
 * hash compare. The PNG is only encoded and upserted when the resolved text
 * actually changed.
 *
 * The stored row still feeds `mural_manifest` for the dashboard (`get_mural`);
 * its `model` column becomes "deterministic" since no compressor model rendered
 * it (the compressor model is recorded per-cue, not per-render).
 */

export const DETERMINISTIC_MURAL_MODEL = "deterministic";

export interface EnsureMuralResult {
    /** True when a resolved cue pool exists (the mural block should be injected). */
    hasMural: boolean;
    /** data URL of the current mural PNG, when hasMural. */
    dataUrl?: string;
    /** sha256 of the mural PNG bytes — the m0 mural fold identity. */
    contentHash?: string;
    /** True when this call re-rendered + upserted (the text changed or was new). */
    rerendered: boolean;
}

/**
 * Ensure the stored mural reflects the current compressed cue pool, rendering
 * and upserting only when the deterministic mural TEXT changed since the stored
 * render. Returns the wire data for the injection path.
 *
 * @param budgetTokens the project memory injection budget, so the overflow set
 *   matches exactly what the m0 path dropped.
 */
export function ensureMuralRendered(
    db: Database,
    projectIdentity: string,
    budgetTokens: number = DEFAULT_MURAL_MEMORY_BUDGET,
): EnsureMuralResult {
    const entries = resolveMural(db, projectIdentity, budgetTokens);
    if (entries.length === 0) {
        // Empty cue pool → no mural block. Leave any stale stored row alone (the
        // dashboard can still show the last render); the injection path omits the
        // block when hasMural is false.
        return { hasMural: false, rerendered: false };
    }

    const rendered = renderMural(entries);
    // The mural TEXT (not the PNG) is the change-detection key: it's cheap to
    // assemble and deterministic, so an unchanged pool re-derives the same hash
    // and we skip PNG re-encode + DB write entirely.
    const textHash = createHash("sha256").update(rendered.sha256Input).digest("hex");

    const existing = getMural(db, projectIdentity);
    if (existing && existing.contentHash === textHash) {
        // Unchanged: reuse the stored PNG (already the right bytes) without re-encoding.
        return {
            hasMural: true,
            dataUrl: `data:image/png;base64,${existing.image.toString("base64")}`,
            contentHash: existing.contentHash,
            rerendered: false,
        };
    }

    upsertMural(db, {
        projectPath: projectIdentity,
        image: Buffer.from(rendered.png),
        // content_hash is the TEXT hash (the change-detection key), not a PNG
        // hash: identical text always yields identical PNG bytes, and hashing the
        // text is what lets the unchanged-pool fast path above skip re-encoding.
        contentHash: textHash,
        renderedAt: Date.now(),
        model: DETERMINISTIC_MURAL_MODEL,
        memoryIds: rendered.renderedIds,
        width: MURAL_WIDTH,
        height: MURAL_HEIGHT,
    });

    return {
        hasMural: true,
        dataUrl: rendered.dataUrl,
        contentHash: textHash,
        rerendered: true,
    };
}

/** True only when the given model's cached provider metadata accepts images. A
 *  model key is `provider/model`; unknown capability means no image. */
function modelKeyAcceptsImages(modelKey: string | undefined): boolean {
    if (!modelKey) return false;
    const separator = modelKey.indexOf("/");
    if (separator <= 0) return false;
    return modelSupportsVision(modelKey.slice(0, separator), modelKey.slice(separator + 1));
}

/**
 * Resolve the mural WIRE options for the m0 injection path: gate on the mural
 * feature flag AND the outgoing model's vision capability, then ensure the
 * deterministic mural is rendered and return its data URL + content hash. This
 * is the on-demand render trigger — called from the HARD-fold materialization so
 * the injected data-url only swaps on a natural fold (defer passes replay the
 * baked-in bytes), matching the existing inject-compartments cache rule.
 *
 * Returns `{ enabled: false }` (no image) when the feature is off, the model
 * can't take images, or the cue pool is empty.
 */
export function resolveMuralWire(
    db: Database,
    projectIdentity: string | undefined,
    modelKey: string | undefined,
    enabled: boolean,
    budgetTokens: number = DEFAULT_MURAL_MEMORY_BUDGET,
): MuralWireOptions {
    if (!enabled || !projectIdentity || !modelKeyAcceptsImages(modelKey)) {
        return { enabled, supportsVision: false };
    }
    const result = ensureMuralRendered(db, projectIdentity, budgetTokens);
    if (!result.hasMural) return { enabled: true, supportsVision: true };
    return {
        enabled: true,
        supportsVision: true,
        dataUrl: result.dataUrl,
        contentHash: result.contentHash,
    };
}

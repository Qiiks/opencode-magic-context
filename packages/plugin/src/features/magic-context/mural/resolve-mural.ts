import { modelSupportsVision } from "../../../shared/models-dev-cache";
import type { Database } from "../../../shared/sqlite";
import { getMural, muralDataUrl } from "./storage-mural";

export interface MuralWireOptions {
    enabled: boolean;
    supportsVision: boolean;
    dataUrl?: string;
    contentHash?: string;
}

/** Resolve the project image only for models whose cached provider metadata accepts images. */
export function resolveMuralForModel(
    db: Database,
    projectPath: string,
    modelKey: string | undefined,
    enabled: boolean,
): MuralWireOptions {
    const result: MuralWireOptions = {
        enabled,
        supportsVision: false,
    };
    if (!enabled || !modelKey) return result;
    const separator = modelKey.indexOf("/");
    if (separator <= 0) return result;
    if (!modelSupportsVision(modelKey.slice(0, separator), modelKey.slice(separator + 1))) {
        return result;
    }
    const row = getMural(db, projectPath);
    return row
        ? {
              enabled: true,
              supportsVision: true,
              dataUrl: muralDataUrl(row),
              contentHash: row.contentHash,
          }
        : result;
}

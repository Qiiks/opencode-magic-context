export type RustAuthorityDomain = "memories" | "notes";
export type RustAuthorityState = "TS" | "PREPARING" | "MODULE" | "DRAINING";

export interface RustNoteToolRequest {
    sessionId: string;
    projectRoot: string;
    projectPath: string;
    action: "write" | "read" | "update" | "dismiss";
    content?: string;
    surfaceCondition?: string;
    filter?: "all" | "active" | "pending" | "ready" | "dismissed";
    limit?: number;
    offset?: number;
    noteId?: number;
}

export interface RustMemoryToolRequest {
    sessionId: string;
    projectRoot: string;
    projectPath: string;
    action: "write" | "update" | "archive" | "merge" | "get";
    content?: string;
    category?: string;
    ids?: number[];
    reason?: string;
}

export interface RustToolBackends {
    reduce?: (args: {
        sessionId: string;
        projectRoot: string;
        drop: string;
        commandId: string;
    }) => Promise<unknown>;
    authorityState?: (args: {
        projectPath: string;
        domain: RustAuthorityDomain;
    }) => Promise<RustAuthorityState | null>;
    /** Route ctx_note only after notes authority reports MODULE. */
    note?: (args: RustNoteToolRequest) => Promise<unknown>;
    /** Route ctx_memory only after memories authority reports MODULE. */
    memory?: (args: RustMemoryToolRequest) => Promise<unknown>;
    /** Smart-note writes fail closed when the host evaluator cannot send note.evaluate. */
    noteEvaluationAvailable?: () => boolean;
    memorySync?: (sessionId: string) => void;
}

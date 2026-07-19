export interface RustNoteToolRequest {
    sessionId: string;
    projectRoot: string;
    action: "write" | "read" | "update" | "dismiss";
    content?: string;
    surfaceCondition?: string;
    filter?: "all" | "active" | "pending" | "ready" | "dismissed";
    limit?: number;
    offset?: number;
    noteId?: number;
}

export interface RustToolBackends {
    reduce?: (args: {
        sessionId: string;
        projectRoot: string;
        drop: string;
        commandId: string;
    }) => Promise<unknown>;
    /** Route project-owned ctx_note mutations through the module when notes authority is MODULE. */
    note?: (args: RustNoteToolRequest) => Promise<unknown>;
    memorySync?: (sessionId: string) => void;
}

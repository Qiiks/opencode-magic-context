export interface RustToolBackends {
    reduce?: (args: {
        sessionId: string;
        projectRoot: string;
        drop: string;
        commandId: string;
    }) => Promise<unknown>;
    memorySync?: (sessionId: string) => void;
}

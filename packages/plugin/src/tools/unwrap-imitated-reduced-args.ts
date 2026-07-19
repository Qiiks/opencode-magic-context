export interface ImitatedReducedArgs {
    reduced?: boolean;
    summary?: string;
}

/**
 * Models can imitate the clamped argument shape they see in reduced tool-call
 * history. Decode that shape once at the tool boundary so the intended call
 * reaches the normal validation and dispatch path.
 */
export function unwrapImitatedReducedArgs<T extends object>(
    args: T,
    primaryFields: readonly string[],
): T {
    const record = args as Record<string, unknown>;
    if (
        primaryFields.some((field) => record[field] !== undefined) ||
        record.reduced !== true ||
        typeof record.summary !== "string"
    ) {
        return args;
    }

    try {
        const parsed: unknown = JSON.parse(record.summary);
        if (parsed !== null && typeof parsed === "object" && !Array.isArray(parsed)) {
            return parsed as T;
        }
    } catch {
        // Keep the original arguments so the tool reports its usual validation error.
    }

    return args;
}

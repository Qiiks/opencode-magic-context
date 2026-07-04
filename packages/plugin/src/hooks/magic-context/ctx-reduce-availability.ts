import { BoundedSessionMap } from "../../shared/bounded-session-map";
import { sessionLog } from "../../shared/logger";
import { openCodeDbExists, withReadOnlySessionDb } from "./read-session-db";

/**
 * Whether ctx_reduce is actually CALLABLE in a session's tool set.
 *
 * ctx_reduce is registered process-globally, but a parent agent can spawn a
 * session with an explicit allow-list tools map ({"*": false, read: true, ...})
 * that filters it out. For such sessions the entire reduce surface — §N§
 * prefixes, reduce guidance, Channel 1/2 nudges — is pure overhead urging a
 * tool the model cannot call (plus §N§ cargo-cult risk with no benefit).
 *
 * CACHE STABILITY: the verdict is resolved ONCE per session from the FIRST
 * user message's tools map and cached for the process lifetime. Per-turn tool
 * maps can differ (mode switches toggle edit tools), and a flapping verdict
 * would oscillate the system-prompt guidance block — a per-turn HARD bust.
 * The first-message map is fixed at session spawn, so the verdict is
 * deterministic across passes and restarts.
 *
 * Fail-open: no tools map (normal sessions), no wildcard-deny, or an
 * unreadable OpenCode DB all resolve to "available" — current behavior.
 */
const availabilityBySession = new BoundedSessionMap<boolean>(500);

/** Verdict from one tools map; null = map carries no signal. */
function verdictFromToolsMap(tools: unknown): boolean | null {
    if (tools === null || typeof tools !== "object" || Array.isArray(tools)) return null;
    const map = tools as Record<string, unknown>;
    if (map.ctx_reduce === true) return true;
    if (map.ctx_reduce === false) return false;
    // Explicit allow-list (wildcard deny) without ctx_reduce → filtered out.
    if (map["*"] === false) return false;
    return null;
}

/**
 * Resolve from the in-memory transform message array (preferred — free).
 * Caches the verdict on first resolution.
 */
export function resolveCtxReduceAvailabilityFromMessages(
    sessionId: string,
    messages: ReadonlyArray<{ info?: { role?: string; tools?: unknown } }>,
): boolean {
    const cached = availabilityBySession.get(sessionId);
    if (cached !== undefined) return cached;

    for (const message of messages) {
        if (message.info?.role !== "user") continue;
        // First user message decides: explicit signal, or no-signal → available.
        // Either way the verdict is final — freeze it.
        const verdict = verdictFromToolsMap(message.info.tools) ?? true;
        availabilityBySession.set(sessionId, verdict);
        return verdict;
    }
    // No user message in the array at all (not a real prompt — e.g. a stray
    // pass on an empty session). Fail open but do NOT freeze: caching true here
    // would lock a deny-list session into the reduce surface before its first
    // user message ever arrives to say otherwise.
    return true;
}

/** Availability verdict plus whether it is final for the session's lifetime. */
export interface CtxReduceAvailabilityVerdict {
    callable: boolean;
    /** True when resolved from the session's first user message (cached).
     *  False when the verdict is a provisional fail-open default — consumers
     *  that PERSIST state derived from the verdict (e.g. the system-prompt
     *  hash) must skip persistence until a frozen verdict exists, or a later
     *  final verdict flips the persisted bytes and busts the prompt cache. */
    frozen: boolean;
}

/**
 * Resolve from the OpenCode DB (system-prompt hook path — may run before the
 * transform has seen any messages). Falls back to "available" when the DB is
 * absent (Pi-only installs) or the read fails.
 */
export function resolveCtxReduceAvailability(sessionId: string): CtxReduceAvailabilityVerdict {
    const cached = availabilityBySession.get(sessionId);
    if (cached !== undefined) return { callable: cached, frozen: true };
    // No opencode.db at all: this handler only runs inside OpenCode, where the
    // DB always exists — this branch is test/degraded-install territory, not
    // the pre-first-user race. Treat as final so hash persistence proceeds.
    if (!openCodeDbExists()) return { callable: true, frozen: true };
    try {
        const row = withReadOnlySessionDb(
            (db) =>
                db
                    .prepare(
                        `SELECT json_extract(data, '$.tools') AS tools FROM message
                          WHERE session_id = ? AND json_extract(data, '$.role') = 'user'
                          ORDER BY time_created ASC LIMIT 1`,
                    )
                    .get(sessionId) as { tools: string | null } | undefined,
        );
        if (!row) return { callable: true, frozen: false }; // session not persisted yet
        const verdict = row.tools === null ? null : verdictFromToolsMap(JSON.parse(row.tools));
        const resolved = verdict ?? true;
        availabilityBySession.set(sessionId, resolved);
        return { callable: resolved, frozen: true };
    } catch (error) {
        sessionLog(sessionId, "ctx_reduce availability read failed (fail-open):", error);
        return { callable: true, frozen: false };
    }
}

export function clearCtxReduceAvailability(sessionId: string): void {
    availabilityBySession.delete(sessionId);
}

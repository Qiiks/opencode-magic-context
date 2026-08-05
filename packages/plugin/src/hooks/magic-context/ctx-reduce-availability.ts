import { BoundedSessionMap } from "../../shared/bounded-session-map";
import { sessionLog } from "../../shared/logger";
import { openCodeDbExists, withReadOnlySessionDb } from "./read-session-db";

/**
 * Whether a given tool is actually CALLABLE in a session's tool set.
 *
 * Magic Context registers tools process-globally, but a parent agent (or the
 * user's OpenCode config) can spawn a session with an explicit allow-list
 * tools map ({"*": false, read: true, ...}) that filters a tool out. For such
 * sessions any surface that urges or replays that tool — ctx_reduce §N§
 * prefixes and nudges, or synthetic todowrite pairs — is pure overhead urging
 * a tool the model cannot call (plus cargo-cult risk with no benefit).
 *
 * CACHE STABILITY: the verdict is resolved ONCE per session per tool from the
 * FIRST user message's tools map and cached for the process lifetime. Per-turn
 * tool maps can differ (mode switches toggle edit tools), and a flapping
 * verdict would oscillate provider-visible bytes — a per-turn HARD bust. The
 * first-message map is fixed at session spawn, so the verdict is deterministic
 * across passes and restarts.
 *
 * Fail-open: no tools map (normal sessions), no wildcard-deny, or an
 * unreadable OpenCode DB all resolve to "available" — current behavior.
 */

/** Availability verdict plus whether it is final for the session's lifetime. */
export interface ToolAvailabilityVerdict {
    callable: boolean;
    /** True when resolved from the session's first user message (cached).
     *  False when the verdict is a provisional fail-open default — consumers
     *  that PERSIST state derived from the verdict (e.g. the system-prompt
     *  hash) must skip persistence until a frozen verdict exists, or a later
     *  final verdict flips the persisted bytes and busts the prompt cache. */
    frozen: boolean;
}

// `ctx_reduce` is registered process-globally by the tool registry at boot.
// In compaction-off mode (the `compaction` config block's `enabled` field set
// to false) the registry skips registering it, so no session can ever call
// the tool — regardless of the per-session spawn tools map (a normal session
// with no tools map would otherwise fail-open to "callable"). This
// process-global override makes unregistration flow naturally to every
// ctx_reduce-availability consumer: the no-reduce guidance variant
// (system-prompt-hash.ts), Channel-1/Channel-2 nudges, and §N§ prefix
// injection (transform.ts). Default true (registered) preserves today's
// behavior for tests/legacy callers that never call the setter. The
// compaction-off mode reuses the existing no-reduce guidance variant
// machinery rather than minting a third template.
let ctxReduceRegisteredGlobally = true;

/**
 * Set whether `ctx_reduce` is registered process-globally. Called once at
 * plugin boot from the tool registry resolution. When false, every
 * `resolveCtxReduceAvailability*` call returns a frozen `callable: false`
 * verdict without consulting the per-session tools map or the OpenCode DB.
 */
export function setCtxReduceRegisteredGlobally(registered: boolean): void {
    ctxReduceRegisteredGlobally = registered;
}

/** Test-only reset so the availability suite's default-true baseline is
 *  unaffected by a prior test that flipped the override. Production code
 *  never needs to re-enable mid-process (boot-resolved, process-stable). */
export function resetCtxReduceRegisteredGloballyForTest(): void {
    ctxReduceRegisteredGlobally = true;
}

/** Historical alias. ctx_reduce was the first consumer of this resolver; the
 * verdict shape is identical for every tool, so the name is kept so existing
 * ctx_reduce call sites stay untouched.
 */
export type CtxReduceAvailabilityVerdict = ToolAvailabilityVerdict;

const CTX_REDUCE_TOOL = "ctx_reduce";
const TODOWRITE_TOOL = "todowrite";

/**
 * Verdicts are cached per (tool, session). Two tools resolve independently for
 * the same session, so the key is a composite. The NUL separator cannot appear
 * in either a tool name or an OpenCode session id. Cap covers ~500 sessions
 * across the two tools we currently resolve (ctx_reduce + todowrite).
 */
const availabilityBySession = new BoundedSessionMap<boolean>(1000);

function cacheKey(toolName: string, sessionId: string): string {
    return `${toolName}\u0000${sessionId}`;
}

/** Verdict for one tool from one tools map; null = map carries no signal. */
function verdictFromToolsMap(tools: unknown, toolName: string): boolean | null {
    if (tools === null || typeof tools !== "object" || Array.isArray(tools)) return null;
    const map = tools as Record<string, unknown>;
    if (map[toolName] === true) return true;
    if (map[toolName] === false) return false;
    // Explicit allow-list (wildcard deny) without this tool → filtered out.
    if (map["*"] === false) return false;
    return null;
}

/**
 * Resolve from the in-memory transform message array (preferred — free).
 * Caches the verdict on first resolution.
 */
export function resolveToolAvailabilityFromMessages(
    sessionId: string,
    toolName: string,
    messages: ReadonlyArray<{ info?: { role?: string; tools?: unknown } }>,
): ToolAvailabilityVerdict {
    // Process-global registration override: when ctx_reduce is not registered
    // (compaction-off mode) the tool is uncallable for every session, full
    // stop. Frozen so the system-prompt hash persists this verdict as the
    // session baseline (no provisional-then-flip cache bust).
    if (toolName === CTX_REDUCE_TOOL && !ctxReduceRegisteredGlobally) {
        return { callable: false, frozen: true };
    }
    const key = cacheKey(toolName, sessionId);
    const cached = availabilityBySession.get(key);
    if (cached !== undefined) return { callable: cached, frozen: true };

    for (const message of messages) {
        if (message.info?.role !== "user") continue;
        // First user message decides: explicit signal, or no-signal → available.
        // Either way the verdict is final — freeze it.
        const verdict = verdictFromToolsMap(message.info.tools, toolName) ?? true;
        availabilityBySession.set(key, verdict);
        return { callable: verdict, frozen: true };
    }
    // No user message in the array at all (not a real prompt — e.g. a stray
    // pass on an empty session). Fail open but do NOT freeze: caching true here
    // would lock a deny-list session into the tool's surface before its first
    // user message ever arrives to say otherwise.
    return { callable: true, frozen: false };
}

/**
 * Resolve from the OpenCode DB (paths that may run before the transform has
 * seen any messages — e.g. the system-prompt hook, or tool.execute.after).
 * Falls back to "available" when the DB is absent (Pi-only installs) or the
 * read fails.
 */
export function resolveToolAvailability(
    sessionId: string,
    toolName: string,
): ToolAvailabilityVerdict {
    // Process-global registration override (see resolveToolAvailabilityFromMessages).
    if (toolName === CTX_REDUCE_TOOL && !ctxReduceRegisteredGlobally) {
        return { callable: false, frozen: true };
    }
    const key = cacheKey(toolName, sessionId);
    const cached = availabilityBySession.get(key);
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
        const verdict =
            row.tools === null ? null : verdictFromToolsMap(JSON.parse(row.tools), toolName);
        const resolved = verdict ?? true;
        availabilityBySession.set(key, resolved);
        return { callable: resolved, frozen: true };
    } catch (error) {
        sessionLog(sessionId, `${toolName} availability read failed (fail-open):`, error);
        return { callable: true, frozen: false };
    }
}

/** Drop a cached verdict for one tool of one session (test/reset helper). */
export function clearToolAvailability(sessionId: string, toolName: string): void {
    availabilityBySession.delete(cacheKey(toolName, sessionId));
}

// --- ctx_reduce convenience wrappers (behavior-identical to the original) ---

export function resolveCtxReduceAvailabilityFromMessages(
    sessionId: string,
    messages: ReadonlyArray<{ info?: { role?: string; tools?: unknown } }>,
): CtxReduceAvailabilityVerdict {
    return resolveToolAvailabilityFromMessages(sessionId, CTX_REDUCE_TOOL, messages);
}

export function resolveCtxReduceAvailability(sessionId: string): CtxReduceAvailabilityVerdict {
    return resolveToolAvailability(sessionId, CTX_REDUCE_TOOL);
}

export function clearCtxReduceAvailability(sessionId: string): void {
    clearToolAvailability(sessionId, CTX_REDUCE_TOOL);
}

// --- todowrite convenience wrappers ---

export function resolveTodowriteAvailabilityFromMessages(
    sessionId: string,
    messages: ReadonlyArray<{ info?: { role?: string; tools?: unknown } }>,
): ToolAvailabilityVerdict {
    return resolveToolAvailabilityFromMessages(sessionId, TODOWRITE_TOOL, messages);
}

export function resolveTodowriteAvailability(sessionId: string): ToolAvailabilityVerdict {
    return resolveToolAvailability(sessionId, TODOWRITE_TOOL);
}

export function clearTodowriteAvailability(sessionId: string): void {
    clearToolAvailability(sessionId, TODOWRITE_TOOL);
}

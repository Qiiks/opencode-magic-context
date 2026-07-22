/**
 * RustTestHarness — facade for the Rust-mode (ck-mc over subc) e2e lane.
 *
 * Reuses the OpenCode e2e machinery UNCHANGED — the mock Anthropic provider and
 * the `opencode serve` subprocess + SDK session driving — and layers on the two
 * things Rust mode needs that the TS lane does not:
 *
 *   1. a hermetic subc daemon + ck-mc module (HermeticSubcStack) whose
 *      connection file is placed where the plugin's Rust client already looks
 *      (`${dataDir}/cortexkit/run/subc-connection.json`), and
 *   2. serve RESTART support that keeps the same data dir (opencode.db +
 *      context.db + module store all survive), for the cold-start-drop-seed and
 *      module-restart scenarios.
 *
 * Boot order matters: the env is allocated first so the daemon can write its
 * connection file BEFORE opencode boots and the plugin's first transform runs.
 *
 * Assertion surface: wire captures come from the fake provider's full request
 * bodies (the same source the TS lane asserts on). Rust transform decisions are
 * ALSO surfaced from the plugin diagnostic log (redirected per-suite via
 * MAGIC_CONTEXT_LOG_PATH) as a secondary signal — `readRustPasses()` parses the
 * `rust pass: decision=… served_from=… applied=…` lines the Rust transform emits.
 */

import { Database } from "bun:sqlite";
import { existsSync, readFileSync, rmSync } from "node:fs";
import { join } from "node:path";
import { MockProvider, type MockResponse } from "./mock-provider/server";
import {
    createIsolatedEnv,
    type IsolatedEnv,
    type SpawnedOpencode,
    spawnOpencode,
} from "./opencode-runner/spawn";
import {
    buildHermeticBinaries,
    detectRustModePrereqs,
    HermeticSubcStack,
    type RustModePrereqs,
} from "./rust-runner/hermetic-subc";

export interface RustTestHarnessOptions {
    /** magic-context USER-tier config overrides (thresholds, memory, etc.). */
    magicContextConfig?: Record<string, unknown>;
    /** Extra opencode.json config. Merged onto test defaults. */
    openCodeConfigExtra?: Record<string, unknown>;
    /** Override the mock model's context token limit. Default 200000. */
    modelContextLimit?: number;
    /** Default response used when the mock queue is empty. */
    mockDefault?: MockResponse;
    /**
     * Start opencode in TS mode instead of Rust mode. The hermetic daemon still
     * runs (so a later `restart({ rust: true })` can flip to Rust against the
     * same data dir) but the plugin transforms in TS on this boot. Used by the
     * cold-start-drop-seed scenario to build TS-mode state, then restart in Rust.
     * Default: false (boot straight into Rust mode).
     */
    startInTsMode?: boolean;
}

export interface SdkClient {
    session: {
        create: (opts: {
            query: { directory: string };
            body?: { parentID?: string; title?: string };
        }) => Promise<{ data?: { id: string } }>;
        prompt: (opts: {
            path: { id: string };
            body: {
                model: { providerID: string; modelID: string };
                parts: Array<{ type: "text"; text: string }>;
                agent?: string;
            };
        }) => Promise<{ data?: unknown }>;
        revert: (opts: {
            path: { id: string };
            body: { messageID: string; partID?: string };
        }) => Promise<{ data?: unknown }>;
        messages: (opts: {
            path: { id: string };
        }) => Promise<{ data?: unknown }>;
    };
}

const DEFAULT_MOCK_RESPONSE: MockResponse = {
    text: "ok",
    usage: {
        input_tokens: 100,
        output_tokens: 20,
        cache_creation_input_tokens: 100,
        cache_read_input_tokens: 0,
    },
};

/** One parsed `rust pass:` diagnostic line. */
export interface RustPassLine {
    decision: string;
    reason: string;
    servedFrom: string;
    inputCount: number;
    outputCount: number;
    applied: boolean;
    raw: string;
}

export class RustTestHarness {
    readonly mock: MockProvider;
    readonly env: IsolatedEnv;
    readonly subc: HermeticSubcStack;
    readonly logPath: string;

    private opencodeInstance: SpawnedOpencode;
    private clientInstance: SdkClient;
    private contextDbCached: Database | null = null;
    private modelContextLimit: number | undefined;
    private mockDefault: MockResponse;
    private readonly mockBaseURL: string;

    private constructor(args: {
        mock: MockProvider;
        mockBaseURL: string;
        env: IsolatedEnv;
        subc: HermeticSubcStack;
        opencode: SpawnedOpencode;
        client: SdkClient;
        logPath: string;
        modelContextLimit: number | undefined;
        mockDefault: MockResponse;
    }) {
        this.mock = args.mock;
        this.mockBaseURL = args.mockBaseURL;
        this.env = args.env;
        this.subc = args.subc;
        this.opencodeInstance = args.opencode;
        this.clientInstance = args.client;
        this.logPath = args.logPath;
        this.modelContextLimit = args.modelContextLimit;
        this.mockDefault = args.mockDefault;
    }

    /**
     * Preflight the lane. Cheap and never throws — call it in a describe-level
     * guard so a machine without cargo / the subconscious sibling / a supported
     * platform SKIPs with a printed reason instead of failing or hanging.
     */
    static detectPrereqs(): RustModePrereqs {
        return detectRustModePrereqs();
    }

    static async create(options: RustTestHarnessOptions = {}): Promise<RustTestHarness> {
        const prereqs = detectRustModePrereqs();
        if (!prereqs.ok || !prereqs.subconsciousRoot) {
            throw new Error(
                `RustTestHarness prerequisites unmet: ${prereqs.skipReason ?? "unknown"}. ` +
                    "Guard the suite with RustTestHarness.detectPrereqs() and skip instead of creating.",
            );
        }

        const { ckMcBin, ckSubcBin } = await buildHermeticBinaries(prereqs.subconsciousRoot);

        const mock = new MockProvider();
        const { baseURL } = await mock.start();
        const mockDefault = options.mockDefault ?? DEFAULT_MOCK_RESPONSE;
        mock.setDefault(mockDefault);

        // Env first: the daemon must write its connection file into
        // <dataDir>/cortexkit/run/ before opencode boots and the plugin's Rust
        // client connects on the first transform.
        const env = createIsolatedEnv();
        const subc = await HermeticSubcStack.start({ dataDir: env.dataDir, ckMcBin, ckSubcBin });

        const logPath = join(env.dataDir, "cortexkit", "magic-context-e2e.log");

        const opencode = await RustTestHarness.spawnServe({
            env,
            mockURL: baseURL,
            connectionFile: subc.connectionFile,
            logPath,
            options,
            rustMode: !options.startInTsMode,
        });

        const sdk = await import("@opencode-ai/sdk");
        const client = sdk.createOpencodeClient({ baseUrl: opencode.url }) as unknown as SdkClient;

        return new RustTestHarness({
            mock,
            mockBaseURL: baseURL,
            env,
            subc,
            opencode,
            client,
            logPath,
            modelContextLimit: options.modelContextLimit,
            mockDefault,
        });
    }

    private static spawnServe(args: {
        env: IsolatedEnv;
        mockURL: string;
        connectionFile: string;
        logPath: string;
        options: RustTestHarnessOptions;
        rustMode: boolean;
    }): Promise<SpawnedOpencode> {
        return spawnOpencode({
            mockProviderURL: args.mockURL,
            existingEnv: args.env,
            modelContextLimit: args.options.modelContextLimit,
            openCodeConfigExtra: args.options.openCodeConfigExtra,
            magicContextConfig: args.options.magicContextConfig,
            // The connection_file value is only used to flip `userTierHasSubc`
            // true (the resolver gate). The actual transport still reads the
            // DEFAULT connection path — which this same file happens to be.
            userSubcConnectionFile: args.connectionFile,
            projectMagicContextConfig: {
                transform_mode: args.rustMode ? "rust" : "ts",
            },
            extraEnv: { MAGIC_CONTEXT_LOG_PATH: args.logPath },
        });
    }

    get opencode(): SpawnedOpencode {
        return this.opencodeInstance;
    }

    get client(): SdkClient {
        return this.clientInstance;
    }

    /**
     * Restart `opencode serve` against the SAME data dir (opencode.db, context.db,
     * module store, and the running daemon all persist). Optionally flip the
     * project transform_mode (ts↔rust) — the cold-start-drop-seed scenario builds
     * TS-mode state then restarts in Rust to prove drop-tag state seeds correctly.
     */
    async restart(opts: { rust?: boolean; magicContextConfig?: Record<string, unknown> } = {}): Promise<void> {
        if (this.contextDbCached) {
            try {
                this.contextDbCached.close();
            } catch {
                // ignore
            }
            this.contextDbCached = null;
        }
        await this.opencodeInstance.kill();
        this.opencodeInstance = await RustTestHarness.spawnServe({
            env: this.env,
            mockURL: this.mockBaseURL,
            connectionFile: this.subc.connectionFile,
            logPath: this.logPath,
            options: {
                modelContextLimit: this.modelContextLimit,
                magicContextConfig: opts.magicContextConfig,
            },
            rustMode: opts.rust ?? true,
        });
        const sdk = await import("@opencode-ai/sdk");
        this.clientInstance = sdk.createOpencodeClient({
            baseUrl: this.opencodeInstance.url,
        }) as unknown as SdkClient;
    }

    /** Create a session bound to the isolated workdir. Throws on failure. */
    async createSession(): Promise<string> {
        const maxAttempts = 5;
        for (let i = 1; i <= maxAttempts; i++) {
            const res = await this.clientInstance.session.create({
                query: { directory: this.env.workdir },
            });
            if (res.data) return res.data.id;
            if (i < maxAttempts) {
                await Bun.sleep(200 * i);
                continue;
            }
            throw new Error(
                `session.create failed after ${maxAttempts} attempts. stderr:\n${this.opencodeInstance.stderr()}`,
            );
        }
        throw new Error("session.create failed");
    }

    /**
     * Generate ~`tokens` tokens of varied prose ballast. Copied from the TS
     * harness: the protected-tail boundary measures true-raw content, so pressure
     * turns must carry real mass, and varied prose tokenizes at a stable rate.
     */
    ballast(tokens: number): string {
        const words = [
            "boundary", "historian", "compartment", "schedule", "pressure",
            "tokens", "window", "publish", "transform", "session", "marker",
            "budget", "eligible", "protected", "ordinal", "snapshot", "replay",
            "decision", "threshold", "baseline", "measure", "archive", "deliver",
        ];
        const target = Math.max(0, Math.round(tokens * 4));
        const parts: string[] = [];
        let length = 0;
        let i = 0;
        while (length < target) {
            const w = words[i % words.length]!;
            parts.push(`${w}${i % 17 === 0 ? "." : ""}`);
            length += w.length + 1;
            i += 1;
        }
        return parts.join(" ");
    }

    async sendPrompt(
        sessionId: string,
        text: string,
        options: { agent?: string; timeoutMs?: number } = {},
    ): Promise<unknown> {
        const timeoutMs = options.timeoutMs ?? 180_000;
        const promptPromise = this.clientInstance.session.prompt({
            path: { id: sessionId },
            body: {
                model: { providerID: "mock-anthropic", modelID: "mock-sonnet" },
                parts: [{ type: "text", text }],
                ...(options.agent ? { agent: options.agent } : {}),
            },
        });
        const timeout = new Promise<null>((r) => setTimeout(() => r(null), timeoutMs));
        const result = await Promise.race([promptPromise, timeout]);
        if (result === null) {
            throw new Error(
                `sendPrompt did not complete within ${timeoutMs}ms. stderr:\n${this.opencodeInstance
                    .stderr()
                    .slice(-2000)}\nmodule log:\n${this.subc.moduleLog().slice(-2000)}`,
            );
        }
        return result;
    }

    /**
     * Remove a message from the session the way session.revert does — the
     * message.removed event path the ordinal resolver must self-heal from. Reverts
     * TO the given message (dropping it and everything after), then unshares so the
     * revert is applied as a real removal opencode persists to its DB.
     */
    async revertMessage(sessionId: string, messageId: string): Promise<void> {
        await this.clientInstance.session.revert({
            path: { id: sessionId },
            body: { messageID: messageId },
        });
    }

    /** Fetch the session's messages via the SDK (for choosing a mid-session id to remove). */
    async listMessages(sessionId: string): Promise<Array<{ info?: { id?: string; role?: string } }>> {
        const res = await this.clientInstance.session.messages({ path: { id: sessionId } });
        const data = (res as { data?: unknown }).data;
        return Array.isArray(data) ? (data as Array<{ info?: { id?: string; role?: string } }>) : [];
    }

    // ── wire captures (from the fake provider) ────────────────────────────────

    /** Full captured provider request bodies for the main agent (Magic Context system prompt present). */
    mainRequests() {
        return this.mock
            .requests()
            .filter((r) => JSON.stringify(r.body.system ?? "").includes("## Magic Context"));
    }

    /** The messages array of the most recent main-agent request. */
    lastMainMessages(): Array<{ role?: string; content?: unknown }> {
        const req = this.mainRequests().at(-1);
        const messages = req?.body.messages;
        return Array.isArray(messages) ? (messages as Array<{ role?: string; content?: unknown }>) : [];
    }

    /**
     * Byte size of the full serialized messages array of the most recent
     * main-agent request, with `cache_control` stripped (it is provider cache
     * bookkeeping that varies pass to pass and is not part of the logical wire).
     */
    lastMainWireBytes(): number {
        const req = this.mainRequests().at(-1);
        if (!req) return 0;
        return Buffer.byteLength(stableSerialize(req.body.messages ?? []));
    }

    /** Stable serialization of the most recent main-agent messages array (cache_control stripped). */
    lastMainWireSerialized(): string {
        const req = this.mainRequests().at(-1);
        return stableSerialize(req?.body.messages ?? []);
    }

    // ── plugin-log rust-pass decisions (secondary signal) ─────────────────────

    /**
     * Wait until at least `minCount` rust-pass lines are visible, then return
     * them. The plugin logger buffers and flushes every ~500ms, so a read
     * immediately after a prompt returns can miss the just-emitted pass. Polling
     * removes that race without a fixed sleep (the no-sleeps-as-sync rule).
     */
    async waitForRustPasses(minCount: number, timeoutMs = 15_000): Promise<RustPassLine[]> {
        return this.waitFor(
            () => {
                const passes = this.readRustPasses();
                return passes.length >= minCount ? passes : null;
            },
            { timeoutMs, label: `>= ${minCount} rust passes` },
        );
    }

    /** Parse every `rust pass:` diagnostic line the Rust transform emitted so far. */
    readRustPasses(): RustPassLine[] {
        if (!existsSync(this.logPath)) return [];
        const lines = readFileSync(this.logPath, "utf8").split("\n");
        const parsed: RustPassLine[] = [];
        for (const line of lines) {
            const idx = line.indexOf("rust pass: ");
            if (idx < 0) continue;
            const body = line.slice(idx + "rust pass: ".length);
            parsed.push({
                decision: field(body, "decision"),
                reason: field(body, "reason"),
                servedFrom: field(body, "served_from"),
                inputCount: Number(field(body, "in") || "0"),
                outputCount: Number(field(body, "out") || "0"),
                applied: field(body, "applied") === "true",
                raw: line,
            });
        }
        return parsed;
    }

    /** Poll until `predicate` returns truthy or `timeoutMs` elapses. */
    async waitFor<T>(
        predicate: () => T | null | undefined | false,
        opts: { timeoutMs?: number; intervalMs?: number; label?: string } = {},
    ): Promise<T> {
        const timeoutMs = opts.timeoutMs ?? 60_000;
        const intervalMs = opts.intervalMs ?? 100;
        const deadline = Date.now() + timeoutMs;
        while (Date.now() < deadline) {
            const value = predicate();
            if (value) return value as T;
            await Bun.sleep(intervalMs);
        }
        throw new Error(
            `waitFor timed out after ${timeoutMs}ms${opts.label ? ` (${opts.label})` : ""}`,
        );
    }

    // ── context.db access (plugin state) ──────────────────────────────────────

    private contextDbPath(): string {
        return join(this.env.dataDir, "cortexkit", "magic-context", "context.db");
    }

    contextDb(): Database {
        if (this.contextDbCached) return this.contextDbCached;
        const dbPath = this.contextDbPath();
        if (!existsSync(dbPath)) {
            throw new Error(`context.db not found at ${dbPath} — plugin may not have initialized yet.`);
        }
        this.contextDbCached = new Database(dbPath, { readonly: true });
        return this.contextDbCached;
    }

    hasContextDb(): boolean {
        return existsSync(this.contextDbPath());
    }

    countTagsByStatus(sessionId: string, status: string): number {
        try {
            const row = this.contextDb()
                .prepare("SELECT COUNT(*) AS n FROM tags WHERE session_id = ? AND status = ?")
                .get(sessionId, status) as { n: number } | null;
            return row?.n ?? 0;
        } catch {
            return 0;
        }
    }

    /**
     * Override one session's cache TTL for a deterministic cache-busting probe.
     * The regular config path only refreshes this value on a new session; tests
     * that preserve a warm module session need to change it without recreating
     * the queued module-side drop.
     */
    setSessionCacheTtl(sessionId: string, cacheTtl: string): void {
        if (this.contextDbCached) {
            try {
                this.contextDbCached.close();
            } catch {
                // Best-effort close: a failure must not prevent reopening the database below.
            }
            this.contextDbCached = null;
        }
        const dbPath = this.contextDbPath();
        const db = new Database(dbPath);
        try {
            const result = db
                .prepare("UPDATE session_meta SET cache_ttl = ? WHERE session_id = ?")
                .run(cacheTtl, sessionId) as { changes?: number };
            if (result.changes !== 1) {
                throw new Error(`session cache TTL update affected ${result.changes ?? 0} rows`);
            }
        } finally {
            db.close();
        }
    }

    /** All mock requests received in this suite. */
    requests() {
        return this.mock.requests();
    }

    async dispose(): Promise<void> {
        if (this.contextDbCached) {
            try {
                this.contextDbCached.close();
            } catch {
                // ignore
            }
            this.contextDbCached = null;
        }
        // Kill order: opencode (holds the plugin's subc client) → module → daemon.
        try {
            await this.opencodeInstance.kill();
        } catch {
            // ignore
        }
        try {
            await this.subc.stop();
        } catch {
            // ignore
        }
        try {
            await this.mock.stop();
        } catch {
            // ignore
        }
        // Reclaim the per-suite temp tree (best-effort).
        try {
            rmSync(join(this.env.dataDir, ".."), { recursive: true, force: true });
        } catch {
            // ignore
        }
    }
}

/** Extract `key=value` (value = up to the next space) from a rust-pass log body. */
function field(body: string, key: string): string {
    const match = body.match(new RegExp(`(?:^|\\s)${key}=([^\\s]+)`));
    return match ? match[1]! : "";
}

/** Serialize a value with every `cache_control` key stripped, for byte-identity checks. */
export function stableSerialize(value: unknown): string {
    return JSON.stringify(stripCacheControl(value));
}

function stripCacheControl(value: unknown): unknown {
    if (Array.isArray(value)) return value.map(stripCacheControl);
    if (value && typeof value === "object") {
        const out: Record<string, unknown> = {};
        for (const [key, child] of Object.entries(value)) {
            if (key === "cache_control") continue;
            out[key] = stripCacheControl(child);
        }
        return out;
    }
    return value;
}

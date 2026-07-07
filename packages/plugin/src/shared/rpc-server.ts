import { randomBytes, timingSafeEqual } from "node:crypto";
import {
    chmodSync,
    mkdirSync,
    readdirSync,
    readFileSync,
    renameSync,
    rmSync,
    unlinkSync,
    writeFileSync,
} from "node:fs";
import { dirname } from "node:path";
import type { Server, ServerWebSocket } from "bun";
import { log } from "./logger";
import {
    drainNotifications,
    type NotificationSink,
    registerNotificationSink,
} from "./rpc-notifications";
import { isPidAlive, parseRpcPortFile, rpcPortDir, rpcPortFilePath } from "./rpc-utils";

type RpcHandler = (params: Record<string, unknown>) => Promise<Record<string, unknown>>;

/** Max body for an HTTP /rpc call. Matches the previous node:http guard. */
const MAX_BODY_BYTES = 1_048_576;
/** A WS client that doesn't authenticate within this window is closed. */
const WS_AUTH_TIMEOUT_MS = 5_000;
/** WS close code for an auth failure (private; client treats every close as
 *  expected and reconnects after rediscovery, so the exact code is advisory). */
const WS_CLOSE_UNAUTHORIZED = 4401;

/** Per-socket state carried on `ServerWebSocket.data`. */
interface WsData {
    authed: boolean;
    sessionId?: string;
    /** Removes this socket's sink from the notification registry. */
    unregister?: () => void;
    /** Fires if the client never sends a valid hello. */
    authTimer?: ReturnType<typeof setTimeout>;
}

/**
 * Constant-time bearer-token comparison. `timingSafeEqual` throws on
 * length-mismatched buffers, so guard on length first (the length itself is not
 * secret — the token bytes are). Avoids leaking the token via response-timing on
 * the loopback auth check.
 */
function tokensMatch(presented: string, expected: string): boolean {
    const a = Buffer.from(presented, "utf8");
    const b = Buffer.from(expected, "utf8");
    if (a.length !== b.length) return false;
    return timingSafeEqual(a, b);
}

function bearerToken(req: Request): string {
    const auth = req.headers.get("authorization");
    return typeof auth === "string" ? auth.replace(/^Bearer\s+/i, "") : "";
}

function websocketToken(req: Request, url: URL): string {
    return bearerToken(req) || url.searchParams.get("token") || "";
}

/**
 * Plugin-private localhost RPC server for TUI ↔ server-plugin communication.
 *
 * Runs on Bun (the OpenCode server runner is a Bun Worker), so it uses
 * `Bun.serve` to host BOTH:
 *  - HTTP request/reply routes (`/health`, `/rpc/<method>`) — the TUI's snapshot
 *    and dialog-result calls, which are event-driven, not idle; and
 *  - a WebSocket endpoint (`/ws`) — a single persistent connection per TUI over
 *    which the server PUSHES notifications (dialog/toast actions). This replaces
 *    the old 500ms HTTP poll, whose new-connection-per-tick cost was the source
 *    of idle TUI CPU (#200). Pi never imports this module, so `Bun.serve` is safe.
 */
export class MagicContextRpcServer {
    private server: Server<WsData> | null = null;
    private port = 0;
    private handlers = new Map<string, RpcHandler>();
    private portFilePath: string;
    private portDir: string;
    private startedAt = Date.now();
    private readonly instanceId = randomBytes(8).toString("hex");
    /** Every authenticated WS socket, so dispose can close them all. */
    private sockets = new Set<ServerWebSocket<WsData>>();
    // Unguessable per-process bearer token, published in the (user-private) port
    // file and required on every non-health RPC call AND in the WS hello. Defends
    // side-effecting endpoints (recomp/upgrade/dismiss) and the push channel
    // against any local process or browser-origin script that merely
    // discovers/guesses the port.
    private readonly token = randomBytes(32).toString("hex");

    constructor(storageDir: string, directory: string) {
        this.portFilePath = rpcPortFilePath(storageDir, directory, process.pid, this.instanceId);
        this.portDir = rpcPortDir(storageDir, directory);
    }

    /** Register an RPC method handler. */
    handle(method: string, handler: RpcHandler): void {
        this.handlers.set(method, handler);
    }

    /** Start the server on a random port, write port to disk. */
    async start(): Promise<number> {
        this.startedAt = Date.now();
        const self = this;
        const server = Bun.serve<WsData>({
            port: 0,
            hostname: "127.0.0.1",
            fetch(req, srv) {
                return self.handleFetch(req, srv);
            },
            websocket: {
                open(ws) {
                    // Close the socket if it doesn't authenticate promptly. A
                    // never-authenticated socket holds no sink and is harmless,
                    // but we don't want to keep raw connections open forever.
                    ws.data.authTimer = setTimeout(() => {
                        if (!ws.data.authed) ws.close(WS_CLOSE_UNAUTHORIZED, "auth timeout");
                    }, WS_AUTH_TIMEOUT_MS);
                },
                message(ws, raw) {
                    self.handleWsMessage(ws, raw);
                },
                close(ws) {
                    if (ws.data.authTimer) clearTimeout(ws.data.authTimer);
                    ws.data.unregister?.();
                    self.sockets.delete(ws);
                },
            },
        });

        this.server = server;
        this.port = server.port ?? 0;

        // Write a per-instance port file atomically. Multi-instance OpenCode is
        // supported: TUI discovery scans all live files and picks the most
        // recent instead of cross-wiring via one shared project file.
        try {
            this.warnIfOtherLiveInstance();
            const dir = dirname(this.portFilePath);
            // The port file holds the per-process bearer token that gates
            // side-effecting RPC endpoints and the push channel. Under the default
            // umask 0o022 a plain write lands at 0o644 in a 0o755 dir —
            // world-readable, so any local UID could read the token. Restrict both:
            // dir 0o700, file 0o600.
            mkdirSync(dir, { recursive: true, mode: 0o700 });
            try {
                chmodSync(dir, 0o700);
            } catch {
                // best-effort
            }
            const tmpPath = `${this.portFilePath}.tmp`;
            // A stale tmp from a crashed write could exist with loose perms;
            // writeFileSync's mode only applies on create, so remove it first.
            try {
                rmSync(tmpPath, { force: true });
            } catch {
                // best-effort
            }
            // Synchronous write so the renameSync below sees a fully-written file
            // (a 0o600 mode keeps the bearer token out of world-readable reach;
            // renameSync preserves the tmp's mode for the final path).
            writeFileSync(
                tmpPath,
                JSON.stringify({
                    port: this.port,
                    pid: process.pid,
                    started_at: this.startedAt,
                    token: this.token,
                    instance_id: this.instanceId,
                }),
                { encoding: "utf-8", mode: 0o600 },
            );
            renameSync(tmpPath, this.portFilePath);
            try {
                chmodSync(this.portFilePath, 0o600);
            } catch {
                // best-effort
            }
            log(`[rpc] server listening on 127.0.0.1:${this.port}`);
        } catch (err) {
            log(`[rpc] failed to write port file: ${err}`);
        }

        return this.port;
    }

    /** Stop the server: close every socket, stop accepting, remove port file. */
    stop(): void {
        for (const ws of this.sockets) {
            try {
                if (ws.data.authTimer) clearTimeout(ws.data.authTimer);
                ws.data.unregister?.();
                ws.close();
            } catch {
                // best-effort
            }
        }
        this.sockets.clear();
        if (this.server) {
            // `stop(true)` closes active connections too, not just the listener.
            this.server.stop(true);
            this.server = null;
        }
        try {
            unlinkSync(this.portFilePath);
        } catch {
            // Intentional: port file may already be gone
        }
    }

    private warnIfOtherLiveInstance(): void {
        try {
            for (const entry of readdirSync(this.portDir)) {
                if (!entry.startsWith("port-") || !entry.endsWith(".json")) continue;
                const record = parseRpcPortFile(readFileSync(`${this.portDir}/${entry}`, "utf-8"));
                if (!record || record.pid === process.pid || !isPidAlive(record.pid)) continue;
                log(
                    `[rpc] another Magic Context RPC server is active for this project (pid ${record.pid}, port ${record.port}); starting separate instance on a new port`,
                );
                return;
            }
        } catch {
            // No discovery directory yet, or unreadable stale file. Not fatal.
        }
    }

    /** HTTP route handler (Bun fetch). Returns a Response, or undefined when the
     *  request was upgraded to a WebSocket. */
    private async handleFetch(req: Request, srv: Server<WsData>): Promise<Response | undefined> {
        const url = new URL(req.url);

        // WebSocket upgrade — the persistent push channel. Authenticate before
        // `srv.upgrade` so an unauthorized request never becomes a live socket.
        if (url.pathname === "/ws") {
            if (!tokensMatch(websocketToken(req, url), this.token)) {
                return new Response("Unauthorized", { status: 401 });
            }
            const ok = srv.upgrade(req, { data: { authed: false } });
            if (ok) return undefined;
            return new Response("upgrade failed", { status: 400 });
        }

        // No wildcard CORS: the only legitimate client is the in-process TUI
        // client, not a browser origin.
        if (req.method === "GET" && url.pathname === "/health") {
            return json({ ok: true, pid: process.pid });
        }

        if (req.method !== "POST" || !url.pathname.startsWith("/rpc/")) {
            return new Response("Not Found", { status: 404 });
        }

        // Require the per-process bearer token on every side-effecting call.
        if (!tokensMatch(bearerToken(req), this.token)) {
            return json({ error: "Unauthorized" }, 401);
        }

        const method = url.pathname.slice(5); // strip "/rpc/"
        const handler = this.handlers.get(method);
        if (!handler) {
            return json({ error: `Unknown method: ${method}` }, 404);
        }

        const bodyText = await req.text();
        if (bodyText.length > MAX_BODY_BYTES) {
            return new Response("Request too large", { status: 413 });
        }
        let params: Record<string, unknown> = {};
        if (bodyText.length > 0) {
            try {
                params = JSON.parse(bodyText);
            } catch {
                return json({ error: "Invalid JSON" }, 400);
            }
        }

        try {
            const result = await handler(params);
            return json(result);
        } catch (err) {
            log(`[rpc] handler error: ${method} => ${err}`);
            return json({ error: String(err) }, 500);
        }
    }

    /** WS message handler: hello (auth + sink registration + backlog drain) and
     *  ack (cursor advance → queue prune). All other messages are ignored. */
    private handleWsMessage(ws: ServerWebSocket<WsData>, raw: string | Buffer): void {
        let msg: {
            type?: string;
            token?: string;
            sessionId?: string;
            lastReceivedId?: number;
            globalLastReceivedId?: number;
            ackScope?: string;
        };
        try {
            msg = JSON.parse(typeof raw === "string" ? raw : raw.toString("utf8"));
        } catch {
            return;
        }

        if (msg.type !== "hello" && !ws.data.authed) return;

        if (msg.type === "hello") {
            if (!tokensMatch(typeof msg.token === "string" ? msg.token : "", this.token)) {
                ws.send(JSON.stringify({ type: "error", error: "unauthorized" }));
                ws.close(WS_CLOSE_UNAUTHORIZED, "bad token");
                return;
            }
            if (ws.data.authTimer) {
                clearTimeout(ws.data.authTimer);
                ws.data.authTimer = undefined;
            }
            ws.data.authed = true;
            ws.data.sessionId =
                typeof msg.sessionId === "string" && msg.sessionId.length > 0
                    ? msg.sessionId
                    : undefined;

            // A session switch re-sends hello on the same socket. Remove the old
            // sink first so the registry has exactly one live sink per socket.
            ws.data.unregister?.();
            ws.data.unregister = undefined;

            // Register a live sink so future pushes reach this socket immediately.
            const sink: NotificationSink = {
                sessionId: ws.data.sessionId,
                send: (notification) => {
                    ws.send(JSON.stringify({ type: "notification", notification }));
                },
            };
            ws.data.unregister = registerNotificationSink(sink);
            this.sockets.add(ws);

            // Deliver any backlog the client hasn't seen (at-least-once). Modern
            // clients send separate session and global cursors; keeping those
            // watermarks independent prevents a high id in one session from pruning
            // lower unseen notifications in another session.
            const lastReceivedId = Number(msg.lastReceivedId ?? 0);
            const sessionCursor = Number.isFinite(lastReceivedId) ? lastReceivedId : 0;
            const hasGlobalCursor = typeof msg.globalLastReceivedId === "number";
            const globalLastReceivedId = hasGlobalCursor
                ? Number.isFinite(msg.globalLastReceivedId)
                    ? msg.globalLastReceivedId
                    : 0
                : 0;
            const backlog =
                ws.data.sessionId === undefined && hasGlobalCursor
                    ? drainNotifications(globalLastReceivedId, undefined, { globalOnly: true })
                    : drainNotifications(
                          sessionCursor,
                          ws.data.sessionId,
                          hasGlobalCursor
                              ? { globalLastReceivedId: globalLastReceivedId }
                              : undefined,
                      );
            for (const notification of backlog) {
                ws.send(JSON.stringify({ type: "notification", notification }));
            }
            ws.send(JSON.stringify({ type: "hello-ack" }));
            return;
        }

        if (msg.type === "ack") {
            // Advance only the cursor scope the client handled. Acking session A
            // must not prune session B, and acking a global notification must not
            // become a session watermark. Cheap, event-driven queue cleanup.
            const lastReceivedId = Number(msg.lastReceivedId ?? 0);
            if (Number.isFinite(lastReceivedId) && lastReceivedId > 0) {
                if (msg.ackScope === "global") {
                    drainNotifications(lastReceivedId, undefined, { globalOnly: true });
                } else if (typeof msg.sessionId === "string" && msg.sessionId.length > 0) {
                    drainNotifications(lastReceivedId, msg.sessionId, { sessionOnly: true });
                } else {
                    // Compatibility path for older clients that only know one
                    // cursor for their current socket scope.
                    drainNotifications(lastReceivedId, ws.data.sessionId);
                }
            }
        }
    }
}

/** JSON Response helper. */
function json(body: unknown, status = 200): Response {
    return new Response(JSON.stringify(body), {
        status,
        headers: { "Content-Type": "application/json" },
    });
}

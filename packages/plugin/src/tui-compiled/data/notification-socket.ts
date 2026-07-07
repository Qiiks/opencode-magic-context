/**
 * Persistent WebSocket to the server plugin's RPC server, replacing the old
 * 500ms HTTP notification poll.
 *
 * Why this exists: the TUI plugin and the server plugin run in separate Bun
 * runners in the same process, so they bridge over a localhost socket. The old
 * bridge polled `pending-notifications` over HTTP every 500ms — and each poll
 * opened a NEW loopback TCP connection (Bun's fetch isn't pooled to our server),
 * which was the entire source of idle TUI CPU (#200). A single long-lived WS
 * carries server→TUI pushes with zero per-event connection cost, and the server
 * pushes notifications the instant they're queued (no polling latency).
 *
 * Session scope: the socket carries the TUI's active session in its `hello` so
 * the server delivers only that session's (plus global) notifications and its
 * `isTuiConnected(session)` routing stays correct. The active session is tracked
 * with a cheap watcher that only reads `api.route.current` (a property access,
 * no IPC) and re-scopes the socket ONLY when the session actually changes — so
 * unlike the old poll it does no network work at idle.
 */

import { getRpcClient, getRpcGeneration } from "./context-db";

export interface SocketNotification {
    id: number;
    type: string;
    payload: Record<string, unknown>;
    sessionId?: string;
}

interface NotificationSocketOptions {
    /** Current active session id (re-read cheaply to follow session switches). */
    getSessionId: () => string | null;
    /** Handle one delivered notification. Returns true if it was consumed (so its
     *  id can advance the ack cursor). Async because dialog handlers await. */
    onNotification: (notification: SocketNotification) => boolean | Promise<boolean>;
}

const RECONNECT_BASE_MS = 500;
const RECONNECT_MAX_MS = 10_000;
/** Cheap session-watch interval. Reads a property only; no network. The CPU bug
 *  was the per-tick fetch, not the timer — this tick does zero IPC at idle. */
const SESSION_WATCH_MS = 1_000;

let socket: WebSocket | null = null;
let reconnectTimer: ReturnType<typeof setTimeout> | undefined;
let sessionWatchTimer: ReturnType<typeof setInterval> | undefined;
let reconnectAttempt = 0;
let closed = false;
let helloedSession: string | null = null;
let opts: NotificationSocketOptions | null = null;
/** Generation of the rpc client at connect time; a dispose/reinit bumps it and
 *  invalidates an in-flight socket so its late callbacks are ignored. */
let connectGeneration = 0;

const GLOBAL_CURSOR_KEY = "global";
const SESSION_CURSOR_PREFIX = "session:";
const MAX_DEDUPED_NOTIFICATION_IDS = 500;

/**
 * Notification ids are process-global, but acknowledgement cursors are scoped. A
 * high id consumed in session A must not become the prune watermark for session B;
 * global notifications also carry their own cursor so they cannot skip session
 * backlog. The id set de-dupes at-least-once re-delivery when a reconnect sends
 * a session cursor that is intentionally lower than the global cursor.
 */
const lastHandledIdByCursor = new Map<string, number>();
const handledNotificationIds = new Set<number>();
const handledNotificationIdOrder: number[] = [];

/** Open the persistent notification socket. Idempotent: a second call while open
 *  is a no-op. Reconnects on its own after any drop. */
export function startNotificationSocket(options: NotificationSocketOptions): void {
    opts = options;
    closed = false;
    connectGeneration = getRpcGeneration();
    connect();
    if (!sessionWatchTimer) {
        sessionWatchTimer = setInterval(watchSession, SESSION_WATCH_MS);
    }
}

/** Close the socket and stop reconnecting. Call on TUI dispose. */
export function stopNotificationSocket(): void {
    closed = true;
    if (reconnectTimer) {
        clearTimeout(reconnectTimer);
        reconnectTimer = undefined;
    }
    if (sessionWatchTimer) {
        clearInterval(sessionWatchTimer);
        sessionWatchTimer = undefined;
    }
    try {
        socket?.close();
    } catch {
        // best-effort
    }
    socket = null;
    helloedSession = null;
    reconnectAttempt = 0;
}

function scheduleReconnect(): void {
    if (closed) return;
    if (reconnectTimer) return;
    const delay = Math.min(RECONNECT_BASE_MS * 2 ** reconnectAttempt, RECONNECT_MAX_MS);
    reconnectAttempt += 1;
    reconnectTimer = setTimeout(() => {
        reconnectTimer = undefined;
        connect();
    }, delay);
}

async function connect(): Promise<void> {
    if (closed) return;
    if (socket) return; // already connected/connecting

    const client = getRpcClient();
    if (!client) {
        scheduleReconnect();
        return;
    }
    const endpoint = await client.resolveEndpoint();
    // The generation may have bumped (dispose/reinit) while resolving — abandon.
    if (closed || getRpcGeneration() !== connectGeneration) return;
    if (!endpoint) {
        scheduleReconnect();
        return;
    }

    let ws: WebSocket;
    try {
        const tokenQuery = `?token=${encodeURIComponent(endpoint.token ?? "")}`;
        ws = new WebSocket(`ws://127.0.0.1:${endpoint.port}/ws${tokenQuery}`);
    } catch {
        client.reset();
        scheduleReconnect();
        return;
    }
    socket = ws;

    ws.addEventListener("open", () => {
        if (socket !== ws) return;
        reconnectAttempt = 0;
        sendHello(ws, endpoint.token);
    });

    ws.addEventListener("message", (event) => {
        if (socket !== ws) return;
        void handleSocketMessage(ws, String((event as MessageEvent).data));
    });

    const onDown = () => {
        client.reset();
        if (socket === ws) {
            socket = null;
            helloedSession = null;
        }
        scheduleReconnect();
    };
    ws.addEventListener("close", onDown);
    ws.addEventListener("error", onDown);
}

function sendHello(ws: WebSocket, token: string | null): void {
    const sessionId = opts?.getSessionId() ?? undefined;
    helloedSession = sessionId ?? null;
    ws.send(
        JSON.stringify({
            type: "hello",
            token: token ?? "",
            sessionId,
            lastReceivedId: cursorForKey(cursorKeyForSession(sessionId)),
            globalLastReceivedId: cursorForKey(GLOBAL_CURSOR_KEY),
        }),
    );
}

async function handleSocketMessage(ws: WebSocket, raw: string): Promise<void> {
    let msg: { type?: string; notification?: SocketNotification; error?: string };
    try {
        msg = JSON.parse(raw);
    } catch {
        return;
    }

    if (msg.type === "notification" && msg.notification) {
        const notification = msg.notification;
        // Client-side session filter mirrors the old poller's per-message re-check:
        // a session-scoped notification is only acted on while the TUI is actually
        // viewing that session (the active session can change between queueing and
        // delivery). Global (session-less) notifications always apply.
        const active = opts?.getSessionId() ?? null;
        if (notification.sessionId && active && notification.sessionId !== active) {
            // Not for the session we're viewing — do NOT ack it (a TUI on the right
            // session, or a later switch back, should still get it). Just skip.
            return;
        }
        if (handledNotificationIds.has(notification.id)) {
            sendAck(ws, notification);
            return;
        }

        let consumed = false;
        try {
            consumed = await Promise.resolve(opts?.onNotification(notification) ?? false);
        } catch {
            consumed = false;
        }
        // A dispose/reinit during an awaited dialog handler invalidates this socket.
        if (socket !== ws || getRpcGeneration() !== connectGeneration) return;
        if (consumed) {
            rememberHandledId(notification.id);
            advanceCursor(notificationCursorKey(notification), notification.id);
            // Ack only the notification's own cursor scope. A dropped ack is safe:
            // the next hello sends the same per-scope cursors and duplicates are
            // ignored locally.
            sendAck(ws, notification);
        }
        return;
    }

    if (msg.type === "error") {
        // Server rejected us (bad token, etc.). Close and let backoff retry after
        // rediscovering the port/token (the server may have been replaced).
        try {
            ws.close();
        } catch {
            // best-effort
        }
    }
}

function cursorKeyForSession(sessionId: string | null | undefined): string {
    return sessionId ? `${SESSION_CURSOR_PREFIX}${sessionId}` : GLOBAL_CURSOR_KEY;
}

function notificationCursorKey(notification: SocketNotification): string {
    return notification.sessionId
        ? `${SESSION_CURSOR_PREFIX}${notification.sessionId}`
        : GLOBAL_CURSOR_KEY;
}

function cursorForKey(key: string): number {
    return lastHandledIdByCursor.get(key) ?? 0;
}

function advanceCursor(key: string, id: number): void {
    if (id > cursorForKey(key)) lastHandledIdByCursor.set(key, id);
}

function rememberHandledId(id: number): void {
    if (handledNotificationIds.has(id)) return;
    handledNotificationIds.add(id);
    handledNotificationIdOrder.push(id);
    while (handledNotificationIdOrder.length > MAX_DEDUPED_NOTIFICATION_IDS) {
        const evicted = handledNotificationIdOrder.shift();
        if (evicted !== undefined) handledNotificationIds.delete(evicted);
    }
}

function sendAck(ws: WebSocket, notification: SocketNotification): void {
    const lastReceivedId = cursorForKey(notificationCursorKey(notification));
    const ack = notification.sessionId
        ? { type: "ack", sessionId: notification.sessionId, lastReceivedId }
        : { type: "ack", ackScope: "global", lastReceivedId };
    try {
        ws.send(JSON.stringify(ack));
    } catch {
        // best-effort; reconnect hello re-syncs via per-scope cursors
    }
}

export function _resetNotificationSocketStateForTesting(): void {
    stopNotificationSocket();
    opts = null;
    lastHandledIdByCursor.clear();
    handledNotificationIds.clear();
    handledNotificationIdOrder.length = 0;
}

/** Cheap session-change watcher: re-scope the socket only when the active session
 *  actually changes. Reads a property; no network at idle. */
function watchSession(): void {
    if (closed || !socket || socket.readyState !== WebSocket.OPEN) return;
    const current = opts?.getSessionId() ?? null;
    if (current === helloedSession) return;
    // Re-hello with the new session; the server replaces this socket's sink scope.
    const client = getRpcClient();
    void client?.resolveEndpoint().then((endpoint) => {
        if (!socket || socket.readyState !== WebSocket.OPEN) return;
        sendHello(socket, endpoint?.token ?? null);
    });
}

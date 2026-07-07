import { afterEach, describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
    drainNotifications,
    isTuiConnected,
    pushNotification,
} from "../../shared/rpc-notifications";
import { MagicContextRpcServer } from "../../shared/rpc-server";
import { closeRpc, initRpcClient } from "./context-db";
import {
    _resetNotificationSocketStateForTesting,
    type SocketNotification,
    startNotificationSocket,
} from "./notification-socket";

const originalXdgDataHome = process.env.XDG_DATA_HOME;
const tempDirs: string[] = [];
const servers: MagicContextRpcServer[] = [];

afterEach(() => {
    _resetNotificationSocketStateForTesting();
    closeRpc();
    for (const server of servers.splice(0)) {
        server.stop();
    }
    drainNotifications(Number.MAX_SAFE_INTEGER);
    for (const dir of tempDirs.splice(0)) {
        rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
    }
    if (originalXdgDataHome === undefined) {
        delete process.env.XDG_DATA_HOME;
    } else {
        process.env.XDG_DATA_HOME = originalXdgDataHome;
    }
});

function makeDataHome(): string {
    const dir = mkdtempSync(join(tmpdir(), "mc-notification-socket-"));
    tempDirs.push(dir);
    process.env.XDG_DATA_HOME = dir;
    return dir;
}

function storageDir(dataHome: string): string {
    return join(dataHome, "cortexkit", "magic-context");
}

async function startServer(dataHome: string, directory: string): Promise<MagicContextRpcServer> {
    const server = new MagicContextRpcServer(storageDir(dataHome), directory);
    await server.start();
    servers.push(server);
    return server;
}

async function waitFor(condition: () => boolean, label: string, timeoutMs = 4_000): Promise<void> {
    const start = Date.now();
    while (Date.now() - start < timeoutMs) {
        if (condition()) return;
        await new Promise((resolve) => setTimeout(resolve, 25));
    }
    throw new Error(`Timed out waiting for ${label}`);
}

describe("notification socket", () => {
    test("uses the active session cursor when switching sessions", async () => {
        drainNotifications(Number.MAX_SAFE_INTEGER);
        const dataHome = makeDataHome();
        const directory = "/repo-session-cursors";
        await startServer(dataHome, directory);
        initRpcClient(directory);

        let activeSession = "ses_A";
        const received: SocketNotification[] = [];
        startNotificationSocket({
            getSessionId: () => activeSession,
            onNotification: (notification) => {
                received.push(notification);
                return true;
            },
        });

        await waitFor(() => isTuiConnected("ses_A"), "socket connected to session A");
        pushNotification("for-b", { action: "show-status-dialog" }, "ses_B");
        pushNotification("for-a", { action: "show-status-dialog" }, "ses_A");
        await waitFor(
            () => received.some((notification) => notification.type === "for-a"),
            "session A notification",
        );

        activeSession = "ses_B";
        await waitFor(
            () => received.some((notification) => notification.type === "for-b"),
            "session B backlog after switching sessions",
        );

        expect(received.map((notification) => notification.type)).toContain("for-a");
        expect(received.map((notification) => notification.type)).toContain("for-b");
    });

    test("resets the cached endpoint after websocket close so reconnect discovers a new server", async () => {
        drainNotifications(Number.MAX_SAFE_INTEGER);
        const dataHome = makeDataHome();
        const directory = "/repo-reconnect";
        const first = await startServer(dataHome, directory);
        initRpcClient(directory);

        const received: SocketNotification[] = [];
        startNotificationSocket({
            getSessionId: () => "ses_R",
            onNotification: (notification) => {
                received.push(notification);
                return true;
            },
        });
        await waitFor(() => isTuiConnected("ses_R"), "initial websocket connection");

        const second = await startServer(dataHome, directory);
        expect(second).toBeDefined();
        first.stop();
        await waitFor(() => !isTuiConnected("ses_R"), "old websocket sink removed");
        await waitFor(() => isTuiConnected("ses_R"), "websocket reconnected to replacement server");

        pushNotification("after-restart", { action: "show-status-dialog" }, "ses_R");
        await waitFor(
            () => received.some((notification) => notification.type === "after-restart"),
            "notification from replacement server",
        );
    });
});

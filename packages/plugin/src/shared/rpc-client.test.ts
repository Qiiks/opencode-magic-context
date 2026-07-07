import { afterEach, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { MagicContextRpcClient } from "./rpc-client";
import { drainNotifications, isTuiConnected, pushNotification } from "./rpc-notifications";
import { MagicContextRpcServer } from "./rpc-server";
import { parseRpcPortFile, type RpcPortFileRecord, rpcPortDir, rpcPortFilePath } from "./rpc-utils";

interface TestServer {
    port: number;
    close: () => Promise<void>;
}

const tempDirs: string[] = [];
let servers: TestServer[] = [];

afterEach(async () => {
    for (const server of servers.splice(0)) {
        await server.close();
    }
    for (const dir of tempDirs.splice(0)) {
        try {
            rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
        } catch {
            /* Ignore EBUSY on Windows */
        }
    }
});

function makeTempDir(): string {
    const dir = mkdtempSync(join(tmpdir(), "mc-rpc-client-"));
    tempDirs.push(dir);
    return dir;
}

function writePortFile(storageDir: string, directory: string, port: number): void {
    const portFile = rpcPortFilePath(storageDir, directory);
    mkdirSync(dirname(portFile), { recursive: true });
    writeFileSync(
        portFile,
        JSON.stringify({ port, pid: process.pid, started_at: Date.now() }),
        "utf-8",
    );
}

function writePortFileForPid(
    storageDir: string,
    directory: string,
    port: number,
    pid: number,
    startedAt: number,
): void {
    const portFile = rpcPortFilePath(storageDir, directory, pid);
    mkdirSync(dirname(portFile), { recursive: true });
    writeFileSync(portFile, JSON.stringify({ port, pid, started_at: startedAt }), "utf-8");
}

function readNewestPortRecord(storageDir: string, directory: string): RpcPortFileRecord | null {
    const records: RpcPortFileRecord[] = [];
    for (const entry of readdirSync(rpcPortDir(storageDir, directory))) {
        if (!entry.startsWith("port-") || !entry.endsWith(".json")) continue;
        const record = parseRpcPortFile(
            readFileSync(join(rpcPortDir(storageDir, directory), entry), "utf-8"),
        );
        if (record) records.push(record);
    }
    records.sort((a, b) => b.started_at - a.started_at);
    return records[0] ?? null;
}

async function waitFor(condition: () => boolean, label: string, timeoutMs = 2_000): Promise<void> {
    const start = Date.now();
    while (Date.now() - start < timeoutMs) {
        if (condition()) return;
        await new Promise((resolve) => setTimeout(resolve, 25));
    }
    throw new Error(`Timed out waiting for ${label}`);
}

async function openSocket(port: number, token: string): Promise<WebSocket> {
    const ws = new WebSocket(`ws://127.0.0.1:${port}/ws?token=${encodeURIComponent(token)}`);
    await new Promise<void>((resolve, reject) => {
        const timeout = setTimeout(() => reject(new Error("socket open timed out")), 2_000);
        ws.addEventListener(
            "open",
            () => {
                clearTimeout(timeout);
                resolve();
            },
            { once: true },
        );
        ws.addEventListener(
            "error",
            () => {
                clearTimeout(timeout);
                reject(new Error("socket open failed"));
            },
            { once: true },
        );
    });
    return ws;
}

function waitForJsonMessage<T extends { type?: string }>(
    ws: WebSocket,
    predicate: (message: T) => boolean,
    timeoutMs = 2_000,
): Promise<T> {
    return new Promise((resolve, reject) => {
        const timeout = setTimeout(() => {
            ws.removeEventListener("message", onMessage);
            reject(new Error("socket message timed out"));
        }, timeoutMs);
        const onMessage = (event: MessageEvent) => {
            let message: T;
            try {
                message = JSON.parse(String(event.data)) as T;
            } catch {
                return;
            }
            if (!predicate(message)) return;
            clearTimeout(timeout);
            ws.removeEventListener("message", onMessage);
            resolve(message);
        };
        ws.addEventListener("message", onMessage);
    });
}

async function startRpcServer(handler: (method: string) => Response | object): Promise<TestServer> {
    const server = createServer(async (req, res) => {
        if (req.method === "GET" && req.url === "/health") {
            res.writeHead(200, { "Content-Type": "application/json" });
            res.end(JSON.stringify({ ok: true }));
            return;
        }

        if (req.method === "POST" && req.url?.startsWith("/rpc/")) {
            const method = req.url.slice("/rpc/".length);
            const result = handler(method);
            if (result instanceof Response) {
                res.writeHead(result.status, { "Content-Type": "application/json" });
                res.end(await result.text());
                return;
            }
            res.writeHead(200, { "Content-Type": "application/json" });
            res.end(JSON.stringify(result));
            return;
        }

        res.writeHead(404);
        res.end("Not Found");
    });

    await new Promise<void>((resolve, reject) => {
        server.once("error", reject);
        server.listen(0, "127.0.0.1", () => resolve());
    });
    const addr = server.address();
    if (!addr || typeof addr === "string") throw new Error("failed to bind test server");

    const testServer = {
        port: addr.port,
        close: () =>
            new Promise<void>((resolve, reject) => {
                server.close((err) => (err ? reject(err) : resolve()));
            }),
    };
    servers.push(testServer);
    return testServer;
}

async function closeServer(server: TestServer): Promise<void> {
    servers = servers.filter((s) => s !== server);
    await server.close();
}

describe("MagicContextRpcClient", () => {
    test("re-reads the port file after the cached server restarts on a new port", async () => {
        const storageDir = makeTempDir();
        const directory = "/repo";
        const client = new MagicContextRpcClient(storageDir, directory);

        const first = await startRpcServer(() => ({ value: "first" }));
        writePortFile(storageDir, directory, first.port);
        expect(await client.call<{ value: string }>("value")).toEqual({ value: "first" });

        await closeServer(first);
        const second = await startRpcServer(() => ({ value: "second" }));
        writePortFile(storageDir, directory, second.port);

        expect(await client.call<{ value: string }>("value")).toEqual({ value: "second" });
    });

    test("authenticates against a real server with the published token", async () => {
        const storageDir = makeTempDir();
        const directory = "/repo-auth";
        const server = new MagicContextRpcServer(storageDir, directory);
        server.handle("ping", async () => ({ pong: true }));
        await server.start();
        try {
            const client = new MagicContextRpcClient(storageDir, directory);
            // Real round-trip: client must read the token from the port file and
            // send it as Bearer auth, or the server returns 401.
            expect(await client.call<{ pong: boolean }>("ping")).toEqual({ pong: true });
        } finally {
            server.stop();
        }
    });

    test("a request without the token is rejected 401 by the server", async () => {
        const storageDir = makeTempDir();
        const directory = "/repo-noauth";
        const server = new MagicContextRpcServer(storageDir, directory);
        server.handle("ping", async () => ({ pong: true }));
        const port = await server.start();
        try {
            // Sanity: the port file carries a non-empty token.
            const record = readNewestPortRecord(storageDir, directory);
            expect(typeof record?.token).toBe("string");
            expect((record?.token ?? "").length).toBeGreaterThan(0);

            // A raw fetch with no Authorization header must be rejected.
            const res = await fetch(`http://127.0.0.1:${port}/rpc/ping`, {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: "{}",
            });
            expect(res.status).toBe(401);

            // Health stays open (no token required) for discovery.
            const health = await fetch(`http://127.0.0.1:${port}/health`);
            expect(health.status).toBe(200);
        } finally {
            server.stop();
        }
    });

    test("websocket upgrade rejects missing bearer token before a socket is created", async () => {
        const storageDir = makeTempDir();
        const directory = "/repo-ws-auth";
        const server = new MagicContextRpcServer(storageDir, directory);
        const port = await server.start();
        try {
            const res = await fetch(`http://127.0.0.1:${port}/ws`);
            expect(res.status).toBe(401);
            expect(isTuiConnected()).toBe(false);
        } finally {
            server.stop();
        }
    });

    test("re-hello replaces the previous websocket notification sink", async () => {
        drainNotifications(Number.MAX_SAFE_INTEGER);
        const storageDir = makeTempDir();
        const directory = "/repo-ws-rehello";
        const server = new MagicContextRpcServer(storageDir, directory);
        const port = await server.start();
        const record = readNewestPortRecord(storageDir, directory);
        expect(typeof record?.token).toBe("string");

        const ws = await openSocket(port, record?.token ?? "");
        const notifications: unknown[] = [];
        ws.addEventListener("message", (event) => {
            const message = JSON.parse(String(event.data)) as {
                type?: string;
                notification?: unknown;
            };
            if (message.type === "notification") notifications.push(message.notification);
        });

        try {
            ws.send(JSON.stringify({ type: "hello", token: record?.token, sessionId: "ses_A" }));
            await waitForJsonMessage(ws, (message) => message.type === "hello-ack");
            expect(isTuiConnected("ses_A")).toBe(true);

            ws.send(JSON.stringify({ type: "hello", token: record?.token, sessionId: "ses_B" }));
            await waitForJsonMessage(ws, (message) => message.type === "hello-ack");
            expect(isTuiConnected("ses_A")).toBe(false);
            expect(isTuiConnected("ses_B")).toBe(true);

            ws.send(JSON.stringify({ type: "hello", token: record?.token, sessionId: "ses_B" }));
            await waitForJsonMessage(ws, (message) => message.type === "hello-ack");
            pushNotification("live", { ok: true }, "ses_B");
            await waitFor(() => notifications.length >= 1, "one live notification");
            await new Promise((resolve) => setTimeout(resolve, 50));
            expect(notifications).toHaveLength(1);

            ws.close();
            await waitFor(() => !isTuiConnected(), "socket sink cleanup");
        } finally {
            try {
                ws.close();
            } catch {
                // best-effort
            }
            server.stop();
        }
    });

    test("same-process servers keep distinct port files during overlap", async () => {
        const storageDir = makeTempDir();
        const directory = "/repo-port-collision";
        const first = new MagicContextRpcServer(storageDir, directory);
        const second = new MagicContextRpcServer(storageDir, directory);
        await first.start();
        const secondPort = await second.start();

        try {
            const files = readdirSync(rpcPortDir(storageDir, directory)).filter(
                (entry) => entry.startsWith("port-") && entry.endsWith(".json"),
            );
            expect(files.length).toBeGreaterThanOrEqual(2);

            first.stop();
            const remaining = readNewestPortRecord(storageDir, directory);
            expect(remaining?.port).toBe(secondPort);

            const client = new MagicContextRpcClient(storageDir, directory);
            expect((await client.resolveEndpoint())?.port).toBe(secondPort);
        } finally {
            first.stop();
            second.stop();
        }
    });

    test("gives up when the port file points at a dead server", async () => {
        const storageDir = makeTempDir();
        const directory = "/repo";
        const dead = await startRpcServer(() => ({ ok: true }));
        const port = dead.port;
        await closeServer(dead);
        writePortFile(storageDir, directory, port);

        const client = new MagicContextRpcClient(storageDir, directory);
        await expect(client.call("value")).rejects.toThrow(
            "Magic Context RPC server not available",
        );
    }, 20_000);

    test("re-resolves and retries transient 5xx responses", async () => {
        const storageDir = makeTempDir();
        const directory = "/repo";
        let calls = 0;
        const server = await startRpcServer(() => {
            calls++;
            if (calls === 1) {
                return new Response(JSON.stringify({ error: "warming up" }), { status: 503 });
            }
            return { value: "ok" };
        });
        writePortFile(storageDir, directory, server.port);

        const client = new MagicContextRpcClient(storageDir, directory);
        expect(await client.call<{ value: string }>("value")).toEqual({ value: "ok" });
        expect(calls).toBe(2);
    });

    test("ignores newer stale pid files and discovers the latest live instance", async () => {
        const storageDir = makeTempDir();
        const directory = "/repo";
        const live = await startRpcServer(() => ({ value: "live" }));
        writePortFileForPid(storageDir, directory, 65535, 999_999_999, Date.now() + 10_000);
        writePortFileForPid(storageDir, directory, live.port, process.pid, Date.now());

        const client = new MagicContextRpcClient(storageDir, directory);
        expect(await client.call<{ value: string }>("value")).toEqual({ value: "live" });
    });
});

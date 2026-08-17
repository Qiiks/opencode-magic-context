/// <reference types="bun-types" />

import { afterEach, describe, expect, it } from "bun:test";
import { waitForReady } from "./spawn";

let server: ReturnType<typeof Bun.serve> | undefined;

function readyProviderConfig(): Response {
    return Response.json({
        providers: [
            {
                id: "mock-anthropic",
                models: { "mock-sonnet": { id: "mock-sonnet" } },
            },
        ],
    });
}

afterEach(() => {
    server?.stop(true);
    server = undefined;
});

describe("opencode readiness", () => {
    it("waits for the session API after the documentation endpoint responds", async () => {
        let sessionAttempts = 0;
        let toolAttempts = 0;
        let providerAttempts = 0;
        const observedDirectories: string[] = [];
        server = Bun.serve({
            port: 0,
            fetch(request) {
                const requestUrl = new URL(request.url);
                if (requestUrl.pathname === "/doc") return new Response("ready");
                if (requestUrl.pathname === "/experimental/tool/ids") {
                    toolAttempts += 1;
                    return Response.json(["ctx_search"]);
                }
                if (requestUrl.pathname === "/config/providers") {
                    providerAttempts += 1;
                    return readyProviderConfig();
                }
                if (requestUrl.pathname !== "/session") return new Response(null, { status: 404 });

                sessionAttempts += 1;
                observedDirectories.push(requestUrl.searchParams.get("directory") ?? "");
                if (sessionAttempts === 1) return new Response(null, { status: 503 });
                if (sessionAttempts === 2) return Response.json({ warming: true });
                return Response.json([]);
            },
        });

        const url = `http://127.0.0.1:${server.port}`;
        expect((await fetch(`${url}/doc`)).ok).toBe(true);

        await waitForReady(url, "/isolated/workdir", 2_000);

        expect(sessionAttempts).toBe(3);
        expect(toolAttempts).toBe(1);
        expect(providerAttempts).toBe(1);
        expect(observedDirectories).toEqual([
            "/isolated/workdir",
            "/isolated/workdir",
            "/isolated/workdir",
        ]);
    });

    it("waits for Magic Context hooks and the configured model after sessions are ready", async () => {
        let sessionAttempts = 0;
        let toolAttempts = 0;
        let providerAttempts = 0;
        const observedDirectories = new Set<string>();
        server = Bun.serve({
            port: 0,
            fetch(request) {
                const requestUrl = new URL(request.url);
                observedDirectories.add(requestUrl.searchParams.get("directory") ?? "");
                if (requestUrl.pathname === "/session") {
                    sessionAttempts += 1;
                    return Response.json([]);
                }
                if (requestUrl.pathname === "/experimental/tool/ids") {
                    toolAttempts += 1;
                    return Response.json(toolAttempts >= 2 ? ["ctx_search"] : []);
                }
                if (requestUrl.pathname === "/config/providers") {
                    providerAttempts += 1;
                    return providerAttempts >= 3
                        ? readyProviderConfig()
                        : Response.json({ providers: [] });
                }
                return new Response(null, { status: 404 });
            },
        });

        await waitForReady(`http://127.0.0.1:${server.port}`, "/isolated/workdir", 2_000);

        expect(sessionAttempts).toBe(1);
        expect(toolAttempts).toBe(3);
        expect(providerAttempts).toBe(3);
        expect([...observedDirectories]).toEqual(["/isolated/workdir"]);
    });

    it("rejects session-only readiness when Magic Context hooks never register", async () => {
        server = Bun.serve({
            port: 0,
            fetch(request) {
                const requestUrl = new URL(request.url);
                if (requestUrl.pathname === "/session") return Response.json([]);
                if (requestUrl.pathname === "/experimental/tool/ids") return Response.json([]);
                if (requestUrl.pathname === "/config/providers") return readyProviderConfig();
                return new Response(null, { status: 404 });
            },
        });

        await expect(
            waitForReady(`http://127.0.0.1:${server.port}`, "/isolated/workdir", 20),
        ).rejects.toThrow("Magic Context tool registry is not ready");
    });
});

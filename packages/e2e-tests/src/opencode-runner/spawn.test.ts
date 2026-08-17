/// <reference types="bun-types" />

import { afterEach, describe, expect, it } from "bun:test";
import { waitForReady } from "./spawn";

let server: ReturnType<typeof Bun.serve> | undefined;

afterEach(() => {
    server?.stop(true);
    server = undefined;
});

describe("opencode readiness", () => {
    it("waits for the session API after the documentation endpoint responds", async () => {
        let sessionAttempts = 0;
        const observedDirectories: string[] = [];
        server = Bun.serve({
            port: 0,
            fetch(request) {
                const requestUrl = new URL(request.url);
                if (requestUrl.pathname === "/doc") return new Response("ready");
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
        expect(observedDirectories).toEqual([
            "/isolated/workdir",
            "/isolated/workdir",
            "/isolated/workdir",
        ]);
    });
});

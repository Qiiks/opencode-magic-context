import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

const directory = await mkdtemp(join(tmpdir(), "retina-local-fs-smoke-"));
const path = join(directory, "ready.txt");
const cli = new URL("../src/cli.ts", import.meta.url).pathname;

async function invoke(cursor: object | null): Promise<{ events: unknown[]; cursor: object }> {
    const child = Bun.spawn({
        cmd: ["bun", cli],
        stdin: "pipe",
        stdout: "pipe",
        stderr: "pipe",
    });
    child.stdin.write(JSON.stringify({ cursor, config: { kind: "path_exists", path } }));
    child.stdin.end();
    const [exitCode, stdout, stderr] = await Promise.all([
        child.exited,
        new Response(child.stdout).text(),
        new Response(child.stderr).text(),
    ]);
    if (exitCode !== 0) {
        throw new Error(`provider exited ${exitCode}: ${stderr.trim()}`);
    }
    return JSON.parse(stdout) as { events: unknown[]; cursor: object };
}

try {
    await writeFile(path, "ready\n");
    const first = await invoke(null);
    const second = await invoke(first.cursor);
    if (first.events.length !== 1 || second.events.length !== 0) {
        throw new Error("cursor round-trip did not suppress the unchanged observation");
    }
    process.stdout.write(`${JSON.stringify({ first, second })}\n`);
} finally {
    await rm(directory, { recursive: true, force: true });
}

import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const pluginRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const bundlePath = process.argv[2]
    ? resolve(process.cwd(), process.argv[2])
    : join(pluginRoot, "dist/transformers-node-wasm.js");
const transformers = (await import(pathToFileURL(bundlePath).href)) as {
    env: {
        allowLocalModels: boolean;
        cacheDir: string | null;
        fetch: (input: string | URL | Request, init?: RequestInit) => Promise<Response>;
        useFS: boolean;
        useFSCache: boolean;
    };
    __magicContextGetModelFile: (
        model: string,
        filename: string,
        fatal: boolean,
        options: Record<string, unknown>,
        returnPath: boolean,
    ) => Promise<string | Uint8Array | null>;
};

if (!transformers.env.useFS || !transformers.env.useFSCache) {
    throw new Error("Node WASM bundle initialized without filesystem model caching");
}

const cacheDir = await mkdtemp(join(tmpdir(), "mc-transformers-node-wasm-"));
const original = {
    allowLocalModels: transformers.env.allowLocalModels,
    cacheDir: transformers.env.cacheDir,
    fetch: transformers.env.fetch,
};
const payload = new Uint8Array([17, 29, 43, 71]);
let fetches = 0;
try {
    transformers.env.allowLocalModels = false;
    transformers.env.cacheDir = cacheDir;
    transformers.env.fetch = async () => {
        fetches++;
        return new Response(payload, {
            status: 200,
            headers: { "content-length": String(payload.byteLength) },
        });
    };

    const first = await transformers.__magicContextGetModelFile(
        "fixture/model",
        "onnx/model.onnx",
        true,
        {},
        true,
    );
    if (typeof first !== "string") {
        throw new Error("Node WASM FileCache did not return a persisted model path");
    }
    const persisted = await readFile(first);
    if (!persisted.equals(Buffer.from(payload))) {
        throw new Error("Node WASM FileCache persisted different model bytes");
    }

    transformers.env.fetch = async () => {
        throw new Error("offline cache verification unexpectedly downloaded the model again");
    };
    const second = await transformers.__magicContextGetModelFile(
        "fixture/model",
        "onnx/model.onnx",
        true,
        {},
        true,
    );
    if (second !== first || fetches !== 1) {
        throw new Error("Node WASM model did not reload from its persistent cache");
    }
} finally {
    transformers.env.allowLocalModels = original.allowLocalModels;
    transformers.env.cacheDir = original.cacheDir;
    transformers.env.fetch = original.fetch;
    await rm(cacheDir, { recursive: true, force: true });
}

console.log("transformers-node-wasm filesystem cache probe passed");

import { mkdir } from "node:fs/promises";
import { createRequire } from "node:module";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const pluginRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const outputDir = resolve(process.cwd(), process.argv[2] ?? join(pluginRoot, "dist"));
const entrypoint = join(
    pluginRoot,
    "src/features/magic-context/memory/transformers-node-wasm-entry.ts",
);
const nativeShim = join(
    pluginRoot,
    "src/features/magic-context/memory/onnxruntime-node-wasm-shim.ts",
);
const sharpShim = join(pluginRoot, "src/features/magic-context/memory/sharp-wasm-shim.ts");
const requireFromPlugin = createRequire(join(pluginRoot, "package.json"));
const transformersEntry = requireFromPlugin.resolve("@huggingface/transformers");
const transformersRoot = dirname(dirname(transformersEntry));
const transformersSource = join(transformersRoot, "src/transformers.js");
const transformersHub = join(transformersRoot, "src/utils/hub.js");

await mkdir(outputDir, { recursive: true });
const result = await Bun.build({
    entrypoints: [entrypoint],
    outdir: outputDir,
    naming: "transformers-node-wasm.js",
    target: "node",
    format: "esm",
    external: ["onnxruntime-web", "onnxruntime-web/*"],
    plugins: [
        {
            name: "magic-context-transformers-node-wasm",
            setup(build) {
                build.onResolve({ filter: /^@huggingface\/transformers$/ }, () => ({
                    path: transformersSource,
                }));
                build.onResolve({ filter: /^onnxruntime-node$/ }, () => ({ path: nativeShim }));
                build.onResolve({ filter: /^sharp$/ }, () => ({ path: sharpShim }));
                build.onResolve(
                    { filter: /^@magic-context\/transformers-node-hub$/ },
                    () => ({ path: transformersHub }),
                );
            },
        },
    ],
});

if (!result.success) {
    for (const message of result.logs) console.error(message);
    process.exit(1);
}

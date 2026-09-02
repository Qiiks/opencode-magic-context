// This Node-target entry preserves real node:fs support while the build aliases
// optional native addons away from their platform loaders. The browser-target
// sibling remains unchanged for Electron and other web-compatible consumers.
export * from "@huggingface/transformers";

// The load probe verifies the upstream FileCache path used by the production
// bundle. This specifier is resolved only by build-transformers-node-wasm.ts.
// @ts-expect-error Build-time virtual module backed by Transformers.js internals.
export { getModelFile as __magicContextGetModelFile } from "@magic-context/transformers-node-hub";

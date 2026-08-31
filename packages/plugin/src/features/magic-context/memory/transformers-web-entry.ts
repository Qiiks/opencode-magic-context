// This entry is built with Bun's browser condition so transformers resolves its
// web export. The Node plugin bundle uses the native export in a separate lazy
// chunk and falls back to this file when the optional native addon is absent.
export * from "@huggingface/transformers";

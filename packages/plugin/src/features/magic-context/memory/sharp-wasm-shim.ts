// Text embeddings do not use Transformers.js image decoding. Avoid evaluating
// Sharp's optional native addon in the Node WASM bundle; image callers fail
// explicitly instead of crashing module initialization on an absent binding.
export default function sharpUnavailable(): never {
    throw new Error("Sharp is unavailable in the local embedding WASM runtime");
}

Thank you, @vinayakkulkarni — the decoded `napi_module_register` frame and the
embedding-only correlation identify the same class as our earlier #95 report:
an upstream Bun NAPI environment-cleanup race in onnxruntime-node, not a damaged
Magic Context database or an embedding-model error.

Bun fixed the underlying issue in oven-sh/bun#30291, included in Bun 1.4.0.
OpenCode currently embeds Bun 1.3.14, so Magic Context now protects affected Bun
hosts structurally: with `embedding.local_runtime: "auto"` (the default), Bun
below 1.4.0 uses `onnxruntime-web` WASM and does not import the NAPI addon at
all. Node keeps native ONNX, Electron keeps its existing web injection, and a
Bun host on 1.4.0 or newer automatically returns to native. There is no OpenCode
upgrade timeline implied here. `embedding.local_runtime: "native"` remains an
escape hatch for users who prefer speed and accept the pre-1.4.0 risk;
`"wasm"` forces the safe runtime.

We measured cached-model native MiniLM on 60 real local records on the
maintainer machine: 1.87 s cold pipeline load, 16.35 ms p50/item, and 23.04 ms
p95/item. We could not responsibly publish a WASM comparison from that machine:
the installed transformers web bundle could not read its local cache under Bun,
and that environment also failed Hugging Face certificate verification. So there
is no claimed WASM ratio yet; the report calls this out rather than treating a
failed run as a performance result. If WASM is over 5× slower at p50 in a clean
measurement, that temporary trade-off should be reconsidered explicitly.

Your WAL concern is separate from this exit-time addon panic. SQLite committed
transactions survive a post-quit teardown panic; WAL replay handles committed
frames on the next open. A leftover `-wal` file is therefore an expected recovery
artifact after an abnormal exit, not evidence of corruption by itself.

Your child-process suggestion is also apt, and the AFT comparison is fair. The
certified out-of-process local provider (Synapse) is the architectural end-state.
WASM-in-host is the smallest safe current-scale fix while the MiniLM lane is
normally in the roughly 10–40 ms/item range; it removes the affected NAPI addon
from pre-fix Bun VMs without adding a daemon dependency.

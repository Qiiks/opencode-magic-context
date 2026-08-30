# GitHub #392: Bun NAPI teardown mitigation

## Finding

The reported `NAPI FATAL ERROR: Error::New napi_create_error` crash in
`napi_module_register` is the same onnxruntime-node teardown class tracked by
closed issue #95 and upstream [oven-sh/bun#30291](https://github.com/oven-sh/bun/pull/30291).
That upstream fix merged as `cf5bf1b04803deceb095a248743f204c640f97f9` and is
contained in Bun 1.4.0, not Bun 1.3.14. Current OpenCode releases embed Bun
1.3.14, so a local embedding session can load the NAPI addon and later race
Bun VM teardown on TUI exit.

A simple load/embed/exit reproduction was deliberately not used as proof: the
race needs long-session finalizer state, and clean exits do not disprove it.

## Change

`embedding.local_runtime` is a user-level local-provider setting:

- `auto` (default): Node selects native; Electron retains its existing early
  web-runtime injection; Bun below 1.4.0 selects `onnxruntime-web` WASM before
  transformers can import `onnxruntime-node`; Bun 1.4.0 and newer select native.
- `native`: explicitly selects native, including on an affected Bun release.
  This is an opt-in acceptance of the pre-1.4.0 teardown crash risk.
- `wasm`: explicitly selects WASM.

The version comparison is numeric semver, including the `1.10.0 > 1.4.0`
case. The protection is structural: the vulnerable Bun branch loads the web
bundle directly and never imports the NAPI binding. It self-heals when an
OpenCode host upgrades to Bun 1.4.0 or later.

The CLI runtime probe now checks the runtime selected by the configuration and
host. For a selected WASM runtime it probes only `onnxruntime-web` and reports
that it did not probe or load the native addon. Native-load failure fallback
remains unchanged.

## Verification and mutation evidence

- Plugin targeted suite: 58 passing tests, including the runtime-selection
  matrix, Electron path, schema, existing native-to-WASM fallback, and generated
  config reference parity.
- CLI targeted suite: runtime probe and Pi doctor tests pass after the doctor
  assertion changed from “native runtime OK” to “native runtime selected and
  OK”; this reflects the new selected-runtime contract.
- Executed mutation: replaced the vulnerable-Bun WASM branch with `if (false)`
  and ran `bun test src/features/magic-context/memory/embedding-local.test.ts`.
  It failed at `embedding-local.test.ts:191` (`1.3.14` received `native` instead
  of `wasm`) and at `:248` (the provider could not initialize because the native
  seam was reached). The original guarded branch was restored before any other
  verification.
- The resolver test pins `1.3.14 → wasm`, `1.4.0 → native`, and
  `1.10.0 → native`; changing the guarded selection condition makes the
  pre-fix assertion fail.

## Measurement

The intended benchmark is 60 real `compartments.p1` records from this machine's
context database, one pipeline invocation per item, with a cached MiniLM model
and a fresh process/pipeline for cold-load timing.

| Runtime | n | cold load | p50/item | p95/item |
|---|---:|---:|---:|---:|
| native | 60 | 1872.68 ms | 16.35 ms | 23.04 ms |
| WASM | — | unavailable | unavailable | unavailable |

WASM could not be measured honestly in this worktree: the installed
transformers 4.2.0 web bundle cannot read its local model cache under Bun
(`ERR_INVALID_URL` for `/models/...`), then the environment's Hugging Face TLS
interception fails certificate verification. Native uses the cached MiniLM
successfully. No performance ratio is claimed; measure the released fallback
in an environment with a working WASM model fetch/cache before making a
performance promise. The native result is consistent with the existing
~10–40 ms/item local lane; it does not establish the temporary WASM tax.

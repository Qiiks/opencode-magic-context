# Six-axis review: external PR #342

- **PR:** `cortexkit/magic-context#342`, “fix(embedding): guard Windows Bun and support trusted headers”
- **Reviewed head:** `fb57aa75931e5abf3976ab287539e26ae5044041`
- **PR base:** `b27a6d7672cb9c8a119998b9aa0ae1b9017cc87f`
- **Primary incident:** issue #283, Bun 1.3.14/Windows ONNX segmentation fault
- **Related request:** issue #310, host-agent embedding providers/custom authentication
- **Review date:** 2026-08-19

## Verdict: needs-rework

The Windows mechanism is well matched to issue #283: the local provider refuses Bun+win32 before importing Transformers/ONNX, the registry never constructs the unsafe provider, semantic search degrades to keyword/full-text search (FTS), and status plus both diagnostic (`doctor`) surfaces explain the safe alternatives. The existing permanent `nativeRuntimeMissing` latch remains intact rather than being overloaded with a platform verdict.

The trusted-header implementation also respects the most important #5562 boundary: `embedding.headers` is parsed only from trusted user configuration and is stripped from project configuration before merge. A cloned repository does **not** gain header control.

The PR is not merge-ready because the new credentials are not protected across the complete request/diagnostic lifecycle:

1. **The doctor probe lacks the runtime client's egress protections.** `probeEmbeddingEndpoint()` checks only the URL scheme, follows redirects by default, and does not call `blockedEmbeddingEndpointReason()`. The runtime provider correctly uses the metadata/link-local guard and `redirect: "error"`. A trusted endpoint that returns a 307/308 can therefore make doctor forward a custom `X-API-Key` or other arbitrary credential header to a second destination even though normal embedding traffic would refuse the redirect.
2. **Blanket config redaction does not prevent response-echo leaks.** OpenCode doctor logs sanitized error-body previews, and the runtime provider logs the first 200 characters of malformed 200 responses. `sanitizeDiagnosticText()` knows common token shapes and key/value forms, but it does not know that an innocuous string such as `workspace-credential-17` is the value of `X-Workspace`. An endpoint that echoes that value can put it in logs/diagnostics despite the PR's claim that every header value is hidden.

All 14 current GitHub checks pass; the optional `[code]smith` check is skipped. The suite does not test redirect refusal, parity with the runtime's server-side request forgery (SSRF) guard, or an echoed benign-named header secret.

## 1. Windows/Bun mechanism

### Guard behavior

`getLocalEmbeddingUnavailableReason()` returns a platform verdict only when:

- `process.platform === "win32"`; and
- the host is Bun, detected from `process.versions.bun` or `globalThis.Bun`.

`LocalEmbeddingProvider.initialize()` checks that verdict before the dynamic `@huggingface/transformers` import. That ordering is the essential safety property: an ONNX native crash cannot be caught in JavaScript, so the code must never enter the import/inference path.

The guard is not a mutable process-level failure latch. It is a deterministic check over runtime and OS properties that cannot change during the process. `windowsBunDisabledLogged` suppresses duplicate diagnostics only; the latch-permanence guard correctly classifies it as `DIAGNOSTIC` rather than a cached capability verdict.

### Existing permanent-latch semantics

The pre-existing `nativeRuntimeMissing` behavior is preserved:

- missing `onnxruntime-node`, a missing binding, and ONNX-specific `ERR_DLOPEN_FAILED` remain permanent environmental failures for that process;
- transient protobuf/EBUSY/model-load failures are not promoted into the permanent latch;
- the Windows/Bun platform guard runs before either import or latch check, so it never probes a crash-prone runtime merely to classify it.

This separation is correct. “Unsupported host combination” and “repairable installation defect” have different doctor guidance and should not share one mutable state bit.

### Registry and doctor routing

- `registerProjectEmbedding()` maps unavailable local registration to the off provider identity, records an `unavailableReason`, and avoids provider construction.
- `embedTextForProject()`/batch paths therefore return no vector and allow existing keyword/FTS fallback behavior to continue.
- `/ctx-embed-status` reports the reason and suggests `openai-compatible` or `off`.
- OpenCode doctor evaluates the target installation: CLI is treated as Bun, while Desktop/Electron is not falsely disabled.
- Pi doctor checks its actual host and avoids the native child probe when local embeddings are unsafe.

This directly addresses issue #283 without weakening Node-on-Windows or Bun-on-macOS/Linux.

## 2. Trusted headers and trust tier

### Accepted surface

`embedding.headers` is a string-to-string map on `openai-compatible` configuration. A shared helper:

- validates names and values through the platform `Headers` implementation;
- canonicalizes header names;
- forces `content-type: application/json`;
- uses a custom `Authorization` value in preference to the legacy `api_key` bearer header;
- supports header-only authentication such as `X-API-Key`.

The same builder is used by runtime requests and doctor probes, which avoids precedence drift.

### #5562 project-tier audit: pass

`project-security.ts` now includes `headers` in `EMBEDDING_DESTINATION_FIELDS` together with endpoint/provider/fallback routing. Project config is stripped before merge, and the warning names fields rather than values. Pi doctor separately mirrors the runtime loader's rules: project variable tokens do not expand, unsafe project fields are removed, and then trusted user/project config is merged.

A project may still tune model/request fields against the user's trusted destination, but it cannot choose the destination or inject a header. Therefore the specific user instruction—project-tier header control is a blocking secret-exfiltration vector—is **not** triggered by this PR.

### Redaction that is present

- Config/issue diagnostics replace every value nested under `embedding.headers`, regardless of whether the header name looks secret.
- Invalid-header errors are generic and do not echo the rejected name/value.
- Runtime failure logs normally print status, not outbound headers.
- Header material is not persisted in plaintext; provider/model identifiers contain only derived hashes.

### Blocking redaction gap: echoed values

The blanket redactor is path-aware only while it is walking the config object. Once an endpoint returns text, that key path is gone. Two output paths remain capable of reproducing a custom secret:

- `probeEmbeddingEndpoint()` returns response previews, and OpenCode doctor logs those previews after generic sanitization.
- `OpenAICompatibleEmbeddingProvider.embedBatch()` logs a snippet of a malformed/non-JSON 200 body.

A response containing only `workspace-credential-17` does not match the generic secret patterns. The exact outbound header values need scoped redaction before any response/error text is returned or logged.

**Required change:** build a per-request secret set from non-empty `api_key` and every custom header value, replace exact occurrences in response previews/errors/snippets before generic sanitization, and add tests with a benign-named header whose value is echoed by 401/500/malformed-200 responses. Alternatively, stop logging response bodies for authenticated/custom-header requests and return only status/length/content-type.

## 3. SSRF and egress interaction

### Runtime provider

The normal OpenAI-compatible provider keeps its existing protections:

- `blockedEmbeddingEndpointReason()` rejects malformed URLs, cloud metadata hostnames, IPv4/IPv6 link-local addresses, and mapped link-local forms;
- loopback and private LAN addresses remain intentionally allowed for Ollama/LM Studio/self-hosted use;
- `redirect: "error"` prevents an allowed origin from redirecting credentials and memory text to a different target.

Custom headers do not bypass those checks, and project config cannot alter their destination.

### Blocking probe divergence

`probeEmbeddingEndpoint()` performs neither of the two runtime checks. This was existing debt for `api_key`, but arbitrary trusted headers broaden the credential surface and make the mismatch part of this feature's safety contract. Fetch implementations generally strip `Authorization` across cross-origin redirects, but do not promise to strip every custom secret header such as `X-API-Key` or `X-Workspace`.

**Required change:** run the same `blockedEmbeddingEndpointReason()` preflight before constructing/sending headers and set `redirect: "error"` in probe fetch options. Add non-vacuous tests proving a metadata/link-local endpoint makes zero fetch calls and the request init explicitly rejects redirects.

## 4. Provider blast radius

| Provider/lane | Result |
|---|---|
| Local, direct | **Safe on Bun+win32.** Registry degrades to off/keyword instead of creating the provider. Other host combinations are unchanged. |
| OpenAI-compatible, direct | **Headers reach runtime and doctor consistently.** Authorization precedence and header-only auth work. Probe egress/redaction gaps remain blocking. |
| Synapse, healthy primary | **Unaffected.** HTTP fallback fields are not sent to the Synapse lane. |
| Synapse → openai-compatible fallback | **Covered.** `fallbackConfig()` carries trusted headers, API key, model, endpoint, input types, and truncate into the selected HTTP lane. |
| Synapse → local fallback | **Safe on Bun+win32.** Routing warns; registry marks the selected local lane unavailable rather than loading ONNX. |
| Synapse → off fallback | **Unaffected.** No provider or headers are created. |
| Shadow Synapse lane | **Unaffected.** Headers remain on the HTTP primary/fallback configuration and do not enter the shadow daemon path. |
| Existing FTS/keyword fallback | **Preserved.** Local unavailability disables semantic vectors rather than memory search as a whole. |

### Issue #310 relationship

This PR implements one alternative explicitly proposed in #310: static custom headers for `openai-compatible`. It does **not** implement host-provider delegation, reuse OpenCode/Pi-managed OAuth, or refresh dynamic tokens. Users still duplicate credentials into user config (possibly through `{env:...}`/`{file:...}` substitution). The issue should remain open and the PR should describe itself as partial relief, not host-agent embedding support.

### Identity and re-embedding behavior

Normalized custom header values feed the provider/model identity hash, while the full config also feeds the runtime fingerprint. This guarantees that any header rotation recreates the provider, but it also assigns a new stored model identity and causes re-embedding/stale-vector lifecycle work even when only an authentication credential rotated. Existing `api_key` handling deliberately separates those concerns: key presence affects vector identity, while the runtime fingerprint recreates the provider when the value changes.

This is conservative for headers that semantically route to a different tenant/model, but expensive and unnecessary for known authentication headers.

**Requested refinement:** separate lifecycle identity from vector-space identity. At minimum, treat `Authorization`, `Proxy-Authorization`, and conventional API-key header values like `api_key`—presence in the provider identity, value only in the runtime fingerprint. If selected routing headers intentionally change vector identity, document or allowlist that policy rather than hashing every credential value into persisted model IDs.

## 5. Test adequacy

### Strengths

- The provider test would fail if initialization entered the mocked Transformers pipeline under Bun+win32; the core crash guard is behavior-tested rather than shape-tested.
- Registry tests prove no provider is constructed, vectors return null, coverage reports unavailable, and status guidance is actionable.
- Routing and both doctor surfaces distinguish Bun-on-Windows from safe host combinations; OpenCode Desktop is explicitly protected from a false positive.
- Permanent native-runtime failure classification keeps transient errors out of the latch.
- Schema/request tests cover invalid headers, custom Authorization precedence, header-only auth, normalized identity behavior, and provider recreation.
- Project-security tests prove a repository-supplied header is removed while non-destination tuning fields survive.
- OpenCode/Pi diagnostic tests blanket-redact innocently named header values.

### Gaps

- There is no auditable red-first commit: no published commit shows the new regression failing before the implementation is added. The first commit introduces implementation and tests together; the later security lifecycle commit updates code and tests in the same change.
- Current CI has no Windows job. The mocked platform test is mutation-sensitive and valuable, but the process-crash incident itself is not exercised on a Windows runner.
- No probe test asserts `redirect: "error"` or applies the runtime metadata/link-local guard.
- No test returns an outbound header value in an HTTP error preview or malformed-200 body and proves it cannot reach logs/diagnostics.
- No test locks the desired distinction between credential rotation (provider recreation only) and vector-space/routing-header change (possible re-embedding).

## 6. Code fit

### Good fit

- The unsafe import is stopped at the narrowest useful boundary, before Transformers/ONNX evaluation.
- Host/platform detection is centralized and reused by runtime, registry, routing, status, and doctor.
- The platform diagnostic latch is correctly classified instead of becoming another permanent capability cache.
- Header validation and Authorization precedence live in one shared utility.
- Project security strips the field before merge rather than trying to sanitize an already merged secret.
- Generated schema and user-facing configuration docs are updated.

### Changes needed for house fit

- Runtime and doctor must share one complete HTTP safety policy, not just header construction.
- “Every header value is redacted” must include endpoint-controlled response text, not only config serialization.
- Keep secret rotation out of durable vector identity where it cannot change the embedding space; use the existing runtime-fingerprint layer for provider recreation.
- State #310 scope precisely: static user headers are supported; host-managed providers and dynamic OAuth are not.

## Exact change requests

1. **Harden `probeEmbeddingEndpoint()`.** Apply `blockedEmbeddingEndpointReason()` before fetch, set `redirect: "error"`, and add zero-fetch metadata/link-local plus redirect-init regressions.
2. **Redact exact outbound credential values from all returned/logged text.** Cover probe previews/network errors and runtime malformed-body snippets; test a benign-named header value echoed by the server.
3. **Separate credential lifecycle from vector identity.** Rotate the provider on secret change without forcing a new model ID/re-embedding for known auth headers; document any headers intentionally treated as vector-space routing inputs.
4. **Add an explicit egress/redaction integration test.** Use the same configured header through runtime and doctor, assert identical Authorization precedence, redirect refusal, SSRF refusal, and no plaintext value in captured diagnostics.
5. **Keep issue #310 open and scope the release note.** This PR supports static trusted headers only; it does not delegate to OpenCode/Pi provider configuration or refresh host-managed OAuth.

## Verification notes

- Reviewed the complete three-commit diff at `fb57aa75931e5abf3976ab287539e26ae5044041` against issues #283 and #310, the #5562 project-tier doctrine, local-provider/registry routing, OpenAI-compatible runtime requests, Synapse fallback selection, doctors, and redaction surfaces.
- `git diff --check b27a6d7672cb9c8a119998b9aa0ae1b9017cc87f fb57aa75931e5abf3976ab287539e26ae5044041` passed.
- The requested `gh pr view 342 --json statusCheckRollup` view reports 14 successful checks, no failures, and one skipped optional `[code]smith` check at the reviewed head.
- No merge, post, push, or PR mutation was performed.

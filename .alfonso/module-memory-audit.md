# `ck-mc` process-lifetime memory-holder audit

Audit date: 2026-08-14

Scope: production (`#[cfg(test)]` excluded) ownership reachable from `crates/mc-module/src/lib.rs`, `transform.rs`, `historian.rs`, and `selection.rs`. Definitions in `ck_wire.rs`, `codec/sidecar.rs`, `config.rs`, and `mc-store` are followed only where needed to establish transitive ownership. This is a source audit, not a live-heap census: RSS and `vmmap` include allocator arenas/high-water fragmentation as well as currently reachable objects.

## Executive findings

1. **The configured production ceilings are much larger than the stated 550–600 MiB premise.** The production native-attachment cache is **256 MiB total / 192 MiB per entry**, not the 1 MiB test size (`crates/mc-module/src/lib.rs:2068-2076`, `crates/mc-module/src/lib.rs:2187-2193`). Adding all explicit production byte ceilings gives **912 MiB of charged resident/staged data**, or **976 MiB** if a full 64 MiB of snapshot `Arc` leases survives map eviction:

   | Holder | Explicit ceiling |
   |---|---:|
   | Projection cache | 256 MiB total, 192 MiB/entry (`crates/mc-module/src/lib.rs:477-485`, `crates/mc-module/src/lib.rs:2352-2358`) |
   | Native-attachment cache | 256 MiB total, 192 MiB/entry (`crates/mc-module/src/lib.rs:2068-2076`, `crates/mc-module/src/lib.rs:2187-2193`) |
   | Transform-page staging | 128 MiB (`crates/mc-module/src/lib.rs:450-455`, `crates/mc-module/src/lib.rs:896-905`) |
   | Ready transform snapshots | 64 MiB (`crates/mc-module/src/lib.rs:477-489`, `crates/mc-module/src/lib.rs:1742-1756`) |
   | Active snapshot leases after/alongside cache residency | 64 MiB and 8 leases (`crates/mc-module/src/lib.rs:484-489`, `crates/mc-module/src/lib.rs:1751-1756`, `crates/mc-module/src/lib.rs:1841-1872`) |
   | Serialized-output cache | 64 MiB (`crates/mc-module/src/transform.rs:133-134`, `crates/mc-module/src/transform.rs:294-307`) |
   | Process-wide tag-baseline cache | 64 MiB (`crates/mc-module/src/transform.rs:133-134`, `crates/mc-module/src/transform.rs:6137-6140`) |
   | State-sync seed staging | 32 MiB (`crates/mc-module/src/lib.rs:446-449`, `crates/mc-module/src/lib.rs:767-775`) |
   | State-import staging | 32 MiB (`crates/mc-module/src/lib.rs:473-476`, `crates/mc-module/src/lib.rs:1170-1180`) |
   | Boundary-token cache | 16 MiB (`crates/mc-module/src/lib.rs:477-483`, `crates/mc-module/src/lib.rs:2005-2013`) |

   The 912 MiB subtotal excludes uncharged completed-page response bytes, unbounded metadata maps, outer `HashMap`/LRU keys, and allocator fragmentation. Therefore ~950 MiB dirty is not above the source-level aggregate ceilings; the more useful question is which ceilings are populated and how far their accounting diverges from heap reality.

2. **The strongest accounting defect is in the ready transform snapshot.** A tail-delta request is expanded by cloning the retained full CK and native prefixes into `parsed.messages`/`parsed.native_messages` (`crates/mc-module/src/lib.rs:3161-3223`), but `finish_ready` charges only the *new inbound delta frame length* (`crates/mc-module/src/lib.rs:7260-7277`). The cached entry then owns the expanded full `Arc<TransformRequest>` (`crates/mc-module/src/lib.rs:1683-1693`, `crates/mc-module/src/lib.rs:1818-1828`). A few-KiB or few-MiB delta can therefore keep a many-thousand-message request tree while consuming only that delta in the 64 MiB budget.

3. **Even full-frame snapshot accounting is not conservative.** CK deserialization retains a complete original `serde_json::Value`, a typed message, and per-block original `Value`s (`crates/mc-store/src/lib.rs:84-124`, `crates/mc-store/src/lib.rs:190-218`). Thus one payload string commonly has three object-tree owners before native-message duplication or map/vector overhead. Charging serialized body length (`crates/mc-module/src/lib.rs:7260-7267`) materially undercounts the deserialized request on the MALLOC_SMALL-heavy workload in the incident.

4. **There is no per-pass in-memory history accumulation in `historian.rs` or `selection.rs`.** `selection.rs` describes and implements a pure deterministic function over caller-owned inputs (`crates/mc-module/src/selection.rs:1-27`); its maps/vectors are call-local. Production `historian.rs` ends without a static collection; durable state is read/written through `McStore`, and its only `OnceLock<BTreeMap>` is inside the test module beginning at `crates/mc-module/src/historian.rs:1724`. Pass traces and the 256-entry scheduler history are SQLite rows, not process registries (`crates/mc-module/src/transform.rs:1707-1746`, `crates/mc-store/src/lib.rs:7014-7039`, `crates/mc-store/src/lib.rs:7041-7100`).

5. **No owning `Arc` cycle was found.** There are several `Arc`-into-cache/registry patterns that delay reclamation until work finishes, but all reverse edges are absent and the session guards remove registry entries in `Drop` (`crates/mc-module/src/lib.rs:2546-2571`, `crates/mc-module/src/lib.rs:2576-2599`, `crates/mc-module/src/lib.rs:2650-2683`). A wedged detached historian task can retain its firing payload and guard for the duration of the wedge, but it is a stuck task, not an `Arc` cycle (`crates/mc-module/src/lib.rs:2825-2835`, `crates/mc-module/src/lib.rs:4343-4389`, `crates/mc-module/src/lib.rs:4559-4571`).

## Estimation model

The arithmetic below assumes the live 64-bit target: a `String`/`Vec` has a 24-byte inline header plus heap capacity; each owned `String` has a separate allocation and allocator rounding; `HashMap` bucket capacity exceeds item count (normally about 1/0.875 at high occupancy) and carries control bytes; `BTreeMap` pays node/pointer slack; `Arc<T>` pays a strong/weak-count allocation header; and `Vec`/map capacity, not logical length, determines the allocation. These are estimates, not Rust ABI guarantees.

This matters here because the source shapes are dominated by short strings, `Value` objects/arrays, `HashMap` entries, and cloned CK structs. That is exactly the class of ownership for which serialized-byte or string-content accounting misses 24-byte headers, buckets/nodes, capacities, and allocator size classes.

## Complete cache/stager inventory

All handler-owned production holders are constructed together at `crates/mc-module/src/lib.rs:2871-2924` and declared at `crates/mc-module/src/lib.rs:2437-2510`.

### 1. Ready transform snapshots and in-flight markers

- **Construction:** `McHandler::new_with_connection_file` creates `TransformSnapshotCache::new(64 MiB)` at `crates/mc-module/src/lib.rs:2882-2884`; fields are at `crates/mc-module/src/lib.rs:1725-1739`.
- **Bound:** ready entries are charged to 64 MiB; in-flight markers are separately capped at 4,096 entries; active leases are capped at 64 MiB and 8 (`crates/mc-module/src/lib.rs:477-489`, `crates/mc-module/src/lib.rs:1742-1756`).
- **Eviction:** ready entries use LRU-by-session (`crates/mc-module/src/lib.rs:1827-1838`, `crates/mc-module/src/lib.rs:1841-1872`); in-flight entries use insertion order and become `Missing` when over 4,096 (`crates/mc-module/src/lib.rs:1768-1793`). Route teardown explicitly removes the session (`crates/mc-module/src/lib.rs:3344-3347`).
- **One ready entry transitively owns:** two session-id strings (outer map key and ready-LRU key), the enum metadata, and `Arc<TransformRequest>`. The request owns dozens of strings/options/maps, `Vec<CkIngressMessage>`, optional `Vec<Value>` native messages, prompt-surface descriptions, lineage constituents, and other request fields (`crates/mc-module/src/transform.rs:531-713`). Each ingress message owns a message id and a `CkWireMessage` (`crates/mc-module/src/ck_wire.rs:26-31`), and deserialized CK retains typed plus original JSON trees (`crates/mc-store/src/lib.rs:84-124`, `crates/mc-store/src/lib.rs:190-218`).
- **What an in-flight marker actually owns:** `generation: u64` only (`crates/mc-module/src/lib.rs:1683-1686`), plus the outer `HashMap<String, enum>` key and a second session-id `String` in `in_flight_lru` (`crates/mc-module/src/lib.rs:1725-1732`, `crates/mc-module/src/lib.rs:1772-1778`). At roughly 0.2–0.4 KiB per typical 36–128 byte session id, 4,096 markers are approximately **0.8–1.6 MiB**, not a material part of 1 GiB. The comment saying “no byte charge” is accurate only for budget bookkeeping, not literal heap ownership.
- **Accounting gaps:** serialized inbound length ignores object overhead and retained original JSON; tail-delta expansion makes the charge unrelated to retained full-request size (`crates/mc-module/src/lib.rs:3161-3223`, `crates/mc-module/src/lib.rs:7260-7277`). `HashMap`/LRU metadata is also uncharged. A lease holds the request after map eviction until `SnapshotLease::drop` decrements the budget (`crates/mc-module/src/lib.rs:1702-1715`).

### 2. Projection cache

- **Construction:** production `ProjectionCache::default()` at `crates/mc-module/src/lib.rs:2887`; 256 MiB total / 192 MiB per entry at `crates/mc-module/src/lib.rs:477-483`, `crates/mc-module/src/lib.rs:2352-2375`.
- **Bound/eviction:** byte-charged session LRU; entries over either limit are refused, and oldest sessions are removed until charged bytes fit (`crates/mc-module/src/lib.rs:2403-2434`). Route teardown removes the session (`crates/mc-module/src/lib.rs:3356-3359`). A channel replacement cleans most old-session caches but notably omits `projections` (`crates/mc-module/src/lib.rs:3038-3067`), so that old projection waits for LRU pressure or a later teardown.
- **One entry transitively owns:** outer session/LRU strings; context session/profile/render-config strings; optional full-array fingerprint; and `Arc<FlatProjection>` (`crates/mc-module/src/lib.rs:2317-2349`). A projection owns `Vec<FlatBlock>`, `BTreeMap<String, Vec<BlockIdentity>>`, one message-end `usize` per message, and one `Arc<ProjectionState>` per message (`crates/mc-module/src/ck_wire.rs:84-102`). Every block owns multiple strings, an `Arc<str>` serialized block, optional cloned tool-input `Arc<Value>`, and cloned `Arc<CkWireBlock>` (`crates/mc-module/src/ck_wire.rs:33-64`, `crates/mc-module/src/ck_wire.rs:427-489`).
- **Growth:** proportional to **blocks plus messages per session**; `states_after_messages` is one state allocation per message (`crates/mc-module/src/ck_wire.rs:345-359`). This makes the 2,600-message session a primary bulk holder.
- **Accounting:** `FlatProjection::retained_bytes` charges `size_of::<FlatBlock>()`, string content, three copies of block bytes, identity arrays, frontier string contents, and message-end integers (`crates/mc-module/src/ck_wire.rs:109-171`). It does **not** charge B-tree nodes/load slack, `Arc` headers, capacities, outer session/LRU keys, or most `ProjectionState` container overhead. More importantly, the “three copies” comment assumes one canonical string + one CK tree + one tool-input (`crates/mc-module/src/ck_wire.rs:124-128`), while the CK tree itself retains both typed and original block JSON (`crates/mc-store/src/lib.rs:190-218`), and tool-call flattening makes another `input.clone()` (`crates/mc-module/src/ck_wire.rs:440-450`). Tool-input-heavy projections can therefore retain roughly a fourth payload copy plus small-object overhead.
- **Arc eviction gap:** cache snapshotting clones the projection `Arc` (`crates/mc-module/src/lib.rs:2384-2400`, `crates/mc-module/src/lib.rs:10503-10518`). LRU eviction cannot reclaim an active transform's clone; unlike transform-snapshot leases, no separate active-projection byte budget exists.

### 3. Native-attachment cache

- **Construction:** `NativeAttachmentCache::default()` at `crates/mc-module/src/lib.rs:2886`; production default is 256 MiB / 192 MiB per entry (`crates/mc-module/src/lib.rs:2068-2076`, `crates/mc-module/src/lib.rs:2187-2193`). The 1 MiB constructor is `#[cfg(test)]` only (`crates/mc-module/src/lib.rs:2196-2200`).
- **Bound/eviction:** byte-charged session LRU. An oversized entry first discards sidecar trees, then is refused if still over 192 MiB; oldest sessions are removed above 256 MiB (`crates/mc-module/src/lib.rs:2242-2306`). Route teardown removes it (`crates/mc-module/src/lib.rs:3352-3355`). A non-native pass does not touch or remove an existing native entry because attach returns without cache access when `serve_native` is false (`crates/mc-module/src/lib.rs:10700-10720`).
- **One entry transitively owns:** context strings/fingerprint; `Arc<DecodeSidecar>`; `Vec<[u8; 32]>` message keys; two string-keyed maps of sidecar hashes/sizes; and `Vec<NativeEncodedChunk>`, each with an `Arc<Value>` encoded native chunk (`crates/mc-module/src/lib.rs:2078-2120`). The sidecar itself owns an ordered `Vec<String>`, a `BTreeMap<String, Arc<HarnessMessageMeta>>`, pin strings, and each meta's raw JSON and block metadata/raw JSON (`crates/mc-module/src/codec/sidecar.rs:36-45`, `crates/mc-module/src/codec/sidecar.rs:84-109`).
- **Growth:** proportional to served messages/blocks and native metadata per session. The source's 4,600-message/15,000-block fixture calls the native representation approximately 49 MiB (`crates/mc-module/src/lib.rs:2068-2071`).
- **Accounting:** chunks recursively charge `size_of::<Value>()` and content lengths (`crates/mc-module/src/lib.rs:10603-10620`); sidecar rows charge a serialized estimate; total charge adds encoded chunks, **2× served CK canonical bytes**, and sidecar estimates (`crates/mc-module/src/lib.rs:2123-2151`, `crates/mc-module/src/lib.rs:10558-10561`). Missing pieces include map/B-tree nodes, key `String` headers, capacities, `Arc` headers, and allocator rounding. Conversely, the 2× served-byte term is a proxy for allocations the native snapshot does not itself own. It may compensate at aggregate scale, but it is not a faithful deep-size measurement and cannot identify which entry actually consumes memory.
- **Arc eviction gap:** `snapshot()` clones sidecar and chunk `Arc`s (`crates/mc-module/src/lib.rs:2219-2240`). An in-flight attachment can keep evicted values alive with no active-clone budget.

### 4. Serialized-output cache

- **Construction:** `SerializedOutputCache::default()` at `crates/mc-module/src/lib.rs:2885`; 64 MiB default at `crates/mc-module/src/transform.rs:133`, `crates/mc-module/src/transform.rs:294-307`.
- **Bound/eviction:** session LRU based only on the sum of `served.canonical_bytes.len()` (`crates/mc-module/src/transform.rs:337-371`). Revert-epoch mismatch and route teardown remove a session (`crates/mc-module/src/transform.rs:317-334`, `crates/mc-module/src/lib.rs:3348-3351`).
- **One entry transitively owns:** the per-session inner `HashMap` key (normally `tail:<mid>`), a second identity `String`, and optional `ServedMessage` (`crates/mc-module/src/transform.rs:260-292`, `crates/mc-module/src/transform.rs:9032-9046`, `crates/mc-module/src/transform.rs:9514-9537`). `ServedMessage` owns `Arc<CkWireMessage>`, canonical byte `Arc<[u8]>`, a 32-byte hash, output-identity `Arc<str>`, and `Arc<[(String, usize)]>` block fingerprints (`crates/mc-module/src/transform.rs:136-201`). There is normally one cache entry per emitted tail message, so growth is explicitly proportional to messages per session (`crates/mc-module/src/transform.rs:9480-9515`, `crates/mc-module/src/transform.rs:9692-9704`).
- **Accounting gap:** the charge counts **only canonical bytes**, not the typed/original message tree, output identity, fingerprint strings, keys, identity strings, map buckets, or `Arc` allocations (`crates/mc-module/src/transform.rs:345-349`). `served: None` entries are completely free in the byte budget but still own key/identity/map metadata; `replace` inserts them even when total charged bytes are zero (`crates/mc-module/src/transform.rs:337-363`). On unchanged pass-through output, cloned CK can retain full message original JSON plus per-block original JSON and typed data (`crates/mc-store/src/lib.rs:84-124`, `crates/mc-store/src/lib.rs:190-218`). For small-string-heavy messages, actual ownership can plausibly be 3–5× canonical-byte charge.
- **Transient double residency:** `snapshot()` clones the entire inner map (keys and identity strings deeply, `ServedMessage` by `Arc`) while the old cache entry remains (`crates/mc-module/src/transform.rs:317-334`). Payload trees are shared, but map/string metadata doubles during each pass.

### 5. Boundary-token cache

- **Construction:** 16 MiB at `crates/mc-module/src/lib.rs:2888`, using `BOUNDARY_TOKEN_CACHE_BUDGET_BYTES` (`crates/mc-module/src/lib.rs:477-483`).
- **Bound/eviction:** byte-charged session LRU; empty or over-budget snapshots are refused (`crates/mc-module/src/lib.rs:1997-2065`). Route teardown removes it (`crates/mc-module/src/lib.rs:3360-3363`).
- **One entry transitively owns:** for source blocks, block-id `String` plus `{byte_size, [u8;32] content_hash, token_count}`; a second map keys formatted text by hash and stores counts (`crates/mc-module/src/lib.rs:1916-1995`). No source content is retained. `retain_projection` drops ids no longer present (`crates/mc-module/src/lib.rs:1966-1975`).
- **Accounting:** source keys/values get a 64-byte allowance; formatted entries get another 64-byte allowance (`crates/mc-module/src/lib.rs:1977-1987`). This is much closer to the actual small-object shape than the raw/output caches, though the outer session/LRU key and capacity slack are still omitted. Snapshot cloning temporarily duplicates both maps (`crates/mc-module/src/lib.rs:2022-2037`).

### 6. Transform-page coordinator

- **Construction:** handler-wide 128 MiB serialized-page staging and 64 pending transforms (`crates/mc-module/src/lib.rs:450-455`, `crates/mc-module/src/lib.rs:885-905`, `crates/mc-module/src/lib.rs:2917-2919`). Individual pages are capped at 512 KiB (`crates/mc-module/src/lib.rs:452`, `crates/mc-module/src/lib.rs:8155-8168`).
- **Bound/eviction:** collectors are bounded by staged serialized bytes and pending count (`crates/mc-module/src/lib.rs:1007-1030`, `crates/mc-module/src/lib.rs:1101-1113`), but there is **no TTL/background eviction**. Route replacement/teardown and protocol errors call `discard` (`crates/mc-module/src/lib.rs:3038-3047`, `crates/mc-module/src/lib.rs:3335-3343`, `crates/mc-module/src/lib.rs:8059-8168`).
- **One collecting entry transitively owns:** transform id, generation/counts, one digest `String` per page, and `Vec<Value>` containing every deserialized page (`crates/mc-module/src/lib.rs:843-860`). The `page_bytes` charge is serialized JSON length, not deep `Value` ownership (`crates/mc-module/src/lib.rs:993-1006`, `crates/mc-module/src/lib.rs:8155-8161`). Small-object-heavy pages can cost 2–4× their charge.
- **Completed entry:** one transform id, generation, final digest, and the entire encoded response `Vec<u8>` (`crates/mc-module/src/lib.rs:862-874`, `crates/mc-module/src/lib.rs:8261-8292`). **Completed response bytes are not included in `total_staged_bytes` or any other coordinator budget.** They persist for redrive (`crates/mc-module/src/lib.rs:8170-8178`) until the next successful completion overwrites them or `discard` clears them.
- **Only-insert shell:** `discard` resets phase/completed but does not remove `sessions[session_id]` (`crates/mc-module/src/lib.rs:950-965`), and completion inserts/retains the session (`crates/mc-module/src/lib.rs:8265-8292`). Thus the coordinator's session map and capacity grow with every paged session ever seen by this handler even after route teardown; only the large page/result payload is cleared.

### 7. State-sync seed coordinator

- **Construction:** 32 MiB handler-wide staging at `crates/mc-module/src/lib.rs:446-449`, `crates/mc-module/src/lib.rs:759-775`, `crates/mc-module/src/lib.rs:2917`.
- **Bound/eviction:** serialized `batch_bytes` feed the 32 MiB cap (`crates/mc-module/src/lib.rs:7609-7623`, `crates/mc-module/src/lib.rs:7699-7718`). Collecting entries have a 10-minute TTL, but stale eviction runs only when another paged seed is staged (`crates/mc-module/src/lib.rs:825-840`, `crates/mc-module/src/lib.rs:7540-7544`), so an otherwise idle process retains an expired partial collector indefinitely. Route teardown uses `evict`, which removes the session (`crates/mc-module/src/lib.rs:820-823`, `crates/mc-module/src/lib.rs:3335-3338`).
- **One collecting entry transitively owns:** seed id, a digest `Vec<String>`, and `Vec<ModuleStateSyncWire>`; each batch contains compartment/memory/mutation/drop/hint/strip vectors and many owned strings/values (`crates/mc-module/src/lib.rs:502-624`, `crates/mc-module/src/lib.rs:713-724`). Serialized bytes undercount these typed collections and headers.
- **Completed entry:** metadata plus response `Vec<u8>` (`crates/mc-module/src/lib.rs:734-748`, `crates/mc-module/src/lib.rs:7788-7800`), uncharged and retained until replaced/evicted. It is normally a small acknowledgement, unlike a transform response.
- **Count bookkeeping defect:** `pending_seed_count` is declared/decremented but never incremented in production (`crates/mc-module/src/lib.rs:759-775`, `crates/mc-module/src/lib.rs:791-811`, staging at `crates/mc-module/src/lib.rs:7622-7653`). There is no effective entry-count cap, although the 32 MiB byte cap limits nonempty collectors. Zero-byte/idle session shells remain until route eviction.

### 8. State-import coordinator

- **Construction:** 32 MiB / 64 pending imports / 5-minute stale threshold (`crates/mc-module/src/lib.rs:473-476`, `crates/mc-module/src/lib.rs:1160-1180`, `crates/mc-module/src/lib.rs:2921`).
- **Bound/eviction:** serialized batch bytes and pending count gate admission; collecting entries are lazily evicted on a later `stage`; apply completion removes the session (`crates/mc-module/src/lib.rs:1218-1247`, `crates/mc-module/src/lib.rs:1345-1438`, `crates/mc-module/src/lib.rs:4720-4729`). Route teardown also discards it (`crates/mc-module/src/lib.rs:3340-3343`).
- **One entry transitively owns:** import/digest strings and `Vec<StoredCompartment>`; each compartment carries title/content/P1–P4, message ids/dates, and episode type (`crates/mc-module/src/lib.rs:665-710`, `crates/mc-module/src/lib.rs:1143-1158`). Serialized request length undercounts string/vector headers and allocation slack. Unlike transform pages, there is no retained completed result.

### 9. Process-wide tag baseline

- **Construction:** a static `OnceLock<Mutex<TagBaselineCache>>` with a 64 MiB budget (`crates/mc-module/src/transform.rs:133-134`, `crates/mc-module/src/transform.rs:6084-6140`). This is process-global, not per `McHandler`.
- **Bound/eviction:** byte-charged session LRU; empty and individually over-budget entries are refused (`crates/mc-module/src/transform.rs:6104-6134`). Its `remove` method is used only by replacement; no handler route-gone/session-delete hook can call this private global cache (`crates/mc-module/src/transform.rs:6111-6125`, `crates/mc-module/src/lib.rs:3304-3369`, `crates/mc-module/src/lib.rs:5100-5131`).
- **One entry transitively owns:** session/LRU strings and `Arc<Vec<McTagRow>>`; each row owns block-id/kind strings and a full `source_bytes: Vec<u8>` (`crates/mc-module/src/transform.rs:6051-6064`, `crates/mc-store/src/lib.rs:3693-3701`). Growth is proportional to tagged blocks and their source bytes per session.
- **Accounting:** row `size_of`, content lengths, and 64 bytes per row are charged (`crates/mc-module/src/transform.rs:6142-6152`), so this estimator is relatively credible. It still omits outer keys/LRU and can temporarily exceed budget when an active pass holds an old `Arc<Vec<_>>` after LRU replacement (`crates/mc-module/src/transform.rs:6172-6227`).

### 10. Process-wide tag-mint frontier

- **Construction:** static `OnceLock<Mutex<HashMap<String, TagMintFrontierMemo>>>` at `crates/mc-module/src/transform.rs:6433-6438`.
- **Bound/eviction:** **none**—no byte cap, entry cap, LRU, TTL, route hook, or remove call. Every tagging-enabled session clones/inserts its memo (`crates/mc-module/src/transform.rs:6978-7022`).
- **One entry transitively owns:** session-id map key and `Vec<[u8;32]> block_keys`, two 32-byte digests, frontier, and candidate count (`crates/mc-module/src/transform.rs:6239-6257`). The vector is rebuilt with one 32-byte key per projection block (`crates/mc-module/src/transform.rs:6337-6360`). At 15,000 blocks this is about **480,000 bytes plus vector/map overhead per session**; two such sessions are under 1 MiB, so this is a real leak-by-session but not the incident's main bulk under the stated two-session workload.

### 11. Process-wide M1 pending-log buckets

- **Construction:** static `OnceLock<Mutex<HashMap<String, usize>>>` (`crates/mc-module/src/transform.rs:116-119`).
- **Bound/eviction:** no session-count bound or removal. A session is inserted only after its pending age crosses a logging threshold, and the value is replaced up to bucket 3 (`crates/mc-module/src/transform.rs:383-400`).
- **One entry:** session-id `String`, one `usize`, and map overhead. This is negligible for two sessions but grows with all historically logged session ids.

## Registries and session-keyed maps

The following maps/sets are outside the byte-budgeted caches. “Replace” means repeated passes for the same session do not add another value.

| Holder | Construction and one-entry ownership | Growth/bound | Removal path |
|---|---|---|---|
| `bindings: HashMap<u16, SessionBinding>` | Field at `crates/mc-module/src/lib.rs:2484-2489`, constructed `crates/mc-module/src/lib.rs:2913`. One entry owns project `PathBuf`, harness/session/model strings, and a cloned config containing model-chain strings, cache-TTL string, and a model→TTL `BTreeMap<String,String>` (`crates/mc-module/src/lib.rs:126-143`, `crates/mc-module/src/config.rs:81-108`). | Active route channels; last-write-wins (`crates/mc-module/src/lib.rs:3016-3033`). | `on_route_gone` → `unbind_route` removes channel (`crates/mc-module/src/lib.rs:3304-3323`, `crates/mc-module/src/lib.rs:10197-10201`). |
| `transform_route_channels` | Channel → `(session String, canonical PathBuf)` (`crates/mc-module/src/lib.rs:2493-2495`, `crates/mc-module/src/lib.rs:2915`). | Active transform channels; replace by channel (`crates/mc-module/src/lib.rs:6917-6921`). | Removed on bind replacement and route gone (`crates/mc-module/src/lib.rs:3019-3023`, `crates/mc-module/src/lib.rs:3304-3309`). |
| `transform_session_roots` | Session `String` → `HashSet<PathBuf>` (`crates/mc-module/src/lib.rs:2496-2498`, `crates/mc-module/src/lib.rs:2916`). | **Only inserts**, proportional to historical sessions × distinct canonical roots (`crates/mc-module/src/lib.rs:3428-3460`, `crates/mc-module/src/lib.rs:6922-6927`). | **Never removed by design**; field comment says it survives route teardown (`crates/mc-module/src/lib.rs:2496-2498`). `session.delete` does not touch it (`crates/mc-module/src/lib.rs:5100-5124`). |
| `scheduler_observations` | Session → two scalar fields (`crates/mc-module/src/lib.rs:2455`, `crates/mc-module/src/lib.rs:2837-2841`, `crates/mc-module/src/lib.rs:2889`). | One replaced value per successfully responding session (`crates/mc-module/src/lib.rs:3557-3568`, `crates/mc-module/src/lib.rs:7258-7259`). | Removed when final route closes (`crates/mc-module/src/lib.rs:3331-3334`); not by `session.delete`. |
| `guidance_dates` | Session → date-line `String` (`crates/mc-module/src/lib.rs:2456`, `crates/mc-module/src/lib.rs:2890`). | Only inserted while durable guidance date is absent; replace by session (`crates/mc-module/src/lib.rs:6627-6657`). | Removed when durable date is observed or a transform commits (`crates/mc-module/src/lib.rs:6633-6639`, `crates/mc-module/src/lib.rs:7233-7239`). **No route-gone or session-delete cleanup** (`crates/mc-module/src/lib.rs:3304-3369`, `crates/mc-module/src/lib.rs:5100-5124`), so failed/missing-row sessions can persist. |
| `prompt_surface_epochs` | Session → model/config strings and `BTreeMap<String,String>` tool descriptions (`crates/mc-module/src/lib.rs:2457`, `crates/mc-module/src/lib.rs:2891`, `crates/mc-module/src/prompt_surface.rs:85-93`). | One replaced/frozen value per session; no byte cap beyond request limits (`crates/mc-module/src/lib.rs:6381-6400`). | Removed on bind replacement/final route close (`crates/mc-module/src/lib.rs:3064-3067`, `crates/mc-module/src/lib.rs:3364-3367`); not by `session.delete`. |
| `note_evaluation_capabilities` | Canonical project-root `String` → bool (`crates/mc-module/src/lib.rs:2490-2492`, `crates/mc-module/src/lib.rs:2914`). | One entry/project root, replace (`crates/mc-module/src/lib.rs:3075-3083`). | Removed after the last binding for that root disappears (`crates/mc-module/src/lib.rs:3094-3108`). |
| `missing_facade_command_id_sessions` | `HashSet<String>` (`crates/mc-module/src/lib.rs:2507-2509`, `crates/mc-module/src/lib.rs:2923`). | **Only inserts**, one warning key per historically affected session (`crates/mc-module/src/lib.rs:9005-9015`). | **No removal path** in production, including route gone and `session.delete`. |
| `reattaching_sessions` | Session-id set (`crates/mc-module/src/lib.rs:2446`, `crates/mc-module/src/lib.rs:2878`). One active reattach owns a duplicate id in its `StringSetGuard`. | At most one active reattach/session (`crates/mc-module/src/lib.rs:3686-3729`). | Guard removes in `Drop` (`crates/mc-module/src/lib.rs:2560-2571`); `session.delete` also removes (`crates/mc-module/src/lib.rs:5112-5115`). |
| `live_historian_sessions` | Session → `{Arc<()> token, Arc<Notify>}` (`crates/mc-module/src/lib.rs:2447`, `crates/mc-module/src/lib.rs:2576-2587`, `crates/mc-module/src/lib.rs:2879`). | One active historian/session (`crates/mc-module/src/lib.rs:3628-3653`). | `SessionSetGuard::drop` notifies/removes the matching token (`crates/mc-module/src/lib.rs:2589-2599`). `session.delete` does not explicitly remove it. |
| `wrapup_sessions` | Session → `{Arc<()> token, rounds}` (`crates/mc-module/src/lib.rs:2448`, `crates/mc-module/src/lib.rs:2650-2660`, `crates/mc-module/src/lib.rs:2880`). | One active wrapup/session (`crates/mc-module/src/lib.rs:3666-3684`). | Guard removes in `Drop` (`crates/mc-module/src/lib.rs:2674-2683`); `session.delete` also removes (`crates/mc-module/src/lib.rs:5116-5119`). |
| `recomp_sessions` | Session-id set (`crates/mc-module/src/lib.rs:2449`, `crates/mc-module/src/lib.rs:2881`). | One active recomp/session (`crates/mc-module/src/lib.rs:3655-3664`). | `StringSetGuard::drop` removes (`crates/mc-module/src/lib.rs:2565-2571`); `session.delete` also removes (`crates/mc-module/src/lib.rs:5120-5123`). |
| `active_dreamer_runs` | Session-id set plus `DreamerRunGuard` (`crates/mc-module/src/lib.rs:2504-2506`, `crates/mc-module/src/lib.rs:2922`). | One registered active dreamer id (`crates/mc-module/src/lib.rs:8304-8313`). | Guard and explicit unregister remove (`crates/mc-module/src/lib.rs:2546-2557`, `crates/mc-module/src/lib.rs:8315-8320`); route teardown also unregisters dreamer ids (`crates/mc-module/src/lib.rs:3327-3330`). |
| `ConfigCache` | One user tier, one current project tier, one effective config (`crates/mc-module/src/lib.rs:2443`, `crates/mc-module/src/lib.rs:2875`; `crates/mc-module/src/config.rs:198-210`). Each tier may own a parsed config `Value`. | Fixed two tiers, **not per-session**; path/mtime changes replace prior values (`crates/mc-module/src/config.rs:218-251`). | Replaced on config resolution; drops with handler. |
| `store: OnceLock<Arc<McStore>>` | Exactly one store handle opened after HELLO_ACK (`crates/mc-module/src/lib.rs:2439-2442`, `crates/mc-module/src/lib.rs:10146-10163`). | Fixed one. Any caches internal to `McStore` are outside this four-module audit. | Process/handler drop only. |

### Fixed-size process globals

For completeness, the remaining production statics are fixed-size and do not grow by session/pass: `DISPATCH_HEALTH` is five atomics (`crates/mc-module/src/lib.rs:181-201`, `crates/mc-module/src/lib.rs:279`); emergency-reasoning exclusion is one atomic (`crates/mc-module/src/transform.rs:118-126`); native/prefix differential switches are one cached bool each (`crates/mc-module/src/lib.rs:10690-10697`, `crates/mc-module/src/transform.rs:1912-1919`); four lazily compiled regexes are single immutable automata (`crates/mc-module/src/transform.rs:7413-7420`, `crates/mc-module/src/transform.rs:8043-8047`, `crates/mc-module/src/transform.rs:8180-8187`); and optional drive-fault state is one atomic plus one `Option<DriveFault>` behind the feature flag (`crates/mc-module/src/lib.rs:11061-11083`). None is a plausible multi-megabyte holder. There are no production statics in `selection.rs` or `historian.rs`; the latter's only static map is inside its `#[cfg(test)]` module (`crates/mc-module/src/historian.rs:1724-1827`).

### Route-gone versus `session.delete`

`unbind_route` is the real local-memory cleanup hook: on the final session route it removes scheduler observation, state-sync state, page payloads, imports, snapshots, serialized output, native attachment, projection, boundary tokens, and prompt surface (`crates/mc-module/src/lib.rs:3304-3369`). It intentionally does not remove transform-root provenance, and it cannot reach the process-global tag baseline/frontier/M1 log maps.

`session.delete` is **not** a cache eviction hook. After deleting durable rows it removes only reattach, wrapup, and recomp latches (`crates/mc-module/src/lib.rs:5100-5124`). While the route remains bound, all byte caches and most registries remain until epoch mismatch, replacement, LRU pressure, or route teardown. This distinction should be explicit in any remediation design.

## Durable state that is not a process-lifetime holder

- **Pass traces and scheduler history rings:** transform receives/completes/stable breadcrumbs through `McStore` (`crates/mc-module/src/lib.rs:6982-6992`, `crates/mc-module/src/transform.rs:1707-1746`). The store writes a JSON scheduler history capped at 256 elements (`crates/mc-store/src/lib.rs:7041-7100`); it is not a handler `VecDeque`.
- **Divergence trackers:** the transform loads one durable snapshot (`crates/mc-module/src/transform.rs:2363-2374`), computes the current served fingerprint, writes it through `TransformCommit`, then drops locals (`crates/mc-module/src/transform.rs:4316-4326`, `crates/mc-module/src/transform.rs:4351-4384`). No divergence registry exists in the four files.
- **Caveman state:** caveman payloads are `FrozenUnit`s in the loaded `CoreState`; output rendering looks them up during a pass (`crates/mc-module/src/transform.rs:9596-9618`) and commits the resulting core through `TransformCommit` (`crates/mc-module/src/transform.rs:4351-4384`). The serialized-output cache can retain rendered messages containing those bytes, but there is no separate caveman session map.
- **Reduction ledger/state:** pending drops are loaded from the store and reductions become frozen units in `CoreState`; consumed ids are passed to `commit_transform` (`crates/mc-module/src/transform.rs:4331-4361`). Selection's agent-drop maps/sets are caller-owned pass inputs (`crates/mc-module/src/selection.rs:167-209`) and are freed after selection.
- **Historian collector/state:** `historian.rs` operates on borrowed requests and durable `HistorianDurableState` (`crates/mc-module/src/historian.rs:893-938`, `crates/mc-module/src/historian.rs:1195-1388`). The handler can own one active `HistorianFiringTask` per session; its `AssembledHistorianFiring` owns prompt/chunk/identity vectors while the producer run is active (`crates/mc-module/src/historian_chunk.rs:491-508`, `crates/mc-module/src/lib.rs:4163-4195`). The registry guard drops after the task. This is bounded active work, not per-pass accumulation.
- **Selection:** all arc maps/sets/vectors are local to the pure selection call (`crates/mc-module/src/selection.rs:1-27`, `crates/mc-module/src/selection.rs:237-392`, `crates/mc-module/src/selection.rs:1090-1395`). There is no static or handler registry in `selection.rs`.

## Accounting-versus-reality gaps

### A. Retained-original CK creates multiple payload trees

`CkWireMessage::deserialize` first materializes a full `Value`, clones it into typed data, and keeps the original (`crates/mc-store/src/lib.rs:109-124`). Every `CkWireBlock` repeats that pattern (`crates/mc-store/src/lib.rs:206-218`). For unchanged ingress, a content string can therefore exist in:

1. the retained original message `Value`,
2. the retained original block `Value`, and
3. the typed `CkKind`,

before any projection serialized string, projection tool-input clone, output message, native sidecar, or response encoding. This ownership pattern is a direct match for millions of small `String`/`Vec`/map allocations and invalidates “serialized length bounds typed tree” as a conservative assumption.

### B. Tail-delta snapshot charge uses the wrong object

The retained object after `expand_transform_tail_delta` is full history (`crates/mc-module/src/lib.rs:3212-3217`), but its charge remains the incoming delta body (`crates/mc-module/src/lib.rs:7260-7277`). This is not a percentage-level header omission; it can be orders of magnitude. It also means the 64 MiB LRU may see both active sessions as tiny and never evict either full tree.

### C. Projection estimator misses a fourth tool-input copy and containers

Projection flattening stores serialized block bytes, a cloned block (typed + retained original), and a separately cloned tool-input `Value` (`crates/mc-module/src/ck_wire.rs:427-489`). The estimator charges three payload copies (`crates/mc-module/src/ck_wire.rs:124-128`) but describes the CK tree as one copy; retained original JSON makes that premise false. B-tree nodes, message-state `Arc` allocations, vector capacities, and session/LRU keys are additional gaps (`crates/mc-module/src/ck_wire.rs:84-102`, `crates/mc-module/src/ck_wire.rs:141-170`).

### D. Output cache charges one of several owners

Only canonical byte length is charged (`crates/mc-module/src/transform.rs:345-349`), while each cached output also holds a message tree, output identity, block fingerprint strings, per-entry identity, key, maps, and `Arc` headers (`crates/mc-module/src/transform.rs:136-201`, `crates/mc-module/src/transform.rs:260-292`). This is likely the largest multiplicative gap after the tail-delta snapshot.

### E. Native estimator mixes undercount and proxy overcount

Recursive `Value` size omits key headers/map buckets/capacities (`crates/mc-module/src/lib.rs:10603-10620`), while the total adds 2× served CK bytes that are not owned by the native snapshot (`crates/mc-module/src/lib.rs:2145-2150`). This can keep aggregate admission conservative on one fixture but cannot reliably enforce a hard live-heap limit across different small-object shapes.

### F. Staging charges serialized bytes, not heap bytes

Transform pages retain `Value`; state-sync retains typed wires; state import retains typed compartments, but all charge serialized request length (`crates/mc-module/src/lib.rs:8155-8161`, `crates/mc-module/src/lib.rs:7492-7498`, `crates/mc-module/src/lib.rs:4573-4579`). A 128 MiB page collector can therefore represent several hundred MiB of small-object heap. State-sync/import TTL eviction is traffic-driven rather than timed (`crates/mc-module/src/lib.rs:825-840`, `crates/mc-module/src/lib.rs:1218-1234`).

### G. Completed page response and outer metadata are free

The latest paged-transform response is a full uncharged `Vec<u8>` (`crates/mc-module/src/lib.rs:8261-8292`). LRU session ids, outer maps, cache context strings, zero-byte output entries, completed seed acknowledgements, and empty coordinator session shells likewise do not count toward their byte budgets.

### H. Peak allocation can remain in RSS after objects drop

`respond_transform` serializes non-CK response fields into a fresh `Value` and byte vector, then constructs another output vector containing all canonical CK messages (`crates/mc-module/src/lib.rs:11143-11207`). Native messages are part of the temporary response `Value`; paged handling then clones the final response bytes for redrive (`crates/mc-module/src/lib.rs:8261-8292`). Tail-delta expansion also clones full CK/native prefixes (`crates/mc-module/src/lib.rs:3212-3217`). These are transient, not process-lifetime holders, but repeated high-water allocation of millions of MALLOC_SMALL objects can leave dirty allocator pages resident after Rust drops the trees. A heap profile is required to separate currently reachable cache payloads from allocator retention.

## `Arc` cycle/retention audit

No cycle was found:

- `TransformSnapshotCache` owns `Arc<TransformRequest>`; a `SnapshotLease` owns the request and a budget `Arc`, but neither request nor budget points back to cache/lease (`crates/mc-module/src/lib.rs:1683-1708`).
- `ProjectionCache` owns `Arc<FlatProjection>`; projection states own `Arc<ProjectionState>`, but states do not point to projection/cache (`crates/mc-module/src/lib.rs:2329-2349`, `crates/mc-module/src/ck_wire.rs:84-102`).
- Native chunks own `Arc<Value>` and sidecar owns `Arc<HarnessMessageMeta>`; values/meta have no cache back-reference (`crates/mc-module/src/lib.rs:2094-2120`, `crates/mc-module/src/codec/sidecar.rs:36-45`, `crates/mc-module/src/codec/sidecar.rs:84-109`).
- `ServedMessage` owns message/bytes/fingerprint `Arc`s with no reverse reference (`crates/mc-module/src/transform.rs:136-201`).
- Historian/wrapup/dreamer guards own registry `Arc`s, but registry values do not own guards/tasks; their `Drop` implementations remove matching entries (`crates/mc-module/src/lib.rs:2546-2599`, `crates/mc-module/src/lib.rs:2650-2683`).

There are, however, **eviction-defeating active clones**: snapshot leases, projection snapshots, native snapshots, serialized-output snapshots, and tag-baseline snapshots can keep an evicted payload alive until the current pass/task drops it (`crates/mc-module/src/lib.rs:1841-1872`, `crates/mc-module/src/lib.rs:2384-2400`, `crates/mc-module/src/lib.rs:2219-2240`, `crates/mc-module/src/transform.rs:317-334`, `crates/mc-module/src/transform.rs:6172-6227`). Only transform snapshots have a separate active-lease budget.

## Ranked top five candidates

The arithmetic is illustrative because the incident evidence gives message counts and one ~150 MiB projection, not live cache counters. Ranges deliberately count ownership headers/trees rather than only content. They are intended to prioritize instrumentation/fixes, not claim a precise heap total.

### 1. Expanded raw `TransformRequest` snapshots — **very high confidence**

- **Why:** one full request per ready session, with retained-original CK multiplicity, optional full native `Vec<Value>`, and the tail-delta charge defect (`crates/mc-module/src/lib.rs:1683-1693`, `crates/mc-module/src/lib.rs:3161-3223`, `crates/mc-module/src/lib.rs:7260-7277`; CK ownership at `crates/mc-store/src/lib.rs:84-124`, `crates/mc-store/src/lib.rs:190-218`).
- **Sketch:** if the large session's canonical CK part is 35–50 MiB, three retained CK representations alone are roughly 105–150 MiB; native `Value` arrays and headers can take that entry to roughly 130–220 MiB. Scaling a 700-message neighbor at 0.2–0.35 of the large shape gives another 30–70 MiB: **~160–290 MiB live**, potentially charged as only the newest small deltas.
- **Cheapest structural fix:** add a deep `TransformRequest::retained_bytes()` and charge the **post-expansion object**, not `inbound_bytes`. It must include retained original CK/native values, headers/capacity allowances, outer session/LRU keys, and active leases. A cheaper-memory follow-up is to store a compact reconstruction snapshot (fingerprint + CK/native arrays without duplicated typed/original forms), but correcting admission accounting is the smallest safe change.

### 2. Projection cache — **very high confidence**

- **Why:** the incident already identifies one approximately 150 MiB projection; source confirms one block-heavy `FlatProjection` per cached session and a 256/192 MiB production budget (`crates/mc-module/src/lib.rs:477-483`, `crates/mc-module/src/lib.rs:2329-2434`). The estimator misses tool-input and container/allocator ownership (`crates/mc-module/src/ck_wire.rs:109-171`, `crates/mc-module/src/ck_wire.rs:427-489`).
- **Sketch:** 150 MiB charged for the large session plus approximately 30–60 MiB for a 700-message neighbor is 180–210 MiB charged. A 1.15–1.45 deep-size correction for tool inputs, retained block originals, trees, capacities, and `Arc`/B-tree allocations gives **~210–305 MiB actual**.
- **Cheapest structural fix:** correct `FlatProjection::retained_bytes`: explicitly deep-size the cloned `CkWireBlock` (including original JSON), separately cloned tool input, `ProjectionState` maps/nodes, all capacities, `Arc` headers, and outer cache metadata. Keep the current LRU/caps initially; accurate charging will reject/evict the oversized cases without a behavioral redesign. Add an active-clone budget analogous to snapshot leases if concurrent transforms are allowed.

### 3. Serialized-output cache — **high confidence**

- **Why:** it stores one rendered message tree and canonical buffer per emitted message but charges only the buffer (`crates/mc-module/src/transform.rs:136-201`, `crates/mc-module/src/transform.rs:337-371`, `crates/mc-module/src/transform.rs:9480-9704`). With 2,600+ messages, per-message headers and retained original trees are bulk, and MALLOC_SMALL dominance is exactly the expected signature.
- **Sketch:** for 30–60 MiB combined canonical CK output across two sessions, one canonical buffer plus 2–4 equivalent typed/original/tree-and-header units is approximately **90–300 MiB actual**, while the budget sees only 30–60 MiB. Even at 2,600 entries, key + identity + fingerprint metadata alone is several hundred bytes/message, roughly another 1–3 MiB before payloads.
- **Cheapest structural fix:** charge each `SerializedOutputCacheEntry` by deep message size + canonical bytes + output identity + fingerprints + key/identity strings + bucket/load-factor allowance; charge `served: None` metadata too. Add a per-entry/session cap so one pathological many-message map cannot consume uncharged metadata.

### 4. Native-attachment cache — **medium-high confidence**

- **Why:** the production cap is 256 MiB, and one entry owns both encoded native `Value` chunks and a sidecar carrying raw metadata trees (`crates/mc-module/src/lib.rs:2078-2120`, `crates/mc-module/src/codec/sidecar.rs:36-45`, `crates/mc-module/src/codec/sidecar.rs:84-109`). Its mixed proxy estimator is shape-sensitive (`crates/mc-module/src/lib.rs:2123-2151`, `crates/mc-module/src/lib.rs:10603-10620`).
- **Sketch:** source reports about 49 MiB native representation at 4,600 messages/15,000 blocks (`crates/mc-module/src/lib.rs:2068-2071`). Linear message scaling for 3,300 incident messages gives ~35 MiB before payload-shape differences; encoded tree + sidecar raw trees + headers/capacities can plausibly make **~80–180 MiB**, with tool-heavy/native-rich content higher. The configured 256 MiB permits that amount outright.
- **Cheapest structural fix:** replace `native_value_retained_bytes`/sidecar proxies with one deep-size routine that includes `Value` object/array capacities, key `String` headers, tree nodes, sidecar maps, `Arc` headers, chunks, and outer metadata. Remove or separately label the phantom `served_bytes × 2` term so the metric reports owned bytes; if cross-cache aggregate protection is desired, enforce one shared aggregate budget explicitly.

### 5. Transform-page coordinator (completed response plus possible stranded collector) — **medium confidence, potentially dominant if a collector is present**

- **Why:** every paged session retains its latest full encoded response without byte accounting (`crates/mc-module/src/lib.rs:862-874`, `crates/mc-module/src/lib.rs:8261-8292`). A partial collector has no TTL and holds deserialized `Value` pages under a 128 MiB *serialized* budget (`crates/mc-module/src/lib.rs:843-894`, `crates/mc-module/src/lib.rs:993-1140`).
- **Sketch:** one completed response includes CK bytes and, for native serve, native bytes; **40–130 MiB across two large active session responses** is plausible. If any partial collector is stranded, 128 MiB serialized pages at 2–4× `Value` expansion can add **~256–512 MiB** until discard/route teardown. `oldest_queued_at_ms` exposes whether a collector exists (`crates/mc-module/src/lib.rs:974-984`, `crates/mc-module/src/lib.rs:3117-3127`).
- **Cheapest structural fix:** include completed-result length in a handler-wide completed-replay LRU/budget; deep-charge staged `Value`s; add an actual timer/periodic TTL for collectors; and make `discard` remove the session entry rather than leaving an empty shell. A low-risk first step is a strict per-session completed-result cap plus total completed-result budget.

### Near misses

- **Tag baseline:** can legitimately occupy close to 64 MiB because it retains source bytes for every tag row, but its estimator is comparatively conservative (`crates/mc-module/src/transform.rs:6142-6152`). It is a likely 10–64 MiB contributor, not the first multiplicative gap.
- **State-sync/import staging:** either can multiply a 32 MiB serialized charge into substantially more heap, but only while a partial collector remains. Their lazy TTLs make this possible; active collector metrics would decide quickly (`crates/mc-module/src/lib.rs:825-840`, `crates/mc-module/src/lib.rs:1218-1234`).
- **Unbounded registries/frontiers:** real lifetime growth defects, but two sessions imply only kilobytes to about 1 MiB for the tag frontier and roots/warning/log maps. They do not explain this incident's bulk.
- **In-flight markers:** 4,096 keys/markers are roughly 1 MiB, not hundreds of MiB (`crates/mc-module/src/lib.rs:1683-1686`, `crates/mc-module/src/lib.rs:1725-1735`, `crates/mc-module/src/lib.rs:1768-1793`).

## Overall incident interpretation

A central estimate from the first five holders—roughly 200–250 MiB snapshots + 220–270 MiB projection + 120–200 MiB serialized output + 100–170 MiB native attachment + 50–100 MiB completed page responses—already reaches **~690–990 MiB** before tag/boundary caches, active historian work, stagers, outer metadata, and allocator retention. These holders are mostly **replacement caches**, not pass-count accumulators: repeated passes over the same two session ids refresh one entry each. The 18-hour uptime matters mainly because it allows caches to warm to their largest shapes, detached/partial work to become stranded, unbounded historical-session maps to accumulate (if more than the two currently active sessions were seen), and the allocator to retain high-water MALLOC_SMALL arenas.

Consequently, the cheapest diagnostic sequence before redesign is:

1. expose per-holder charged bytes, entry/session counts, completed-page result bytes, and whether a transform/state-sync/import collector is active;
2. add deep-size telemetry for the two live snapshots/projections/output/native entries without changing admission;
3. take a live allocation profile while idle after a pass to distinguish reachable cache trees from allocator-retained pages; and
4. fix the tail-delta snapshot charge first, then output/projection deep accounting.

Those steps preserve behavior while making the next RSS sample attributable. No code change is proposed or implemented by this audit.

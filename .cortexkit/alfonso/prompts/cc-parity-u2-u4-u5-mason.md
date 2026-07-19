# Task: CC-leg parity — U2 guidance.get + U4 ctx_expand transcripts + U5 ctx_note

Read the design doc first: .alfonso/plans/cc-leg-parity-v1.md (v2, all review findings
folded). You implement units U2, U4, U5 — the units independent of the AIPROXY splice
contract. Do NOT touch healing.rs profile gates, selection.rs, or anything in U1/U3
(tagging/nudges) — those gate on an external deploy.

Repo: ~/Work/Projects/CortexKit/magic-context, branch base = subc-migration HEAD.
Crates only: crates/mc-store, crates/mc-module. No packages/ changes.

## U2 — guidance.get module op

- New request arm alongside health/status/transform in crates/mc-module/src/lib.rs
  dispatch: `{"kind":"guidance.get","session_id":"..."}` →
  `{"ok":true,"bytes":"<full guidance text>","hash":"<sha256 hex of bytes>"}`.
- Guidance text: port the FULL OpenCode guidance block VERBATIM from the TS source —
  find it via packages/plugin/src/hooks/magic-context/system-prompt-hash.ts (which
  composes the injected block) and the prompt constants it pulls in (ctx_reduce §N§
  discipline, drop/keep rules, ctx_note, ctx_memory, ctx_search, ctx_expand guidance,
  reduction guidance). Store as a Rust constant (include_str! from an assets file in
  the crate is fine). The ctx_* tool names stay identical.
- Frozen-date discipline, session-aware: the block ends with the `Today's date:` line.
  Per session, the date is stored in ModuleMeta (new field `guidance_date: String`,
  serde(default)); on first guidance.get for a session it freezes to today; it
  advances ONLY when the module commits a cache-busting pass (HARD or SOFT — wire the
  update where the pass commit already knows its class; defer passes never change it).
  guidance.get is a READ — it must not write anything except the first-freeze (which
  is a plain meta write via the existing commit path; if the session row doesn't
  exist yet, freeze lazily and persist on the first pass commit instead — do NOT
  invent a new write path).
- hash = sha256 over the exact bytes returned (guidance + date line). Consumers cache
  per session; a hash change is a deliberate rare event.

## U4 — ctx_expand over persisted chunk transcripts

- mc-store: new table `mc_chunk_transcripts` (session_id, compartment_seq, start_ordinal,
  end_ordinal, transcript_deflate BLOB, created_at_ms; PK (session_id, compartment_seq)).
  Compression: flate2/deflate (add dep if absent; keep it light — no zstd native dep).
- Persist INSIDE the publish CAS transaction in publish_historian_chunk (Oracle pin:
  pre-CAS writes orphan on conflict, post-CAS writes lose transcripts on crash). The
  transcript bytes = the exact chunk lines the historian prompt was built from — thread
  the rendered chunk text from the assemble/fire path into the publish call (the
  chunk builder output already renders U:/A:/TC: lines; find the seam where the
  producer output and chunk meet publish and carry the transcript alongside).
- Retention: cap per compartment (256KB compressed) and per session (8MB total,
  oldest-first eviction in the same transaction). Evicted/missing spans must produce
  the explicit "no longer recoverable" notice on read.
- Facade tool `ctx_expand` (register alongside ctx_memory/ctx_search in the facade
  arm): args `{start, end}` (ordinals) → resolve compartments whose ranges intersect,
  serve decompressed transcripts concatenated with per-compartment headers; or
  `{message: N}` → the single compartment covering ordinal N with a clearly stated
  truncation caveat (this leg serves the chunk-builder view, not full raw). Output
  shapes mirror the TS tool's text output conventions (readable text, not JSON).
- Scope: facade calls resolve session via the same binding/scope machinery as
  ctx_memory (FacadeScope conversation lane).

## U5 — ctx_note in mc-store

- mc-store: new table `mc_notes` (id INTEGER PK AUTOINCREMENT, project_path,
  session_id, content, status TEXT active|dismissed, surface_condition TEXT NULL,
  anchor_block_id TEXT NULL, created_at_ms, updated_at_ms).
- Facade tool `ctx_note`: actions write (content, optional surface_condition —
  stored inert v1, tool output says evaluation arrives later), read (paginated,
  default 25, newest first, active + inert-condition notes), update (note_id,
  content), dismiss (note_id, optional resolution appended to content). Mirror the
  TS tool's parameter names exactly (action, content, note_id, limit, offset,
  surface_condition) so agent-facing schemas match OpenCode.
- anchor_block_id = newest live non-synthetic flat block id at write time when the
  binding has one (best-effort; null is fine from a facade call with no live pass).
- ctx_search notes source: extend the facade ctx_search's lexical search to include
  mc_notes (status-labelled, same result formatting conventions as its memory hits).

## Tests (all three units)

- guidance.get: bytes stable across repeated calls same session; date frozen (mock
  clock or inject); hash matches sha256; date advances only via busting-pass commit
  (drive a HARD then re-read); different sessions freeze independently.
- Transcripts: publish persists in-transaction (CAS conflict leaves NO transcript row
  — simulate the conflict the existing publish tests use); eviction caps enforced;
  expand range serves correct decompressed text; missing span notice; message=N
  caveat present.
- Notes: CRUD + pagination + dismiss resolution; ctx_search returns note hits;
  facade scope isolation (two sessions, notes don't bleed across projects).
- Existing suites stay green: cargo test -p mc-store -p mc-module --lib, real_daemon,
  clippy -D warnings, fmt. check_comments before commit.

## Rules

- Comments explain invariants (why in-CAS-transaction, why date freezes, why the
  truncation caveat) without referencing this plan or review process.
- Commit trailer: Co-authored-by: Alfonso [Magic Context] <288211368+alfonso-magic-context@users.noreply.github.com>
- If the guidance-text port hits ambiguity about which TS blocks compose the full
  guidance (there are conditional variants), STOP and report the exact variants
  found rather than guessing — the requirement is the FULL block as a primary
  OpenCode session with all five tools available would receive it.

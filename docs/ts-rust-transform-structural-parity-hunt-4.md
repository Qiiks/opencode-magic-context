# TS ↔ Rust transform structural parity hunt #4

## Method and denominator

Served provider bytes and durable decision/store rows remain the only production ground truth. A project name or session label is not lane evidence. `scripts/audit-transform-wire-parity.py` now reads `<project>/.cortexkit/magic-context.jsonc` for every project root extracted from served system bytes before admitting a dump to either denominator. An absent `transform_mode` means the TypeScript default; an unreadable config excludes the dump. The built-in expected-Rust assertions contain ASTROCYTE and ENGRAM only. SUBCONSCIOUS is not in that set: its intentionally empty config resolves to TypeScript.

The differ also now emits:

- matched `ctx_expand` / `ctx_note` / `ctx_search` request classes, output hashes and unexplained byte classes;
- compartments born in the selected window, tier completeness, importance, durable date fields and samples;
- promoted historian facts and side-effect/outbox rows;
- every live table with an exact `session_id` column, which is the lifecycle-delete coverage domain; and
- configured-versus-observed authority alongside the existing decision, geometry, reminder and wire-shape evidence.

Reproduce the production audit on the host that owns the dumps and databases:

```sh
python3 scripts/audit-transform-wire-parity.py \
  "$TMPDIR/opencode-anthropic-auth-dumps" \
  --date 2026-08-27 --per-session 1000 \
  --context-db "$HOME/.local/share/cortexkit/magic-context/context.db" \
  --store-db "$HOME/.local/share/cortexkit/magic-context/store.db"
```

No production dumps or user databases are versioned in this repository. Therefore this change does not invent “this week” row counts. `scripts/audit-transform-wire-parity.test.py` executes the same lane, facade, historian-row and lifecycle inventory paths against two synthetic served wires and two real SQLite files; production counts remain an explicit post-deploy evidence step.

## Findings and fixes

### F1 — fixed: Rust `ctx_expand` did not render the TypeScript tool facade

A completed OpenCode tool is one TypeScript part containing input and output. The CK encoder represents it as adjacent `tool_call` and `tool_result` blocks. Rust rendered those as two tool sections in full recovery and two bullets in verbose recovery; it also exposed `step-start` and called file parts `media`. The served tool result therefore differed despite carrying the same underlying message.

The TypeScript-authority generator `crates/mc-module/gen/gen-ctx-facade-golden.ts` now records production CK encoder output and the exact TypeScript full/verbose renderer bytes in `crates/mc-module/testdata/ctx-facade-golden.json`. `ctx_expand_renderers_match_typescript_facade_golden` consumes that fixture in Rust. Rust now coalesces an adjacent matching call/result, suppresses structural step markers, and renders both opaque OpenCode files and CK media as `[file]` (`crates/mc-module/src/lib.rs`, `render_cached_message_expand` and `render_verbose_expand_message`).

The Rust facade also accepted ordinal zero. TypeScript’s own error copy documented “positive integers”, but its provider schema accepted any number and range execution accepted fractions. That was a TypeScript violation of its documented invariant, so both seams were corrected: TypeScript now advertises integer/minimum-1 arguments and explicitly rejects zero/fractions; Rust uses the same minimum and errors. The prior Rust test’s contract changed deliberately: `ctx_expand_accepts_native_ordinal_zero_in_message_and_range_forms` became `ctx_expand_rejects_ordinal_zero_like_the_typescript_facade`, with TypeScript validation cases in `packages/plugin/src/tools/ctx-expand/tools.test.ts`. Native zero-based history remains valid inside boundary machinery; this change is only the public `ctx_expand` contract.

### F2 — structural: `ctx_search` is not the TypeScript tool

The TypeScript tool searches and ranks memories, hidden raw messages, compartments, notes, Primers and optional git commits, supports a `sources` filter and direct memory-id lookup, and renders prose with scores, ordinals/ranges and expansion hints (`packages/plugin/src/tools/ctx-search/tools.ts:132-175,238-294`; result variants in `packages/plugin/src/features/magic-context/search.ts:140-211`).

The module facade searches only memories, compartment title/body rows and notes, ignores `sources`, and returns a compact JSON array with module-specific source names and no score (`crates/mc-module/src/lib.rs:10614-10669`). This is user-visible behavior, not merely a different implementation behind equal bytes.

**Brief:** choose one public contract. The lowest-risk route is to make the module return the TypeScript `UnifiedSearchResult` vocabulary and run the same formatter, while adding module indexes for message, Primer and git-commit sources (or explicitly route `ctx_search` to TypeScript until all sources exist). Acceptance requires one shared fixture per source, source filtering, numeric-id lookup, live-tail exclusion, visible-memory exclusion, ranking ties, no-results copy and byte-identical rendered text.

### F3 — structural: completed-tool descriptions are lost before the module facade

TypeScript full recovery includes `state.title` / metadata title as a `description:` line. The production CK encoder intentionally projects only tool id, name, input and output (`packages/plugin/src/hooks/magic-context/module-wire.ts:695-727`), so the module’s cached and durable CK messages cannot reconstruct a title. The new golden omits title rather than blessing fabricated parity.

**Brief:** retain original native tool metadata in a historian recovery sidecar, or add a non-decision-bearing native transcript payload. Do not add title to decision fingerprints. Acceptance is a TypeScript-generated completed-tool fixture with `state.title` whose immediate and post-restart module `ctx_expand(message=N)` bytes exactly match TypeScript.

### F4 — structural policy difference: historian event publication durability

Both lanes atomically publish compartments, coverage and marker-deferral state. Both gate fact promotion on memory plus auto-promote; Rust’s end-to-end gate is pinned by `publish_gates_facts_when_memory_or_auto_promote_is_off` (`crates/mc-module/src/historian.rs:3853-3928`), while TypeScript applies the two gates and promotion in its publish transaction (`compartment-runner-incremental.ts:590-685`).

Event policy is not the same. TypeScript catches event insertion failure inside publication and commits the compartment anyway because events are declared best-effort/re-derivable (`compartment-runner-incremental.ts:687-701`). Rust atomically enqueues every accepted event/Primer/user-observation beside the compartment CAS and retries delivery from `mc_historian_side_channel_outbox` (`crates/mc-store/src/lib.rs`, migration 40 and `publish_historian_chunk`). Today’s provider bytes can agree while one lane retains an event after a transient write/delivery failure and the other does not.

**Brief:** document one durability contract. If side effects matter to future dreamer aggregation, give TypeScript an outbox with the Rust retry semantics. If they are intentionally lossy, simplify Rust and assert that policy. Acceptance is a fault-injected matched publish where compartment, coverage, facts, marker and each side-channel disposition are compared after restart.

Date storage differs without establishing a served-byte bug: Rust compartments durably store `start_date`/`end_date`; TypeScript derives date ranges from raw message timestamps when rendering and has no date columns. The differ reports that distinction explicitly instead of treating absent TypeScript columns as missing rows.

## Per-axis verdicts

### A. Historian trigger, prompt and publication

**Trigger and prompt verdict: pass on matched fixtures.** `boundary-golden.json` is generated from TypeScript and pins constants, protected-tail resolution, true raw eligible tokens, oversize atomic units and trigger fire/reason/coverage. Rust checks are `boundary_constants_match_ts_sources`, `boundary_golden_matches_ts_resolution` and `trigger_golden_matches_ts_decision_core` (`crates/mc-module/src/boundary.rs:2175-2355`). Chunk formatting/budget behavior is independently generated from production TypeScript (`gen-historian-chunk-golden.ts:59-330`) and consumed by `historian_chunk_golden_fixture_matches_builder`. Producer prompt bytes, seed selection, session references, memory-on/off and extraction-free shape are exact in `historian_prompt_golden_matches_typescript_reference` (`historian_prompt.rs:483-584`).

**Publication verdict: compartments/facts/coverage pass; event durability differs as F4.** Live-week row counts are an evidence gap until the host command above runs.

### B. Wrapup and lifecycle operations

**Wrapup verdict: semantic pass by source mapping; shared matched-state fixture missing.** TypeScript outcomes map as follows: no runnable window → done/nothing; existing run or lease owner → skipped; any stop after admission → partial with “run again”; full drain → done (`wrapup-orchestrator.ts:269-527`). Module dispositions are `nothing_to_compact`, `already_in_progress`, `retryable` and `completed`; the host maps them to the same four user outcomes (`command-handler.ts:218-241`). The headings/summary prose are not byte-identical and are not claimed as such.

**Lifecycle verdict: pass.** TypeScript’s `SESSION_SCOPED_TABLES` is checked against every live schema table containing exact `session_id`, then `clearSession` is tested to empty all of them (`storage-db.test.ts:333-390`). Rust `session.delete` discovers that same ownership shape dynamically and deletes every table with exact `session_id`, preserving project-owned smart notes (`crates/mc-store/src/lib.rs:6841-6887,17183-17253`). The extended differ prints both live inventories.

**Fixture brief:** generate one shared wrapup state matrix covering empty, active, lease timeout, zero progress, partial progress, producer failure, ownership loss and success. Compare normalized outcome, progress, retryability, keep watermark, coverage and durable command replay.

### C. `ctx_*` facades

- `ctx_expand`: **fixed/pass** for deterministic full and verbose fixtures; title preservation remains F3. Default historian transcript slicing remains covered by module tests.
- `ctx_note`: **source-shape pass, cross-language golden gap**. Plain write/read/update/dismiss and smart-note capability gating exist in module facade tests, but no shared TypeScript-rendered byte corpus exists. Add it when note copy changes.
- `ctx_search`: **structural fail**, F2.

The differ now hashes actual served outputs for matched facade inputs after removing only the leading transform tag and temporal carrier. Different output sets land in `unexplained_byte_classes`; lane-only calls remain evidence gaps rather than false failures.

### D. LKG and failure recovery

**Verdict: documented policy alignment, shared fault-matrix gap.** On module failure below the trusted emergency wall, Rust validates and serves LKG, otherwise bounded raw input; raw input that exceeds a known wall is refused. At or above 95% or under provider-proven overflow recovery, it fails closed before LKG (`rust-mode-transform.ts:1408-1499,1679-1727,2845-2887`). A TypeScript session-meta storage read failure intentionally returns the untouched raw array (`transform.ts:724-731`); later thrown transform failures use the shared outer LKG seam. Thus “storage unavailable before state exists” is raw fail-open in both lanes, while “Rust authority unavailable with valid state” gains an LKG rung that has no TS-authority analogue.

**Fixture brief:** table-drive both modes through metadata read failure, overflow-state read failure, transform/module failure, invalid LKG seam, oversized LKG, oversized raw, 94%, 95% and provider-overflow arm. Assert served source and throw/return disposition, not logs alone.

### E. Channel-1 `{U,T}` math

**Verdict: exact pass.** The TypeScript-generated `nudge-hygiene-golden.json` covers tagged text, full call/result arcs, queued drops, protected exemplars, synthetic rows, reasoning exclusions, media and band edges. Rust previously allowed a 3%/12-token tolerance even though the shared tokenizer now produces identical values. The test now requires exact U and T for every case and still requires exact band selection (`tail_hygiene.rs:1234-1331`). Existing mutant checks prove reasoning changes neither term while tagged visible text changes the measurement.

### F. Boundary and geometry

**Verdict: pass.** Boundary constants and matched outcomes are exact in the TypeScript-generated boundary golden cited above. The host derives geometry once and transports `usable_soft`/`usable_hard` unchanged; tests pin 255,616/368,000 shared-upfront and 128,000/168,000 separate-window cases (`rust-mode-transform.test.ts:358-397`). Rust uses soft for scheduler/historian denominators and hard for emergency walls (`transform.rs:5792-5835`); first-pass historian fallback to soft is pinned in `lib.rs:14854-14887`.

### G. Differ unexplained-byte classes

**Verdict: machinery classes added; production bucket pending host artifacts.** The hermetic differ test produces one byte-equal matched `ctx_expand` class and no unexplained class while proving config-derived lane correction, historian tier/date/fact rows and both lifecycle inventories. Production output is deliberately not asserted without production files.

## Honest-empty declaration

Hunt #4 is **not empty**: it lands two `ctx_expand` behavior corrections and exact Channel-1 math enforcement, and records three concrete structural follow-ups (`ctx_search`, tool-title recovery, and historian event durability). No master push is part of this work.

## Post-delivery correction (review): ctx_expand ordinal domains deliberately differ

The delivered unification of the ctx_expand ordinal domain to positive integers
(schema minimum 1 + runtime <=0 rejects, both legs) was reverted at merge. The
missing evidence: Claude Code chunk transcripts store 0-BASED ordinals (the D5
drive sessions pinned `ordinals 0..17` in the module store), so a minimum of 1
makes a CC session's first message permanently unexpandable. The correct
contract, now pinned by `ctx_expand_accepts_ordinal_zero_because_cc_transcripts_are_zero_based`:
the module facade accepts ordinal 0 (CC's space), the TypeScript facade keeps
rejecting it (OpenCode/Pi transcripts are 1-based) — same-schema-everywhere is
false parity when the underlying ordinal spaces differ. The advertised-schema
byte changes were additionally release-window material (cache surface on both
legs) and are dropped rather than deferred: the runtime integer tightening on
the TS side (Number.isInteger checks, no wire bytes) is kept.

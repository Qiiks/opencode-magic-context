# Fix: temporal-awareness date attributes lost in the v2 decay renderer

Repo: subc-migration branch (HEAD 9abac3b9). A v2-cutover regression: with `temporal_awareness` enabled, rendered compartments are supposed to carry `start-date="YYYY-MM-DD" end-date="YYYY-MM-DD"` attributes (the v1 renderer `buildCompartmentBlock` in `packages/plugin/src/features/magic-context/compartment-storage.ts:440` still does this via `CompartmentDateRanges`), but the v2 decay renderer (`packages/plugin/src/hooks/magic-context/decay-render.ts` `renderOneCompartment`) renders only `start/end/title` — dates were never carried over. Same gap in the Rust mirror (`crates/mc-module/src/decay_render.rs`). Fix all legs so the byte-shape stays identical across TS and Rust (the shadow-mode byte-compare depends on it).

## Unit 1 — shared TS renderer
`decay-render.ts`:
- `DecayRenderCompartment` gains optional `startDate?: string | null` and `endDate?: string | null` (pre-formatted `YYYY-MM-DD`).
- `renderOneCompartment` renders ` start-date="…" end-date="…"` (both attrs, only when BOTH present) into `baseAttrs`, positioned AFTER `end` and BEFORE `title` — matching the v1 attribute order in compartment-storage.ts:440-442 so downstream regexes (`tag-content-primitives.ts` mentions the attr shape) and the dashboard parser see one canonical order. Applies to ALL render arms (tiered, legacy/flat, self-close `<compartment … />`).
- Dates are part of the rendered bytes — they participate in the budget guard measurement naturally (no special-casing).

## Unit 2 — OpenCode injection wiring
`packages/plugin/src/hooks/magic-context/inject-compartments.ts`: the v2 path that builds `DecayRenderCompartment[]` (find where compartments map into the decay renderer for m[0] and m[1]) must populate startDate/endDate when `temporalAwareness` is on, using the SAME mechanism the v1 path uses at lines ~367-385: batch `getMessageTimesFromOpenCodeDb(sessionId, ids)` over each compartment's startMessageId/endMessageId, `formatDate(ms)`. Notes:
- Do it ONCE per render (batched query), not per compartment.
- Missing times (message deleted/compacted from opencode.db, or Pi-only install with no opencode.db) → omit both attrs for that compartment. Never partial.
- CACHE DISCIPLINE: the new bytes must only appear where fresh renders already happen (materializeM0 / renderM1 on bust passes). Do NOT add any new render or invalidation trigger — a defer pass replays cached bytes and must stay byte-identical. Read the mc:protected section of ARCHITECTURE.md first; violating replay byte-identity is the one unforgivable failure here.
- The temporal_awareness flag reaches the hook config (`temporal_awareness` field). Verify how the v1 path receives `temporalAwareness` and thread the same way.

## Unit 3 — Pi parity
`packages/pi-plugin/src/inject-compartments-pi.ts` (and its date source — Pi has no opencode.db; check how Pi's v1 path resolved dates, if it ever did: search pi-plugin for CompartmentDateRanges / getMessageTimes / temporal). If Pi's v1 never had compartment dates (only OpenCode did), then Pi keeps rendering without dates and you DOCUMENT that in PARITY.md as an existing divergence — do not invent a new date source for Pi in this fix. If Pi did have them, restore identically.

## Unit 4 — Rust decay renderer + shadow wire
- `crates/mc-store`: `StoredCompartment` gains nullable `start_date`/`end_date` TEXT columns (new store migration — follow the existing migration pattern in mc-store; bump its schema version per the crate's convention).
- Shadow wire: `ShadowCompartmentWire` (crates/mc-module/src/lib.rs) gains optional `start_date`/`end_date` (serde default); TS `serializeCompartment` (shadow-sender.ts) sends them (computed from the same batched time lookup — the shadow sender already reads raw messages; reuse `rawById` which it has, mapping message time fields; check what RawMessage carries for created-time and mirror formatDate exactly).
- `crates/mc-module/src/decay_render.rs`: render the attrs byte-identically to the TS renderer (same order, same omit-when-either-missing rule). Update the differential golden: the TS reference generator for decay-render golden lives at crates/mc-core or mc-module testdata (find gen-golden for decay render — `crates/mc-core/testdata/` per STRUCTURE; regenerate fixtures so TS and Rust agree, including at least one case WITH dates and one without).
- Shadow fixture: regenerate `crates/mc-module/testdata/shadow-wire-fixture.json` via `packages/plugin/scripts/generate-shadow-wire-fixture.ts` including the new fields; update the strict Rust mirror structs in the fixture test.
- Native CC-leg publish (the Rust historian's own compartments): the module has no message-timestamp source today — leave publish writing NULL dates and add a code comment noting dates currently flow only via shadow sync; do NOT invent a timestamp source.

## Tests
- decay-render.ts unit: with dates → attrs present in tiered, legacy, and self-close arms in canonical order; without → absent; partial (start only) → absent.
- inject-compartments: temporal_awareness on → rendered m0 block carries date attrs (use the existing test harness patterns in inject-compartments tests; check how temporalAwareness is passed there); off → no attrs; byte-identity on defer replay unaffected (there are existing cache-invariant tests — make sure they still pass, and if none covers a date-bearing render, add one asserting two consecutive defer passes replay identical bytes with dates present).
- Rust: differential golden with dates matches TS byte-for-byte; store round-trip of start_date/end_date; shadow state_sync carrying dates lands them in the store and the composed m0 renders them.

## Gates
bun test packages/plugin + pi-plugin, tsc both, lint; cargo test -p mc-module -p mc-store, clippy -D warnings, fmt --check; real_daemon test (binary name ck-mc); fixture round-trips green. Commit per unit, co-author trailer:
Co-authored-by: Alfonso <alfonso-magic-context@users.noreply.github.com>
Comments explain rationale for context-free readers (no "Unit N" / audit references).

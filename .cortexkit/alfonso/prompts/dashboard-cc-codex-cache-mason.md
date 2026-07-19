# Task: Dashboard cache page — Claude Code + Codex harness support

Add Claude Code and Codex sessions to the dashboard's Cache Diagnostics page.
Scope: cache page ONLY (session cache stats + per-session cache events + timeline).
No SessionViewer support, no cause enrichment for the new harnesses (their
`transform_decisions` lookup is simply absent — the existing Option-shaped plumbing
already tolerates that).

You own `packages/dashboard/` only (Rust src-tauri + frontend). Do not touch crates/
or packages/plugin/.

## Ground truth (verified live on this machine — trust these shapes)

### Claude Code store
`~/.claude/projects/<path-slug>/<session-uuid>.jsonl`, one JSON object per line.
Assistant rows: `type == "assistant"`, with:
- `message.usage.input_tokens` (NON-cached input — same semantics as OpenCode's
  `tokens.input`), `message.usage.cache_read_input_tokens` (→ cache_read),
  `message.usage.cache_creation_input_tokens` (→ cache_write),
  `message.usage.output_tokens`
- `uuid` (stable per-message id), `sessionId`, `timestamp` (ISO 8601), `cwd`,
  `isSidechain` (bool), `message.model`
- total_tokens: compute input + cache_read + cache_write + output.
Rules: skip rows with `isSidechain == true` (subagent sidechains interleave separate
conversations and would pollute retention analysis); skip rows with zero usage.

### Codex store
`~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl`.
- `session_meta` line (first): carries session id + cwd (verify exact field names at
  source when parsing; the id is also in the filename).
- `event_msg` lines with `payload.type == "token_count"`:
  `payload.info.last_token_usage.{input_tokens, cached_input_tokens, output_tokens,
  total_tokens}` + `payload.info.model_context_window` + top-level `timestamp` (ISO).
SEMANTIC NORMALIZATION (critical): Codex `input_tokens` INCLUDES the cached portion.
Map: cache_read = cached_input_tokens; input_tokens(event) = input_tokens −
cached_input_tokens; cache_write = 0 ALWAYS (OpenAI implicit caching has no write
signal). Synthesize message_id as `<session>-tc-<line_index>` (no per-event id).
`model_context_window` gives the context limit directly — feed it into the existing
per-event limit field the timeline uses for segmentation (flag NOT estimated).

## Managed vs unmanaged (the product core of this feature)

A CC session is MANAGED when the MC Rust module has durable state for it:
read-only open of `<XDG_DATA_HOME>/cortexkit/magic-context/store.db` (default
`~/.local/share/cortexkit/magic-context/store.db`; note this is the SAME directory as
context.db — different file), table `mc_cache_state`, column `session_id`. Match rule
(verified against the live prod store): managed iff a row's session_id equals the CC
JSONL sessionId exactly, OR starts with `<sessionId>` + the U+241F separator char
(composite subagent keys fold `(wire_sid, agent_id, epoch)` with U+241F, wire sid
always leftmost). Use one query with `= ?1 OR (session_id > ?1 || char(0x241F) AND
session_id < ?1 || char(0x2420))`-style range or a LIKE with escaped literal — your
call, but no full-table scan per session; batch the check for the window's sessions.
Graceful degradation: store.db absent / table absent (pre-shadow builds) / locked →
every session is unmanaged, no error. Codex sessions: always unmanaged today (no
codex MITM leg live); same detection code path must still run so it lights up later.

## Filter UX
Default: cache page shows MANAGED sessions only for the CC/Codex harnesses (OpenCode
and Pi sessions are always shown — they are managed by definition via the plugin).
A visible toggle ("Show unmanaged sessions" or similar, your wording — concise) flips
to all; persist the choice in the existing UI-preferences mechanism the dashboard
already uses for view state. Managed CC sessions get a small badge ("Managed") in the
session strip/card; when managed-only is on and there are zero managed CC/Codex
sessions, the CC/Codex entries simply don't appear (no empty-state banner needed).

## Retention predictor
- CC: Anthropic semantics identical to OpenCode — reuse the existing predictor
  UNCHANGED.
- Codex: no-write variant — never expect cache_write; retention/severity from
  read-ratio only (cache_read / (cache_read + input)); the "first event = expected
  cold write" heuristic must not mark Codex first events as misses-with-missing-write.
  Look at how the predictor consumes cache_write and add the minimal variant switch on
  harness, not a parallel predictor.

## Wiring inventory (follow existing per-harness patterns — Pi is the template)
- Rust: `Harness` enum + FromStr/labels; new readers alongside the Pi JSONL reader;
  integrate into `get_session_cache_stats_from_db` (top-10 windows) and
  `get_session_cache_events` (per-session last-N + since_timestamp incremental with
  the 1-event overlap dedup contract); first-message detection for the new harnesses
  follows the Pi pattern (JSONL-derived), not the OC message-table path.
- transform_decisions cause lookup: keyed by (Harness, session, message) — new
  harnesses simply have no entries; verify the join degrades to None cleanly.
- Frontend: harness type unions in api.ts / components, harness label/badge in the
  session strip and timeline toolbar, the managed filter toggle, Managed badge.
- `--serve` mode: the new commands ride the existing invoke dispatch — confirm the
  serve dispatch table includes any NEW command names you add (prefer extending
  existing commands with fields over new commands).

## Tests
- Rust unit tests with fixture JSONL files for both formats (use the REAL shapes from
  this brief; include a CC sidechain row that must be skipped, a CC session with
  cache_creation split, a Codex token_count with cached_input_tokens > 0 asserting the
  input normalization and cache_write==0).
- Managed detection: fixture store.db with an exact-match row, a composite-key row
  (U+241F), and a non-matching row; absent-file degradation test.
- Predictor: Codex no-write variant (first event not a warning; read-ratio severity).
- Frontend: build + tsc + biome (the dashboard has no component test harness — do not
  invent one).

## Gates
cargo test (src-tauri), cargo clippy -D warnings, frontend build + tsc + biome, full
`bun run check-dashboard` if present at repo root. check_comments before commit.

## Rules
- Base: subc-migration HEAD.
- Comments explain semantics (Codex input-includes-cached, sidechain exclusion,
  managed-key shape) without narrating process.
- Commit trailer: Co-authored-by: Alfonso [Magic Context] <288211368+alfonso-magic-context@users.noreply.github.com>
- If a live-shape assumption in this brief contradicts what you find in real files,
  STOP and report rather than improvising a parser.

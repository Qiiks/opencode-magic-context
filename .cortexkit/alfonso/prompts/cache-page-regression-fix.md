# Fix: cache page load/CPU regression — remove the full-DB aggregate scan from the hot path

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration, packages/dashboard (Rust src-tauri + frontend). PROVEN root cause (profiled, not guessed — do not re-litigate, but verify at source as you work):

The CC/Codex integration changed CacheDiagnostics.tsx from listSessions() (indexed session-table list, ~ms) to getSessionCacheStatsFromDb(50) for the session list, AND the 1s reconcile tick re-calls it every second. That function's OpenCode leg (db.rs load_raw_db_cache_events(200, None)) is a full-table scan: json_extract('$.role') over ALL 453k message rows (165k qualify) with a ROW_NUMBER window — measured 4.3s cold / 3.7s WARM in RELEASE build (SQLite re-parses every JSON blob per run; no index applies because the predicate lives inside JSON). Result: ~6s first paint + 77% sustained CPU from the tick. The released v0.9.3 page loads in ~1s against the SAME database because it never calls this function.

Measurement tool (committed at src-tauri/src/bin/profile_cache_page.rs — keep it, it's the durable diagnostic): 
  cargo run --release --bin profile_cache_page              # full page sequence, cold + tick
  cargo run --release --bin profile_cache_page -- --repeat  # + warm pass
  cargo run --release --bin profile_cache_page -- --components  # per-loader breakdown
Current numbers on this machine: [1] stats 12.0s cold / 10.4s warm; components: opencode 4.3s/3.7s, pi 2.0s/0.24s, cc+codex 0.14s/0.001s. Your gate: [1] < 300ms cold, tick stats < 100ms warm, full cold wall < 1.5s — measured with this bin, before/after table in your report.

## Fix shape (0.9.3's shape, kept harness-aware)

REPLACE the stats function's row-scan basis for the LIST use-case. The page needs, per session: harness, session_id, last_activity_ms, is_subagent, title, managed — plus aggregate display fields (event_count, totals, hit_ratio, bust_count).

1. Session LIST from cheap sources:
   - OpenCode: the indexed `session` table (what 0.9.3's listSessions used — see list_sessions in db.rs) or an index-friendly newest-N query on message(session_id,time_created) WITHOUT json_extract predicates. NO full-table json scan.
   - Pi / CC / Codex: the meta scans (pi_sessions::scan_pi_session_dir, external scan_*) — already mtime-cached, carry modified timestamps; CC/Codex metas are ~0 warm after the perf fix.
   - managed flags: load_managed_cache_sessions (the CTE against mc_cache_state) — cheap, keep as-is.
   - titles/subagent flags: existing loaders keyed on the final small key set, keep.
2. AGGREGATES: compute per-session display aggregates from a BOUNDED per-session window (e.g. the newest 200 events of the ~10 recent sessions actually shown — which get_session_cache_events already fetches index-friendly per session with a LIMIT), NOT from a global 200-per-session scan across all history of all sessions. Two acceptable designs — pick one and justify: (a) stats list carries only cheap fields + lazy aggregates computed per displayed session from its window fetch (frontend already holds the windows; may need a small api addition), or (b) backend computes aggregates only for the top ~15 sessions by last-activity using per-session LIMIT queries (each is ms — the profiler's [2] step proves 15-60ms/session even on giant sessions). Preserve the wire shape of SessionCacheStats if (b), so the frontend change is minimal.
3. The 1s tick: whatever you pick, the per-tick cost must be the cheap list + per-session since-queries for ADVANCED sessions only (the tick already does the latter correctly). No full aggregate recompute per tick.
4. since_timestamp variant of load_raw_db_cache_events (used by incremental window updates) is index-friendlier (time floor) but still json-scans within the floor — verify its per-tick cost via the profiler [3] step (currently 11-290ms per advanced session; the 290ms ones are giant sessions — if trivially improvable with the same time-floor + LIMIT pattern, do it; do not restructure beyond that).
5. get_cache_events_from_db (the merged timeline path, db.rs:1731) has the same full-scan basis — check its call sites: if it's on the page's hot path, apply the same treatment; if it only serves a one-shot view, add a time-floor (e.g. last 7 days) and note it.

## Constraints
- Behavior parity: same sessions in the list (newest-first by activity), same managed/subagent filtering semantics, hit_ratio/bust_count over the SAME window definition the UI documents (if the aggregate basis changes from all-history-200 to displayed-window, that's FINE — the UI shows recent-window stats — but say so in the report and keep it consistent between list and chart).
- Keep the profiler bin working (adjust it if signatures change; it's part of the deliverable).
- The loaders made pub for profiling (load_raw_db_cache_events, load_raw_pi_cache_events, load_raw_external_cache_events_parallel, load_cache_session_titles, load_cache_subagent_flags, RawDbCacheEvent) stay pub.
- Do NOT add new crates/deps.

## Gates
cargo test (all existing + new: a test proving the list path executes no full-table json scan — e.g. assert the new list SQL carries a time floor or LIMIT via EXPLAIN QUERY PLAN check or by construction), clippy --all-targets -D warnings, fmt, tsc + frontend build if TS changes. check_comments clean. Report: profiler before/after table (cold wall, [1], [3] tick), the aggregate-basis decision, and any semantics delta.

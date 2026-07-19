# Dashboard: cache page extremely slow after CC/Codex external-session integration

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration, packages/dashboard (Rust backend src-tauri + frontend). User-reported: the Cache Diagnostics page became extremely slow after external (Claude Code / Codex) session support landed. Investigate with measurements FIRST (add temporary timing eprintln or a bench test over the real dirs), then fix. Real-world scale on the affected machine: ~/.claude/projects = 24MB across 33 project dirs (many JSONL each), ~/.codex/sessions = 2,026 files / 7.4MB.

## Traced hot-path problems (verify each at source; I read the code — citations)

The frontend cache page runs a 1s reconcile loop per session window plus initial top-10 loads (SessionWindow model). Every tick that touches external harnesses hits db.rs::load_raw_external_cache_events (db.rs:1042-1080), which:

1. Calls scan() — scan_claude_code_session_dir/scan_codex_session_dir (external_cache_sessions.rs:91-151) → scan_jsonl_files WALKS THE ENTIRE TREE (:456-485) on EVERY call, then read_cached_meta for EVERY file. The meta reader for CC is read_claude_code_session_detail_uncached(...).map(|d| d.meta) (:223-228) — i.e. a full-file parse on first touch, cached by mtime after. The walk + per-file stat runs every tick regardless.
2. For EVERY session meta, calls read_detail (cached by mtime) — but read_cached_detail RETURNS (*detail).clone() (:202-221): a DEEP CLONE of the full event vector for every session on every tick. 2,000+ sessions × all their events × 1Hz = the likely dominant cost, even with a warm cache.
3. since_timestamp filtering happens AFTER the detail read (db.rs:1057-1065) — an incremental poll still deep-clones every session's full history just to filter almost all of it away.
4. db.rs:1841-1850 (project cards / session listing path) also iterates the full scans — check its call frequency too.

## Fix shape (verify + measure, then implement; keep behavior identical)

A. INCREMENTAL SKIP BY MTIME: when since_timestamp is provided, skip any file whose mtime (already stat'd in scan) is <= since_timestamp — an unchanged file cannot contain newer events. This turns the 1s tick into stat-walk-only for idle sessions. (Clock-skew guard: compare against mtime + small slack, e.g. 2s.)
B. STOP DEEP-CLONING: return Arc<JsonlSessionDetail> from read_cached_detail (or expose an events-iterator API) so warm-cache reads are refcount bumps, not vector clones. Adjust the two consumers (db.rs:1054+, 2185/2203 detail endpoints).
C. BOUND THE SCAN: cache the directory listing itself for a short TTL (e.g. 5s) so the full-tree walk + N stats doesn't run at 1Hz; the mtime cache keys stay per-file. Newest-first ordering can also let the meta pass early-exit once metas are older than the window's needs — evaluate whether the callers actually need ALL sessions per tick (the events path truncates to limit*10 globally sorted newest-first; a newest-mtime pre-sort + early cutoff at global_cap candidates is safe because events can't be newer than their file's mtime).
D. If measurements show the FIRST page-load parse (24MB cold) is also user-visible: parse off the main thread / parallelize with rayon if already a dep (do NOT add heavy deps for this).

## Gates
Measure before/after on the real dirs (report ms per tick cold + warm at the user's scale, and page-load feel). cargo test in src-tauri green (existing external_cache_sessions tests + add: mtime-skip correctness — a file with mtime <= since yields zero events but reappears when touched; Arc-cache identity — warm read does not clone (assert via Arc::strong_count or pointer equality)). clippy -D warnings, fmt, tsc + frontend build if any TS changes (likely none). check_comments clean. Keep the module's test seams (test roots) working.

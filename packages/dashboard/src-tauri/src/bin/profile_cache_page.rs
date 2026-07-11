//! Replicates the cache page's cold-start invoke sequence with per-step timing,
//! so a slow first load can be attributed to a specific backend step instead of
//! guessed at. Run against the real data on this machine:
//!
//!   cargo run --release --bin profile_cache_page          # release timings
//!   cargo run --bin profile_cache_page                    # debug timings (dev mode)
//!   cargo run --release --bin profile_cache_page -- --repeat  # adds a warm second pass
//!
//! Sequence mirrored from CacheDiagnostics.tsx onMount + one reconcile tick:
//!   1. get_session_cache_stats_from_db(50, false)   — the session list
//!   2. per recent session (top 10 + managed filter): get_session_cache_events(harness, sid, 60)
//!   3. one reconcile tick: stats re-list + incremental event fetches (since=lastSeen)
//! Timings print per step and per session so the dominant cost is unambiguous.

use std::time::Instant;

use magic_context_dashboard_lib::db;
use magic_context_dashboard_lib::db::Harness;

const CACHE_STATS_FETCH_LIMIT: usize = 50;
const RECENT_SESSIONS_LIMIT: usize = 10;
const WINDOW_SIZE: usize = 60;

fn ms(from: Instant) -> f64 {
    from.elapsed().as_secs_f64() * 1000.0
}

fn run_pass(label: &str) {
    println!("=== {label} ===");
    let total = Instant::now();

    // Step 1: session list (the page's first await).
    let t = Instant::now();
    let stats = db::get_session_cache_stats_from_db(CACHE_STATS_FETCH_LIMIT, false);
    println!("[1] get_session_cache_stats_from_db(50, managed-only): {:8.1}ms  ({} sessions)", ms(t), stats.len());

    // Managed filter + top-N, mirroring recentSessionRows with hideSubagents=false.
    let recent: Vec<_> = stats
        .iter()
        .filter(|s| !matches!(s.harness, Harness::ClaudeCode | Harness::Codex) || s.managed)
        .take(RECENT_SESSIONS_LIMIT)
        .collect();

    // Step 2: full window load per recent session (page does these in parallel;
    // serial here so each one's cost is attributable — note the sum vs wall gap).
    let t_windows = Instant::now();
    let mut last_seen: Vec<(Harness, String, i64)> = Vec::new();
    for row in &recent {
        let t = Instant::now();
        let events =
            db::get_session_cache_events(row.harness, &row.session_id, Some(WINDOW_SIZE), None);
        println!(
            "[2] window {:11} {:24} {:8.1}ms  ({} events)",
            format!("{:?}", row.harness),
            &row.session_id[..row.session_id.len().min(24)],
            ms(t),
            events.len()
        );
        let anchor = events.last().map(|e| e.timestamp).unwrap_or(0);
        last_seen.push((row.harness, row.session_id.clone(), anchor));
    }
    println!("[2] all windows total: {:8.1}ms", ms(t_windows));

    // Step 3: one reconcile tick (what the 1s loop pays every second).
    let t_tick = Instant::now();
    let t = Instant::now();
    let _stats2 = db::get_session_cache_stats_from_db(CACHE_STATS_FETCH_LIMIT, false);
    println!("[3] tick: stats re-list: {:8.1}ms", ms(t));
    for (harness, sid, anchor) in &last_seen {
        let t = Instant::now();
        let fresh = db::get_session_cache_events(*harness, sid, None, Some(*anchor));
        let cost = ms(t);
        if cost > 5.0 {
            println!(
                "[3] tick incr {:11} {:24} {:8.1}ms  ({} new)",
                format!("{harness:?}"),
                &sid[..sid.len().min(24)],
                cost,
                fresh.len()
            );
        }
    }
    println!("[3] tick total: {:8.1}ms", ms(t_tick));
    println!("=== {label} wall: {:8.1}ms ===\n", ms(total));
}

/// Times each loader inside get_session_cache_stats_from_db separately, twice,
/// so the slow component and its cold-vs-warm behavior are unambiguous.
fn profile_stats_components() {
    for round in ["cold", "warm"] {
        println!("=== stats components ({round}) ===");
        let t = Instant::now();
        let oc = db::load_raw_db_cache_events(200, None).unwrap_or_default();
        println!("[c] opencode raw (200,None): {:8.1}ms ({} rows)", ms(t), oc.len());
        let t = Instant::now();
        let pi = db::load_raw_pi_cache_events(200, None);
        println!("[c] pi raw (200,None):       {:8.1}ms ({} rows)", ms(t), pi.len());
        let t = Instant::now();
        let (cc, cx) = db::load_raw_external_cache_events_parallel(200, None);
        println!(
            "[c] cc+codex raw (200,None): {:8.1}ms ({} + {} rows)",
            ms(t),
            cc.len(),
            cx.len()
        );
    }
}

fn main() {
    let repeat = std::env::args().any(|a| a == "--repeat");
    let components = std::env::args().any(|a| a == "--components");
    if components {
        profile_stats_components();
        return;
    }
    run_pass("cold (process start, empty in-process caches)");
    if repeat {
        run_pass("warm (same process, caches primed)");
    }
}

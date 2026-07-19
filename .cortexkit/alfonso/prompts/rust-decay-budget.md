# Rust mode: decay budget non-compliance on HARD re-tier (investigate first, then fix)

Branch from `subc-migration` HEAD. Cache-affecting; investigation MUST precede any code change.

## Live evidence (drive session ses_OqknfoW2O3LTOcjLvOMQoREVPtz1, store ~/.local/share/cortexkit/magic-context/store.db — work on a COPY via VACUUM INTO, never the live file)

- User-tier execute threshold was lowered 65 -> 35 (drive rig config), TUI restarted, /ctx-status confirms threshold 35 propagated.
- A TTL HARD fold ran at 19:58 (rust pass: decision=HARD in=317 out=23). HARD is the only re-tier point.
- After that HARD: m0 frozen_payload is 306,407 chars (~75K tokens) and the status dialog attributes 105K tokens to compartments (388 rows) — while the derived history budget at threshold 35 should be roughly contextLimit 200000 x 0.35 x history_budget_percentage (default in config schema; check the actual value) ~= 42K tokens, and even the DEFAULT_HISTORY_BUDGET_TOKENS fallback is 60K. Earlier the same day at threshold 65 the dialog showed compartments at 82K — the compartment cost GREW when the budget SHRANK, which is backwards.
- Usage sits at ~88% (177K/200K) with a tiny served tail (out=23), i.e. the m0 payload dominates and the decay renderer is not fitting its budget.

## Questions to answer at source (in order)

1. What history_budget_tokens did the adapter actually send on those passes? Read resolveHistoryBudgetTokens usage in packages/plugin/src/hooks/magic-context/rust-mode-transform.ts:509 and its inputs (does the drive path resolve a contextLimit? does historyBudgetPercentage resolve from config?). If the value is None/undefined on the wire, the module falls back to what default? (crates/mc-module: history_budget_tokens f64, transform.rs:1363/1445).
2. Does the module's decay renderer (mc-core decay + compose_m0_from_store) actually consume history_budget_tokens as the budget-pressure input on HARD, and is there a budget-fitting demotion loop equivalent to the TS decay-render.ts (oldest-first past archive boundary render P4/self-close or drop)? Diff the Rust renderer behavior against packages/plugin/src/hooks/magic-context/decay-render.ts on the SAME store copy: render both with budget=42K and compare total rendered token cost. This is the core differential test — build it as a harness test that survives (gen a fixture from the store copy with real 388-compartment shape, redact content to synthetic strings of the same lengths BEFORE committing any fixture: no session content may enter the repo).
3. Token estimation: the module estimates tokens via mc-tokenizer on HARD. Is the pressure input computed from estimated rendered cost vs budget, and is the estimate running over the right text?
4. Why did the status dialog's compartment attribution grow 82K -> 105K across the threshold change? Check the dialog's m0-token-breakdown path in rust mode (m0-token-breakdown.ts) — if it measures the module-composed m0 correctly this is just the symptom of (2); if it double-counts in rust mode, that is a separate truth bug worth its own fix.

## Fix

Whatever (1)-(3) convict: budget threading, pressure input, or a missing demotion loop. The contract is the TS renderer's: on HARD, the rendered compartment block fits the budget by tier demotion oldest-first, archive-boundary P4/self-close, then drop; deterministic; byte-stable on subsequent defers. Add a regression: HARD with budget B renders compartment block <= B (with the small tolerance the TS renderer allows), and shrinking B strictly never grows the rendered cost.

## Gates

cargo test -p mc-core -p mc-module + clippy; focused TS tests if the adapter changes; the differential harness result table (budget, TS cost, Rust cost, per-tier counts) in your report. No em-dashes. Do not touch the live store; do not commit real compartment content.

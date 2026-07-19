# Root-cause two Linux CI flakes blocking the v0.32.1 recut

Two test groups failed the release workflow (release.yml, ubuntu-latest, bun 1.3.14) but PASSED ci.yml's equivalent job on the SAME commit (5bc72c33). They are flaky, not deterministic. Your job: root-cause each with a real Linux repro, then fix STRUCTURALLY. Hard rules: no bare timeout bumps without proving WHY the operation is legitimately slow (and then size to the proven cost, documented); no test deletion/skip; no retries. macOS passes everything — you must use Docker (`oven/bun:1.3.14` image; docker is running) to reproduce. Repo root is your worktree.

## Flake 1: tui-config trio (packages/plugin/src/shared/tui-config.test.ts)
In the failed run, three tests failed with instant (0.4-0.6ms) assertion failures:
- "upgrades bare npm name to @latest while preserving tuple options" — Expected true, Received false
- "creates tui.jsonc (not tui.json) on a fresh install" — existsSync(join(root,"tui.jsonc")) false (test line ~70)
- "writes into the existing tui.jsonc when both files exist"

These pass single-file everywhere (I verified in the Linux container). They failed only in the FULL suite run (root `bun run test` → packages/plugin `bun test`, 287 files, single process, sequential). So this is cross-file pollution.

Leading hypothesis to verify or refute (do not assume): `ensureTuiPluginEntry` resolves the config dir per-call from `process.env.OPENCODE_CONFIG_DIR` (src/shared/opencode-config-dir.ts:12-23). If a stray async continuation from an EARLIER test file deletes/overwrites that env var mid-test (several files mutate it: config/index.test.ts, config/migrate-config-location.test.ts, plugin/embedding-bootstrap.test.ts, shared/conflict-fixer.test.ts, shared/conflict-detector.test.ts, shared/tui-preferences.test.ts), ensureTuiPluginEntry writes to the REAL ~/.config/opencode (writable on runners) and returns true while the test's root stays empty. Note the failure shape differs by machine state, which fits.

Repro approach: in the container, run the plugin suite (or a bisected subset ending with tui-config) in a loop; consider `--timeout 30000` to keep box-load timeouts from polluting the signal (the host is loaded; ~6-8s spurious per-test stalls happen). To catch the polluter, instrument: snapshot process.env.OPENCODE_CONFIG_DIR at test start vs at the moment resolveTuiConfigPath computes the dir (temporary debug fork of tui-config.ts is fine for DIAGNOSIS; the final fix must not carry instrumentation).

Acceptable structural fixes, in preference order: (1) find and fix the actual polluter (an unawaited promise/timer in another test file that mutates env after its file completes — fix that file's lifecycle); (2) make tui-config tests immune by injecting the config dir explicitly (add an options param to ensureTuiPluginEntry or a test-seam) so they don't depend on ambient process-global env; both is best. If you find the polluter, check whether its leak can affect OTHER env-dependent tests too and say so in your report.

## Flake 2: pi env-guard timeout (packages/pi-plugin/src/index-env-guard.test.ts)
"Pi full extension subagent env guard > registers the full runtime when the subagent guard is absent" timed out at 5000ms (ran 7667ms) on the runner; passes locally in ~1-2s and passed ci.yml the same day. Read the test: it likely spawns a bun subprocess that imports the full Pi extension entry. Determine what the 7.6s is made of on a 2-core runner (bun cold boot + module graph import + ...). Use `docker run --cpus 2` to emulate the runner. If the cost is legitimate boot latency on slow hardware, size the per-test timeout explicitly to the measured cost with margin and a comment explaining the measurement (this is correct-sizing, not masking — but only after you've measured and ruled out a hang/race). If there's an actual race or unawaited teardown making it occasionally slow, fix that instead.

## Deliverables
1. Both flakes root-caused with evidence (container runs, measurements) in your report.
2. Fixes committed in your worktree, all plugin+pi+cli suites green locally AND the affected files green in the Linux container (show the runs).
3. No new config knobs, no skips, no retries, comments explain WHY per repo rule.

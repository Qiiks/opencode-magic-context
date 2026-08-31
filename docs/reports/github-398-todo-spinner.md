# GitHub issue #398 — static Pi todo progress glyph

Date: 2026-08-31

## Finding

Issue #398 is valid. The reporter's mechanism matches the source: PR #383 added a wall-clock animation to the Pi todo overlay's `in_progress` glyph. While a painted in-progress row was present, `TodoOverlay` armed one unref'd `setInterval` at 160 ms and each tick called the Pi TUI's global `requestRender()`. Pi's regular-mode renderer redraws the full transcript for that request, so the approximately six full redraws per second could pull terminal scrollback away from a user reading an earlier part of the transcript.

The upstream behavior named by the report is [earendil-works/pi#8002](https://github.com/earendil-works/pi/issues/8002). The earlier timer-driven UI reports cited by the issue describe the same class of scroll-position disruption.

## PR #383 surface audit

The merged PR commit is `a4f55320fe133493b229e161f972f0036573aa15` (`Animate the in_progress todo glyph on wall clock (#383)`), merged 2026-08-29. `git show --stat` confirms that its full product surface was one file, `packages/pi-plugin/src/tools/todo-view-pi.ts`, with 35 additions and one deletion.

| PR #383 change | Resolution |
|---|---|
| `SPIN_FRAMES` and `spinGlyph()`, deriving a glyph from `Date.now()` every 160 ms | Deleted. |
| Conditional use of `spinGlyph()` for `in_progress` rows | Deleted; `STATUS_GLYPH.in_progress` is again the static `◐`. |
| `hasAnimatedRow()` and its capped painted-row predicate | Deleted with the animation gate. |
| `TodoOverlay.spinTimer`, `syncSpinTimer()`, the 160 ms `setInterval`, `requestRender()` callback, `unref()`, and timer-clearing hooks | Deleted entirely, rather than leaving a disabled timer path. |
| Existing static statuses, colors, heading, row capping, widget factory lifecycle, and event-driven refresh | Retained; these were not timer-driven changes from #383. |

No schema, persistence, package, or Rust/module fence moved.

## Fix and invariant

The overlay now renders `STATUS_GLYPH.in_progress` (`◐`) exactly like the other statuses. It no longer imports or invokes any timer API. The existing `this.tui?.requestRender()` in `TodoOverlay.update()` remains only on the already-registered update path. That call is the legitimate state-change refresh: a todowrite transition or completed-task display transition updates the widget once. Rendering an unchanged in-progress widget does not call `requestRender()`, and no wall-clock callback can do so between state changes.

`packages/pi-plugin/src/tools/todo-view-pi.test.ts` pins both sides of that distinction. The invariant test spies on `setTimeout` and `setInterval`, renders an in-progress widget repeatedly, and requires zero timer calls and zero `requestRender()` calls. The neighboring lifecycle test changes a todo from pending to in progress and requires the one event-driven `requestRender()` refresh. The rendered glyph is asserted as `◐` in both tool-result and overlay paths.

Mutation evidence follows the issue #10588 discipline. Reintroducing `setInterval(() => this.tui?.requestRender(), 160)` with a `NON-VACUITY BREAK` marker made the focused invariant test fail at `packages/pi-plugin/src/tools/todo-view-pi.test.ts:354`: the interval spy received one call instead of zero. The mutation was removed immediately. No `NON-VACUITY BREAK` marker remains in the tree.

## Verification

- Frozen Pi dependency install: `bun install --frozen-lockfile` in `packages/pi-plugin` — passed; no manifest or lockfile changes.
- Focused todo overlay suite: `bun test src/tools/todo-view-pi.test.ts` in `packages/pi-plugin` — passed after restoration (18 tests, 61 expectations).
- Mutation guard run: `bun test src/tools/todo-view-pi.test.ts -t "keeps in-progress rendering static without timers between state changes"` — intentionally failed at line 354 with the interval reintroduced, then passed after restoration.
- Full Pi-plugin suite: `bun run test` in `packages/pi-plugin` — passed (882 tests, 3,116 expectations across 77 files).
- Pi-plugin TypeScript gate: `bun run typecheck` in `packages/pi-plugin` — passed.
- Pi-plugin lint: `bun run lint` in `packages/pi-plugin` — passed with one pre-existing warning in `src/context-handler.ts:5453` (`noNonNullAssertion`), outside this change.

## Reply draft for #398

Thanks for the report — your diagnosis is correct. PR #383 (merged 2026-08-29) made the in-progress todo glyph wall-clock animated: a visible in-progress row armed one 160 ms interval, and every tick called Pi's global `tui.requestRender()`. In Pi regular mode that redraws the entire transcript, so the roughly six full redraws per second can disrupt manual scrollback for the whole time a task remains in progress. This is the timer-driven rendering behavior documented upstream in [pi#8002](https://github.com/earendil-works/pi/issues/8002).

We reverted that animation and restored the static `◐` glyph. The timer, interval callback, `unref`, animation-frame helper, and all timer lifecycle plumbing were deleted rather than gated off. Pudgey's #383 contribution was a reasonable attempt to make a long-running step look alive; the regression is the mismatch between that wall-clock animation and Pi's current full-transcript regular-mode renderer, not the todo state itself. State-change refreshes remain: when a todowrite transition changes the todo state, the existing widget is asked to render once.

Regression coverage spies on both timer APIs and the widget's `requestRender` callback while an in-progress row is rendered repeatedly. It proves there are no timer arms or repaint calls between state changes, while separately preserving the legitimate one-call refresh for a todo transition. We would be happy to revisit animation if Pi ships partial or damage-tracked rendering, where a changing glyph would not redraw and disturb the entire transcript.

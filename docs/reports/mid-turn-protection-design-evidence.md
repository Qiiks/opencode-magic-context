# Mid-turn protection: current-provider design evidence

Date: 2026-08-31  
Scope: investigation and measurement only; no runtime behavior or fence changed

## Verdict

The original invariant is directionally right but its strongest cost claim is not.

Changing an already-served message invalidates the provider prefix at the changed block. That is still true for Anthropic, OpenAI exact-prefix caching, and the Claude Code/Thalamus Anthropic leg. However, a stable revised prefix is cacheable immediately. A one-time drop therefore rewrites the retained suffix **once**; the next extending request can read the revised prefix. It does not rewrite the same suffix on every later step unless Magic Context changes it again, the cache expires/evicts, requests race before the first write becomes visible, or a marker is attached to changing content.

This distinction changes the decision:

- Anthropic 1-hour writes remain expensive enough that coalescing mutations at the turn boundary is usually the right default.
- Anthropic 5-minute, OpenAI API (1.25x write), and Codex/OAuth (no write premium) cross break-even at different horizons; provider name alone is not enough.
- A forced 85%/95% bust remains the bad tail outcome, but it was uncommon in the measured cohort: 3 of 137 recent marathon turns that touched 70-85% later reached 85%, and none reached 95%.
- The protection should not be retired globally. At turn boundaries the existing execute path already applies the queued B-shape; the only contested cell is whether to hold during the remaining steps of the current tool-using turn. The best next design is **cache-regime-aware protection with a narrow pressure escape**, after a live billing run confirms the mock's one-write result.

The reusable instrument is in:

- `packages/e2e-tests/src/mid-turn-prefix-ab.ts`
- `packages/e2e-tests/scripts/run-mid-turn-prefix-ab.ts`
- `scripts/drive-rig/run-mid-turn-ab.sh`

## 1. Design-rationale archaeology

### Introduction sequence

The complete introduction is a compact 2026-05-14 series:

| Commit | Change |
|---|---|
| `56a099c2` | Migration v15 and CAS helpers for nullable `session_meta.deferred_execute_state`. |
| `6b264005` | OpenCode and Pi mid-turn detection. |
| `df34645d` | Pure `applyMidTurnDeferral` decision table and force/explicit/subagent bypasses. |
| `1b93699d` | OpenCode transform wiring. |
| `6ecd45b3` | End-of-postprocess re-peek-and-drain. |
| `e6825fa6` | Pi wiring and the successful-work witness. |
| `e46a661d` | OpenCode/Pi integration, CAS-race, and boundary tests. |
| `fd6118d1` | The architecture invariant and original intent. |
| `925e4aa9` | Merge of “boundary execution v8.” |

The merge says the goal explicitly: prevent mid-turn cache busts that “destroy ~50% of multi-step `cache_write` spend.” Its re-peek-and-drain design is intentional: the early gate may CAS-seed deferred intent, but only successful execute-gated work may CAS-clear it later. The flag never promotes a later base `defer`; pressure idempotently produces a new execute decision at the next eligible boundary.

The referenced `.alfonso/plans/boundary-execution-v8.md` was not committed in the merge, its second parent, or the pre-document-purge backup. The surviving evidence is therefore the merge message, patches, tests, architecture text, and contemporaneous cache diagnostics—not the plan's private review transcript.

### Triggering evidence, not a named incident

No issue or incident identifier survives in the boundary commits. The closest contemporaneous empirical record is `f5f013da`, immediately before the feature. It fixes cache diagnostics for a real five-step turn whose mid-turn bust was hidden by parent-row aggregation and records a nonsensical 823k aggregate versus a 541k actual final prompt. The boundary merge then cites ~50% destroyed multi-step cache-write spend.

That supports an observed mid-turn cache-bust problem, but not the stronger modern doctrine that the same revised suffix must be rewritten on every remaining step.

### Current state and later corrections

The current TypeScript rule remains simple: base `defer` stays defer; bypassed execute stays execute; an ordinary mid-turn execute becomes defer and sets the flag (`boundary-execution.ts:34-47`). Rust has the same rule and bypasses force/emergency (`scheduler.rs:532-552`). Pi records the same durable state.

Two later fixes reinforce that the boundary was intended as a real cross-harness invariant:

- `a140c350` removed a Pi promotion from deferred flag to execute; the flag is drain-only.
- `0d2a1bb6` stopped Pi deferred publication/heuristics from draining mid-turn through a dependency inversion.

## 2. What current caches charge

Let:

- `D` = tokens removed by the drop;
- `Q` = retained tokens after the earliest changed position, excluding `D` itself;
- `R` = provider requests remaining before the turn ends;
- `P` = base uncached input price per token;
- `r` = cache-read multiplier, currently `0.1`;
- `k` = effective rewrite multiplier: `1` for Codex/OAuth implicit cache, `1.25` for OpenAI API or Anthropic 5m, and `2` for Anthropic 1h.

Holding the mass for the rest of the turn costs:

```text
hold = R × r × P × D
```

A stable one-time mid-turn application costs an incremental suffix write:

```text
apply_once = k × P × Q
```

The next request reads the revised prefix. Application wins when:

```text
R > kQ/(rD) = 10k × Q/D
```

The former repeated-write doctrine would be:

```text
apply_repeated = R × k × P × Q
```

Under that model `R` cancels and application almost never wins. Current Anthropic documentation and the byte instrument reject that model: the revised prefix written on the mutation request is available to the next extension.

Anthropic's current documented multipliers are 1.25x for 5m writes, 2x for 1h writes, and 0.1x for reads. Current per-million base input prices relevant to the sampled sessions are $5 for Opus 5 and $10 for Fable 5. See [Anthropic prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching) and [Anthropic pricing](https://platform.claude.com/docs/en/about-claude/pricing#prompt-caching).

## 3. Real Magic Context drop geometry

The databases were opened read-only. No prompt or tool-result content is reproduced here. Session/turn labels below are SHA-256-derived pseudonyms.

Data sources:

- `~/.local/share/opencode/opencode.db`
- `~/.local/share/cortexkit/magic-context/context.db`
- recent window: 2026-08-01 through 2026-08-31

From 981 completed recent `ctx_reduce` calls, 966 could be joined to the call's tag boundary. Stored original token counts give:

| Quantity | p25 | Median | p75 |
|---|---:|---:|---:|
| Tags dropped | 8 | 20 | 43 |
| Dropped bytes | 15,174 | 38,177 | 81,177 |
| Dropped tokens `D` | 4,189 | 10,736 | 22,649 |
| Bytes from earliest target through call boundary | 37,188 | 141,079 | 394,335 |
| Tokens from earliest target through call boundary | 9,968 | 37,132 | 80,740 |
| Inclusive suffix / provider prompt | 2.4% | 8.5% | 18.0% |

The median retained rewrite suffix is therefore approximately `Q = 37,132 - 10,736 = 26,396` tokens, or `Q/D = 2.46`.

Median break-even remaining requests:

| Regime | `10k × Q/D` |
|---|---:|
| OpenAI Codex/OAuth implicit, no premium (`k=1`) | 24.6 steps |
| OpenAI API or Anthropic 5m (`k=1.25`) | 30.7 steps |
| Anthropic 1h (`k=2`) | 49.2 steps |

### Four real marathon turns

The selected turns are current Anthropic sessions (Opus/Fable) and use the actual midpoint prompt. `Q` scales the measured 8.5% inclusive-suffix ratio and subtracts the measured median `D`. Costs are normalized to base-input-token equivalents; multiply by the model's `P` to get dollars.

| Turn | Steps | Drop after | `R` | Midpoint prompt | Inclusive suffix | Retained `Q` | Hold (`0.1RD`) | 5m one-write | 1h one-write | Break-even 5m / 1h |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `425e87f2` | 104 | 52 | 52 | 471,625 | 40,088 | 29,352 | 55,827 | 36,690 | 58,704 | 34.2 / 54.7 |
| `cc7069d1` | 140 | 70 | 70 | 491,951 | 41,816 | 31,080 | 75,152 | 38,850 | 62,160 | 36.2 / 57.9 |
| `e9222adf` | 111 | 55 | 56 | 317,137 | 26,957 | 16,221 | 60,122 | 20,276 | 32,442 | 18.9 / 30.2 |
| `f430f08d` | 100 | 50 | 50 | 541,235 | 46,005 | 35,269 | 53,680 | 44,086 | 70,538 | 41.1 / 65.7 |

All four beat holding under a one-write Anthropic 5m model. Two beat holding under 1h; two do not. Under the rejected repeated-write model, the first row's 5m charge would be 1,907,880 equivalent tokens rather than 36,690.

This is why provider TTL/write premium and suffix geometry belong in the decision. Drop size alone is insufficient.

## 4. Quiet-zone risk

For the ten large sessions used to find representative turns, the current usable context limit is 872,000 tokens. With the default `T=70`, the relevant thresholds are 610,400 (70%), 741,200 (85%), and 828,400 (95%).

Among 1,977 turns with at least 15 provider steps in the recent window:

- 137 touched 70-85%;
- 3 later reached 85% in the same turn (2.19% conditional probability);
- 0 reached 95%.

This is a cohort estimate, not a universal probability. It uses today's session limit for historical rows and over-represents this repository's unusually long agent sessions. It nevertheless bounds the observed risk better than doctrine: force escalation was rare, but not zero.

The cost tail is asymmetric. At 85%, the forced bust can rewrite a much earlier prefix than a median user drop and may include emergency batching. Avoiding that event is worth more than the median 2.19% suggests when the session sits only a few points below force.

## 5. Named scenario: Sarenik's “ride versus double bust” claim

Community claim relayed by the owner:

> Keeping 70-85% context cached per turn costs more than paying the double bust.

Anchors: about $0.10 per high-fill cached request, about $0.25 to rewrite after dropping to ~30%, and about $0.04 per cached request at the 30% floor.

### Token-geometry validation against a real Sol-class session

A real `gpt-5.6-sol-fast` Codex/OAuth session with a 308,000 usable limit supplies 2,090 requests in 70-85%. OpenCode records token buckets but `cost=0` on this route, so the dollar columns below apply the public API sheet only to validate the community anchors' scale; they are **not** an OAuth invoice:

| Metric | Observed |
|---|---:|
| Mean prompt | 236,510 tokens |
| Mean cache read | 229,743 tokens |
| Mean uncached input | 6,767 tokens |
| Mean reported cache write | 0 tokens |
| Read cost at $0.40/M cached input | **$0.0919/request** |
| Full-prompt uncached cost at $4/M | $0.946/request |

The observed 229,743-token read validates the $0.10 ride anchor's token geometry ($0.0919 under the public API cached rate). Codex/OAuth itself has no separate write premium and must be modeled by its route-specific effective accounting.

In the same session, 13 transitions fell from 70-85% to 25-35%:

| Metric | Observed |
|---|---:|
| Mean before | 220,956 tokens |
| Mean after | 93,843 tokens |
| Mean uncached after | 71,453 tokens |
| Mean cached after | 22,390 tokens |
| Uncached charge at $4/M | **$0.2858** |

Across all Sol-family sessions, 30 such collapses averaged 78,801 uncached tokens ($0.3152 at the API input rate). The $0.25 rewrite anchor is slightly optimistic but directionally validated in token mass. At stable 25-35% fill, 16,125 cache-dominant requests averaged 90,998 cached tokens ($0.0364 at the API cached rate), validating the $0.04 floor's geometry. Actual OAuth dollars require OAuth billing evidence; the named 280k scenario below supplies its own effective rate.

The database cannot causally identify the proposed second historian-fold write from usage alone. It must remain an explicit term rather than being silently omitted.

### Owner-supplied 280k Codex/OAuth geometry

The corrected named scenario is:

- usable window `W=280k`;
- at 70%: 14k system + 42k m0/m1 + 140k raw tail = `H=196k`;
- early drop reaches `L=84k` (30%);
- both arms add 42k over six requests and the historian returns at that point;
- both then fold to the same ~98k state;
- at least the 14k system prefix survives every implicit-cache bust.

The shared final fold cancels in the arm comparison but must remain in both absolute totals. The unique early rewrite is the post-drop prefix after the reusable implicit prefix; per future request, early reclaim saves cached reads on `H-L=112k` tokens.

The owner's expected totals are internally reproducible from those differentials. A $0.014/request advantage implies an effective cached rate of $0.125/M (base-equivalent $1.25/M), because `112k × $0.125/M = $0.014`. A break-even of 5.6 requests implies a unique early rewrite of `5.6 × 112k × 0.1 = 62.72k` base-equivalent tokens, or $0.0784. Therefore at six requests:

```text
early - ride = $0.0784 - 6×$0.014 = -$0.0056
ride ≈ $0.257  =>  early ≈ $0.2514 ≈ $0.252
```

That verifies the stated $0.257/$0.252 dead heat and the ~$0.014 advantage for every later request. It also exposes one hidden parameter: 62.72k rewritten out of an 84k post-drop prefix means about **21.28k**, not exactly 14k, was reused. This is consistent with the owner's “14k+” empirical wording (system plus roughly 7.3k of stable envelope/tools). If exactly and only 14k survives, the unique write is 70k and break-even is 6.25 requests (or 5.25 *future* requests when the current decision request is counted separately). Either interpretation is a dead heat around the sixth request; the 5.6 point requires the measured 14k-plus retained prefix.

### Correct comparison

Let:

- `H` = cached-read cost at high fill;
- `L(d)` = cached-read cost after dropping to depth `d`;
- `E(d)` = early-drop rewrite at depth `d`;
- `F` = the second historian-fold rewrite;
- `b=1` if riding eventually pays the same late rewrite, otherwise `b=0` when cache-keep makes that bust absent;
- `k` = provider write multiplier.

Then:

```text
ride(N,d)  = b·k·E(d) + N·H
 early(N,d) = k·(E(d)+F) + N·L(d)

N* = k·(F + (1-b)E(d)) / (H-L(d))
```

Using Sarenik's anchors at `h=70%`, `d=30%`, `H=$0.10`, `L=$0.04`, `E=$0.25`, and the owner's explicit second fold `F=$0.22`:

- OpenAI Codex/OAuth implicit cache with the same eventual late rewrite (`k=1,b=1`): `N*=3.67`, so early reclaim wins after about **4** further requests/turns in the anchor-only model.
- Anthropic 1h with a cache-kept ride that avoids the late rewrite (`k=2,b=0`): `N*=15.67`, so early reclaim wins after about **16**.

That reconciles the apparently conflicting claims. The answer depends on whether the late write is shared and cancels.

### Break-even surface by post-drop depth

For a reusable surface, scale `E(d)=0.25(d/0.30)` and `L(d)=0.04(d/0.30)`, keep `H=$0.10` and `F=$0.22`, and vary `k`.

If both strategies eventually share the same late rewrite (`b=1`):

| Post-drop depth | Codex/OAuth `k=1` | OpenAI API or Anthropic 5m `k=1.25` | Anthropic 1h `k=2` |
|---:|---:|---:|---:|
| 20% | 3.0 | 3.8 | 6.0 |
| 30% | 3.7 | 4.6 | 7.3 |
| 40% | 4.7 | 5.9 | 9.4 |
| 50% | 6.6 | 8.2 | 13.2 |
| 60% | 11.0 | 13.7 | 22.0 |

If cache-keep lets riding avoid that write (`b=0`):

| Post-drop depth | Codex/OAuth `k=1` | OpenAI API or Anthropic 5m `k=1.25` | Anthropic 1h `k=2` |
|---:|---:|---:|---:|
| 20% | 5.3 | 6.6 | 10.5 |
| 30% | 7.8 | 9.8 | 15.7 |
| 40% | 11.9 | 14.8 | 23.7 |
| 50% | 19.1 | 23.9 | 38.2 |
| 60% | 36.0 | 45.0 | 72.0 |

`N` must mean future provider requests before the relevant shared/avoided bust. Mid-turn protection normally delays only to the end of the current agentic turn, not four to sixteen completed user turns. At a turn boundary the current scheduler already executes queued drops at threshold—the early-drop B-shape is already shipped there. The contested cell is only the cache-read cost of holding through the **remaining provider steps of this turn**. Across completed user turns the scenario is mainly a historian/belt scheduling question.

### Robustness bonus: smaller prefixes make unrelated busts cheaper

A strict production proxy was computed from recent `transform_decisions`: materialized decisions not attributed to pressure/coverage fold, m1/hard fold, drop/flush were classified as unplanned. This includes system-hash, TTL, first-render, epoch/model/renderer changes, and boundary recuts. It is not a direct provider-miss detector, so treat it as an upper-bound operational frequency.

| Observed regime | Requests | Unplanned materializations | Frequency |
|---|---:|---:|---:|
| Anthropic | 41,292 | 247 | 0.598% |
| OpenAI Codex/OAuth sessions | 36,030 | 391 | 1.085% |

In the 280k scenario, both arms add the same 42k over six requests. Their average resident prefixes are about 217k (ride) and 105k (early). After a 14k reusable system prefix, an unplanned bust rewrites about 203k versus 91k: the early arm is **2.23x smaller**. With the 21.3k measured retained prefix implied by the expected totals, the ratio is 2.34x.

The expected per-request robustness advantage is:

```text
unplanned_frequency × k × P × (217k - 105k)
```

Normalized to base-input tokens, that is 837 tokens/request for Anthropic 5m, 1,339 for Anthropic 1h, 1,519 for OpenAI API using the observed OpenAI frequency as a proxy, and 1,215 for Codex/OAuth. At Opus 5 pricing the Anthropic terms are about $0.0042/$0.0067 per request; at Fable 5 they are $0.0084/$0.0134. At the scenario's inferred Codex/OAuth base-equivalent $1.25/M, its term is about $0.0015/request.

This bonus does not change the one-time planned-write algebra, but it moves close calls toward early reclaim and grows linearly with unexpected bust frequency. The retained system/tools prefix cheapens both arms and cancels from the **difference**; it must still be subtracted when pricing either arm absolutely.

### “threshold + 5” is not missing

The suggested dynamic force threshold is already represented more conservatively. `escalationBands` ships:

```text
force = max(85, effectiveThreshold + 2)
emergency = 95
```

For the normal `T=70`, force is 85—ten points above `T+5`. For unusually high thresholds it follows `T+2` while preserving the 95% wall. There is no hard-coded 80% force band to fix.

## 6. Live/mock A/B instrument

### Shape

Both arms build the same large served history and append a deterministic sequence of `tool_use`/`tool_result` pairs. The last tool result carries the moving Anthropic cache marker.

- Arm A: retain the large old message.
- Arm B: replace it with a dropped placeholder at step 4.

The mock uses the repository's Anthropic-compatible HTTP provider and an exact UTF-8 prefix oracle. It records exact stable bytes in addition to Anthropic-shaped usage fields.

### Dockerized run

The existing drive container was built and started, then the isolated wrapper was run:

```text
scripts/drive-rig/run-mid-turn-ab.sh
```

Observed table:

| Step | Hold read | Hold create | Apply read | Apply create | Apply recache bytes | Event |
|---:|---:|---:|---:|---:|---:|---|
| 1 | 0 | 10,772 | 0 | 10,772 | 43,085 | append |
| 2 | 10,772 | 104 | 10,772 | 104 | 416 | append |
| 3 | 10,876 | 104 | 10,876 | 104 | 416 | append |
| 4 | 10,980 | 104 | 587 | 500 | **2,000** | drop applied |
| 5 | 11,084 | 104 | 1,087 | 104 | **416** | append |
| 6 | 11,188 | 104 | 1,191 | 104 | 416 | append |
| 7 | 11,292 | 104 | 1,295 | 104 | 416 | append |

The marker-aware oracle retains the tools-plus-system breakpoint (587 tokens at the mutation), rewrites the retained suffix, and installs the revised final marker. The immediately following step returns to the ordinary 416-byte append. This is the critical result: the suffix rewrite is one-time in a stable representation.

The test also verifies exact divergence offsets and contains a negative control: a result forged to repeat the mutation-sized rewrite on the next step is rejected.

### Live billing status

No `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN` was available in the drive environment, so the live billing arm was **not executed**. The script supports both credentials and 5m/1h TTLs:

```text
ANTHROPIC_API_KEY=... scripts/drive-rig/run-mid-turn-ab.sh --live
MC_MIDTURN_AB_TTL=1h ANTHROPIC_API_KEY=... scripts/drive-rig/run-mid-turn-ab.sh --live
```

The live arm records `input_tokens`, `cache_read_input_tokens`, and `cache_creation_input_tokens` per step. A provider/routing miss will remain visible rather than being normalized away.

## 7. Provider matrix

| Regime | Cache model | Mutation effect | Current-repo/evidence | Policy implication |
|---|---|---|---|---|
| **1. Anthropic explicit** (OpenCode/Pi and Claude Code/Thalamus) | Exact cumulative prefix; 0.1x reads; 1.25x 5m or 2x 1h writes; TTL controllable and cache-keep possible | Change misses from the changed block, writes revised suffix once, then can read it. Stable tools+system breakpoints survive and cheapen every bust. | E2E wire analyzer checks byte identity. `SerializerProfile::ClaudeCodeAnthropic` and `TransformResponse.cache_ttl` hand 5m/1h marker TTL to Thalamus; marker placement remains gateway-owned. | Keep coalescing by default, especially at 1h. Use `k=1.25/2`; measure Thalamus wire placement before a CC policy change. |
| **2. OpenAI API** | Implicit exact prefix; 0.1x reads; current GPT-5.6 [API write price](https://developers.openai.com/api/docs/pricing) 1.25x | Old-byte change silently loses reuse after the change; revised prefix is reusable later. | API pricing is $4/M input, $0.40/M read, $5/M write. Magic Context does not author Anthropic-style markers in this lane. | Use `k=1.25`; do not lump it with OAuth merely because both caches are implicit. |
| **3. OpenAI Codex/OAuth** | Implicit exact prefix; 0.1x reads; no cache-write premium; stable system prefix survives busts | Same one-time suffix rewrite, but at `k=1`; owner observes at least 14k retained, and the named scenario implies ~21.3k system/envelope retention. | Sampled Sol/OAuth rows report cache reads and zero `cache.write`. `sentinel.ts` avoids manufacturing busts for request parameters outside the prompt key. | Earliest economic break-even. Use `k=1` and subtract the observed retained system prefix from absolute rewrite size. |

Provider policy must key off the **effective cache contract** (API versus OAuth route, marker mode, TTL, write multiplier, and retained prefix), not only `providerID="openai"` or `providerID="anthropic"`.

## 8. Historian and issue #401 belt

### Historian can fire mid-turn, but cannot make the current wire smaller

`checkCompartmentTrigger` has no mid-turn veto. The historian reads a fenced raw range and runs out of band. Publication persists compartment state; it need not mutate the already-served main-agent request immediately. That is why firing mid-turn is safe.

It does not remove `D` from the current provider prefix. Materializing the published fold and pending drops is still execute/bust-gated, and #401 documents the in-flight historian veto on ordinary pending-op application. Mid-turn historian work therefore:

- can prepare a future fold;
- can make coalescing the drop and fold into one later bust more attractive;
- cannot eliminate cached-read cost for the remaining current steps;
- contributes the second-write term `F` if early drop and fold are materialized separately.

The best policy is to merge busts when the historian is already in flight unless pressure is close enough to force that waiting is riskier.

### Interaction with the #401 self-healing belt

The #401 belt intentionally counts only eligible non-mid-turn execute misses. That remains correct. Ordinary same-turn deferral is not queue-stall evidence.

If an escape valve is later added:

- a successful mid-turn application is reclaim progress and should reset the belt;
- a mid-turn defer must remain budget-neutral;
- a failed bypass must preserve pending state and must not count as an eligible miss;
- the 85%/95% bands remain unchanged;
- projected protected mass still needs the belt's eligibility-aware accounting.

The belt repairs “eligible passes made no progress.” The proposed escape repairs “there may be no eligible pass before force.” They are complementary.

## 9. Recommendation

### Keep protection, but make the next design provider-aware

Do not retire the boundary lock globally. Also do not preserve the old repeated-write rationale in documentation. This policy must answer only whether to mutate during the current turn: the existing non-mid-turn execute path already applies queued drops at threshold, so it is not a proposal to delay all 70% reclamation.

Recommended policy for owner review:

1. **Anthropic 1h / CC Anthropic (`k=2`):** keep boundary coalescing as the normal path. If a historian is in flight, prefer one combined fold/drop bust.
2. **Anthropic 5m and OpenAI API (`k=1.25`):** allow an economic escape only when a conservative remaining-request estimate satisfies `R_min > 12.5Q/D`. They share a multiplier, not marker semantics or TTL behavior.
3. **OpenAI Codex/OAuth (`k=1`):** use `R_min > 10Q/D`; the measured median is about 25 remaining requests. Subtract its measured 14k-plus stable system/envelope prefix when pricing `Q`. The exact 280k scenario is a dead heat around the sixth remaining request because its drop is much deeper than the median.
4. **All providers:** add a pressure escape when usage is within `M=3` percentage points of force **and** the drop reclaims at least `N=3` percentage points of usable context. At default thresholds this means usage ≥82% and projected post-drop usage ≤79%. This pairs the escape size with the headroom it restores instead of using raw bytes.

An exact candidate predicate is:

```text
base == execute
&& midTurn
&& bypassReason == none
&& (
     (usage >= forceBand - 3
      && D/contextLimit >= 0.03
      && projectedPostDrop <= forceBand - 6)
     || reliableRemainingRequestLowerBound > 10 * writeMultiplier * Q/D
   )
```

`writeMultiplier` must come from the effective provider cache mode (`1`, `1.25`, or `2`), not a hard-coded provider allowlist. `Q` must be retained suffix after the earliest changed position, not total prompt size. If no reliable lower-bound forecast exists, omit the economic branch and ship only the pressure branch.

The `3/3` pressure values are deliberately above the observed median drop (~1.2% of an 872k window) and near the observed p75 (~2.6%). They reserve mid-turn busts for materially headroom-restoring batches, not routine cleanup. The resulting 79% target also leaves six points before force rather than oscillating at 82-84%.

### Decision status

This report does not recommend shipping the predicate yet. First run both live Anthropic TTL arms and one real OpenAI implicit arm with the instrument. If they confirm the one-write shape and observed multipliers, the evidence supports a provider-aware size/pressure escape rather than the current absolute lock.

Until then, keep the current behavior and correct the doctrine: mid-turn mutation is a potentially expensive **one-time suffix rewrite**, not an inevitable suffix rewrite on every subsequent step.

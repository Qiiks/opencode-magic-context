# WAL/checkpoint headroom investigation — GitHub #362

**Date:** 2026-08-24
**Scope:** source review plus read-only observation of this host's shared `context.db`. No product code, configuration, or production database contents were changed.

## Decision

**No production checkpoint change is recommended now. Document the headroom/runbook instead.**

The issue's framing is right: this is capacity and observability work, not a correctness defect. The full issue body corrects the earlier 44-events/9-days figure to **75 fail-open events over 11 days**, with 14 on the busiest day. Its described behavior is temporary best-effort degradation/requeue, not data loss or a fail-closed outage. The reporter also correctly says that WAL pinning is an inference until checkpoint outcomes are measured.

On this host, a larger 4.71 GB database was observed for five minutes with two actual holder processes (OpenCode and Pi). Its WAL fell from 3.23 MB to 0.15 MB and never reached the 1,000-page (~4 MB) automatic-checkpoint trigger. This is evidence against a continuously pinned WAL **in this sample window**. It does not reproduce the reporter's 11-process, transform-spike workload, so it is not evidence that their event burst did not happen.

Most importantly, a 20 MB WAL *file length* alone does not establish starvation: SQLite normally recycles a WAL rather than truncating it after a successful checkpoint. The discriminating signal is `wal_checkpoint`'s `busy, log, checkpointed` result (or equivalent instrumentation), not the sidecar's size alone. SQLite documents both the default 1,000-page trigger and normal recycling behavior [1](https://www.sqlite.org/wal.html#ckpt), [2](https://www.sqlite.org/wal.html#avoid_wal_excessively_large).

## Issue record used

- Issue: [cortexkit/magic-context#362](https://github.com/cortexkit/magic-context/issues/362), “Production multi-process contention on a large context.db: WAL stays past its checkpoint threshold and writes occasionally exceed busy_timeout”.
- Author: `iceteaSA`; created 2026-08-24 11:30:11Z; open at investigation time.
- Full body and the one maintainer comment were read. There are no linked pull requests. The issue is a split from #312; it says the test-path collision from that issue is already fixed.
- The issue body supersedes the prompt's older 44/9-day measurement: it reports 75/11 days, 11 concurrent processes at the later measurement, and a 24-minute high-concurrency transform burst containing 10 of that day's 14 events. It specifically disconfirms the initially suspected dreamer run as the cause of that burst.

## SQLite facts that control the interpretation

1. A connection is not itself a pinned WAL reader. A **read transaction with an active statement/cursor** has a fixed WAL end mark; a checkpoint must stop when it reaches pages past that end mark. A long-lived connection doing finished autocommit reads does not retain that snapshot. SQLite describes the end mark and checkpoint stopping rule here: [WAL concurrency](https://www.sqlite.org/wal.html#concurrency).
2. SQLite starts an implicit read transaction at `SELECT`; it ends when the last active statement finishes. A prepared statement is only guaranteed finished when reset or finalized, so a cached statement is not automatically safe merely because it is prepared: the important question is whether a cursor is left active. See [SQLite transaction control §2.1 and §2.3](https://www.sqlite.org/lang_transaction.html#read_transactions_versus_write_transactions).
3. Automatic checkpoints are PASSIVE and fire at a commit that leaves the WAL at least 1,000 pages. A PASSIVE checkpoint does as much work as possible without interfering with readers; it may not complete. It requires a **writable** database connection. [SQLite WAL §§2.1, 3.1, 3.2](https://www.sqlite.org/wal.html#ckpt).
4. With a 4,096-byte page, the nominal automatic trigger is 1,000 × 4,096 = 4,096,000 bytes (3.91 MiB, before WAL frame overhead). It is a trigger, not a hard file-size cap.

## Reader/connection lifetime map

“Pins WAL?” below means “has source evidence of a long active read transaction/cursor,” not “has an open connection.” Static review cannot prove that a third-party runtime has reset every cursor at runtime, so the conclusion is deliberately scoped to the repository's call patterns.

| Holder / path | Connection lifetime | Read-transaction finding | Pins WAL? |
| --- | --- | --- | --- |
| OpenCode Magic Context runtime | `storage-db.ts` keeps a module-local `Map<string, Database>` keyed by database path; `openDatabase()` returns that existing handle on later calls. It is therefore one long-lived handle per loaded core module/process, not one handle per request. (`packages/plugin/src/features/magic-context/storage-db.ts:51-55`, `2175-2197`) | The initialization path sets WAL and the 5,000 ms connection-local busy timeout but does not open a long read transaction. (`storage-db.ts:843-854`) Storage does cache prepared statements by `Database` in `WeakMap`s, but the shown cached SELECT executes synchronously with `.all()` and returns materialized rows; it is not an exposed streaming cursor. (`storage-tags.ts:7-25`, `304-348`) | **Long-lived connection: yes. Long-lived read transaction: no source evidence.** Cached prepared statements need runtime cursor discipline, but preparation alone is not evidence of a pin. |
| TUI / RPC | The TUI asks the server over RPC; handlers call `getDb()`, which returns the server's cached handle rather than opening a browser/TUI SQLite connection. (`packages/plugin/src/plugin/rpc-handlers.ts:124-130`, `892-910`) | The direct count example is a synchronous `prepare(...).get()` inside the handler, with no `BEGIN`. (`rpc-handlers.ts:952-966`) Sidebar/detail reads follow the same server-handle pattern. | **No source evidence of a pin.** RPC reads are short autocommit statements on the server connection, assuming the runtime completes/resets `.get()` as expected. |
| Dashboard | The dashboard does read SQLite directly, not through the plugin RPC: `open_readonly()` returns a new `rusqlite::Connection` with `SQLITE_OPEN_READ_ONLY` and `busy_timeout=5000`. (`packages/dashboard/src-tauri/src/db.rs:71-80`) Each Tauri command binds that connection in the request function scope. (`commands.rs:17-48`) The alternate HTTP/serve dispatcher does the same. (`serve/dispatch.rs:281-342`, `782-789`) | No read `Transaction` is opened in those request paths; ownership drops the connection at command return. Dashboard mutations similarly open a request-scoped read-write connection. (`commands.rs:117-185`) | **No source evidence of a pin.** Direct dashboard reads can be expensive, but they are connection/request scoped rather than a persistent reader. |
| Pi main extension | Pi opens the shared DB once during startup using `openDatabaseAsync()` and passes it into its runtime. (`packages/pi-plugin/src/index.ts:763-769`, `848-861`) It intentionally does not close the cached handle at session shutdown because Pi reload keeps the process alive and a closed cached handle would poison future callers. (`index.ts:2253-2265`) | Explicit `BEGIN IMMEDIATE` sites found in Pi are write-side serialization (for example `context-handler.ts:1882-1895`), not a read transaction held around an `await`. | **Long-lived connection: yes. Long-lived reader: no source evidence.** |
| Pi child / “seat” processes | A Pi subagent extension can independently open the shared DB on its own `session_start` and save it in module state. (`packages/pi-plugin/src/subagent-entry.ts:68-103`) In-process reinitialization is explicitly suppressed, avoiding a second full runtime in the same Pi process. (`packages/pi-plugin/src/index.ts:732-745`) | This is a possible extra long-lived connection when a separately spawned child loads the extension. No such child held this host DB during the sample. Mason worktree processes also did not appear among DB holders. | **Possible connection, not observed; no source evidence of a long reader.** |
| Dream timer | The per-process timer opens through `openDatabase()`, so it reuses the same cached handle; it is not a timer-specific connection. (`packages/plugin/src/plugin/dream-timer.ts:151-177`, `265-291`) It ticks every 15 minutes, starts after a boot quiet period, and has per-project startup jitter. (`dream-timer.ts:66-70`, `182-240`, `314-336`) | The timer performs scheduled work on that shared handle. Its leases deliberately use short `BEGIN IMMEDIATE` write transactions so competing processes serialize ownership. (`features/magic-context/dreamer/lease.ts:99-124`) The timer currently runs `PRAGMA optimize`, **not** a WAL checkpoint. (`dream-timer.ts:287-291`, `storage-db.ts:772-785`) | **Writer/maintenance contender: yes. Reader pin: no source evidence.** A task should not hold a transaction while awaiting network/child work; current lease helpers keep the transaction body synchronous, but this remains a hygiene invariant worth preserving. |
| CLI doctor | Read-only doctor paths use the existing-only readonly opener. (`packages/cli/src/lib/database-access.ts:61-100`) Pi doctor closes in `finally`; OpenCode doctor likewise closes after its probes. (`commands/doctor-pi.ts:638-692`, `commands/doctor-opencode.ts:1259-1285`, `1330-1358`) | One normal doctor invocation can briefly read a large table or run `integrity_check`, but it is an ephemeral connection and not a daemon. Some doctor modes intentionally mutate/reconcile, so they are writers rather than an explanation for read pinning. (`commands/doctor.ts:82-115`) | **No persistent reader.** It can add short-lived load only while manually invoked. |

### Prepared statements, precisely

The repository does cache statements (for example tag statements are `WeakMap<Database, PreparedStatement>` values), so “prepared statement” must not be conflated with “completed statement.” The code reviewed invokes `.get()`, `.all()`, and `.run()` rather than exposing an iterator/row cursor from RPC or dashboard request code. The source therefore does **not** identify a held SELECT cursor or explicit long `BEGIN` read transaction. That is a reason **not** to ship reader-hygiene changes blindly, not a proof that future code cannot introduce one.

The one isolated writer worth noting is transform-decision telemetry: it opens a separate connection per row, deliberately sets `busy_timeout=0`, writes, and closes it in `finally`. It can report/drop contention immediately, but it cannot be a long reader. (`packages/plugin/src/features/magic-context/transform-decision-log.ts:412-436`)

## Live measurement — read-only production observation

**Database:** `$HOME/.local/share/cortexkit/magic-context/context.db` (XDG data home unset).
**Guardrail:** no writable SQLite connection was opened by this investigation. No row, pragma setting, checkpoint, or sidecar was intentionally changed.

Initial read-only PRAGMAs at 2026-08-24T15:49:15+03:00:

- `journal_mode = wal`
- `page_size = 4096`
- `page_count = 1,150,149`
- `page_count × page_size = 4,711,010,304 bytes` (4.71 GB decimal / 4.39 GiB)
- `wal_autocheckpoint = 1000`
- `schema_version = 444`

| Local timestamp | WAL bytes | Approx. 4 KiB pages | Relative to 1,000-page trigger | Holders from `lsof` | `wal_checkpoint(PASSIVE)` result |
| --- | ---: | ---: | --- | --- | --- |
| 15:48:54+03:00 | 2,575,032 | 629 | 63% | not captured in this first filesystem snapshot | not run |
| 15:49:15+03:00 | 3,230,112 | 789 | 79% | `opencode serve` PID 8553; `pi`/Node PID 49725 | unavailable (see below) |
| 15:52:15+03:00 | 2,195,992 | 536 | 54% | same two PIDs | unavailable (see below) |
| 15:54:16+03:00 | 152,472 | 37 | 4% | same two PIDs | unavailable (see below) |

The main database file's mtime advanced during the interval and the WAL fell by 95% from the peak. That is consistent with normal checkpoint/recycling progress and inconsistent with a continuously pinned WAL during this sample. It does **not** attribute the progress to any one process or prove a particular checkpoint mode.

### Why there are no `busy/log/checkpointed` columns

The requested `PRAGMA wal_checkpoint(PASSIVE)` is a checkpoint operation, not an observational read: SQLite documents that an application initiates it using a **writable database connection**. To honor the instruction that all production data stay read-only, the probe was attempted only via `sqlite3 -readonly`. SQLite rejected each attempt with:

```text
Error: stepping, attempt to write a readonly database (8)
```

Thus no `busy, log, checkpointed` values were collected and the probe did not checkpoint production data. This is preferable to silently treating a housekeeping write as read-only. A future, explicitly authorized diagnostic run can collect those columns with a writable connection; its side effect must be disclosed because even PASSIVE may copy WAL pages into the main file.

The live sample also shows why raw WAL bytes alone are insufficient: this host's file shrank naturally below the trigger while the two long-lived connections remained open. Open connections are not the relevant criterion; active read transactions are.

## Strategy adjudication

### (a) Periodic `wal_checkpoint(TRUNCATE)` in a quiet window

**Not recommended now.** The dream timer is the closest existing home only in the sense that it already schedules background maintenance and has database leases. It is not a globally single scheduler: every participating OpenCode/Pi process has a timer and registered projects. A safe implementation would need a new shared checkpoint lease/key and explicit observability, rather than assuming the first timer callback is quiet.

A `TRUNCATE` checkpoint needs a writable connection and tries to complete/reset the WAL. SQLite warns that RESTART/TRUNCATE can block readers; PASSIVE may instead stop short. A checkpoint run during a transform spike can therefore consume up to the existing 5,000 ms `busy_timeout` or return busy/partial progress. Because the timer's SQLite calls are synchronous, waiting can stall that plugin process's maintenance callback; a concurrent process can still be transforming. The timer catches maintenance failures, while transform and index paths deliberately fail open/requeue (`dream-timer.ts:269-295`; `features/magic-context/message-index-async.ts:16-41`). That protects correctness, but it does not make added latency valuable.

**Blast radius if later justified:** `packages/plugin/src/plugin/dream-timer.ts`, the dreamer lease helper, a metric/logging surface, and tests for multiprocess lease ownership, busy/partial checkpoint outcomes, and “never run during active transform” policy. It also introduces a new periodic writer in every deployment. Do not add it merely to make the WAL sidecar visually smaller.

### (b) Tune `wal_autocheckpoint`

**Do not tune now.** This live database reports the SQLite default 1,000 pages, and runtime initialization enables WAL without overriding that threshold (`storage-db.ts:843-854`). Lowering it makes checkpoints more frequent and puts more work on commit paths; raising it reduces checkpoint frequency but permits a larger WAL. Neither clears a WAL whose oldest frame is protected by an active reader. If #362's 20 MB represents a genuinely pinned reader, changing the threshold only changes when more partial checkpoint work starts, not the pinned frame.

### (c) Reader hygiene

**This is the first corrective path if future evidence identifies a pin.** Current source review found long-lived connections but no long explicit read transactions/cursors in the requested holders. The right fix would be narrowly shortening the identified transaction/cursor—finish/reset it before awaits, page a dashboard query, or close an accidental direct-reader connection—not adding checkpoint pressure. No such code change is justified from the present evidence.

A practical regression guard for a future reader change is a two-connection WAL test: leave a deliberate SELECT cursor active, perform writes past the threshold, assert a PASSIVE checkpoint reports incomplete progress; then finish/reset the cursor and assert a subsequent checkpoint reaches the end. That test should use a temporary DB, never the shared live DB.

### (d) Raise `busy_timeout` above 5,000 ms

**Not recommended as the primary response.** The timeout is deliberately installed before WAL setup on every plugin connection (`storage-db.ts:843-854`) and dashboard connection (`dashboard/src-tauri/src/db.rs:71-80`). The issue shows losers spread across writer paths during a transform-concurrency ramp, which is the expected single-writer bottleneck, not a demonstrated checkpoint failure.

A larger timeout can turn a fail-open/retry into success if the owner releases the write lock shortly after five seconds. It also increases time spent blocking an interactive transform or timer callback. Since the affected best-effort paths requeue/reconcile and the issue reports no loss, raising it trades tail latency for fewer log events without reducing the underlying writer overlap. Revisit only if a metric shows a material rate of 5–10 second lock holds and users prefer waiting to one-pass fallback.

## Headroom model and escalation threshold

Database size, process count, and write rate should not be collapsed into one threshold:

- **Database size:** the automatic checkpoint trigger is page-count based and is independent of the 4.71 GB main database size. A larger database can make scans/queries slower, but it does not itself pin a WAL.
- **Process count:** 11 processes is only a proxy. Eleven connections doing short autocommit reads are safe for checkpointing; one connection with an active cursor can be enough to prevent reset.
- **Write rate × oldest active reader duration:** a useful upper-bound model is `pinned WAL pages ≈ write-touched pages/sec × duration of oldest active reader`. Once that exceeds 1,000 pages, automatic checkpoint attempts occur; if no reader gap exists, the WAL can retain/reuse more frames. The reporter's ~20 MB file is approximately 5,000 4 KiB pages, so it is compatible with a reader remaining active across roughly 5,000 page writes—but the file length by itself cannot distinguish that from a recycled allocation.

**User-visible degradation begins when either of these is observed, not when a WAL crosses 4 MB:**

1. An active cursor/reader blocks `checkpointed` from reaching `log` across multiple writer cycles, and WAL growth contributes measurable read slowdown or disk headroom risk; or
2. write-lock hold times repeatedly exceed the 5-second timeout, causing enough fail-open passes that users see sustained untransformed/late results rather than isolated retries.

The corrected #362 sample is a meaningful signal (75 events/11 days, with a 14-event high day during a ten-session transform burst), but it does not yet establish either threshold: the events are fail-open/requeue, are spread across writers, and lack checkpoint outcome telemetry. The local 4.71 GB sample shows substantially more database size alone is not the trigger.

## Recommended runbook / future evidence

1. Add no scheduler, timeout, or WAL-threshold behavior now.
2. Document that a WAL file larger than ~4 MB is not itself a failure; diagnose with checkpoint result triples plus active-cursor/process context.
3. If an operator explicitly authorizes a writable maintenance observation, take several **PASSIVE** samples at and after a transform spike, recording timestamp, `busy/log/checkpointed`, WAL bytes, page size, and DB-holder PIDs. Do not substitute `TRUNCATE` for measurement.
4. If `checkpointed < log` persists, first capture the reader stack/cursor lifetime and fix that holder. Only after reader hygiene is ruled out should a globally leased, quiet-window checkpoint be designed.
5. Separately record write-lock wait duration and retries. Use it to decide whether a timeout change buys a user-visible improvement rather than merely fewer logs.

## Draft public reply for #362

> Thanks for the careful correction and for preserving the fail-open framing. We investigated this as headroom/checkpoint behavior, not a data-loss defect.
>
> Two important findings: (1) an open SQLite connection is not by itself a WAL pin; the pin is an active read transaction/cursor, and (2) WAL file length alone is not proof of a starved checkpoint because SQLite normally recycles rather than truncates a WAL after checkpointing. The deciding measurement is `wal_checkpoint`'s `busy, log, checkpointed` values plus reader/cursor context.
>
> We mapped the current holders. OpenCode and Pi deliberately keep one cached connection per process; RPC/TUI reads use the server connection and short synchronous statements, dashboard opens request-scoped direct readonly connections, doctor closes its readonly probe, and the dream timer reuses the process handle. We found no source evidence of a long explicit read transaction or exposed streaming cursor in those paths. Prepared statements are cached in a few storage modules, but cached preparation is not by itself a snapshot; an unfinished cursor would be the thing to fix.
>
> On a separate live 4.71 GB shared DB, we observed only OpenCode and Pi holding it. Over five minutes its WAL fell from 3.23 MB to 0.15 MB (below the default 1,000-page/~4 MB trigger) while both processes remained connected. That does not reproduce your 11-process transform spike, but it is useful negative evidence: database size and long-lived connections alone did not pin WAL there.
>
> We did not run a writable checkpoint against production during this investigation. `PRAGMA wal_checkpoint(PASSIVE)` is a write-side checkpoint operation, so a read-only probe correctly returned `attempt to write a readonly database`; we will not call that read-only. The next useful, explicitly authorized measurement is several PASSIVE triples during a transform spike, correlated with holder/cursor lifetimes and write-lock duration.
>
> Recommendation for now: no periodic `TRUNCATE`, no autocheckpoint tuning, and no busy-timeout increase. A timer-based TRUNCATE would need a new cross-process lease and could spend the existing 5 seconds blocking during the very contention it is intended to help; lowering/raising the automatic threshold cannot clear a reader-pinned frame; and increasing the timeout trades interactive latency for fewer already-recoverable fail-open events. If the proposed triples show `checkpointed < log` across reader gaps, we will first shorten the identified reader. If they instead show healthy checkpoint progress with >5-second write-lock ownership, we will evaluate targeted writer observability/timeout policy from that data.

## Sources

### Repository source citations

- Shared per-process connection map/open/close: `packages/plugin/src/features/magic-context/storage-db.ts:51-55`, `2175-2227`, `2229-2312`.
- WAL mode and 5-second busy timeout: `packages/plugin/src/features/magic-context/storage-db.ts:843-854`.
- RPC's cached server database and short count query: `packages/plugin/src/plugin/rpc-handlers.ts:124-130`, `892-966`.
- Statement-cache and completed `.all()` reads: `packages/plugin/src/features/magic-context/storage-tags.ts:7-25`, `304-348`.
- Dream timer lifetime/schedule/maintenance: `packages/plugin/src/plugin/dream-timer.ts:151-177`, `182-295`, `365-665`.
- Dream lease uses `BEGIN IMMEDIATE`: `packages/plugin/src/features/magic-context/dreamer/lease.ts:99-124`.
- Fail-open indexer lock behavior: `packages/plugin/src/features/magic-context/message-index-async.ts:16-41`.
- Pi startup and intentional process-lifetime handle: `packages/pi-plugin/src/index.ts:763-769`, `848-861`, `2253-2265`.
- Pi subagent separate opener: `packages/pi-plugin/src/subagent-entry.ts:68-103`.
- Dashboard direct per-request connections: `packages/dashboard/src-tauri/src/db.rs:71-80`; `packages/dashboard/src-tauri/src/commands.rs:17-48`, `117-185`; `packages/dashboard/src-tauri/src/serve/dispatch.rs:281-342`, `782-789`.
- CLI existing-only readonly opener and closure: `packages/cli/src/lib/database-access.ts:61-100`; `packages/cli/src/commands/doctor-pi.ts:638-692`; `packages/cli/src/commands/doctor-opencode.ts:1259-1285`, `1330-1358`.
- Separate best-effort transform-decision writer: `packages/plugin/src/features/magic-context/transform-decision-log.ts:412-436`.

### SQLite documentation

- [WAL mode: checkpointing, concurrency, automatic and application checkpoints](https://www.sqlite.org/wal.html)
- [Avoiding excessively large WAL files / checkpoint starvation](https://www.sqlite.org/wal.html#avoid_wal_excessively_large)
- [Transaction control: read transactions and implicit transaction completion](https://www.sqlite.org/lang_transaction.html)

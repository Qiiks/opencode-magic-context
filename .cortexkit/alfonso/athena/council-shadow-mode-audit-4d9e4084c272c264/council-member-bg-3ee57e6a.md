## Finding 1: Permanent Session Wedge on Reset Failure
- **Severity**: high
- **Location**: `packages/plugin/src/hooks/magic-context/shadow-sender.ts` (lines 878-885, 1025-1043)
- **Confidence**: high
- **Issue**: If a queued `reset` work item fails (e.g. due to a transient connection failure when calling `performReset`), the exception is not caught in `runQueue`'s reset arm. This crashes the queue processing loop, leaving `state.blockedUntilReset = true` and `state.requireResetReason = "ordinal_mismatch"`. Since the `reset` item has been shifted off the queue, the queue becomes empty. On subsequent transform passes, `enqueue` sees `state.blockedUntilReset` is `true` and returns immediately (skipping the pass) without re-queueing a reset. The session is permanently wedged and shadow transforms are never sent again for that session.
- **Evidence**:
  In `runQueue` (lines 878-885):
  ```typescript
            if (item.kind === "reset") {
                await performReset({
                    sessionId,
                    state,
                    reason: item.reason,
                    projectRoot: item.projectRoot,
                });
                continue;
            }
  ```
  In `enqueue` (lines 1025-1031):
  ```typescript
            if (!resolved.ok) {
                if (resolved.reason === "mismatch") {
                    state.counters.ordinal_mismatch += 1;
                    state.queue.length = 0;
                    state.requireResetReason = "ordinal_mismatch";
                    state.blockedUntilReset = true;
  ```
- **Suggested Fix**: Wrap the `reset` arm in `runQueue` in a `try-catch` block. If `performReset` fails, do not discard the reset state; instead, keep `state.blockedUntilReset = true` and re-queue or retry the reset, or clear `state.running` and let the next `enqueue` attempt to re-queue the reset. Alternatively, in `enqueue`, if `state.blockedUntilReset` is true but the queue is empty, re-queue the reset.

## Finding 2: Critical Wire Shape Mismatch for Compartments
- **Severity**: critical
- **Location**: `packages/plugin/src/hooks/magic-context/shadow-sender.ts` (`serializeCompartment`, lines 584-604) vs `crates/mc-module/src/lib.rs` (`ShadowCompartmentWire`, lines 267-295)
- **Confidence**: high
- **Issue**: The TS side serializes compartments with nested `start` and `end` objects containing `flat_id`, `bare_message_id`, and `absolute_ordinal`. However, the Rust side's `ShadowCompartmentWire` struct expects flat fields at the top level: `start_message`, `end_message`, `start_message_id`, and `end_message_id`. Since `start_message` and `end_message` are required fields in Rust (no `#[serde(default)]`), any `state_sync` payload containing compartments will fail to deserialize on the Rust side, returning an `invalid_params` error and preventing shadow state synchronization.
- **Evidence**:
  In TS `serializeCompartment` (lines 584-604):
  ```typescript
    return {
        id: args.compartment.id,
        sequence: args.compartment.sequence,
        start: {
            flat_id: ...,
            bare_message_id: args.compartment.startMessageId,
            absolute_ordinal: startOrdinal,
        },
        end: {
            flat_id: ...,
            bare_message_id: args.compartment.endMessageId,
            absolute_ordinal: endOrdinal,
        },
        ...
  ```
  In Rust `ShadowCompartmentWire` (lines 267-295):
  ```rust
  struct ShadowCompartmentWire {
      sequence: i64,
      start_message: i64,
      end_message: i64,
      #[serde(default)]
      start_message_id: String,
      #[serde(default)]
      end_message_id: String,
      ...
  ```
- **Suggested Fix**: Update `serializeCompartment` in `shadow-sender.ts` to match the flat structure expected by the Rust side:
  ```typescript
  return {
      id: args.compartment.id,
      sequence: args.compartment.sequence,
      start_message: startOrdinal,
      end_message: endOrdinal,
      start_message_id: args.compartment.startMessageId,
      end_message_id: args.compartment.endMessageId,
      ...
  };
  ```

## Finding 3: Missing SQLite Busy Timeout on Read-Only DB Connection
- **Severity**: medium
- **Location**: `packages/plugin/src/hooks/magic-context/read-session-db.ts` (`getReadOnlySessionDb`, lines 56-66)
- **Confidence**: high
- **Issue**: The read-only database connection used by `withReadOnlySessionDb` is opened without setting a busy timeout (`PRAGMA busy_timeout`). In contrast, the main database connection and the dreamer's read-only connection both set a busy timeout of 5000ms. Without a busy timeout, any read query on the read-only connection (such as `readRawSessionMessageByIdFromDb` during the shadow sender's ordinal resolution fallback) will immediately fail with `SQLITE_BUSY` if a write transaction is active on the database, rather than waiting for the write to complete. This can cause transient failures in the shadow sender and other read-only hooks.
- **Evidence**:
  In `getReadOnlySessionDb` (lines 56-66):
  ```typescript
  function getReadOnlySessionDb(): Database {
      const dbPath = getOpenCodeDbPath();
      if (cachedReadOnlyDb?.path === dbPath) {
          return cachedReadOnlyDb.db;
      }

      closeCachedReadOnlyDb();
      const db = new Database(dbPath, { readonly: true });
      cachedReadOnlyDb = { path: dbPath, db };
      return db;
  }
  ```
  Compare with `openOpenCodeDb` in `open-opencode-db.ts` (line 18):
  ```typescript
              const db = new Database(dbPath, { readonly: true });
              db.exec("PRAGMA busy_timeout = 5000");
  ```
- **Suggested Fix**: Add `db.exec("PRAGMA busy_timeout = 5000");` after opening the database in `getReadOnlySessionDb`.

## Finding 4: Discrepancy in Ordinal Calculation for Malformed JSON Messages
- **Severity**: low
- **Location**: `packages/plugin/src/hooks/magic-context/read-session-raw.ts` (`readRawSessionMessageByIdFromDb` vs `readRawSessionMessagesFromDb`)
- **Confidence**: medium
- **Issue**: There is a subtle discrepancy in how message ordinals are calculated when a message contains malformed JSON in the database. `readRawSessionMessagesFromDb` filters out malformed JSON messages entirely during its `flatMap` phase, which shifts the ordinals of all subsequent messages down. However, `readRawSessionMessageByIdFromDb` uses a SQL `COUNT(*)` query that does not check if the JSON is valid (it only filters out compaction summaries). As a result, if there is a malformed JSON message in the database, `readRawSessionMessageByIdFromDb` will count it, leading to a higher ordinal than what `readRawSessionMessagesFromDb` (and the in-memory cache) computes. This could trigger a false-positive ordinal mismatch/reset in the shadow sender.
- **Evidence**:
  In `readRawSessionMessagesFromDb` (lines 106-117):
  ```typescript
      return filtered.flatMap((row, index) => {
          const info = parseJsonRecord(row.data);
          if (!info) return [];
          ...
          return {
              ordinal: index + 1,
              ...
          };
      });
  ```
  In `readRawSessionMessageByIdFromDb` (lines 384-393):
  ```typescript
      const ordinalRow = db
          .prepare(
              `SELECT COUNT(*) AS ordinal FROM message
               WHERE session_id = ?
                 AND NOT (COALESCE(json_extract(data, '$.summary'), 0) = 1
                          AND COALESCE(json_extract(data, '$.finish'), '') = 'stop')
                 AND (time_created < ? OR (time_created = ? AND id <= ?))`,
          )
          .get(sessionId, row.time_created, row.time_created, messageId) as OrdinalRow | null;
  ```
- **Suggested Fix**: While malformed JSON in the database is rare, the SQL query in `readRawSessionMessageByIdFromDb` could be updated to ensure the JSON is valid (e.g. by checking `json_valid(data) = 1` if supported, or parsing it in JS if the performance cost is acceptable).

## Summary
- **Total findings**: 4 (1 Critical, 1 High, 1 Medium, 1 Low)
- **Overall risk assessment**: High. The wire shape mismatch for compartments is a critical blocker that will prevent shadow state synchronization from working in production as soon as compartments are created. The queue wedge bug is a high-severity issue that will permanently disable shadow transforms for a session if a transient connection failure occurs during a reset.
- **Verdict**: **HOLD** due to the critical wire shape mismatch for compartments.
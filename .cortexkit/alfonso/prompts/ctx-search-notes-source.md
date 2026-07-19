# ctx_search: add `note` as a fifth search source

## Why

Notes are exactly the "did we decide something about this?" content — parked design intentions, follow-ups, and dismissed notes carrying a `resolution` ("decided X because Y"). Today they are only reachable by paging `ctx_note read`. Unified search (`packages/plugin/src/features/magic-context/search.ts`) already fuses memories, raw messages, git commits, and compartment chunks; notes become the fifth source.

## Design (locked)

- **Keyword/FTS-style lane only, NO embeddings.** The note pool is tiny (tens of rows per session/project). No vector side-table, no chunker identity, no invalidation machinery. Score notes with the same keyword/probe scoring the raw-message lane uses where reusable, or a simple LIKE/token-overlap scorer if that is cleaner — read `search.ts`'s existing fusion (RRF) and slot notes in as one more ranked list. Given the pool size, even scanning all candidate rows and scoring in JS is acceptable.
- **Which notes**: `type IN ('session','smart')` scoped like the rest of unified search — the current session's session-notes plus the project's notes (`project_path` matching the resolved project identity; look at how the notes storage layer already scopes reads in `packages/plugin/src/tools/ctx-note/` and reuse its helpers rather than writing new SQL from scratch if they fit).
- **ALL statuses searchable**: `active`, `ready`, `dismissed`, `pending`. Explicit search is recall, not surfacing — a pending smart note staying hidden from passive surfacing does not mean it should be invisible to a direct query. Each hit is labeled with its status.
- **Search text**: note `content` plus `ready_reason` when present. (Dismissal resolutions: check where `ctx_note dismiss` stores the resolution text — if it is appended into `content` or a column, include whatever field carries it.)
- **Result shape** (renderer in `search.ts` + the tool output in `packages/plugin/src/tools/ctx-search/tools.ts`): note id (`#N` in the ctx_note numbering the agent already knows), status, created-at recency hint, the content snippet, and — when `anchor_ordinal` is set — an `@msg N` anchor so the agent can `ctx_expand` around where the note was written (mirror the footer hint pattern the message source already uses).
- **No visibility filter**: unlike memories, notes are not injected into context, so there is nothing to exclude as "already visible".
- **Sources enum**: add `"note"` to `VALID_SOURCES` in `packages/plugin/src/tools/ctx-search/tools.ts` (17-19) and the tool description's sources documentation (keep the description edit MINIMAL — one added line item in the sources list + one line in the "picking sources" guidance, e.g. parked decisions/follow-ups → ["note"]; do not rewrite the description). Default broad search (no `sources` arg) includes notes.
- **Ranking weight**: notes should not drown out memories/messages in broad searches; slot them into the existing RRF fusion as one more list (RRF handles balance naturally). Do not invent bespoke boosting.

## Pi parity

`packages/pi-plugin/src/tools/ctx-search.ts` wraps the same `unifiedSearch` core — verify the new source flows through (sources enum/validation may be duplicated there; mirror it and its tool description the same minimal way). Pi's notes live in the same shared DB.

## Tests

Co-located with source:
1. `unifiedSearch` returns note hits for a keyword present in a note; hit carries id/status/anchor.
2. Dismissed note with resolution text is findable and labeled `dismissed`; pending smart note findable and labeled `pending`.
3. Scoping: another project's notes are not returned; another session's session-notes are only returned when they belong to the same project scope decision you implement (match whatever scoping you chose, and assert it).
4. `sources: ["note"]` restricts to notes; broad search includes them fused with other sources.
5. Tool-level (ctx-search tools.test): rendering includes the `#id`, status label, and `@msg` anchor footer behavior.
6. Pi tool test mirroring the sources acceptance.

## Gates

- `cd packages/plugin && bun test && bun run typecheck`
- `cd packages/pi-plugin && bun test && bun run typecheck`
- `bun run lint` from repo root (formatter: 4-space/double-quote in plugin, tabs in pi-plugin)
- `check_comments` — comments explain WHY for a cold reader, no plan references.

Do not touch `crates/`, `packages/dashboard/`, or any config schema (no new knobs — notes search is unconditional). Commit with a clear message.

# Compact the <project-memory> wire format (category-grouped, importance off the wire)

Repo: subc-migration branch (HEAD = the temporal-dates merge). Ufuk-decided format change to stop wasting prefix tokens on repeated XML attributes in the rendered memory block. Current shape (renderMemoryLineV2 in packages/plugin/src/hooks/magic-context/inject-compartments.ts:1369):

```
<project-memory>
  <memory id="4787" category="PROJECT_RULES" importance="80">content…</memory>
  <memory id="6658" category="PROJECT_RULES" importance="75">content…</memory>
  <memory id="5152" category="ARCHITECTURE" importance="82">content…</memory>
</project-memory>
```

New shape (locked):

```
<project-memory>
<PROJECT_RULES>
#4787: content…
#6658: content…
</PROJECT_RULES>
<ARCHITECTURE>
#5152: content…
</ARCHITECTURE>
</project-memory>
```

Rules:
- One `<CATEGORY>` group tag per category present (canonical taxonomy order: PROJECT_RULES, ARCHITECTURE, CONSTRAINTS, CONFIG_VALUES, NAMING; any non-taxonomy category renders after those, alphabetically — do not drop unknown categories).
- Line = `#<id>: <content>`. Workspace-union foreign memories (the ones that today carry ` source="repo"`) render as `#<id> [<repo>]: <content>`.
- IMPORTANCE IS OFF THE WIRE ENTIRELY. It keeps its jobs that don't touch bytes: memorySelectionOrder / trimMemoriesToBudgetV2 still sort/trim by importance. CONSEQUENCE (deliberate, verify with a test): setMemoryClassification importance updates no longer change rendered bytes → classify-memories becomes fully cache-neutral. There is a memory/comment somewhere stating importance-writes are cache-destructive (renderMemoryLineV2 emits it) — update any such code comments to the new reality.
- Content escaping: keep escapeXmlContent for content (it can contain <, &). Ids are numeric. Category tags are trusted taxonomy strings but escape defensively anyway.
- Budget accounting MUST measure the same bytes as the wire (this is renderMemoryLineV2's existing doc-contract): trimMemoriesToBudgetV2 measures per-line cost with the new line shape + amortized group-tag overhead. Group tags: measure exactly — when a trim drops the last memory of a category, the group tag disappears too. Simplest correct approach: recompute the full block per trim iteration or measure lines + per-category constant; pick what keeps the existing trim loop shape and document it.

## Where the format lives (change ALL renderers to the same bytes)
1. OpenCode: renderMemoryLineV2 + renderMemoryBlockV2 (inject-compartments.ts). Both m0 baseline and any m1 arm that renders memory lines. Check renderM1's new-memories block and the <memory-updates> delta block (memory-updates lines that quote memory content/ids — storage-memory-mutation-log render sites): the delta block's update/merge entries should reference `#id` in the new style wherever they currently render <memory …> shapes; if the delta block has its own distinct format (not <memory> tags), leave its shape alone.
2. Pi: packages/pi-plugin inject-compartments-pi.ts uses the shared or mirrored render — find its memory render (search renderMemoryLine / renderMemoryBlock in pi-plugin). Keep byte-parity with OpenCode.
3. Rust: crates/mc-module/src/memory_render.rs render_memory_line/render_memory_block — byte-identical output to the TS renderer. The facade store unit (memory_tool.rs lexical search render?) — check whether any facade response renders <memory> XML lines; tool RESPONSES are not cache-relevant, do not churn them.
4. Guidance texts: search for guidance describing the memory block format (GUIDANCE_TEXT in crates/mc-module, packages/plugin/src/agents/magic-context-prompt.ts, Pi guidance). Add ONE short line where the block is described: memories render grouped by category as `#id: fact` lines; ctx_memory actions take the numeric id. Don't rewrite paragraphs — minimal touch.

## Cache transition
- OpenCode/Pi: NO new triggers. The new bytes appear at each session's next natural HARD fold (m0) / bust (m1). An old frozen m0 with the old format + new m1 with the new format coexisting mid-transition is FINE (they're independent blocks; nothing parses the old shape back). Verify no code PARSES the rendered <memory id=…> lines back (search for regexes/parsers over 'memory id=' — tag-content-primitives.ts has compartment-shape references; check ctx_search dedup and dashboard: they key on memory_block_ids column, not parsed bytes — verify).
- Rust module leg: bump the m0 content epoch so every module session takes ONE coordinated HARD on the new binary. Use the existing fold mechanism (the "mpe" component fold in compartment_coverage / M0ContentEpoch — the same mechanism the covered-system absorb used; find PROFILE_EPOCH/M0ContentEpoch fold and add or bump the appropriate GLOBAL component, e.g. a memory-render-format epoch component rendered only when non-zero, so this applies to ALL profiles, not just CC). Follow the omitted-at-zero pattern so future epoch-0 components stay byte-inert.
- Shadow byte-compare: TS and Rust renderers must produce identical bytes — regenerate the memory-render differential golden if one exists (search crates testdata for memory render golden; if none exists, ADD one now: TS generator emits cases incl. foreign-source attribution and multi-category grouping, Rust test matches byte-for-byte).

## Tests
- TS: renderMemoryBlockV2 new-shape unit tests (grouping, taxonomy order, unknown category, foreign [repo] attribution, escaping); trim test proving budget measured on new bytes (drop-last-of-category removes group tag from measurement); classify-neutrality test: setMemoryClassification importance change → rendered block bytes identical.
- Existing tests asserting the old <memory id=…> shape: update them (there will be many — inject-compartments tests, e2e cache-invariant fixtures if they pin memory bytes, Pi tests).
- Rust: render tests updated; differential golden green; epoch fold test (one HARD then defer stability — mirror the absorb transition test pattern in transform.rs).

## Gates
bun test packages/plugin + pi-plugin, tsc both, lint; cargo test -p mc-module -p mc-store, clippy -D warnings, fmt --check; real_daemon (binary ck-mc); shadow-wire fixture regen only if memory wire fields changed (they should NOT — this is render-side only; wire rows keep raw fields).
Commit logically, co-author trailer: Co-authored-by: Alfonso <alfonso-magic-context@users.noreply.github.com>
Comments explain rationale for context-free readers.

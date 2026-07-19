# Fix issue #226: shared config model picker ignores the Pi model list

Repo ~/Work/Projects/CortexKit/magic-context, branch subc-migration. Dashboard only (packages/dashboard). The reporter's diagnosis is verified-correct and maps the exact sites:

- `App.tsx` (~L82) fetches `getAvailablePiModels` and passes `piModels` to ConfigEditor.
- `ConfigEditor.tsx` (~L2035) `effectiveModels` ignores it — always returns `props.models` (the OpenCode list).
- `ModelSelect.tsx` (~L43) only enables free-text entry when `props.models.length === 0`, and refuses typed commits otherwise.

Net: after the config-tab consolidation (commit 78d3daa7), Pi-only model ids can neither be selected nor typed in the shared config editor's Historian/Dreamer/Sidekick/fallback pickers.

## Fix (two independent parts, both required)

### 1. Union model list, canonical ids
The shared config stores CANONICAL (OpenCode-style) provider prefixes; Pi discovery returns Pi-native prefixes for two providers (openai-codex/<m> ↔ canonical openai/<m>, google-antigravity/<m> ↔ canonical google/<m>; anthropic and others are identical). The plugin's mapping lives at packages/plugin/src/shared/harness-provider-map.ts — mirror the same two-entry mapping in the dashboard (small local util with a unit test; do NOT import across packages if the dashboard doesn't already depend on plugin src).

Build the effective list as: canonicalize(piModels) ∪ opencodeModels, deduped by exact id, sorted stably. Optionally annotate entries available from only one harness with a subtle suffix badge in the dropdown (e.g. "(Pi)" when only the Pi list had it, "(OpenCode)" when only OpenCode) — keep it text-simple, no new styling systems.

### 2. Free-text entry always available
ModelSelect must ALWAYS allow committing a typed value even when the list is non-empty (the current length===0 gate is the worse half of the bug). Keep the dropdown/search as the primary surface; add the standard combobox affordance: if the typed text matches nothing, offer 'Use "<typed>"' as the last option (or accept Enter on unmatched text) with a light shape check (must contain a slash: provider/model) — invalid shape shows the existing hint style rather than silently rejecting.

## Tests
- Unit: the canonicalization mapping (both providers, both directions where relevant, pass-through for unknown prefixes).
- Unit/component: effectiveModels union (Pi-only id present; duplicate collapsed; canonicalized form used).
- Component: ModelSelect commits a typed unlisted id when the list is non-empty; rejects a slashless string with the hint.
- Update any existing ModelSelect/ConfigEditor tests broken by the gate change.

## Gates
cd packages/dashboard && bun test green, tsc green, biome clean, frontend build (bun run build) green. Rust side untouched (data already flows). check_comments clean — comments explain WHY (shared config stores canonical prefixes; free text exists because discovery lists are never exhaustive), no issue numbers in code comments.

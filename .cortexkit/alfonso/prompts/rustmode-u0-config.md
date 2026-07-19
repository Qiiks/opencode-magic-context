# Rust MC mode — U0: `transform_mode` config knob

Part of the per-project Rust MC cutover (plan: `.alfonso/plans/rust-mc-mode-v1.md`, v2 — read the "Config and trust" section first; it is the contract this unit implements). This unit is CONFIG ONLY: schema, trust boundary, resolution helper, docs, tests. No runtime behavior change beyond exposing the resolved mode to callers.

## What to build

1. **Schema** (`packages/plugin/src/config/schema/magic-context.ts`):
   - New top-level field `transform_mode: z.enum(["ts", "rust"]).default("ts")` with a `.describe()` that says: routes the entire Magic Context runtime for the project through the ck-mc Rust module over subc (requires user-level `subc` config); `"ts"` is the current TypeScript pipeline.
   - Regenerate the JSON schema: `bun packages/plugin/scripts/build-schema.ts` (rule: Zod is the single source of truth).

2. **Trust boundary** (`packages/plugin/src/config/project-security.ts`):
   - `transform_mode` is ALLOWED at project tier — that is the point of the feature (a repo config flips that repo to rust mode). Do NOT strip it.
   - BUT the guard from the plan: the `"rust"` value only activates when the USER-tier config carries a `subc` block (`subc.connection_file`). Project tier can never name a socket path (`subc` is already stripped from project configs — verify the existing strip stays). Implement the activation guard in the RESOLUTION helper (below), not by stripping.

3. **Resolution helper** (new file `packages/plugin/src/config/transform-mode.ts`):
   ```ts
   export type ResolvedTransformMode = "ts" | "rust";
   export function resolveTransformMode(args: {
       configured: "ts" | "rust";
       userTierHasSubc: boolean;
       shadowTransformEnabled: boolean;
   }): { mode: ResolvedTransformMode; warnings: string[] }
   ```
   Rules (from the plan, verbatim contract):
   - `configured === "rust"` and `!userTierHasSubc` → mode "ts", warning: rust mode requires user-level subc configuration; running ts.
   - `configured === "rust"` and `shadowTransformEnabled` → mode "rust", warning: shadow_transform is ignored while transform_mode is "rust" (a session cannot shadow itself); shadow disabled for these sessions. (Mutual exclusion: rust wins, warn once per process per project — use a module-level warned set.)
   - Plumb the resolved mode into the per-session hook config surface the same way other config fields reach `buildMagicContextHookConfig` (`packages/plugin/src/plugin/hooks/create-session-hooks.ts`) — CRITICAL: spread-preserving, see the `buildMagicContextHookConfig` bug class where non-spread field mapping silently dropped new fields. Add the field access where the hook config is consumed so later units (U1) read `config.transform_mode` resolved, not raw.
   - The caveman warning from the plan's open items: if resolved mode is "rust" and `caveman_text_compression.enabled` is true, add a warning (caveman is TS-only and inert in rust mode). Do not change caveman behavior.

4. **Pi**: add the same schema field to Pi's config surface (`packages/pi-plugin` reuses the shared schema — verify; if Pi has its own copy/narrowing, mirror the field). Pi runtime ignores it for now (U7 is sequenced later); the field parsing must not warn/crash on Pi.

5. **Docs**: `CONFIGURATION.md` (or wherever top-level knobs are documented — find the existing pattern) gets the field with an honest "experimental, requires subc daemon" note. Dashboard config-parity gate: there is a CI test asserting schema/dashboard parity (`config-parity` — find it); classify the new field there the same way `shadow_transform` was classified (omitted-by-design from the dashboard editor for now).

## Tests

- Schema: parses "ts"/"rust", rejects other values, default "ts".
- Project-tier: a project config with `transform_mode: "rust"` SURVIVES `stripUnsafeProjectConfigFields` (explicit test), while `subc` in project config is still stripped.
- Resolution: all three rule branches + warn-once behavior.
- Schema-parity/config-parity CI gates green.
- Full suite: `cd packages/plugin && bun test`; `cd packages/pi-plugin && bun test`; CLI suite if the schema is shared there.

Commit in the worktree; do not push. Report any place where the existing config plumbing would silently drop the field (that bug class has bitten before — `buildMagicContextHookConfig` must spread).

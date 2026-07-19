# Fix #209: Pi historian spawn ENAMETOOLONG on Windows

All work in `packages/pi-plugin/src/subagent-runner.ts` (+ its test file). Run gates from `packages/pi-plugin/`.

## Root cause (verified)
Pi subagents (historian/dreamer/sidekick) spawn a `pi` subprocess. We pass the historian system prompt (~60KB) as ONE argv token: `args.push("--system-prompt", options.systemPrompt)` (in `buildArgs`, ~line 1121). Windows `CreateProcess` caps the ENTIRE command line at 32,767 chars, so `spawn()` fails with `ENAMETOOLONG` before Pi starts — every historian run on Windows Pi is dead. The existing large-prompt guard (`PROMPT_ARGV_MAX_BYTES = 96 * 1024`) was calibrated for Linux's 128KB per-arg limit and reroutes only the USER message to stdin; it never considered Windows' much smaller TOTAL cap and does not cover the system prompt.

## Pi CLI facts (source-confirmed against pi-coding-agent@0.80.2 by the Pi team — trust these)
- `--system-prompt <value>`: if `existsSync(value)` is true Pi reads that path with `readFileSync(value, "utf-8")` and uses the file contents as the (REPLACEMENT) system prompt; otherwise the value is literal text. Absolute paths required (no tilde expansion). No practical file-size limit.
- Do NOT use `--system-prompt "" --append-system-prompt <file>`: empty string falls back to Pi's default coding-assistant prompt.
- Even with a custom system prompt, Pi appends project context files (AGENTS.md / CLAUDE.md) unless `--no-context-files` is passed.
- stdin (print mode) feeds only the initial USER message; there is no stdin/env channel for system prompts.

## Changes (3)

### 1. System prompt via temp file — ALL platforms, one code path
In `runOnce` (~line 410): when `options.systemPrompt` is non-empty, write it to a unique temp file (UTF-8) and pass the ABSOLUTE path to `buildArgs` instead of the text.
- Unique per invocation: concurrent children must not collide (e.g. `mkdtempSync(join(tmpdir(), "mc-pi-prompt-"))` + a fixed filename inside, or a crypto-random filename; your call, keep it simple). `writeFileSync` is fine (small file).
- Write it just before the spawn block so the abort-before-spawn early return (~line 447) doesn't leak files; clean up (best-effort `rmSync`/`unlinkSync`, wrapped so it NEVER throws) in `settle()` AND on the spawn-failure catch path (~line 542). `settle` is idempotent (guarded by `settled`) so cleanup there is safe.
- `buildArgs` change: replace the `options.systemPrompt` push with a new param (e.g. `opts.systemPromptPath`); when present push `"--system-prompt", path`. Keep the existing comment's intent (replace-not-append rationale) and add WHY a file: Windows caps the whole command line at 32,767 chars and the historian prompt alone is ~60KB, and a file also removes the `existsSync` ambiguity (we always pass a path we created).
- This eliminates the argv-size class on every platform — do NOT make it win32-only.

### 2. User message via stdin unconditionally on win32
`deliverViaStdin` (~line 473) becomes: `promptBytes > PROMPT_ARGV_MAX_BYTES || platform === "win32"`. Keep the existing >96KB behavior for other platforms. The stdin plumbing already exists and works (pipe, end with EOF, EPIPE listener) — only the predicate changes.
- Platform must be injectable for tests: add `platform?: NodeJS.Platform` to the constructor options (like the existing `spawnImpl` seam), default `process.platform`, store on the instance.

### 3. Add `--no-context-files` to the base args (in `buildArgs`, near `--no-skills`/`--no-prompt-templates`)
Hidden one-shot subagents must receive EXACTLY our system prompt; Pi silently appending the project's AGENTS.md/CLAUDE.md is unintended prompt pollution and startup overhead. Comment should say that.

## Tests (extend `subagent-runner.test.ts` — it has a fake-child EventEmitter pattern and `spawnImpl`/`piBinary` seams already; follow the existing style)
1. System prompt lands as a temp-file path: spawn argv contains `--system-prompt <path>` where `<path>` is NOT the prompt text, the file exists at spawn time (assert from within the spawnImpl mock), and its contents are byte-identical to `options.systemPrompt`.
2. Temp file cleanup: after the run settles (success AND the spawn-throw path), the file is gone.
3. win32 stdin: with `platform: "win32"` and a SMALL user message, argv omits the positional message and stdin receives it; on `platform: "linux"` small messages stay positional (regression guard for the existing behavior).
4. argv total size: with a 60KB system prompt + small user message on win32, the joined argv length is well under 32,767.
5. `--no-context-files` present in argv.
6. Do not break existing tests — several assert on exact argv shapes (`toHaveBeenCalledWith`); update them for the new flags/paths deliberately, never by loosening assertions to vacuous.

## Gates (all must be green before commit)
- `cd packages/pi-plugin && bun test` (full suite, no filters)
- `bun run lint` (biome check, whole package) and `bunx tsc --noEmit` if a tsconfig gate exists (check package.json scripts; run what CI runs: the repo CI runs `biome check .` per package)
- Commit with a clear message explaining the Windows command-line cap root cause (no issue-number-only messages; reference #209 at the end).

## Cautions
- `resolvePromptInput` file semantics mean ANY existing-path-shaped literal system prompt would be misread by Pi — our temp-file approach sidesteps this; do not add a fallback that passes literal text.
- Do not touch the OpenCode plugin (packages/plugin) — its historian is a host-side fiber, unaffected.
- Do not change PROMPT_ARGV_MAX_BYTES.
- The runner loops runOnce per fallback model; per-runOnce temp files are fine (each cleans up its own).

# Reply draft for GitHub issue #411

Thanks to @Qiiks for the Windows OMP report and the process-model diagnosis. We confirmed it at source: `inspectLivePiProcesses()` on win32 used `tasklist /FO CSV`, so `command` was the image name only (`omp.exe`). `commandLooksLikePi` treats a bare `omp`/`pi` image as a live harness, and the only skip was `entry.pid === process.pid`.

On Windows, OMP/Pi is two processes for the whole session: `omp.exe`/`pi.exe` launcher (parent) → bun/node child where the extension runs. The launcher is therefore always visible as a "live Pi harness". `readProcessStartTime` also returned null on win32, so the blocker was labeled "started unverified". Restarting cannot clear it, because the new launcher is the same shape.

The guard now does three things together:

1. **Full process facts on Windows.** One PowerShell CIM query (`Win32_Process` → ProcessId, ParentProcessId, CommandLine, CreationDate). JSON is parsed for both a single object and an array, and CreationDate accepts the WMI `yyyyMMddHHmmss.ffffff±UUU` form. If PowerShell is missing or the query fails, tasklist remains the fallback, but image-name-only matches are **inconclusive** (same tri-state as the OpenCode sandbox probe): migration proceeds with an explicit warning, never as a verified live harness. CIM start times also feed `readProcessStartTime` on win32, so the PID-reuse start-time comparison works there.

2. **Ancestor exclusion.** Every PID on the parent chain of `process.pid` is skipped (CIM `ParentProcessId` on Windows; `ps -o ppid=` on POSIX). Walk depth is 16 and cycles stop the walk. The session's own launcher shim cannot be a blocker.

3. **Match on command line, not image.** A process is a verified Pi/OMP harness only when the command line carries a Pi/OMP arc (`pi-coding-agent`, `oh-my-pi`, `@oh-my-pi`, bun/node `dist`/`cljs` entry). Bare `omp.exe`/`pi.exe` is ambiguous → inconclusive.

Verified live older-build harnesses still block, and the refusal still names `PID N, started <time>, cmd: <cmdline>`. Ancestor skips and inconclusive matches each log one line so a later report can tell which arm fired.

This ships in v0.41.2. After upgrade, a pending shared-DB migration on Windows OMP should proceed in the current session instead of fail-closing on `omp.exe`. If a *different* OMP/Pi session is still running an older build, the guard continues to refuse until that process restarts.

# Historian temperature opt-in mutation report

All probes were run on 2026-08-31 and restored before final verification.

| Leg | Restored implicit value | Command | Failing assertion |
| --- | --- | --- | --- |
| OpenCode | Added `temperature: 0.1` to `resolveHistorianAgentOverrides` | `bun test src/agent-registration-drift.test.ts` in `packages/plugin` | `packages/plugin/src/agent-registration-drift.test.ts:106` |
| Rust producer | Passed `Some(0.1)` from `HistorianProducer::start` | `cargo test -p mc-module start_binds_session_at_route_open_and_omits_session_param` | `crates/mc-module/src/historian_producer.rs:1602` |
| Pi resolver | Restored `historian?.temperature ?? 0.1` | `bun test src/resolvers.test.ts` in `packages/pi-plugin` | `packages/pi-plugin/src/resolvers.test.ts:31` |
| Pi runner | Restored `options.temperature ?? 0.1` when constructing the child environment | `bun test src/subagent-runner.test.ts` in `packages/pi-plugin` | `packages/pi-plugin/src/subagent-runner.test.ts:1280` |

The omission assertions inspect the registered or serialized request shape rather than a downstream default, so a synthesized value fails before a provider can reinterpret it.

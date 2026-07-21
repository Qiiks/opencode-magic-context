//! Thin mc-module JSONC config reader for autonomous historian firing.
//!
//! This intentionally reads user and project tiers directly instead of depending on a
//! daemon config plane. Per-leaf trust policy is enforced during the read: model choice
//! is user-tier only because it affects spend; project config may only raise the execute
//! threshold (fire less often), and may override memory, promotion, privacy, and context-limit
//! settings. The Rust module intentionally keeps stricter model-selection policy than the current
//! TypeScript implementation until both implementations are deliberately aligned.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

/// Default execute threshold percentage (65.0). The Rust module reads config without the
/// plugin, so this must stay identical to packages/plugin/src/config/schema/magic-context.ts.
pub const DEFAULT_EXECUTE_THRESHOLD_PERCENTAGE: f64 = 65.0;
/// Default token budget for project-memory injection. It must remain 8,000 tokens so the Rust
/// module and the TypeScript renderer use the same default.
pub const DEFAULT_MEMORY_BUDGET_TOKENS: f64 = 8_000.0;
/// Default token budget for user-profile injection. It must remain 4,000 tokens so the Rust
/// module and the TypeScript renderer use the same default.
pub const DEFAULT_USER_PROFILE_BUDGET_TOKENS: f64 = 4_000.0;
/// Maximum execute threshold percentage (80.0). The Rust module reads config without the
/// plugin, so this must stay identical to packages/plugin/src/config/schema/magic-context.ts.
const MAX_EXECUTE_THRESHOLD_PERCENTAGE: f64 = 80.0;
/// Minimum historian producer chunk size. The derived budget is one quarter of the model
/// context limit, but it is never allowed to fall below 8,000 tokens.
pub const MIN_HISTORIAN_CHUNK_TOKENS: usize = 8_000;
/// Maximum historian producer chunk size. The derived budget is one quarter of the model
/// context limit, but it is never allowed to exceed 50,000 tokens.
pub const MAX_HISTORIAN_CHUNK_TOKENS: usize = 50_000;
/// No module-side model catalog exposes historian context limits yet. Keep the fallback
/// explicit and configurable instead of silently assuming the session model's limit.
pub const DEFAULT_HISTORIAN_CONTEXT_LIMIT_TOKENS: usize = 32_000;

/// Derive the historian producer budget from its own context window, as the TS runner does.
pub fn derive_historian_chunk_tokens(context_limit_tokens: usize) -> usize {
    (((context_limit_tokens as f64) * 0.25).round() as usize)
        .clamp(MIN_HISTORIAN_CHUNK_TOKENS, MAX_HISTORIAN_CHUNK_TOKENS)
}

#[derive(Debug, Clone, PartialEq)]
pub struct McModuleConfig {
    pub model_chain: Vec<String>,
    pub execute_threshold_percentage: f64,
    pub memory_enabled: bool,
    /// Mirrors the TS auto-promote switch. Facts are dropped when this is false.
    pub auto_promote: bool,
    /// Privacy gate controlling whether historian user observations may be collected for later
    /// review and promotion.
    pub user_memory_collection_enabled: bool,
    /// Historian model context limit; configurable until the module has a model catalog.
    pub historian_context_limit_tokens: usize,
    pub memory_budget_tokens: f64,
    pub user_profile_budget_tokens: f64,
    pub smart_drops: bool,
    pub cache_ttl: String,
    /// Kill switch for the shadow byte-compare lane, honored module-side so a
    /// runaway shadow loop can be stopped by a config flip plus module bounce
    /// without restarting any harness process (plugin senders are constructed
    /// once per session hook and hold the old flag until their host restarts).
    pub shadow_enabled: bool,
}

impl Default for McModuleConfig {
    fn default() -> Self {
        Self {
            model_chain: Vec::new(),
            execute_threshold_percentage: DEFAULT_EXECUTE_THRESHOLD_PERCENTAGE,
            memory_enabled: true,
            auto_promote: true,
            user_memory_collection_enabled: false,
            historian_context_limit_tokens: DEFAULT_HISTORIAN_CONTEXT_LIMIT_TOKENS,
            memory_budget_tokens: DEFAULT_MEMORY_BUDGET_TOKENS,
            user_profile_budget_tokens: DEFAULT_USER_PROFILE_BUDGET_TOKENS,
            smart_drops: false,
            shadow_enabled: true,
            cache_ttl: "5m".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default)]
struct TierConfig {
    path: PathBuf,
    mtime: Option<SystemTime>,
    value: Option<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct ConfigCache {
    user: TierConfig,
    project: TierConfig,
    effective: McModuleConfig,
}

impl ConfigCache {
    pub fn effective_for_project(&mut self, project_root: &Path) -> McModuleConfig {
        let user_path = user_config_path();
        self.effective_for_paths(&user_path, project_root)
    }

    pub fn effective_for_paths(&mut self, user_path: &Path, project_root: &Path) -> McModuleConfig {
        let project_path = project_root.join(".cortexkit").join("magic-context.jsonc");
        let user = read_tier_cached(&mut self.user, user_path.to_path_buf());
        let project = read_tier_cached(&mut self.project, project_path);
        self.effective = merge_tiers(user.as_ref(), project.as_ref());
        self.effective.clone()
    }
}

fn user_config_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        return PathBuf::from(xdg)
            .join("cortexkit")
            .join("magic-context.jsonc");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home)
        .join(".config")
        .join("cortexkit")
        .join("magic-context.jsonc")
}

fn read_tier_cached(cache: &mut TierConfig, path: PathBuf) -> Option<Value> {
    let mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();
    if cache.path == path && cache.mtime == mtime {
        return cache.value.clone();
    }
    cache.path = path.clone();
    cache.mtime = mtime;
    cache.value = match fs::read_to_string(&path) {
        Ok(raw) => serde_json::from_str(&strip_jsonc(&raw)).ok(),
        Err(_) => None,
    };
    cache.value.clone()
}

fn merge_tiers(user: Option<&Value>, project: Option<&Value>) -> McModuleConfig {
    let mut cfg = McModuleConfig::default();

    if let Some(user) = user {
        // Module-leg model override. The shared config file serves two consumers whose
        // model namespaces differ: the TS plugin resolves harness-namespace ids (e.g.
        // OpenCode's auth plugins register "google/antigravity-gemini-3.5-flash"),
        // while this module drives llm-runner, whose catalog uses canonical ids
        // ("google/gemini-3.5-flash" + a vault auth method). When module_model is
        // present it REPLACES the plugin-namespace chain entirely (no mixing — a
        // half-translated chain would burn permanent-classified advances every fire);
        // when absent, fall back to the plugin keys so single-namespace setups keep
        // working with one set of keys.
        let module_model = user
            .pointer("/historian/module_model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(model) = module_model {
            cfg.model_chain.push(model.to_string());
            if let Some(fallbacks) = user
                .pointer("/historian/module_fallback_models")
                .and_then(Value::as_array)
            {
                cfg.model_chain.extend(
                    fallbacks
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(ToOwned::to_owned),
                );
            }
        } else {
            if let Some(model) = user.pointer("/historian/model").and_then(Value::as_str) {
                if !model.trim().is_empty() {
                    cfg.model_chain.push(model.trim().to_string());
                }
            }
            if let Some(fallbacks) = user
                .pointer("/historian/fallback_models")
                .and_then(Value::as_array)
            {
                cfg.model_chain.extend(
                    fallbacks
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(ToOwned::to_owned),
                );
            }
        }
        if let Some(threshold) = number_at(user, "/execute_threshold_percentage") {
            cfg.execute_threshold_percentage = threshold;
        }
        if let Some(enabled) = user.pointer("/memory/enabled").and_then(Value::as_bool) {
            cfg.memory_enabled = enabled;
        }
        if let Some(budget) = number_at(user, "/memory/budget_tokens") {
            cfg.memory_budget_tokens = budget.max(1.0);
        }
        if let Some(budget) = number_at(user, "/memory/user_profile_budget_tokens") {
            cfg.user_profile_budget_tokens = budget.max(1.0);
        }
        if let Some(enabled) = user
            .pointer("/memory/auto_promote")
            .and_then(Value::as_bool)
        {
            cfg.auto_promote = enabled;
        }
        if let Some(enabled) = user_memory_collection_at(user) {
            cfg.user_memory_collection_enabled = enabled;
        }
        if let Some(limit) = positive_usize_at(user, "/historian/context_limit_tokens") {
            cfg.historian_context_limit_tokens = limit;
        }
        if let Some(enabled) = user
            .pointer("/shadow_transform/enabled")
            .and_then(Value::as_bool)
        {
            cfg.shadow_enabled = enabled;
        }
        if let Some(enabled) = user.pointer("/smart_drops").and_then(Value::as_bool) {
            cfg.smart_drops = enabled;
        }
        if let Some(cache_ttl) = user.pointer("/cache_ttl").and_then(Value::as_str) {
            if !cache_ttl.trim().is_empty() {
                cfg.cache_ttl = cache_ttl.trim().to_string();
            }
        }
    }

    if let Some(project) = project {
        if let Some(project_threshold) = number_at(project, "/execute_threshold_percentage") {
            if project_threshold > cfg.execute_threshold_percentage {
                cfg.execute_threshold_percentage = project_threshold;
            }
        }
        if let Some(enabled) = project.pointer("/memory/enabled").and_then(Value::as_bool) {
            cfg.memory_enabled = enabled;
        }
        if let Some(enabled) = project
            .pointer("/memory/auto_promote")
            .and_then(Value::as_bool)
        {
            cfg.auto_promote = enabled;
        }
        if let Some(enabled) = user_memory_collection_at(project) {
            cfg.user_memory_collection_enabled = enabled;
        }
        if let Some(limit) = positive_usize_at(project, "/historian/context_limit_tokens") {
            cfg.historian_context_limit_tokens = limit;
        }
        if let Some(budget) = number_at(project, "/memory/budget_tokens") {
            cfg.memory_budget_tokens = budget.max(1.0);
        }
        if let Some(budget) = number_at(project, "/memory/user_profile_budget_tokens") {
            cfg.user_profile_budget_tokens = budget.max(1.0);
        }
        if let Some(enabled) = project.pointer("/smart_drops").and_then(Value::as_bool) {
            cfg.smart_drops = enabled;
        }
    }

    cfg.execute_threshold_percentage = cfg
        .execute_threshold_percentage
        .clamp(1.0, MAX_EXECUTE_THRESHOLD_PERCENTAGE);
    cfg.model_chain.dedup();
    cfg
}

fn user_memory_collection_at(value: &Value) -> Option<bool> {
    if let Some(schedule) = value
        .pointer("/dreamer/tasks/review-user-memories/schedule")
        .and_then(Value::as_str)
    {
        return Some(!schedule.trim().is_empty());
    }
    value
        .pointer("/user_memories/enabled")
        .and_then(Value::as_bool)
}

fn positive_usize_at(value: &Value, pointer: &str) -> Option<usize> {
    value
        .pointer(pointer)
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .filter(|v| *v > 0)
}

fn number_at(value: &Value, pointer: &str) -> Option<f64> {
    value
        .pointer(pointer)
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite())
}

/// Strip JSONC line/block comments and trailing commas while respecting string literals.
/// The module only consumes its own config convention; this is not a general JSONC parser.
pub fn strip_jsonc(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        let next = chars.get(i + 1).copied().unwrap_or('\0');
        if c == '/' && next == '/' {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && next == '*' {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(chars.len());
            continue;
        }
        if c == ',' {
            let mut k = i + 1;
            loop {
                while k < chars.len() && chars[k].is_whitespace() {
                    k += 1;
                }
                if k + 1 < chars.len() && chars[k] == '/' && chars[k + 1] == '/' {
                    k += 2;
                    while k < chars.len() && chars[k] != '\n' {
                        k += 1;
                    }
                    continue;
                }
                if k + 1 < chars.len() && chars[k] == '/' && chars[k + 1] == '*' {
                    k += 2;
                    while k + 1 < chars.len() && !(chars[k] == '*' && chars[k + 1] == '/') {
                        k += 1;
                    }
                    k = (k + 2).min(chars.len());
                    continue;
                }
                break;
            }
            if k < chars.len() && matches!(chars[k], '}' | ']') {
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {

    #[test]
    fn shadow_transform_flag_parses_and_defaults_on() {
        let cfg = merge_tiers(None, None);
        assert!(cfg.shadow_enabled);
        let user: Value =
            serde_json::from_str(r#"{ "shadow_transform": { "enabled": false } }"#).unwrap();
        let cfg = merge_tiers(Some(&user), None);
        assert!(!cfg.shadow_enabled);
        let user: Value =
            serde_json::from_str(r#"{ "shadow_transform": { "enabled": true } }"#).unwrap();
        let cfg = merge_tiers(Some(&user), None);
        assert!(cfg.shadow_enabled);
    }
    use super::*;

    #[test]
    fn tier_policy_ignores_project_models_and_rejects_project_lowering() {
        let user = serde_json::json!({
            "historian": { "model": "cheap", "fallback_models": ["fallback"] },
            "execute_threshold_percentage": 80,
            "memory": { "enabled": false }
        });
        let project = serde_json::json!({
            "historian": { "model": "expensive", "fallback_models": ["expensive2"] },
            "execute_threshold_percentage": 40,
            "memory": { "enabled": true }
        });
        let cfg = merge_tiers(Some(&user), Some(&project));
        assert_eq!(cfg.model_chain, vec!["cheap", "fallback"]);
        assert_eq!(cfg.execute_threshold_percentage, 80.0);
        assert!(cfg.memory_enabled);
    }

    #[test]
    fn project_threshold_may_only_raise() {
        let user = serde_json::json!({ "execute_threshold_percentage": 70 });
        let project = serde_json::json!({ "execute_threshold_percentage": 90 });
        let cfg = merge_tiers(Some(&user), Some(&project));
        assert_eq!(cfg.execute_threshold_percentage, 80.0);
    }

    #[test]
    fn default_threshold_matches_typescript_schema() {
        let cfg = merge_tiers(None, None);
        assert_eq!(cfg.execute_threshold_percentage, 65.0);
    }

    #[test]
    fn historian_budget_derivation_clamps_at_both_bounds() {
        assert_eq!(derive_historian_chunk_tokens(1), 8_000);
        assert_eq!(derive_historian_chunk_tokens(32_000), 8_000);
        assert_eq!(derive_historian_chunk_tokens(128_000), 32_000);
        assert_eq!(derive_historian_chunk_tokens(200_000), 50_000);
        assert_eq!(derive_historian_chunk_tokens(400_000), 50_000);
    }

    #[test]
    fn historian_gates_and_context_limit_parse_from_user_and_project_tiers() {
        let user = serde_json::json!({
            "memory": { "auto_promote": false },
            "dreamer": { "tasks": { "review-user-memories": { "schedule": "daily" } } },
            "historian": { "context_limit_tokens": 128000 }
        });
        let project = serde_json::json!({
            "memory": { "auto_promote": true },
            "user_memories": { "enabled": false },
            "historian": { "context_limit_tokens": 64000 }
        });
        assert!(user_memory_collection_at(&user).unwrap());
        let cfg = merge_tiers(Some(&user), Some(&project));
        assert!(cfg.auto_promote);
        assert!(!cfg.user_memory_collection_enabled);
        assert_eq!(cfg.historian_context_limit_tokens, 64_000);
        let legacy_disabled = serde_json::json!({
            "user_memories": { "enabled": false }
        });
        assert!(!user_memory_collection_at(&legacy_disabled).unwrap());
    }

    #[test]
    fn module_model_replaces_plugin_chain_entirely() {
        let user = serde_json::json!({
            "historian": {
                "model": "google/antigravity-gemini-3.5-flash",
                "fallback_models": ["google/antigravity-claude-opus-4-6-thinking"],
                "module_model": "google/gemini-3.5-flash",
                "module_fallback_models": ["ollama-cloud/kimi-k2.7-code"]
            }
        });
        let cfg = merge_tiers(Some(&user), None);
        // No plugin-namespace ids may leak into the module chain — a mixed chain
        // burns a permanent-classified advance on every historian fire.
        assert_eq!(
            cfg.model_chain,
            vec!["google/gemini-3.5-flash", "ollama-cloud/kimi-k2.7-code"]
        );
    }

    #[test]
    fn module_model_absent_falls_back_to_plugin_keys() {
        let user = serde_json::json!({
            "historian": {
                "model": "deepseek/deepseek-v4-flash",
                "fallback_models": ["ollama-cloud/kimi-k2.7-code"],
                "module_fallback_models": ["ignored/without-module-model"]
            }
        });
        let cfg = merge_tiers(Some(&user), None);
        assert_eq!(
            cfg.model_chain,
            vec!["deepseek/deepseek-v4-flash", "ollama-cloud/kimi-k2.7-code"]
        );
    }

    #[test]
    fn module_model_blank_is_treated_as_absent() {
        let user = serde_json::json!({
            "historian": {
                "model": "deepseek/deepseek-v4-flash",
                "module_model": "   "
            }
        });
        let cfg = merge_tiers(Some(&user), None);
        assert_eq!(cfg.model_chain, vec!["deepseek/deepseek-v4-flash"]);
    }

    #[test]
    fn module_model_is_user_tier_only() {
        let user = serde_json::json!({
            "historian": { "module_model": "google/gemini-3.5-flash" }
        });
        let project = serde_json::json!({
            "historian": {
                "module_model": "evil/expensive-model",
                "module_fallback_models": ["evil/other"]
            }
        });
        let cfg = merge_tiers(Some(&user), Some(&project));
        assert_eq!(cfg.model_chain, vec!["google/gemini-3.5-flash"]);
    }

    #[test]
    fn jsonc_strip_preserves_comment_like_strings() {
        let parsed: Value = serde_json::from_str(&strip_jsonc(
            r#"{ "url": "http://x/y", "a": [1,], /* c */ }"#,
        ))
        .unwrap();
        assert_eq!(parsed["url"], "http://x/y");
        assert_eq!(parsed["a"], serde_json::json!([1]));
    }

    #[test]
    fn mtime_cache_reuses_unchanged_reads_and_invalidates_on_mtime_change() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user.jsonc");
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join(".cortexkit")).unwrap();

        std::fs::write(&user, r#"{ "historian": { "model": "model-a" } }"#).unwrap();
        std::fs::write(
            project.join(".cortexkit/magic-context.jsonc"),
            r#"{ "memory": { "enabled": true } }"#,
        )
        .unwrap();

        let mut cache = ConfigCache::default();
        let first = cache.effective_for_paths(&user, &project);
        assert_eq!(first.model_chain, vec!["model-a"]);

        // Without an mtime change, a different file body is intentionally ignored.
        let original_mtime = std::fs::metadata(&user).unwrap().modified().unwrap();
        std::fs::write(&user, r#"{ "historian": { "model": "model-b" } }"#).unwrap();
        filetime::set_file_mtime(&user, filetime::FileTime::from_system_time(original_mtime))
            .unwrap();
        let unchanged = cache.effective_for_paths(&user, &project);
        assert_eq!(unchanged.model_chain, vec!["model-a"]);

        // Once mtime changes, the cache reloads and picks up the new user-tier model.
        let newer = filetime::FileTime::from_unix_time(
            original_mtime
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs() as i64
                + 2,
            0,
        );
        filetime::set_file_mtime(&user, newer).unwrap();
        let reloaded = cache.effective_for_paths(&user, &project);
        assert_eq!(reloaded.model_chain, vec!["model-b"]);
    }
}

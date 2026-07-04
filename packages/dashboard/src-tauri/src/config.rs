use serde::{Deserialize, Serialize};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Resolves paths to magic-context config files.
pub fn resolve_user_config_path() -> PathBuf {
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".config"));
    config_dir.join("cortexkit").join("magic-context.jsonc")
}

/// Resolves the Pi user-level magic-context config path.
/// Harness-agnostic: Pi reads the same CortexKit user config as OpenCode.
pub fn resolve_pi_config_path() -> PathBuf {
    resolve_user_config_path()
}

/// Resolve the project-level magic-context config path (CortexKit layout).
pub fn resolve_project_config_path(project_path: &str) -> PathBuf {
    PathBuf::from(project_path)
        .join(".cortexkit")
        .join("magic-context.jsonc")
}

/// Canonical dreamer task names (mirrors the plugin's task registry and the
/// frontend DreamerTasksField list). The dashboard renders this fixed set so
/// every project shows the same tasks regardless of its (possibly stale) per-
/// project scheduler snapshot in task_schedule_state.
pub const CANONICAL_DREAM_TASKS: [&str; 11] = [
    "map-memories",
    "verify",
    "verify-broad",
    "curate",
    "classify-memories",
    "retrospective",
    "maintain-docs",
    "evaluate-smart-notes",
    "review-user-memories",
    "promote-primers",
    "refresh-primers",
];

/// Default cron per task (mirrors DEFAULT_TASK_SCHEDULES in the plugin schema and
/// the frontend). Applied when neither the project nor the global config sets a
/// schedule. maintain-docs defaults OFF (empty).
pub fn default_task_schedule(task: &str) -> &'static str {
    match task {
        "map-memories" => "0 2 * * *",
        "verify" => "0 3 * * *",
        "verify-broad" => "0 4 * * 0",
        "curate" => "0 4 * * 0",
        "classify-memories" => "0 6 * * *",
        "retrospective" => "0 5 * * *",
        "maintain-docs" => "",
        "evaluate-smart-notes" => "0 3 * * *",
        "review-user-memories" => "0 3 * * *",
        "promote-primers" => "0 3 * * *",
        "refresh-primers" => "0 3 * * *",
        _ => "",
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ConfigFile {
    pub path: String,
    pub exists: bool,
    pub content: Option<String>,
    pub source: String, // "user", "pi", or "project"
    pub error: Option<String>,
}

pub type ConfigFileResponse = ConfigFile;

pub fn read_config(path: &PathBuf, source: &str) -> ConfigFile {
    let base = || ConfigFile {
        path: path.to_string_lossy().to_string(),
        exists: false,
        content: None,
        source: source.to_string(),
        error: None,
    };

    // Preserve read failures instead of treating them as empty files; the
    // structured editor would otherwise write that empty state over API keys.
    match std::fs::symlink_metadata(path) {
        Ok(_) => match std::fs::read_to_string(path) {
            Ok(content) => ConfigFile {
                exists: true,
                content: Some(content),
                ..base()
            },
            Err(e) => ConfigFile {
                exists: true,
                error: Some(format!("Failed to read config: {e}")),
                ..base()
            },
        },
        Err(e) if e.kind() == ErrorKind::NotFound => base(),
        Err(e) => ConfigFile {
            exists: true,
            error: Some(format!("Failed to inspect config path: {e}")),
            ..base()
        },
    }
}

pub fn write_config(path: &Path, content: &str) -> Result<(), String> {
    write_config_atomic(path, content, None)
}

pub fn write_project_config(project_path: &str, path: &Path, content: &str) -> Result<(), String> {
    let canonical_project = Path::new(project_path)
        .canonicalize()
        .map_err(|e| format!("Invalid project path: {e}"))?;
    validate_project_config_target(&canonical_project, path)?;
    write_config_atomic(path, content, Some(&canonical_project))
}

fn validate_project_config_target(canonical_project: &Path, path: &Path) -> Result<(), String> {
    let expected_config = canonical_project
        .join(".cortexkit")
        .join("magic-context.jsonc");
    let abs_path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        canonical_project.join(path)
    };
    if abs_path != expected_config {
        return Err("Config path is outside the project directory".to_string());
    }

    let parent = expected_config
        .parent()
        .ok_or_else(|| "Config path has no parent directory".to_string())?;
    let canonical_parent = if parent.exists() {
        parent
            .canonicalize()
            .map_err(|e| format!("Invalid config directory: {e}"))?
    } else {
        parent.to_path_buf()
    };
    if !canonical_parent.starts_with(canonical_project) {
        return Err("Config path is outside the project directory".to_string());
    }

    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                return Err(
                    "Refusing to write project config because the config path is a symlink"
                        .to_string(),
                );
            }
            if !file_type.is_file() {
                return Err(
                    "Refusing to write project config because the config path is not a regular file"
                        .to_string(),
                );
            }
            validate_existing_file_no_follow(path)?;
            let canonical_target = path
                .canonicalize()
                .map_err(|e| format!("Invalid config file path: {e}"))?;
            if !canonical_target.starts_with(canonical_project) {
                return Err("Config file resolves outside the project directory".to_string());
            }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => return Err(format!("Failed to inspect config path: {e}")),
    }

    Ok(())
}

#[cfg(unix)]
fn validate_existing_file_no_follow(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map(|_| ())
        .map_err(|e| format!("Failed to open config without following symlinks: {e}"))
}

#[cfg(not(unix))]
fn validate_existing_file_no_follow(_path: &Path) -> Result<(), String> {
    Ok(())
}

fn write_config_atomic(
    path: &Path,
    content: &str,
    project_root: Option<&Path>,
) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Config path has no parent directory".to_string())?;
    std::fs::create_dir_all(parent).map_err(|e| format!("Failed to create directory: {e}"))?;

    let temp_path = create_temp_config_file(parent, path.file_name(), content)?;
    if let Some(root) = project_root {
        if let Err(e) = validate_project_config_target(root, path) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(e);
        }
    }

    replace_with_temp(&temp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&temp_path);
        format!("Failed to replace config atomically: {e}")
    })
}

fn create_temp_config_file(
    parent: &Path,
    file_name: Option<&std::ffi::OsStr>,
    content: &str,
) -> Result<PathBuf, String> {
    let file_name = file_name
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("magic-context.jsonc");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    for attempt in 0..16u8 {
        let temp_path = parent.join(format!(
            ".{file_name}.{}.{}.{}.tmp",
            std::process::id(),
            stamp,
            attempt
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        match options.open(&temp_path) {
            Ok(mut file) => {
                if let Err(e) = file.write_all(content.as_bytes()) {
                    let _ = std::fs::remove_file(&temp_path);
                    return Err(format!("Failed to write config: {e}"));
                }
                if let Err(e) = file.sync_all() {
                    let _ = std::fs::remove_file(&temp_path);
                    return Err(format!("Failed to sync config: {e}"));
                }
                return Ok(temp_path);
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("Failed to create temporary config: {e}")),
        }
    }

    Err("Failed to create a unique temporary config path".to_string())
}

#[cfg(not(windows))]
fn replace_with_temp(temp_path: &Path, path: &Path) -> std::io::Result<()> {
    std::fs::rename(temp_path, path)
}

#[cfg(windows)]
fn replace_with_temp(temp_path: &Path, path: &Path) -> std::io::Result<()> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(temp_path, path)
}

#[tauri::command(async)]
pub fn read_pi_config() -> Result<ConfigFileResponse, String> {
    let path = resolve_pi_config_path();
    Ok(read_config(&path, "pi"))
}

#[tauri::command(async)]
pub fn write_pi_config(content: String) -> Result<(), String> {
    let path = resolve_pi_config_path();
    write_config(&path, &content)
}

#[tauri::command]
pub fn pi_config_path() -> String {
    resolve_pi_config_path().to_string_lossy().to_string()
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ProjectConfigEntry {
    pub project_name: String,
    pub worktree: String,
    pub config_path: String,
    pub exists: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alt_config_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alt_exists: Option<bool>,
}

/// Discover projects with magic-context config files by scanning OpenCode project worktrees.
pub fn discover_project_configs() -> Vec<ProjectConfigEntry> {
    let opencode_db = {
        let data_dir = if cfg!(target_os = "windows") {
            match dirs::data_dir() {
                Some(d) => d,
                None => return vec![],
            }
        } else {
            std::env::var("XDG_DATA_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| {
                    dirs::home_dir()
                        .unwrap_or_default()
                        .join(".local")
                        .join("share")
                })
        };
        data_dir.join("opencode").join("opencode.db")
    };

    if !opencode_db.exists() {
        return vec![];
    }

    let conn = match rusqlite::Connection::open_with_flags(
        &opencode_db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let mut stmt = match conn.prepare("SELECT name, worktree FROM project") {
        Ok(s) => s,
        Err(_) => return vec![],
    };

    let rows: Vec<(String, String)> = match stmt.query_map([], |row| {
        Ok((
            row.get::<_, Option<String>>(0)?.unwrap_or_default(),
            row.get::<_, String>(1)?,
        ))
    }) {
        Ok(mapped) => mapped.flatten().collect(),
        Err(_) => return vec![],
    };

    let mut entries = Vec::new();
    for (name, worktree) in rows {
        let config_path = resolve_project_config_path(&worktree);
        if !config_path.exists() {
            continue;
        }

        let display_name = if name.is_empty() {
            std::path::Path::new(&worktree)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| worktree.clone())
        } else {
            name
        };

        entries.push(ProjectConfigEntry {
            project_name: display_name,
            worktree: worktree.clone(),
            config_path: config_path.to_string_lossy().to_string(),
            exists: true,
            alt_config_path: None,
            alt_exists: None,
        });
    }

    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_config_path_matches_cortexkit_user_path() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", dir.path());

        let expected = dir.path().join("cortexkit/magic-context.jsonc");
        assert_eq!(resolve_pi_config_path(), expected);
        assert_eq!(resolve_user_config_path(), expected);
        assert_eq!(pi_config_path(), expected.to_string_lossy());

        let missing = read_pi_config().unwrap();
        assert_eq!(missing.path, expected.to_string_lossy());
        assert!(!missing.exists);
        assert_eq!(missing.content.as_deref(), None);
        assert_eq!(missing.source, "pi");
        assert!(missing.error.is_none());

        let content = "{\n  \"enabled\": true\n}";
        write_pi_config(content.to_string()).unwrap();

        let existing = read_pi_config().unwrap();
        assert!(existing.exists);
        assert_eq!(existing.content.as_deref(), Some(content));
        assert_eq!(existing.source, "pi");
        assert!(existing.error.is_none());

        std::env::remove_var("XDG_CONFIG_HOME");
    }

    #[test]
    fn read_config_reports_absent_without_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.jsonc");

        let config = read_config(&path, "user");

        assert!(!config.exists);
        assert_eq!(config.content.as_deref(), None);
        assert!(config.error.is_none());
    }

    #[test]
    fn read_config_reports_error_for_present_unreadable_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("magic-context.jsonc");
        std::fs::create_dir(&path).unwrap();

        let config = read_config(&path, "user");

        assert!(config.exists);
        assert_eq!(config.content.as_deref(), None);
        let error = config.error.as_deref().unwrap_or("");
        assert!(
            error.contains("Failed to read config"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn write_project_config_writes_regular_file_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let canonical_project = project.canonicalize().unwrap();
        let path = canonical_project.join(".cortexkit/magic-context.jsonc");

        write_project_config(
            canonical_project.to_str().unwrap(),
            &path,
            "{\n  \"enabled\": true\n}\n",
        )
        .expect("initial write");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"enabled\": true\n}\n"
        );

        write_project_config(
            canonical_project.to_str().unwrap(),
            &path,
            "{\n  \"enabled\": false\n}\n",
        )
        .expect("overwrite regular file");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"enabled\": false\n}\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_project_config_refuses_symlink_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, "do not overwrite").unwrap();
        let canonical_project = project.canonicalize().unwrap();
        let config_path = canonical_project.join(".cortexkit/magic-context.jsonc");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        symlink(&outside, &config_path).unwrap();

        let err = write_project_config(
            canonical_project.to_str().unwrap(),
            &config_path,
            "{\"enabled\":true}\n",
        )
        .expect_err("symlinked config must be refused");
        assert!(err.contains("symlink"), "unexpected error: {err}");
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "do not overwrite"
        );
    }

    #[test]
    fn write_project_config_refuses_non_regular_target() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let canonical_project = project.canonicalize().unwrap();
        let config_path = canonical_project.join(".cortexkit/magic-context.jsonc");
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::create_dir(&config_path).unwrap();

        let err = write_project_config(canonical_project.to_str().unwrap(), &config_path, "{}\n")
            .expect_err("directory target must be refused");
        assert!(err.contains("regular file"), "unexpected error: {err}");
    }
}

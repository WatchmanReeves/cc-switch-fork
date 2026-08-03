use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::session_manager::{SessionMessage, SessionMeta};

use super::utils::{
    extract_text, parse_timestamp_to_ms, path_basename, truncate_summary, TITLE_MAX_CHARS,
};

const PROVIDER_ID: &str = "pi";
const MAX_TREE_ENTRIES: usize = 500_000;
const MAX_TREE_ID_BYTES: usize = 256;
const MAX_SCAN_DEPTH: usize = 8;
const MAX_SESSION_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
enum SessionRootResolution {
    Available {
        root: PathBuf,
        source: &'static str,
    },
    RequiresProjectContext {
        configured_path: String,
        source: &'static str,
    },
    Unavailable {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PiSessionDiscovery {
    Available {
        root: String,
        source: &'static str,
    },
    RequiresProjectContext {
        #[serde(rename = "configuredPath")]
        configured_path: String,
        source: &'static str,
    },
    Unavailable {
        reason: String,
    },
}

#[derive(Debug)]
struct SessionHeader {
    id: String,
    cwd: String,
    timestamp: Option<i64>,
    version: u64,
}

#[derive(Debug)]
struct SessionTree {
    header: SessionHeader,
    active_ids: HashSet<String>,
}

#[derive(Default)]
struct ActiveSessionData {
    messages: Vec<SessionMessage>,
    first_user_message: Option<String>,
    last_message: Option<String>,
    explicit_name: Option<Option<String>>,
    last_active_at: Option<i64>,
}

/// Pi keeps a relative `sessionDir` relative through SessionManager creation;
/// its file operations therefore depend on the launching process cwd. A global
/// session browser has no authoritative launch cwd, so relative values are
/// deliberately non-enumerable and never fall back to another root.
pub fn session_roots() -> Vec<PathBuf> {
    match resolve_session_root() {
        SessionRootResolution::Available { root, .. } => vec![root],
        SessionRootResolution::RequiresProjectContext { .. }
        | SessionRootResolution::Unavailable { .. } => Vec::new(),
    }
}

pub fn session_discovery() -> PiSessionDiscovery {
    match resolve_session_root() {
        SessionRootResolution::Available { root, source } => PiSessionDiscovery::Available {
            root: root.to_string_lossy().into_owned(),
            source,
        },
        SessionRootResolution::RequiresProjectContext {
            configured_path,
            source,
        } => PiSessionDiscovery::RequiresProjectContext {
            configured_path,
            source,
        },
        SessionRootResolution::Unavailable { reason } => PiSessionDiscovery::Unavailable { reason },
    }
}

fn resolve_session_root() -> SessionRootResolution {
    let home = crate::config::get_home_dir();
    if let Some(raw) = std::env::var_os("PI_CODING_AGENT_SESSION_DIR") {
        if !raw.is_empty() {
            return classify_configured_session_dir(
                raw.to_string_lossy().as_ref(),
                &home,
                "environment",
            );
        }
    }

    match crate::pi_config::native_settings::read_pi_native_defaults() {
        Ok(defaults) => {
            if let Some(value) = defaults.session_dir.filter(|value| !value.is_empty()) {
                return classify_configured_session_dir(&value, &home, "settings");
            }
        }
        Err(error) => {
            return SessionRootResolution::Unavailable {
                reason: error.to_string(),
            };
        }
    }

    match crate::pi_config::native::get_pi_agent_dir() {
        Ok(agent_dir) => SessionRootResolution::Available {
            root: agent_dir.join("sessions"),
            source: "default",
        },
        Err(error) => SessionRootResolution::Unavailable {
            reason: error.to_string(),
        },
    }
}

fn classify_configured_session_dir(
    value: &str,
    home: &Path,
    source: &'static str,
) -> SessionRootResolution {
    match resolve_global_session_dir(value, home) {
        Some(root) => SessionRootResolution::Available { root, source },
        None => SessionRootResolution::RequiresProjectContext {
            configured_path: value.to_string(),
            source,
        },
    }
}

fn resolve_global_session_dir(value: &str, home: &Path) -> Option<PathBuf> {
    let path = if value == "~" {
        home.to_path_buf()
    } else if let Some(suffix) = value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        home.join(suffix)
    } else {
        PathBuf::from(value)
    };
    path.is_absolute().then_some(path)
}

pub fn scan_sessions() -> Vec<SessionMeta> {
    let Some(root) = session_roots().into_iter().next() else {
        match session_discovery() {
            PiSessionDiscovery::RequiresProjectContext {
                configured_path, ..
            } => log::warn!(
                "Pi sessionDir '{configured_path}' requires a project cwd and cannot be globally enumerated"
            ),
            PiSessionDiscovery::Unavailable { reason } => {
                log::warn!("Pi session discovery unavailable: {reason}")
            }
            PiSessionDiscovery::Available { .. } => {}
        }
        return Vec::new();
    };
    scan_sessions_in_root(&root)
}

fn scan_sessions_in_root(root: &Path) -> Vec<SessionMeta> {
    let mut files = Vec::new();
    collect_jsonl_files(root, 0, &mut files);
    files
        .into_iter()
        .filter_map(|path| match parse_session(&path) {
            Ok(session) => Some(session),
            Err(error) => {
                log::debug!("Skipping invalid Pi session {}: {error}", path.display());
                None
            }
        })
        .collect()
}

pub fn load_messages(path: &Path) -> Result<Vec<SessionMessage>, String> {
    let root = session_roots()
        .into_iter()
        .next()
        .ok_or_else(|| "Relative Pi sessionDir cannot be globally resolved".to_string())?;
    load_messages_with_root(&root, path)
}

fn load_messages_with_root(root: &Path, path: &Path) -> Result<Vec<SessionMessage>, String> {
    let (_, source) = validate_source_under_root(root, path)?;
    let tree = read_tree(&source)?;
    Ok(read_active_data(&source, &tree)?.messages)
}

pub fn delete_session(root: &Path, path: &Path, session_id: &str) -> Result<bool, String> {
    if !is_valid_tree_id(session_id) {
        return Err("Invalid Pi session ID".to_string());
    }
    let (_, source) = validate_source_under_root(root, path)?;
    let tree = read_tree(&source)?;
    if tree.header.id != session_id {
        return Err(format!(
            "Pi session ID mismatch: expected {session_id}, found {}",
            tree.header.id
        ));
    }
    fs::remove_file(&source)
        .map_err(|error| format!("Failed to delete Pi session {}: {error}", source.display()))?;
    Ok(true)
}

fn parse_session(path: &Path) -> Result<SessionMeta, String> {
    let source = path
        .canonicalize()
        .map_err(|error| format!("Failed to resolve Pi session {}: {error}", path.display()))?;
    let source_path = source
        .to_str()
        .ok_or_else(|| "Pi session path is not valid UTF-8".to_string())?
        .to_string();
    let tree = read_tree(&source)?;
    let data = read_active_data(&source, &tree)?;
    let title = data.explicit_name.flatten().or_else(|| {
        data.first_user_message
            .as_deref()
            .map(|message| truncate_summary(message, TITLE_MAX_CHARS))
            .filter(|message| !message.is_empty())
            .or_else(|| path_basename(&tree.header.cwd))
    });
    let summary = data
        .last_message
        .as_deref()
        .map(|message| truncate_summary(message, 160))
        .filter(|message| !message.is_empty());
    Ok(SessionMeta {
        provider_id: PROVIDER_ID.to_string(),
        session_id: tree.header.id.clone(),
        title,
        summary,
        project_dir: (!tree.header.cwd.trim().is_empty()).then(|| tree.header.cwd.clone()),
        created_at: tree.header.timestamp,
        last_active_at: data.last_active_at.or(tree.header.timestamp),
        source_path: Some(source_path.clone()),
        resume_command: Some(format!(
            "pi --session {}",
            crate::session_manager::terminal::shell_escape(&source_path)
        )),
    })
}

fn read_tree(path: &Path) -> Result<SessionTree, String> {
    validate_file_size(path)?;
    let reader = BufReader::new(
        File::open(path).map_err(|error| format!("Failed to open Pi session: {error}"))?,
    );
    let mut header = None;
    let mut parents = HashMap::<String, Option<String>>::new();
    let mut latest_id = None;
    let mut legacy_previous_id = None;
    let mut entry_index = 0usize;
    for line in reader.lines() {
        let line = line.map_err(|error| format!("Failed to read Pi session: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if header.is_none() {
            header = Some(parse_header(&value)?);
            continue;
        }
        entry_index += 1;
        if entry_index > MAX_TREE_ENTRIES {
            return Err(format!(
                "Pi session exceeds the {MAX_TREE_ENTRIES}-entry safety limit"
            ));
        }
        let version = header
            .as_ref()
            .map_or(1, |item: &SessionHeader| item.version);
        let Some((id, parent_id)) =
            entry_identity(&value, version, entry_index, legacy_previous_id.as_deref())
        else {
            continue;
        };
        if parents.insert(id.clone(), parent_id).is_some() {
            return Err(format!("Pi session contains duplicate entry ID: {id}"));
        }
        latest_id = Some(id.clone());
        legacy_previous_id = Some(id);
    }
    let header = header.ok_or_else(|| "Pi session has no valid header".to_string())?;
    let mut active_ids = HashSet::new();
    let mut current = latest_id;
    while let Some(id) = current {
        if !active_ids.insert(id.clone()) {
            return Err(format!("Pi session tree contains a cycle at entry {id}"));
        }
        current = parents
            .get(&id)
            .ok_or_else(|| format!("Pi session entry references missing parent: {id}"))?
            .clone();
    }
    Ok(SessionTree { header, active_ids })
}

fn read_active_data(path: &Path, tree: &SessionTree) -> Result<ActiveSessionData, String> {
    validate_file_size(path)?;
    let reader = BufReader::new(
        File::open(path).map_err(|error| format!("Failed to open Pi session: {error}"))?,
    );
    let mut data = ActiveSessionData::default();
    let mut saw_header = false;
    let mut entry_index = 0usize;
    let mut legacy_previous_id = None;
    for line in reader.lines() {
        let line = line.map_err(|error| format!("Failed to read Pi session: {error}"))?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if !saw_header {
            if value.get("type").and_then(Value::as_str) == Some("session") {
                saw_header = true;
            }
            continue;
        }
        entry_index += 1;
        if entry_index > MAX_TREE_ENTRIES {
            return Err(format!(
                "Pi session exceeds the {MAX_TREE_ENTRIES}-entry safety limit"
            ));
        }
        let Some((id, _)) = entry_identity(
            &value,
            tree.header.version,
            entry_index,
            legacy_previous_id.as_deref(),
        ) else {
            continue;
        };
        legacy_previous_id = Some(id.clone());
        if value.get("type").and_then(Value::as_str) == Some("session_info") {
            data.explicit_name = Some(
                value
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_string),
            );
        }
        if !tree.active_ids.contains(&id) {
            continue;
        }
        let entry_timestamp = value.get("timestamp").and_then(parse_timestamp_to_ms);
        if let Some(timestamp) = entry_timestamp {
            data.last_active_at = Some(timestamp);
        }
        match value.get("type").and_then(Value::as_str) {
            Some("session_info") => {}
            Some("message") => {
                let Some((role, content)) = value.get("message").and_then(parse_message) else {
                    continue;
                };
                let timestamp = value
                    .get("message")
                    .and_then(|message| message.get("timestamp"))
                    .and_then(parse_timestamp_to_ms)
                    .or(entry_timestamp);
                if role == "user" && data.first_user_message.is_none() {
                    data.first_user_message = Some(content.clone());
                }
                if matches!(role.as_str(), "user" | "assistant") {
                    data.last_message = Some(content.clone());
                }
                data.messages.push(SessionMessage {
                    role,
                    content,
                    ts: timestamp,
                });
            }
            Some("compaction") | Some("branch_summary") => {
                push_system(
                    &mut data.messages,
                    value
                        .get("summary")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    entry_timestamp,
                );
            }
            Some("custom_message")
                if value.get("display").and_then(Value::as_bool) != Some(false) =>
            {
                push_system(
                    &mut data.messages,
                    &value.get("content").map(extract_text).unwrap_or_default(),
                    entry_timestamp,
                );
            }
            _ => {}
        }
    }
    Ok(data)
}

fn push_system(messages: &mut Vec<SessionMessage>, content: &str, ts: Option<i64>) {
    if !content.trim().is_empty() {
        messages.push(SessionMessage {
            role: "system".to_string(),
            content: content.to_string(),
            ts,
        });
    }
}

fn parse_header(value: &Value) -> Result<SessionHeader, String> {
    if value.get("type").and_then(Value::as_str) != Some("session") {
        return Err("Pi session header must be the first valid JSON entry".to_string());
    }
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| is_valid_tree_id(id))
        .ok_or_else(|| "Pi session header has an invalid ID".to_string())?
        .to_string();
    let version = value.get("version").and_then(Value::as_u64).unwrap_or(1);
    if !(1..=3).contains(&version) {
        return Err(format!("Unsupported Pi session version: {version}"));
    }
    Ok(SessionHeader {
        id,
        cwd: value
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        timestamp: value.get("timestamp").and_then(parse_timestamp_to_ms),
        version,
    })
}

fn entry_identity(
    value: &Value,
    version: u64,
    entry_index: usize,
    legacy_previous_id: Option<&str>,
) -> Option<(String, Option<String>)> {
    if version < 2 {
        return Some((
            format!("legacy-{entry_index}"),
            legacy_previous_id.map(str::to_string),
        ));
    }
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| is_valid_tree_id(id))?
        .to_string();
    let parent_id = match value.get("parentId") {
        None | Some(Value::Null) => None,
        Some(Value::String(parent)) if is_valid_tree_id(parent) => Some(parent.clone()),
        _ => return None,
    };
    Some((id, parent_id))
}

fn parse_message(message: &Value) -> Option<(String, String)> {
    let role = message.get("role").and_then(Value::as_str)?;
    let (display_role, content) = match role {
        "user" | "assistant" => (
            role.to_string(),
            message.get("content").map(extract_text).unwrap_or_default(),
        ),
        "toolResult" => (
            "tool".to_string(),
            message.get("content").map(extract_text).unwrap_or_default(),
        ),
        "bashExecution" => (
            "tool".to_string(),
            format!(
                "$ {}\n{}",
                message
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                message
                    .get("output")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            ),
        ),
        "branchSummary" | "compactionSummary" => (
            "system".to_string(),
            message
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ),
        _ => return None,
    };
    (!content.trim().is_empty()).then_some((display_role, content))
}

fn validate_source_under_root(root: &Path, path: &Path) -> Result<(PathBuf, PathBuf), String> {
    let root = root.canonicalize().map_err(|error| {
        format!(
            "Failed to resolve Pi session root {}: {error}",
            root.display()
        )
    })?;
    let source = path
        .canonicalize()
        .map_err(|error| format!("Failed to resolve Pi session {}: {error}", path.display()))?;
    if !source.starts_with(&root) {
        return Err(format!(
            "Pi session source is outside the session root: {}",
            path.display()
        ));
    }
    let metadata = fs::symlink_metadata(&source)
        .map_err(|error| format!("Failed to inspect Pi session {}: {error}", source.display()))?;
    if !metadata.file_type().is_file()
        || source.extension().and_then(|value| value.to_str()) != Some("jsonl")
        || metadata.len() > MAX_SESSION_BYTES
    {
        return Err(format!("Invalid Pi session file: {}", source.display()));
    }
    Ok((root, source))
}

fn validate_file_size(path: &Path) -> Result<(), String> {
    let metadata =
        fs::metadata(path).map_err(|error| format!("Failed to inspect Pi session: {error}"))?;
    if metadata.len() > MAX_SESSION_BYTES {
        Err(format!(
            "Pi session exceeds the {MAX_SESSION_BYTES}-byte safety limit"
        ))
    } else {
        Ok(())
    }
}

fn is_valid_tree_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_TREE_ID_BYTES
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn collect_jsonl_files(root: &Path, depth: usize, output: &mut Vec<PathBuf>) {
    if depth > MAX_SCAN_DEPTH {
        return;
    }
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if file_type.is_dir() {
            collect_jsonl_files(&path, depth + 1, output);
        } else if file_type.is_file()
            && path.extension().and_then(|value| value.to_str()) == Some("jsonl")
            && entry
                .metadata()
                .is_ok_and(|metadata| metadata.len() <= MAX_SESSION_BYTES)
        {
            output.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_leaf_defines_the_active_branch() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        fs::create_dir_all(&root).expect("root");
        let path = root.join("tree.jsonl");
        fs::write(
            &path,
            "{\"type\":\"session\",\"version\":3,\"id\":\"session-1\",\"cwd\":\"/work\"}\n\
             {\"type\":\"message\",\"id\":\"root\",\"parentId\":null,\"message\":{\"role\":\"user\",\"content\":\"question\"}}\n\
             {\"type\":\"message\",\"id\":\"dead\",\"parentId\":\"root\",\"message\":{\"role\":\"assistant\",\"content\":\"abandoned\"}}\n\
             {\"type\":\"message\",\"id\":\"live\",\"parentId\":\"root\",\"message\":{\"role\":\"assistant\",\"content\":\"active\"}}\n",
        )
        .expect("session");
        let messages = load_messages_with_root(&root, &path).expect("messages");
        assert_eq!(
            messages
                .into_iter()
                .map(|message| message.content)
                .collect::<Vec<_>>(),
            vec!["question", "active"]
        );
    }

    #[test]
    fn capture_matches_global_name_and_malformed_line_semantics() {
        // Executed by scripts/pi-transport-capture.mjs against pinned Pi:
        // getSessionName() keeps the latest global session_info even when its
        // branch is inactive, and SessionManager.open() skips a malformed line.
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("captured.jsonl");
        fs::write(
            &path,
            "{\"type\":\"session\",\"version\":3,\"id\":\"session-1\",\"cwd\":\"/work\"}\n\
             {\"type\":\"message\",\"id\":\"root\",\"parentId\":null,\"message\":{\"role\":\"user\",\"content\":\"root\"}}\n\
             {not valid json\n\
             {\"type\":\"session_info\",\"id\":\"dead-name\",\"parentId\":\"root\",\"name\":\"Abandoned branch name\"}\n\
             {\"type\":\"message\",\"id\":\"dead\",\"parentId\":\"dead-name\",\"message\":{\"role\":\"assistant\",\"content\":\"abandoned\"}}\n\
             {\"type\":\"message\",\"id\":\"live\",\"parentId\":\"root\",\"message\":{\"role\":\"user\",\"content\":\"active branch\"}}\n",
        )
        .expect("captured session");

        let session = parse_session(&path).expect("parse capture semantics");
        assert_eq!(session.title.as_deref(), Some("Abandoned branch name"));
        assert_eq!(session.summary.as_deref(), Some("active branch"));
    }

    #[test]
    fn relative_root_is_explicitly_non_enumerable() {
        assert_eq!(
            resolve_global_session_dir(".pi/sessions", Path::new("/home/pi")),
            None
        );
        assert_eq!(
            classify_configured_session_dir(".pi/sessions", Path::new("/home/pi"), "settings"),
            SessionRootResolution::RequiresProjectContext {
                configured_path: ".pi/sessions".to_string(),
                source: "settings",
            }
        );
    }

    #[test]
    fn capture_generated_v3_shape_round_trips_all_consumed_fields() {
        // Generated by scripts/pi-transport-capture.mjs against pinned Pi
        // ab366ebe94cacd419d986be454f12b1b9913aaca using SessionManager APIs.
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        fs::create_dir_all(&root).expect("root");
        let path = root.join("captured.jsonl");
        fs::write(
            &path,
            "{\"type\":\"session\",\"version\":3,\"id\":\"cc-switch-capture-session\",\"timestamp\":\"2023-11-14T22:13:20.000Z\",\"cwd\":\"/work/captured\",\"parentSession\":null}\n\
             {\"type\":\"session_info\",\"id\":\"00000000-0000-7000-8000-000000000001\",\"parentId\":null,\"timestamp\":\"2023-11-14T22:13:20.100Z\",\"name\":\"Captured session\"}\n\
             {\"type\":\"message\",\"id\":\"00000000-0000-7000-8000-000000000002\",\"parentId\":\"00000000-0000-7000-8000-000000000001\",\"timestamp\":\"2023-11-14T22:13:20.200Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"captured question\"}],\"timestamp\":1700000000000}}\n\
             {\"type\":\"message\",\"id\":\"00000000-0000-7000-8000-000000000003\",\"parentId\":\"00000000-0000-7000-8000-000000000002\",\"timestamp\":\"2023-11-14T22:13:21.200Z\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"captured answer\"}],\"api\":\"openai-responses\",\"provider\":\"capture\",\"model\":\"capture-model\",\"usage\":{\"input\":1,\"output\":1,\"cacheRead\":0,\"cacheWrite\":0,\"totalTokens\":2,\"cost\":{\"input\":0,\"output\":0,\"cacheRead\":0,\"cacheWrite\":0,\"total\":0}},\"stopReason\":\"stop\",\"timestamp\":1700000001000}}\n",
        )
        .expect("captured session");

        let session = parse_session(&path).expect("parse capture-generated session");
        assert_eq!(session.session_id, "cc-switch-capture-session");
        assert_eq!(session.title.as_deref(), Some("Captured session"));
        assert_eq!(session.summary.as_deref(), Some("captured answer"));
        assert_eq!(session.project_dir.as_deref(), Some("/work/captured"));
        assert_eq!(session.created_at, Some(1_700_000_000_000));
        assert_eq!(session.last_active_at, Some(1_700_000_001_200));
        // scripts/pi-transport-capture.mjs executes pinned Pi's parseArgs with
        // ["--session", <SessionManager.getSessionFile()>] and records that the
        // exact path is returned in Args.session.
        assert!(session
            .resume_command
            .as_deref()
            .is_some_and(|command| command.starts_with("pi --session ")));

        let messages = load_messages_with_root(&root, &path).expect("load messages");
        assert_eq!(
            messages
                .iter()
                .map(|message| (message.role.as_str(), message.content.as_str(), message.ts))
                .collect::<Vec<_>>(),
            vec![
                ("user", "captured question", Some(1_700_000_000_000)),
                ("assistant", "captured answer", Some(1_700_000_001_000)),
            ]
        );
    }

    #[test]
    fn deletion_requires_containment_and_matching_header_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("sessions");
        fs::create_dir_all(&root).expect("root");
        let path = root.join("session.jsonl");
        fs::write(
            &path,
            "{\"type\":\"session\",\"version\":3,\"id\":\"session-1\",\"cwd\":\"/work\"}\n",
        )
        .expect("session");
        assert!(delete_session(&root, &path, "other").is_err());
        assert!(path.exists());
        assert!(delete_session(&root, &path, "session-1").expect("delete"));
    }
}

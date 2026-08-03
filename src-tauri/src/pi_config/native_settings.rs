//! Exact-field access to Pi's shared `settings.json`.
//!
//! cc-switch owns only `defaultProvider` and `defaultModel`. Every other field
//! remains Pi/user-owned and survives each mutation unchanged.

use crate::error::AppError;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use super::shared_file::{
    compare_exchange_shared_file_bytes, delete_shared_file, read_shared_file, replace_shared_file,
    SharedFileSnapshot,
};

const MAX_PI_SETTINGS_BYTES: u64 = 1024 * 1024;
const MAX_WRITE_ATTEMPTS: usize = 3;
static SETTINGS_WRITE_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiNativeDefaults {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_dir: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PiNativeDefaultsReceipt {
    path: PathBuf,
    before: SharedFileSnapshot,
    after: SharedFileSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PiNativeDefaultsRollback {
    Restored,
    Superseded,
}

impl PiNativeDefaultsReceipt {
    /// Restore the exact file revision replaced by this write. A newer Pi/user
    /// edit wins and is reported as Superseded rather than being overwritten.
    pub(crate) fn rollback(&self) -> Result<PiNativeDefaultsRollback, AppError> {
        let result = match self.before.bytes.as_deref() {
            Some(bytes) => replace_shared_file(
                &self.path,
                &self.after.revision,
                bytes,
                MAX_PI_SETTINGS_BYTES,
                None,
                "Pi settings rollback",
            )
            .map(|_| ()),
            None => delete_shared_file(
                &self.path,
                &self.after.revision,
                MAX_PI_SETTINGS_BYTES,
                "Pi settings rollback",
            )
            .map(|_| ()),
        };
        match result {
            Ok(()) => Ok(PiNativeDefaultsRollback::Restored),
            Err(AppError::Conflict(_)) => Ok(PiNativeDefaultsRollback::Superseded),
            Err(error) => Err(error),
        }
    }
}

pub(crate) fn get_pi_settings_path() -> Result<PathBuf, AppError> {
    Ok(super::native::get_pi_agent_dir()?.join("settings.json"))
}

pub(crate) fn read_pi_native_defaults() -> Result<PiNativeDefaults, AppError> {
    read_pi_native_defaults_at(&get_pi_settings_path()?)
}

pub(crate) fn read_pi_native_defaults_at(path: &Path) -> Result<PiNativeDefaults, AppError> {
    let document = read_settings_document(path)?;
    let root = document.as_object().ok_or_else(|| {
        AppError::Config(format!(
            "Pi settings root must be an object: {}",
            path.display()
        ))
    })?;
    Ok(PiNativeDefaults {
        default_provider: optional_string(root, "defaultProvider", path)?,
        default_model: optional_string(root, "defaultModel", path)?,
        session_dir: optional_string(root, "sessionDir", path)?,
    })
}

pub(crate) fn set_pi_native_default_with_receipt(
    provider_key: &str,
    model_id: &str,
) -> Result<PiNativeDefaultsReceipt, AppError> {
    if provider_key.trim().is_empty() || model_id.trim().is_empty() {
        return Err(AppError::InvalidInput(
            "Pi default provider and model must be non-empty".to_string(),
        ));
    }
    mutate_settings_document(&get_pi_settings_path()?, |root| {
        root.insert(
            "defaultProvider".to_string(),
            Value::String(provider_key.to_string()),
        );
        root.insert(
            "defaultModel".to_string(),
            Value::String(model_id.to_string()),
        );
        Ok(())
    })
}

fn optional_string(
    root: &Map<String, Value>,
    key: &str,
    path: &Path,
) -> Result<Option<String>, AppError> {
    match root.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(AppError::Config(format!(
            "Pi settings field '{key}' must be a string: {}",
            path.display()
        ))),
    }
}

fn mutate_settings_document(
    path: &Path,
    mut mutator: impl FnMut(&mut Map<String, Value>) -> Result<(), AppError>,
) -> Result<PiNativeDefaultsReceipt, AppError> {
    let _guard = SETTINGS_WRITE_LOCK
        .lock()
        .map_err(|error| AppError::Config(format!("Pi settings lock is poisoned: {error}")))?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| AppError::io(parent, error))?;
    }

    for _ in 0..MAX_WRITE_ATTEMPTS {
        let before = read_shared_file(path, MAX_PI_SETTINGS_BYTES, "Pi settings")?;
        let mut document = match before.bytes.as_deref() {
            Some(bytes) => {
                serde_json::from_slice(bytes).map_err(|error| AppError::json(path, error))?
            }
            None => Value::Object(Map::new()),
        };
        let root = document.as_object_mut().ok_or_else(|| {
            AppError::Config(format!(
                "Pi settings root must be an object: {}",
                path.display()
            ))
        })?;
        mutator(root)?;
        let mut serialized = serde_json::to_vec_pretty(&document)
            .map_err(|source| AppError::JsonSerialize { source })?;
        serialized.push(b'\n');

        match compare_exchange_shared_file_bytes(
            path,
            before.bytes.as_deref(),
            &serialized,
            MAX_PI_SETTINGS_BYTES,
            None,
            "Pi settings",
        ) {
            Ok(after) => {
                return Ok(PiNativeDefaultsReceipt {
                    path: path.to_path_buf(),
                    before,
                    after,
                })
            }
            Err(AppError::Conflict(_)) => continue,
            Err(error) => return Err(error),
        }
    }

    Err(AppError::Conflict(format!(
        "Pi settings changed concurrently too many times: {}",
        path.display()
    )))
}

fn read_settings_document(path: &Path) -> Result<Value, AppError> {
    match read_shared_file(path, MAX_PI_SETTINGS_BYTES, "Pi settings")?.bytes {
        Some(bytes) => serde_json::from_slice(&bytes).map_err(|error| AppError::json(path, error)),
        None => Ok(Value::Object(Map::new())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn default_patch_preserves_every_unowned_field() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("settings.json");
        fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "theme": "custom",
                "packages": ["npm:foreign"],
                "sessionDir": "/tmp/pi-sessions",
                "defaultProvider": "old",
                "defaultModel": "old-model"
            }))
            .expect("serialize"),
        )
        .expect("write");

        mutate_settings_document(&path, |root| {
            root.insert("defaultProvider".into(), json!("managed"));
            root.insert("defaultModel".into(), json!("model"));
            Ok(())
        })
        .expect("mutate");

        let saved: Value = serde_json::from_slice(&fs::read(&path).expect("read")).expect("parse");
        assert_eq!(saved["theme"], "custom");
        assert_eq!(saved["packages"], json!(["npm:foreign"]));
        assert_eq!(saved["sessionDir"], "/tmp/pi-sessions");
        assert_eq!(saved["defaultProvider"], "managed");
        assert_eq!(saved["defaultModel"], "model");
    }

    #[test]
    fn external_rename_during_settings_patch_is_reparsed_before_retry() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("settings.json");
        fs::write(
            &path,
            br#"{"theme":"before","defaultProvider":"old","defaultModel":"old"}"#,
        )
        .expect("seed");
        crate::pi_config::shared_file::replace_before_next_compare_exchange(
            &path,
            br#"{"theme":"external","packages":["foreign"],"defaultProvider":"old","defaultModel":"old"}"#,
        );

        mutate_settings_document(&path, |root| {
            root.insert("defaultProvider".into(), json!("managed"));
            root.insert("defaultModel".into(), json!("model"));
            Ok(())
        })
        .expect("retry mutation");

        let saved: Value = serde_json::from_slice(&fs::read(&path).expect("read")).expect("parse");
        assert_eq!(saved["theme"], "external");
        assert_eq!(saved["packages"], json!(["foreign"]));
        assert_eq!(saved["defaultProvider"], "managed");
        assert_eq!(saved["defaultModel"], "model");
    }

    #[cfg(unix)]
    #[test]
    fn settings_symlink_is_rejected() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("target.json");
        let path = temp.path().join("settings.json");
        fs::write(&target, "{}").expect("target");
        symlink(&target, &path).expect("symlink");
        assert!(read_pi_native_defaults_at(&path).is_err());
    }

    #[test]
    fn rollback_receipt_never_overwrites_a_newer_external_default() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("settings.json");
        fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "theme": "before",
                "defaultProvider": "old",
                "defaultModel": "old-model"
            }))
            .expect("serialize"),
        )
        .expect("write");

        let receipt = mutate_settings_document(&path, |root| {
            root.insert("defaultProvider".into(), json!("attempted"));
            root.insert("defaultModel".into(), json!("attempted-model"));
            Ok(())
        })
        .expect("write attempted defaults");
        fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "theme": "external",
                "defaultProvider": "external",
                "defaultModel": "external-model"
            }))
            .expect("serialize external"),
        )
        .expect("external write");

        assert_eq!(
            receipt.rollback().expect("rollback decision"),
            PiNativeDefaultsRollback::Superseded
        );
        assert_eq!(
            read_pi_native_defaults_at(&path)
                .expect("live defaults")
                .default_provider
                .as_deref(),
            Some("external")
        );
        assert_eq!(
            read_settings_document(&path).expect("live document")["theme"],
            "external"
        );
    }
}

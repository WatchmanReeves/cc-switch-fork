//! Pi native instruction files and prompt templates.
//!
//! AGENTS.md is also the Prompt-library projection. SYSTEM.md and
//! APPEND_SYSTEM.md are direct native resources: file presence is activation
//! and there is no shadow enabled flag.

use crate::error::AppError;
use crate::pi_config::native::get_pi_agent_dir;
use crate::pi_config::shared_file::{delete_shared_file, read_shared_file, replace_shared_file};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};
use tokio::sync::{Mutex, OwnedMutexGuard};

const MAX_PROMPT_FILE_BYTES: u64 = 1024 * 1024;
const MAX_TEMPLATE_SLUG_BYTES: usize = 128;
static INSTRUCTION_FILE_LOCK: LazyLock<Arc<Mutex<()>>> = LazyLock::new(|| Arc::new(Mutex::new(())));

pub(crate) type PiInstructionFileGuard = OwnedMutexGuard<()>;

pub(crate) fn lock_instruction_files() -> Result<PiInstructionFileGuard, AppError> {
    Ok(futures::executor::block_on(
        INSTRUCTION_FILE_LOCK.clone().lock_owned(),
    ))
}

pub(crate) async fn lock_instruction_files_async() -> PiInstructionFileGuard {
    INSTRUCTION_FILE_LOCK.clone().lock_owned().await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PiPromptFileKind {
    GlobalContext,
    SystemOverride,
    SystemAppend,
}

impl PiPromptFileKind {
    fn filename(self) -> &'static str {
        match self {
            Self::GlobalContext => "AGENTS.md",
            Self::SystemOverride => "SYSTEM.md",
            Self::SystemAppend => "APPEND_SYSTEM.md",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiPromptFileSnapshot {
    pub kind: PiPromptFileKind,
    pub path: String,
    pub exists: bool,
    pub revision: String,
    pub content: String,
}

pub struct PiPromptFileService;

impl PiPromptFileService {
    pub fn read(kind: PiPromptFileKind) -> Result<PiPromptFileSnapshot, AppError> {
        let guard = lock_instruction_files()?;
        Self::read_under_guard(&guard, kind)
    }

    pub fn replace(
        kind: PiPromptFileKind,
        expected_revision: &str,
        content: &str,
    ) -> Result<PiPromptFileSnapshot, AppError> {
        if kind == PiPromptFileKind::GlobalContext {
            return Err(AppError::InvalidInput(
                "Pi AGENTS.md is managed through the Prompt library".to_string(),
            ));
        }
        validate_direct_instruction_content(content)?;
        let guard = lock_instruction_files()?;
        Self::replace_under_guard(&guard, kind, expected_revision, content)
    }

    pub fn delete(kind: PiPromptFileKind, expected_revision: &str) -> Result<bool, AppError> {
        if kind == PiPromptFileKind::GlobalContext {
            return Err(AppError::InvalidInput(
                "Pi AGENTS.md is managed through the Prompt library".to_string(),
            ));
        }
        let guard = lock_instruction_files()?;
        Self::delete_under_guard(&guard, kind, expected_revision)
    }

    pub(crate) fn read_under_guard(
        _guard: &PiInstructionFileGuard,
        kind: PiPromptFileKind,
    ) -> Result<PiPromptFileSnapshot, AppError> {
        Self::read_at(&get_pi_agent_dir()?, kind)
    }

    pub(crate) fn replace_under_guard(
        _guard: &PiInstructionFileGuard,
        kind: PiPromptFileKind,
        expected_revision: &str,
        content: &str,
    ) -> Result<PiPromptFileSnapshot, AppError> {
        Self::replace_at(&get_pi_agent_dir()?, kind, expected_revision, content)
    }

    pub(crate) fn delete_under_guard(
        _guard: &PiInstructionFileGuard,
        kind: PiPromptFileKind,
        expected_revision: &str,
    ) -> Result<bool, AppError> {
        Self::delete_at(&get_pi_agent_dir()?, kind, expected_revision)
    }

    fn read_at(root: &Path, kind: PiPromptFileKind) -> Result<PiPromptFileSnapshot, AppError> {
        let path = root.join(kind.filename());
        let snapshot = read_shared_file(&path, MAX_PROMPT_FILE_BYTES, "Pi prompt file")?;
        let exists = snapshot.exists();
        let content = match snapshot.bytes {
            Some(bytes) => String::from_utf8(bytes).map_err(|error| {
                AppError::InvalidInput(format!(
                    "Pi prompt file must be UTF-8 ({}): {error}",
                    path.display()
                ))
            })?,
            None => String::new(),
        };
        Ok(PiPromptFileSnapshot {
            kind,
            path: path.to_string_lossy().into_owned(),
            exists,
            revision: snapshot.revision,
            content,
        })
    }

    fn replace_at(
        root: &Path,
        kind: PiPromptFileKind,
        expected_revision: &str,
        content: &str,
    ) -> Result<PiPromptFileSnapshot, AppError> {
        fs::create_dir_all(root).map_err(|error| AppError::io(root, error))?;
        let path = root.join(kind.filename());
        replace_shared_file(
            &path,
            expected_revision,
            content.as_bytes(),
            MAX_PROMPT_FILE_BYTES,
            Some(0o600),
            "Pi prompt file",
        )?;
        Self::read_at(root, kind)
    }

    fn delete_at(
        root: &Path,
        kind: PiPromptFileKind,
        expected_revision: &str,
    ) -> Result<bool, AppError> {
        delete_shared_file(
            &root.join(kind.filename()),
            expected_revision,
            MAX_PROMPT_FILE_BYTES,
            "Pi prompt file",
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PiPromptTemplate {
    pub slug: String,
    pub content: String,
    pub revision: String,
}

pub struct PiPromptTemplateService;

impl PiPromptTemplateService {
    pub fn list() -> Result<Vec<PiPromptTemplate>, AppError> {
        Self::list_at(&get_pi_agent_dir()?.join("prompts"))
    }

    pub fn upsert(
        slug: &str,
        expected_revision: &str,
        content: &str,
    ) -> Result<PiPromptTemplate, AppError> {
        Self::upsert_at(
            &get_pi_agent_dir()?.join("prompts"),
            slug,
            expected_revision,
            content,
        )
    }

    pub fn delete(slug: &str, expected_revision: &str) -> Result<bool, AppError> {
        validate_template_slug(slug)?;
        delete_shared_file(
            &template_path(&get_pi_agent_dir()?.join("prompts"), slug),
            expected_revision,
            MAX_PROMPT_FILE_BYTES,
            "Pi prompt template",
        )
    }

    fn list_at(dir: &Path) -> Result<Vec<PiPromptTemplate>, AppError> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(AppError::io(dir, error)),
        };
        let mut templates = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| AppError::io(dir, error))?;
            let path = entry.path();
            let Some(slug) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            if path.extension().and_then(|value| value.to_str()) != Some("md")
                || validate_template_slug(slug).is_err()
            {
                continue;
            }
            let snapshot = read_shared_file(&path, MAX_PROMPT_FILE_BYTES, "Pi prompt template")?;
            let Some(bytes) = snapshot.bytes else {
                continue;
            };
            let content = String::from_utf8(bytes).map_err(|error| {
                AppError::InvalidInput(format!(
                    "Pi prompt template must be UTF-8 ({}): {error}",
                    path.display()
                ))
            })?;
            templates.push(PiPromptTemplate {
                slug: slug.to_string(),
                content,
                revision: snapshot.revision,
            });
        }
        templates.sort_by(|left, right| left.slug.cmp(&right.slug));
        Ok(templates)
    }

    fn upsert_at(
        dir: &Path,
        slug: &str,
        expected_revision: &str,
        content: &str,
    ) -> Result<PiPromptTemplate, AppError> {
        validate_template_slug(slug)?;
        fs::create_dir_all(dir).map_err(|error| AppError::io(dir, error))?;
        let snapshot = replace_shared_file(
            &template_path(dir, slug),
            expected_revision,
            content.as_bytes(),
            MAX_PROMPT_FILE_BYTES,
            Some(0o600),
            "Pi prompt template",
        )?;
        Ok(PiPromptTemplate {
            slug: slug.to_string(),
            content: content.to_string(),
            revision: snapshot.revision,
        })
    }
}

fn template_path(dir: &Path, slug: &str) -> PathBuf {
    dir.join(format!("{slug}.md"))
}

fn validate_direct_instruction_content(content: &str) -> Result<(), AppError> {
    if content.trim().is_empty() {
        Err(AppError::InvalidInput(
            "Pi SYSTEM.md and APPEND_SYSTEM.md content cannot be blank; delete the file to deactivate it"
                .to_string(),
        ))
    } else {
        Ok(())
    }
}

fn validate_template_slug(slug: &str) -> Result<(), AppError> {
    // Pinned Pi's request-capture discovers `release notes.md`, but
    // expandPromptTemplate("/release notes", ...) leaves the command
    // unchanged because slash-command names are one token. Keep the managed
    // namespace both callable and portable across Unix and Windows.
    let windows_basename = slug
        .split_once('.')
        .map_or(slug, |(basename, _extension)| basename);
    let windows_basename = windows_basename.to_ascii_lowercase();
    let windows_reserved = matches!(
        windows_basename.as_str(),
        "con"
            | "prn"
            | "aux"
            | "nul"
            | "com1"
            | "com2"
            | "com3"
            | "com4"
            | "com5"
            | "com6"
            | "com7"
            | "com8"
            | "com9"
            | "lpt1"
            | "lpt2"
            | "lpt3"
            | "lpt4"
            | "lpt5"
            | "lpt6"
            | "lpt7"
            | "lpt8"
            | "lpt9"
    );
    let valid = !slug.is_empty()
        && slug.len() <= MAX_TEMPLATE_SLUG_BYTES
        && slug != "."
        && slug != ".."
        && !slug.starts_with('.')
        && !slug.ends_with('.')
        && !windows_reserved
        && !slug.chars().any(|character| {
            character.is_control()
                || character.is_whitespace()
                || matches!(
                    character,
                    '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
                )
        });
    if valid {
        Ok(())
    } else {
        Err(AppError::InvalidInput(
            "Pi prompt-template slug must be one portable slash-command token (1-128 UTF-8 bytes)"
                .to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_instruction_files_use_presence_and_revision_as_native_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        for kind in [
            PiPromptFileKind::GlobalContext,
            PiPromptFileKind::SystemOverride,
            PiPromptFileKind::SystemAppend,
        ] {
            let missing = PiPromptFileService::read_at(temp.path(), kind).expect("missing");
            assert!(!missing.exists);
            // scripts/pi-transport-capture.mjs executes pinned Pi's
            // DefaultResourceLoader at
            // ab366ebe94cacd419d986be454f12b1b9913aaca and records all three
            // zero-byte files as present resources.
            let empty = PiPromptFileService::replace_at(temp.path(), kind, "missing", "")
                .expect("create empty instruction file");
            assert!(empty.exists);
            assert_eq!(empty.content, "");
            let saved =
                PiPromptFileService::replace_at(temp.path(), kind, &empty.revision, "content")
                    .expect("replace");
            assert!(saved.exists);
            assert_eq!(saved.content, "content");
            assert!(PiPromptFileService::delete_at(temp.path(), kind, "missing").is_err());
            assert!(
                PiPromptFileService::delete_at(temp.path(), kind, &saved.revision).expect("delete")
            );
        }
    }

    #[test]
    fn direct_instruction_save_rejects_blank_content_without_redefining_native_presence() {
        for content in ["", "  \n\t"] {
            assert!(validate_direct_instruction_content(content).is_err());
        }
        assert!(validate_direct_instruction_content("# Explicit override").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn direct_instruction_entry_never_reports_failure_with_its_attempt_live() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp
            .path()
            .join(PiPromptFileKind::SystemOverride.filename());

        crate::pi_config::shared_file::fail_next_parent_sync_for_test(&path);
        PiPromptFileService::replace_at(
            temp.path(),
            PiPromptFileKind::SystemOverride,
            "missing",
            "created",
        )
        .expect_err("failed create must be compensated");
        assert!(!path.exists());

        let before = PiPromptFileService::replace_at(
            temp.path(),
            PiPromptFileKind::SystemOverride,
            "missing",
            "before",
        )
        .expect("seed");
        crate::pi_config::shared_file::fail_next_parent_sync_for_test(&path);
        PiPromptFileService::replace_at(
            temp.path(),
            PiPromptFileKind::SystemOverride,
            &before.revision,
            "after",
        )
        .expect_err("failed replace must restore its before-image");
        assert_eq!(
            fs::read_to_string(&path).expect("before restored"),
            "before"
        );

        let before = PiPromptFileService::read_at(temp.path(), PiPromptFileKind::SystemOverride)
            .expect("snapshot");
        crate::pi_config::shared_file::fail_next_parent_sync_for_test(&path);
        PiPromptFileService::delete_at(
            temp.path(),
            PiPromptFileKind::SystemOverride,
            &before.revision,
        )
        .expect_err("failed delete must restore its before-image");
        assert_eq!(
            fs::read_to_string(&path).expect("before restored"),
            "before"
        );
    }

    #[test]
    fn templates_reject_ambiguous_or_traversing_slugs() {
        for slug in [
            "",
            ".",
            "..",
            ".hidden",
            "trailing.",
            " padded",
            "internal space",
            "tab\tname",
            "a/b",
            r"a\b",
            "bad:name",
            "bad*name",
            "CON",
            "con.anything",
            "LPT9",
            "nul.json",
        ] {
            assert!(validate_template_slug(slug).is_err(), "{slug:?}");
        }
        for slug in ["review-pr", "release.v2", "评审", "SYSTEM"] {
            assert!(validate_template_slug(slug).is_ok(), "{slug:?}");
        }
    }

    #[test]
    fn empty_template_is_present_and_round_trips_like_pinned_pi() {
        // scripts/pi-transport-capture.mjs executes pinned Pi
        // ab366ebe94cacd419d986be454f12b1b9913aaca and confirms that an empty
        // prompts/empty.md is discovered as an active template.
        let temp = tempfile::tempdir().expect("tempdir");
        let created = PiPromptTemplateService::upsert_at(temp.path(), "empty", "missing", "")
            .expect("create empty template");
        assert_eq!(created.content, "");
        let listed = PiPromptTemplateService::list_at(temp.path()).expect("list templates");
        assert_eq!(listed, vec![created]);
    }
}

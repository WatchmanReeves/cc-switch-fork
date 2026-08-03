//! Ownership-safe Pi Skill deployment reconciliation.
//!
//! Desired state lives on the installed Skill row. Filesystem presence alone
//! is never ownership evidence; only the device-local deployment ledger may
//! authorize replacement or deletion.

use crate::app_config::{AppType, InstalledSkill};
use crate::database::{Database, SkillDeployment, SkillDeploymentMethod};
use crate::error::AppError;
use crate::services::skill::{SkillService, SyncMethod};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

pub(crate) struct PiSkillDeploymentService;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PiSkillOwnership {
    Absent,
    Owned,
    Foreign,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PiSkillDiscovery {
    Absent,
    Active,
    Shadowed,
    Invalid,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillAppStatus {
    pub desired_enabled: bool,
    pub owned_deployment: bool,
    pub effectively_discovered: bool,
    pub ownership: PiSkillOwnership,
    pub discovery: PiSkillDiscovery,
    pub issue: Option<String>,
}

impl PiSkillDeploymentService {
    pub(crate) fn reconcile_skill(
        db: &Arc<Database>,
        skill: &InstalledSkill,
    ) -> Result<(), AppError> {
        let guard = Self::operation_guard();
        Self::reconcile_skill_under_guard(&guard, db, skill)
    }

    pub(crate) fn toggle(
        db: &Arc<Database>,
        skill: &mut InstalledSkill,
        enabled: bool,
    ) -> Result<(), AppError> {
        let guard = Self::operation_guard();
        Self::toggle_under_guard(&guard, db, skill, enabled)
    }

    pub(crate) fn toggle_under_guard(
        _guard: &MutexGuard<'static, ()>,
        db: &Arc<Database>,
        skill: &mut InstalledSkill,
        enabled: bool,
    ) -> Result<(), AppError> {
        if enabled {
            let destination = skill_destination(skill)?;
            let destination_key = destination_key(&destination);
            let existing = db.get_pi_skill_deployment(&skill.id, &destination_key)?;
            let source = skill_source(skill)?;
            deploy(
                db,
                skill,
                &source,
                &destination,
                &destination_key,
                existing,
                Some(true),
            )?;
            cleanup_stale_deployments_after_commit(db, skill, &destination_key);
        } else {
            remove_all_recorded_deployments(db, skill, Some(false))?;
        }
        skill.apps.pi = enabled;
        Ok(())
    }

    /// Apply the Pi desired state selected by the user while importing an
    /// existing Skill from application directories.
    ///
    /// Import is the one operation where an unowned Pi destination may become
    /// managed: the user explicitly selected that exact native tree. Adoption
    /// is allowed only when every byte in the native destination matches the
    /// newly established SSOT source. A mere directory/name match is never
    /// ownership evidence.
    pub(crate) fn import_desired_state_under_guard(
        _guard: &MutexGuard<'static, ()>,
        db: &Arc<Database>,
        skill: &mut InstalledSkill,
        enabled: bool,
    ) -> Result<(), AppError> {
        let destination = skill_destination(skill)?;
        let destination_key = destination_key(&destination);
        let existing = db.get_pi_skill_deployment(&skill.id, &destination_key)?;
        if enabled {
            let source = skill_source(skill)?;
            if existing.is_none() && fs::symlink_metadata(&destination).is_ok() {
                adopt_exact_import(db, skill, &source, &destination, &destination_key)?;
            } else {
                deploy(
                    db,
                    skill,
                    &source,
                    &destination,
                    &destination_key,
                    existing,
                    Some(true),
                )?;
            }
            cleanup_stale_deployments_after_commit(db, skill, &destination_key);
        } else {
            remove_all_recorded_deployments(db, skill, Some(false))?;
        }
        skill.apps.pi = enabled;
        Ok(())
    }

    pub(crate) fn reconcile_all(db: &Arc<Database>) -> Result<(), AppError> {
        let guard = Self::operation_guard();
        Self::reconcile_all_under_guard(&guard, db)
    }

    pub(crate) fn operation_guard() -> MutexGuard<'static, ()> {
        deployment_lock()
    }

    /// Serialize a portable database/SSOT replacement with every Pi Skill
    /// deployment mutation. Callers already hold Pi's switch boundary, so the
    /// global lock order remains `Pi switch -> Skill deployment`.
    pub(crate) fn coordinate_portable_import<T>(
        operation: impl FnOnce() -> Result<T, AppError>,
    ) -> Result<T, AppError> {
        let _guard = Self::operation_guard();
        operation()
    }

    pub(crate) fn import_portable_sql(
        db: &Database,
        source_path: &Path,
    ) -> Result<String, AppError> {
        Self::coordinate_portable_import(|| db.import_portable_sql(source_path))
    }

    /// Keep legacy binary restore outside the Pi ownership domain until the
    /// independent canonical-restore project can preserve local ledgers
    /// atomically. The shared Skill boundary prevents a receipt from appearing
    /// between the live/source checks and the whole-database replacement.
    pub(crate) fn restore_binary_backup_without_pi_ownership(
        db: &Database,
        filename: &str,
    ) -> Result<String, AppError> {
        Self::coordinate_portable_import(|| {
            db.ensure_binary_restore_has_no_pi_ownership(filename)?;
            db.restore_from_backup(filename)
        })
    }

    pub(crate) fn reconcile_skill_under_guard(
        _guard: &MutexGuard<'static, ()>,
        db: &Arc<Database>,
        skill: &InstalledSkill,
    ) -> Result<(), AppError> {
        reconcile_skill_unlocked(db, skill)
    }

    pub(crate) fn reconcile_all_under_guard(
        _guard: &MutexGuard<'static, ()>,
        db: &Arc<Database>,
    ) -> Result<(), AppError> {
        let mut failures = Vec::new();
        let skills = db.get_all_installed_skills()?;

        // The portable catalog may delete or replace a Skill while this
        // device's ownership receipt is deliberately retained across import.
        // Consume those orphaned receipts first: they are the only authority
        // that permits removing the corresponding native destination safely.
        for deployment in db.get_all_pi_skill_deployments()? {
            if !skills.contains_key(&deployment.skill_id) {
                let skill_id = deployment.skill_id.clone();
                if let Err(error) = remove_recorded_deployment(db, &skill_id, deployment) {
                    log::warn!(
                        "orphaned Pi Skill deployment '{skill_id}' could not be reconciled: {error}"
                    );
                    failures.push(format!("{skill_id}={error}"));
                }
            }
        }

        for skill in skills.values() {
            // Portable sync and old databases may contain a poisoned directory
            // name. Reject it before any path join, but do not let that inert
            // row hide every valid Pi Skill during startup/storage migration.
            // Only this syntactic row corruption is skippable: source errors,
            // foreign destinations and stale ownership still fail closed.
            if let Err(error) = validate_directory_name(&skill.directory) {
                log::warn!(
                    "skipping invalid Pi Skill row '{}' during reconciliation: {error}",
                    skill.id
                );
                continue;
            }
            if let Err(error) = reconcile_skill_unlocked(db, skill) {
                log::warn!(
                    "Pi Skill '{}' could not be reconciled; continuing with independent Skills: {error}",
                    skill.id
                );
                failures.push(format!("{}={error}", skill.id));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(AppError::Config(format!(
                "Pi Skill reconciliation incomplete: {}",
                failures.join("; ")
            )))
        }
    }

    pub(crate) fn remove_before_uninstall_under_guard(
        _guard: &MutexGuard<'static, ()>,
        db: &Arc<Database>,
        skill: &InstalledSkill,
    ) -> Result<(), AppError> {
        remove_all_recorded_deployments(db, skill, None)
    }

    pub(crate) fn inspect_all(
        db: &Arc<Database>,
    ) -> Result<BTreeMap<String, SkillAppStatus>, AppError> {
        let _guard = deployment_lock();
        let discovery = scan_pi_discovery()?;
        db.get_all_installed_skills()?
            .into_iter()
            .map(|(id, skill)| {
                inspect_skill_status(db, &skill, &discovery).map(|status| (id, status))
            })
            .collect()
    }

    pub(crate) fn source_digest(path: &Path) -> Result<String, AppError> {
        tree_digest(path)
    }

    /// Remove a just-published SSOT tree only if the exact bytes installed by
    /// this operation still own the path. The namespace move precedes digest
    /// validation so an external replacement is restored, never recursively
    /// deleted after a check/use race.
    pub(crate) fn remove_source_if_unchanged(
        path: &Path,
        expected_digest: &str,
    ) -> Result<(), AppError> {
        let staged = stage_destination(path)?;
        let observed = tree_digest(&staged);
        if !matches!(observed, Ok(ref digest) if digest == expected_digest) {
            restore_staged_destination(&staged, path).map_err(|rollback| {
                AppError::Conflict(format!(
                    "Pi Skill SSOT ownership changed and rollback failed ({rollback}); recovery tree: {}",
                    staged.display()
                ))
            })?;
            return Err(AppError::Conflict(format!(
                "Pi Skill SSOT changed before rollback: {}",
                path.display()
            )));
        }
        remove_path(&staged)
    }
}

#[derive(Debug)]
struct PiDiscoveryScan {
    by_manifest: HashMap<PathBuf, (PiSkillDiscovery, Option<String>)>,
}

fn scan_pi_discovery() -> Result<PiDiscoveryScan, AppError> {
    const MAX_SKILL_MANIFEST_BYTES: u64 = 1024 * 1024;
    const MAX_SKILL_DIRECTORIES: usize = 10_000;

    let root = SkillService::get_app_skills_dir(&AppType::Pi)
        .map_err(|error| AppError::Config(error.to_string()))?;
    let mut entries = match fs::read_dir(&root) {
        Ok(entries) => entries
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::io(&root, error))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PiDiscoveryScan {
                by_manifest: HashMap::new(),
            });
        }
        Err(error) => return Err(AppError::io(&root, error)),
    };
    if entries.len() > MAX_SKILL_DIRECTORIES {
        return Err(AppError::InvalidInput(format!(
            "Pi Skill discovery exceeds {MAX_SKILL_DIRECTORIES} top-level entries"
        )));
    }
    entries.sort_by_key(std::fs::DirEntry::file_name);

    let mut winner_by_name = HashMap::<String, PathBuf>::new();
    let mut by_manifest = HashMap::new();
    for entry in entries {
        let directory = entry.path();
        let metadata = fs::metadata(&directory).map_err(|error| AppError::io(&directory, error))?;
        if !metadata.is_dir() {
            continue;
        }
        let manifest = directory.join("SKILL.md");
        let metadata = match fs::symlink_metadata(&manifest) {
            Ok(metadata) if metadata.file_type().is_file() => metadata,
            Ok(_) => {
                by_manifest.insert(
                    manifest,
                    (
                        PiSkillDiscovery::Invalid,
                        Some("SKILL.md is not a regular file".to_string()),
                    ),
                );
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(AppError::io(&manifest, error)),
        };
        if metadata.len() > MAX_SKILL_MANIFEST_BYTES {
            by_manifest.insert(
                manifest,
                (
                    PiSkillDiscovery::Invalid,
                    Some("SKILL.md exceeds the 1 MiB inspection limit".to_string()),
                ),
            );
            continue;
        }
        let parsed = SkillService::parse_skill_metadata_static(&manifest)
            .map_err(|error| AppError::Config(error.to_string()))?;
        let Some(name) = parsed.name.filter(|name| !name.trim().is_empty()) else {
            by_manifest.insert(
                manifest,
                (
                    PiSkillDiscovery::Invalid,
                    Some("SKILL.md has no non-empty frontmatter name".to_string()),
                ),
            );
            continue;
        };
        if parsed
            .description
            .as_deref()
            .is_none_or(|description| description.trim().is_empty())
        {
            by_manifest.insert(
                manifest,
                (
                    PiSkillDiscovery::Invalid,
                    Some("SKILL.md has no non-empty frontmatter description".to_string()),
                ),
            );
            continue;
        }
        if let Some(winner) = winner_by_name.get(&name) {
            by_manifest.insert(
                manifest,
                (
                    PiSkillDiscovery::Shadowed,
                    Some(format!(
                        "skill name '{name}' is shadowed by {}",
                        winner.display()
                    )),
                ),
            );
        } else {
            winner_by_name.insert(name, manifest.clone());
            by_manifest.insert(manifest, (PiSkillDiscovery::Active, None));
        }
    }
    Ok(PiDiscoveryScan { by_manifest })
}

fn inspect_skill_status(
    db: &Arc<Database>,
    skill: &InstalledSkill,
    discovery: &PiDiscoveryScan,
) -> Result<SkillAppStatus, AppError> {
    let destination = skill_destination(skill)?;
    let destination_key = destination_key(&destination);
    let deployments = db.get_pi_skill_deployments(&skill.id)?;
    let deployment = deployments
        .iter()
        .find(|deployment| deployment.destination_key == destination_key);
    let has_stale_destination = deployments
        .iter()
        .any(|deployment| deployment.destination_key != destination_key);
    let manifest = destination.join("SKILL.md");
    let discovered = discovery.by_manifest.get(&manifest);
    let destination_exists = fs::symlink_metadata(&destination).is_ok();
    let owned_deployment = deployment
        .is_some_and(|deployment| verify_owned_destination(deployment, &destination).is_ok());
    let ownership = if has_stale_destination {
        PiSkillOwnership::Stale
    } else {
        match (deployment.is_some(), destination_exists, owned_deployment) {
            (_, _, true) => PiSkillOwnership::Owned,
            (true, _, false) => PiSkillOwnership::Stale,
            (false, true, false) => PiSkillOwnership::Foreign,
            (false, false, false) => PiSkillOwnership::Absent,
        }
    };
    let (discovery_status, discovery_issue) = discovered.cloned().unwrap_or_else(|| {
        (
            PiSkillDiscovery::Absent,
            destination_exists.then(|| "Pi did not discover this destination".to_string()),
        )
    });
    let effectively_discovered = discovery_status == PiSkillDiscovery::Active;
    let issue = if has_stale_destination {
        Some("recorded Pi deployment remains at a previous agent root".to_string())
    } else {
        match ownership {
            PiSkillOwnership::Stale => {
                Some("recorded Pi deployment no longer matches the live filesystem".to_string())
            }
            PiSkillOwnership::Foreign if skill.apps.pi => {
                Some("desired Pi Skill collides with an unowned live destination".to_string())
            }
            _ => discovery_issue,
        }
    };
    Ok(SkillAppStatus {
        desired_enabled: skill.apps.pi,
        owned_deployment,
        effectively_discovered,
        ownership,
        discovery: discovery_status,
        issue,
    })
}

fn deployment_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn reconcile_skill_unlocked(db: &Arc<Database>, skill: &InstalledSkill) -> Result<(), AppError> {
    if skill.apps.pi {
        let destination = skill_destination(skill)?;
        let destination_key = destination_key(&destination);
        let existing = db.get_pi_skill_deployment(&skill.id, &destination_key)?;
        let source = skill_source(skill)?;
        deploy(
            db,
            skill,
            &source,
            &destination,
            &destination_key,
            existing,
            None,
        )?;
        cleanup_stale_deployments_after_commit(db, skill, &destination_key);
        Ok(())
    } else {
        remove_all_recorded_deployments(db, skill, None)
    }
}

fn cleanup_stale_deployments(
    db: &Arc<Database>,
    skill: &InstalledSkill,
    current_destination_key: &str,
) -> Result<(), AppError> {
    for deployment in db.get_pi_skill_deployments(&skill.id)? {
        if deployment.destination_key != current_destination_key {
            remove_recorded_deployment(db, &skill.id, deployment)?;
        }
    }
    Ok(())
}

/// `deploy` atomically commits the new native destination, its ownership
/// ledger, and (when requested) desired state before old-root cleanup begins.
/// A drifted old root must remain visible as stale ownership evidence, but it
/// must not turn that committed publication into an error: several callers
/// compensate a returned error by deleting the Skill's SSOT/database row.
fn cleanup_stale_deployments_after_commit(
    db: &Arc<Database>,
    skill: &InstalledSkill,
    current_destination_key: &str,
) {
    if let Err(error) = cleanup_stale_deployments(db, skill, current_destination_key) {
        log::warn!(
            "Pi Skill '{}' was published at its current agent root, but stale deployment cleanup \
             remains pending: {error}",
            skill.id
        );
    }
}

fn remove_all_recorded_deployments(
    db: &Arc<Database>,
    skill: &InstalledSkill,
    desired_enabled: Option<bool>,
) -> Result<(), AppError> {
    if let Some(desired_enabled) = desired_enabled {
        // Desired state is one row-level authority, independent of how many
        // old agent roots still have device-local ownership evidence.
        db.set_pi_skill_desired(&skill.id, desired_enabled)?;
    }
    for deployment in db.get_pi_skill_deployments(&skill.id)? {
        remove_recorded_deployment(db, &skill.id, deployment)?;
    }
    Ok(())
}

fn remove_recorded_deployment(
    db: &Arc<Database>,
    skill_id: &str,
    deployment: SkillDeployment,
) -> Result<(), AppError> {
    if deployment.skill_id != skill_id {
        return Err(AppError::Conflict(format!(
            "Pi Skill deployment identity changed from '{skill_id}' to '{}'",
            deployment.skill_id
        )));
    }
    let destination = PathBuf::from(&deployment.destination);
    if destination_key(&destination) != deployment.destination_key {
        return Err(AppError::Conflict(format!(
            "Pi Skill '{}' has inconsistent recorded destination identity",
            skill_id
        )));
    }
    match fs::symlink_metadata(&destination) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            db.delete_pi_skill_deployment(skill_id, &deployment.destination_key)?;
            Ok(())
        }
        Err(error) => Err(AppError::io(&destination, error)),
        Ok(_) => {
            let key = deployment.destination_key.clone();
            remove_owned(db, skill_id, &destination, &key, Some(deployment), None)
        }
    }
}

fn skill_source(skill: &InstalledSkill) -> Result<PathBuf, AppError> {
    validate_directory_name(&skill.directory)?;
    let source = SkillService::get_ssot_dir()
        .map_err(|error| AppError::Config(error.to_string()))?
        .join(&skill.directory);
    validate_source_tree(&source)?;
    Ok(source)
}

fn skill_destination(skill: &InstalledSkill) -> Result<PathBuf, AppError> {
    validate_directory_name(&skill.directory)?;
    Ok(SkillService::get_app_skills_dir(&AppType::Pi)
        .map_err(|error| AppError::Config(error.to_string()))?
        .join(&skill.directory))
}

fn validate_directory_name(value: &str) -> Result<(), AppError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.components().count() != 1
        || matches!(
            path.components().next(),
            Some(std::path::Component::CurDir | std::path::Component::ParentDir)
        )
        || value.starts_with('.')
    {
        return Err(AppError::InvalidInput(format!(
            "invalid Pi Skill directory '{value}'"
        )));
    }
    Ok(())
}

fn destination_key(destination: &Path) -> String {
    #[cfg(windows)]
    {
        destination
            .to_string_lossy()
            .replace('\\', "/")
            .to_lowercase()
    }
    #[cfg(not(windows))]
    {
        destination.to_string_lossy().into_owned()
    }
}

fn source_identity(source: &Path) -> Result<(String, String), AppError> {
    let canonical = source
        .canonicalize()
        .map_err(|error| AppError::io(source, error))?;
    let digest = tree_digest(source)?;
    Ok((
        format!("path:{};digest:{digest}", canonical.display()),
        digest,
    ))
}

fn adopt_exact_import(
    db: &Arc<Database>,
    skill: &InstalledSkill,
    source: &Path,
    destination: &Path,
    destination_key: &str,
) -> Result<(), AppError> {
    validate_source_tree(source)?;
    validate_source_tree(destination)?;
    let (source_identity, source_digest) = source_identity(source)?;
    let destination_digest = tree_digest(destination)?;
    if source_digest != destination_digest {
        return Err(AppError::Conflict(format!(
            "cannot adopt Pi Skill '{}': native destination differs from the imported SSOT",
            skill.directory
        )));
    }

    let previous_desired = db
        .get_installed_skill(&skill.id)?
        .ok_or_else(|| {
            AppError::Conflict(format!(
                "Pi Skill '{}' disappeared before import adoption",
                skill.id
            ))
        })?
        .apps
        .pi;
    let now = chrono::Utc::now().timestamp_millis();
    let deployment = SkillDeployment {
        skill_id: skill.id.clone(),
        destination: destination.to_string_lossy().into_owned(),
        destination_key: destination_key.to_string(),
        method: SkillDeploymentMethod::Copy,
        source_identity,
        deployed_digest: Some(destination_digest),
        created_at: now,
        updated_at: now,
    };
    db.save_pi_skill_deployment_with_desired(&deployment, Some(true))?;

    let final_verification = verify_owned_destination(&deployment, destination).and_then(|_| {
        let current_source_digest = tree_digest(source)?;
        if current_source_digest == source_digest {
            Ok(())
        } else {
            Err(AppError::Conflict(format!(
                "cannot adopt Pi Skill '{}': SSOT changed during import",
                skill.directory
            )))
        }
    });
    if let Err(error) = final_verification {
        let rollback = db.delete_pi_skill_deployment_with_desired(
            &skill.id,
            destination_key,
            Some(previous_desired),
        );
        return match rollback {
            Ok(true) => Err(error),
            Ok(false) => Err(AppError::Conflict(format!(
                "Pi Skill import adoption failed ({error}); ownership rollback found no ledger row"
            ))),
            Err(rollback) => Err(AppError::Conflict(format!(
                "Pi Skill import adoption failed ({error}); ownership rollback failed ({rollback})"
            ))),
        };
    }
    Ok(())
}

fn deploy(
    db: &Arc<Database>,
    skill: &InstalledSkill,
    source: &Path,
    destination: &Path,
    destination_key: &str,
    existing: Option<SkillDeployment>,
    desired_enabled: Option<bool>,
) -> Result<(), AppError> {
    let (source_identity, source_digest) = source_identity(source)?;
    if let Some(existing) = existing.as_ref() {
        verify_owned_destination(existing, destination)?;
    } else if fs::symlink_metadata(destination).is_ok() {
        return Err(AppError::Conflict(format!(
            "Pi Skill destination already exists without ownership evidence: {}",
            destination.display()
        )));
    }

    let requested_method = choose_method();
    let previous = existing.clone();
    let staged_previous = if let Some(previous_deployment) = previous.as_ref() {
        let staged = stage_destination(destination)?;
        if let Err(error) = verify_deployment_identity(previous_deployment, &staged) {
            restore_staged_destination(&staged, destination).map_err(|rollback| {
                AppError::Conflict(format!(
                    "Pi Skill changed while it was staged ({error}); restoring it failed ({rollback})"
                ))
            })?;
            return Err(error);
        }
        Some(staged)
    } else {
        None
    };
    let method = match replace_destination(source, destination, requested_method) {
        Ok(method) => method,
        Err(error) => {
            if let Some(staged) = staged_previous.as_deref() {
                restore_staged_destination(staged, destination).map_err(|rollback| {
                    AppError::Conflict(format!(
                        "Pi Skill deployment failed ({error}); previous deployment rollback failed ({rollback})"
                    ))
                })?;
            }
            return Err(error);
        }
    };
    let now = chrono::Utc::now().timestamp_millis();
    let deployment = SkillDeployment {
        skill_id: skill.id.clone(),
        destination: destination.to_string_lossy().into_owned(),
        destination_key: destination_key.to_string(),
        method,
        source_identity,
        deployed_digest: (method == SkillDeploymentMethod::Copy).then_some(source_digest),
        created_at: previous.as_ref().map_or(now, |value| value.created_at),
        updated_at: now,
    };
    if let Err(error) = verify_owned_destination(&deployment, destination) {
        rollback_verified_replacement(&deployment, destination, staged_previous.as_deref())
            .map_err(|rollback| {
                AppError::Conflict(format!(
                    "Pi Skill deployment identity check failed ({error}); rollback failed ({rollback})"
                ))
            })?;
        return Err(error);
    }
    if let Err(error) = db.save_pi_skill_deployment_with_desired(&deployment, desired_enabled) {
        rollback_verified_replacement(&deployment, destination, staged_previous.as_deref())
            .map_err(|rollback| {
                AppError::Conflict(format!(
                    "Pi Skill ledger write failed ({error}); deployment rollback failed ({rollback})"
                ))
            })?;
        return Err(error);
    }
    if let Some(staged) = staged_previous {
        let previous_deployment = previous.as_ref().ok_or_else(|| {
            AppError::Config(
                "Pi Skill replacement staging lost its previous ownership record".to_string(),
            )
        })?;
        verify_deployment_identity(previous_deployment, &staged)?;
        if let Err(error) = remove_path(&staged) {
            log::warn!(
                "failed to remove committed Pi Skill rollback staging '{}': {error}",
                staged.display()
            );
        }
    }
    Ok(())
}

fn remove_owned(
    db: &Arc<Database>,
    skill_id: &str,
    destination: &Path,
    destination_key: &str,
    existing: Option<SkillDeployment>,
    desired_enabled: Option<bool>,
) -> Result<(), AppError> {
    if let Some(desired_enabled) = desired_enabled {
        // Persist user intent before filesystem validation or cleanup. Drift
        // therefore reports a conflict while the toggle remains off and the
        // ledger is retained as deletion evidence.
        db.set_pi_skill_desired(skill_id, desired_enabled)?;
    }
    let Some(existing) = existing else {
        // Foreign/native discovered directories are preserved.
        return Ok(());
    };
    verify_owned_destination(&existing, destination)?;
    let staged = stage_destination(destination)?;
    if let Err(error) = verify_deployment_identity(&existing, &staged) {
        restore_staged_destination(&staged, destination).map_err(|rollback| {
            AppError::Conflict(format!(
                "Pi Skill changed while it was staged for removal ({error}); restoring it failed ({rollback})"
            ))
        })?;
        return Err(error);
    }
    let ledger_error = match db.delete_pi_skill_deployment(skill_id, destination_key) {
        Ok(true) => None,
        Ok(false) => Some(AppError::Conflict(format!(
            "Pi Skill '{skill_id}' ownership receipt disappeared before native cleanup"
        ))),
        Err(error) => Some(error),
    };
    if let Some(error) = ledger_error {
        restore_staged_destination(&staged, destination).map_err(|rollback| {
            AppError::Conflict(format!(
                "Pi Skill ledger cleanup failed ({error}); file rollback failed ({rollback})"
            ))
        })?;
        return Err(error);
    }
    verify_deployment_identity(&existing, &staged)?;
    if let Err(error) = remove_path(&staged) {
        log::warn!(
            "failed to remove disabled Pi Skill rollback staging '{}': {error}",
            staged.display()
        );
    }
    if fs::symlink_metadata(destination).is_ok() {
        return Err(AppError::Conflict(format!(
            "Pi Skill destination was recreated concurrently after ownership removal: {}",
            destination.display()
        )));
    }
    Ok(())
}

fn choose_method() -> SkillDeploymentMethod {
    match crate::settings::get_skill_sync_method() {
        SyncMethod::Copy => SkillDeploymentMethod::Copy,
        SyncMethod::Symlink | SyncMethod::Auto => SkillDeploymentMethod::Symlink,
    }
}

fn verify_owned_destination(
    deployment: &SkillDeployment,
    destination: &Path,
) -> Result<(), AppError> {
    if Path::new(&deployment.destination) != destination {
        return Err(AppError::Conflict(
            "Pi Skill deployment destination changed since it was recorded".to_string(),
        ));
    }
    verify_deployment_identity(deployment, destination)
}

fn verify_deployment_identity(
    deployment: &SkillDeployment,
    destination: &Path,
) -> Result<(), AppError> {
    match deployment.method {
        SkillDeploymentMethod::Symlink => {
            let metadata = fs::symlink_metadata(destination)
                .map_err(|error| AppError::io(destination, error))?;
            if !metadata.file_type().is_symlink() {
                return Err(AppError::Conflict(format!(
                    "owned Pi Skill symlink was replaced externally: {}",
                    destination.display()
                )));
            }
            let target =
                fs::read_link(destination).map_err(|error| AppError::io(destination, error))?;
            let resolved = if target.is_absolute() {
                target
            } else {
                destination
                    .parent()
                    .unwrap_or_else(|| Path::new("."))
                    .join(target)
            };
            let recorded_source = deployment
                .source_identity
                .strip_prefix("path:")
                .and_then(|identity| {
                    identity
                        .rsplit_once(";digest:")
                        .map(|(path, _digest)| PathBuf::from(path))
                })
                .ok_or_else(|| {
                    AppError::Conflict(format!(
                        "owned Pi Skill has invalid source identity: {}",
                        destination.display()
                    ))
                })?;
            let observed_source = match resolved.canonicalize() {
                Ok(canonical) => canonical,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // Portable sync replaces the SSOT before reconciling its
                    // database catalog. A removed Skill therefore leaves a
                    // dangling but still identifiable owned symlink. Only an
                    // exact recorded target may be removed in that state.
                    let parent = resolved.parent().ok_or_else(|| {
                        AppError::Conflict(format!(
                            "owned Pi Skill symlink target has no parent: {}",
                            destination.display()
                        ))
                    })?;
                    let name = resolved.file_name().ok_or_else(|| {
                        AppError::Conflict(format!(
                            "owned Pi Skill symlink target has no final component: {}",
                            destination.display()
                        ))
                    })?;
                    parent
                        .canonicalize()
                        .map_err(|error| AppError::io(parent, error))?
                        .join(name)
                }
                Err(error) => return Err(AppError::io(&resolved, error)),
            };
            if observed_source != recorded_source {
                return Err(AppError::Conflict(format!(
                    "owned Pi Skill symlink target changed externally: {}",
                    destination.display()
                )));
            }
        }
        SkillDeploymentMethod::Copy => {
            let expected = deployment.deployed_digest.as_deref().ok_or_else(|| {
                AppError::Conflict("copied Pi Skill lacks a recorded digest".to_string())
            })?;
            if tree_digest(destination)? != expected {
                return Err(AppError::Conflict(format!(
                    "owned Pi Skill copy was modified externally: {}",
                    destination.display()
                )));
            }
        }
    }
    Ok(())
}

fn replace_destination(
    source: &Path,
    destination: &Path,
    method: SkillDeploymentMethod,
) -> Result<SkillDeploymentMethod, AppError> {
    if fs::symlink_metadata(destination).is_ok() {
        return Err(AppError::Conflict(format!(
            "Pi Skill replacement destination is not empty: {}",
            destination.display()
        )));
    }
    let parent = destination
        .parent()
        .ok_or_else(|| AppError::InvalidInput("Pi Skill destination has no parent".to_string()))?;
    fs::create_dir_all(parent).map_err(|error| AppError::io(parent, error))?;
    match method {
        SkillDeploymentMethod::Symlink => match create_directory_symlink(source, destination) {
            Ok(()) => Ok(SkillDeploymentMethod::Symlink),
            Err(_) if matches!(crate::settings::get_skill_sync_method(), SyncMethod::Auto) => {
                copy_tree_atomic(source, destination)?;
                Ok(SkillDeploymentMethod::Copy)
            }
            Err(error) => Err(error),
        },
        SkillDeploymentMethod::Copy => {
            copy_tree_atomic(source, destination)?;
            Ok(SkillDeploymentMethod::Copy)
        }
    }
}

fn stage_destination(destination: &Path) -> Result<PathBuf, AppError> {
    let parent = destination
        .parent()
        .ok_or_else(|| AppError::InvalidInput("Pi Skill destination has no parent".to_string()))?;
    let file_name = destination
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            AppError::InvalidInput("Pi Skill destination name is invalid".to_string())
        })?;
    let staged = parent.join(format!(
        ".{file_name}.cc-switch-rollback-{}",
        uuid::Uuid::new_v4().simple()
    ));
    fs::rename(destination, &staged).map_err(|error| AppError::io(destination, error))?;
    Ok(staged)
}

fn restore_staged_destination(staged: &Path, destination: &Path) -> Result<(), AppError> {
    if fs::symlink_metadata(destination).is_ok() {
        return Err(AppError::Conflict(format!(
            "refusing to overwrite a concurrently created Pi Skill destination: {}",
            destination.display()
        )));
    }
    fs::rename(staged, destination).map_err(|error| AppError::io(destination, error))
}

fn rollback_verified_replacement(
    deployment: &SkillDeployment,
    destination: &Path,
    staged_previous: Option<&Path>,
) -> Result<(), AppError> {
    let staged_replacement = stage_destination(destination)?;
    if let Err(error) = verify_deployment_identity(deployment, &staged_replacement) {
        restore_staged_destination(&staged_replacement, destination).map_err(|rollback| {
            AppError::Conflict(format!(
                "replacement ownership was lost before rollback ({error}); preserving it also failed ({rollback})"
            ))
        })?;
        return Err(error);
    }
    remove_path(&staged_replacement)?;
    if let Some(staged) = staged_previous {
        restore_staged_destination(staged, destination)?;
    }
    Ok(())
}

#[cfg(unix)]
fn create_directory_symlink(source: &Path, destination: &Path) -> Result<(), AppError> {
    std::os::unix::fs::symlink(source, destination)
        .map_err(|error| AppError::io(destination, error))
}

#[cfg(windows)]
fn create_directory_symlink(source: &Path, destination: &Path) -> Result<(), AppError> {
    std::os::windows::fs::symlink_dir(source, destination)
        .map_err(|error| AppError::io(destination, error))
}

fn copy_tree_atomic(source: &Path, destination: &Path) -> Result<(), AppError> {
    let parent = destination
        .parent()
        .ok_or_else(|| AppError::InvalidInput("Pi Skill destination has no parent".to_string()))?;
    let temp = parent.join(format!(".pi-skill-{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = copy_tree(source, &temp).and_then(|_| {
        fs::rename(&temp, destination).map_err(|error| AppError::io(destination, error))
    });
    if result.is_err() {
        let _ = fs::remove_dir_all(&temp);
    }
    result
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), AppError> {
    validate_source_tree(source)?;
    fs::create_dir(destination).map_err(|error| AppError::io(destination, error))?;
    for entry in fs::read_dir(source).map_err(|error| AppError::io(source, error))? {
        let entry = entry.map_err(|error| AppError::io(source, error))?;
        let file_type = entry
            .file_type()
            .map_err(|error| AppError::io(entry.path(), error))?;
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), &target).map_err(|error| AppError::io(&target, error))?;
        } else {
            return Err(AppError::InvalidInput(format!(
                "Pi Skill source contains a non-regular entry: {}",
                entry.path().display()
            )));
        }
    }
    Ok(())
}

fn validate_source_tree(source: &Path) -> Result<(), AppError> {
    let metadata = fs::symlink_metadata(source).map_err(|error| AppError::io(source, error))?;
    if !metadata.file_type().is_dir() || !source.join("SKILL.md").is_file() {
        return Err(AppError::InvalidInput(format!(
            "Pi Skill source must be a directory containing SKILL.md: {}",
            source.display()
        )));
    }
    Ok(())
}

fn tree_digest(root: &Path) -> Result<String, AppError> {
    let mut entries = Vec::new();
    collect_digest_entries(root, root, &mut entries)?;
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    let mut hasher = Sha256::new();
    for (relative, bytes) in entries {
        hasher.update((relative.len() as u64).to_le_bytes());
        hasher.update(relative.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

fn collect_digest_entries(
    root: &Path,
    current: &Path,
    output: &mut Vec<(String, Vec<u8>)>,
) -> Result<(), AppError> {
    for entry in fs::read_dir(current).map_err(|error| AppError::io(current, error))? {
        let entry = entry.map_err(|error| AppError::io(current, error))?;
        let path = entry.path();
        let kind = entry
            .file_type()
            .map_err(|error| AppError::io(&path, error))?;
        if kind.is_dir() {
            collect_digest_entries(root, &path, output)?;
        } else if kind.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| AppError::Config("Pi Skill path escaped its root".to_string()))?
                .to_string_lossy()
                .replace('\\', "/");
            output.push((
                relative,
                fs::read(&path).map_err(|error| AppError::io(&path, error))?,
            ));
        } else {
            return Err(AppError::InvalidInput(format!(
                "Pi Skill tree contains a symlink or special entry: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

fn remove_path(path: &Path) -> Result<(), AppError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(AppError::io(path, error)),
    };
    if metadata.file_type().is_symlink() || metadata.file_type().is_file() {
        fs::remove_file(path).map_err(|error| AppError::io(path, error))
    } else if metadata.file_type().is_dir() {
        fs::remove_dir_all(path).map_err(|error| AppError::io(path, error))
    } else {
        Err(AppError::InvalidInput(format!(
            "refusing to remove special Pi Skill entry: {}",
            path.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_config::SkillApps;

    #[test]
    #[serial_test::serial]
    fn portable_import_waits_for_the_skill_ownership_boundary() {
        use std::sync::mpsc;
        use std::time::Duration;

        let db = Database::memory().expect("database");
        let missing = tempfile::tempdir()
            .expect("tempdir")
            .path()
            .join("missing.sql");
        let guard = PiSkillDeploymentService::operation_guard();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            ready_tx.send(()).expect("signal worker ready");
            let result = PiSkillDeploymentService::import_portable_sql(&db, &missing);
            result_tx.send(result).expect("signal import result");
        });

        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker reaches import entry");
        assert!(
            result_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "portable import must not pass capture/publish while a Skill mutation owns the boundary"
        );
        drop(guard);
        let result = result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("import proceeds after ownership boundary release");
        assert!(
            result.is_err(),
            "the intentionally missing import must fail"
        );
        worker.join().expect("worker");
    }

    #[test]
    #[serial_test::serial]
    fn binary_restore_waits_for_the_skill_ownership_boundary() {
        use std::sync::mpsc;
        use std::time::Duration;

        let db = Database::memory().expect("database");
        db.claim_pi_projection_key("local-provider", "local-key")
            .expect("local ownership");
        let guard = PiSkillDeploymentService::operation_guard();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            ready_tx.send(()).expect("signal worker ready");
            let result = PiSkillDeploymentService::restore_binary_backup_without_pi_ownership(
                &db,
                "missing.db",
            );
            result_tx.send(result).expect("signal restore result");
        });

        ready_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker reaches restore entry");
        assert!(
            result_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "binary restore must not pass the Pi Skill ownership boundary"
        );
        drop(guard);
        let error = result_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("restore proceeds after boundary release")
            .expect_err("live ownership is rejected");
        assert!(error.to_string().contains("portable SQL"));
        worker.join().expect("worker");
    }

    #[test]
    #[serial_test::serial]
    fn binary_restore_service_rejects_ownership_from_the_selected_backup() {
        struct EnvGuard(Option<std::ffi::OsString>);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
                    None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
                }
            }
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let _home = EnvGuard(std::env::var_os("CC_SWITCH_TEST_HOME"));
        std::env::set_var("CC_SWITCH_TEST_HOME", temp.path());
        let backup_dir = crate::config::get_app_config_dir().join("backups");
        fs::create_dir_all(&backup_dir).expect("backup directory");
        let source = rusqlite::Connection::open(backup_dir.join("owned.db")).expect("owned backup");
        source
            .execute_batch(
                "CREATE TABLE pi_provider_projections (
                    provider_id TEXT PRIMARY KEY,
                    provider_key TEXT NOT NULL
                 );
                 INSERT INTO pi_provider_projections (provider_id, provider_key)
                 VALUES ('historical-provider', 'native-key');",
            )
            .expect("historical ownership");
        drop(source);

        let db = Database::memory().expect("database");
        let error =
            PiSkillDeploymentService::restore_binary_backup_without_pi_ownership(&db, "owned.db")
                .expect_err("historical ownership must be rejected before restore");
        match error {
            AppError::Localized { en, .. } => {
                assert!(en.contains("selected backup"));
                assert!(en.contains("portable SQL"));
            }
            other => panic!("expected structured ownership rejection, got {other:?}"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn binary_restore_service_keeps_working_without_pi_ownership() {
        struct EnvGuard(Option<std::ffi::OsString>);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
                    None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
                }
            }
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let _home = EnvGuard(std::env::var_os("CC_SWITCH_TEST_HOME"));
        std::env::set_var("CC_SWITCH_TEST_HOME", temp.path());
        let db = Database::init().expect("live database");
        let backup = db
            .backup_database_file()
            .expect("create backup")
            .expect("live database exists");
        let filename = backup
            .file_name()
            .and_then(|value| value.to_str())
            .expect("backup filename");

        let safety =
            PiSkillDeploymentService::restore_binary_backup_without_pi_ownership(&db, filename)
                .expect("empty ownership boundary permits legacy binary restore");
        assert!(!safety.is_empty());
    }

    #[test]
    fn digest_includes_hidden_files_and_rejects_symlinks() {
        let temp = tempfile::tempdir().expect("tempdir");
        fs::write(temp.path().join("SKILL.md"), "skill").expect("manifest");
        fs::write(temp.path().join(".hidden"), "one").expect("hidden");
        let first = tree_digest(temp.path()).expect("digest");
        fs::write(temp.path().join(".hidden"), "two").expect("hidden update");
        assert_ne!(tree_digest(temp.path()).expect("digest"), first);
    }

    #[test]
    fn rollback_preserves_a_destination_without_matching_ownership_evidence() {
        let temp = tempfile::tempdir().expect("tempdir");
        let destination = temp.path().join("skill");
        fs::create_dir(&destination).expect("foreign destination");
        fs::write(destination.join("SKILL.md"), "foreign").expect("foreign manifest");
        let deployment = SkillDeployment {
            skill_id: "skill".to_string(),
            destination: destination.to_string_lossy().into_owned(),
            destination_key: destination_key(&destination),
            method: SkillDeploymentMethod::Copy,
            source_identity: "path:/managed;digest:sha256:managed".to_string(),
            deployed_digest: Some("sha256:not-the-foreign-tree".to_string()),
            created_at: 1,
            updated_at: 1,
        };

        assert!(rollback_verified_replacement(&deployment, &destination, None).is_err());
        assert_eq!(
            fs::read_to_string(destination.join("SKILL.md")).expect("foreign content survives"),
            "foreign"
        );
    }

    #[test]
    fn disabling_a_drifted_owned_skill_persists_intent_and_keeps_ledger() {
        let temp = tempfile::tempdir().expect("tempdir");
        let destination = temp.path().join("skill");
        fs::create_dir(&destination).expect("destination");
        fs::write(
            destination.join("SKILL.md"),
            "---\nname: skill\ndescription: before\n---\n",
        )
        .expect("manifest");
        let original_digest = tree_digest(&destination).expect("original digest");
        let mut skill = InstalledSkill {
            id: "local:skill".to_string(),
            name: "Skill".to_string(),
            description: Some("before".to_string()),
            directory: "skill".to_string(),
            repo_owner: None,
            repo_name: None,
            repo_branch: None,
            readme_url: None,
            apps: SkillApps::only(&AppType::Pi),
            installed_at: 1,
            content_hash: Some(original_digest.clone()),
            updated_at: 1,
        };
        let db = Arc::new(Database::memory().expect("database"));
        db.save_skill(&skill).expect("save skill");
        let key = destination_key(&destination);
        db.save_pi_skill_deployment(&SkillDeployment {
            skill_id: skill.id.clone(),
            destination: destination.to_string_lossy().into_owned(),
            destination_key: key.clone(),
            method: SkillDeploymentMethod::Copy,
            source_identity: format!("path:{};digest:{original_digest}", destination.display()),
            deployed_digest: Some(original_digest),
            created_at: 1,
            updated_at: 1,
        })
        .expect("save ledger");

        fs::write(destination.join("changed.txt"), "external drift").expect("drift");
        let error = remove_owned(
            &db,
            &skill.id,
            &destination,
            &key,
            db.get_pi_skill_deployment(&skill.id, &key)
                .expect("read ledger"),
            Some(false),
        )
        .expect_err("drift must block deletion");
        assert!(matches!(error, AppError::Conflict(_)));

        skill = db
            .get_installed_skill(&skill.id)
            .expect("read skill")
            .expect("skill remains");
        assert!(!skill.apps.pi, "desired state must remain disabled");
        assert!(
            db.get_pi_skill_deployment(&skill.id, &key)
                .expect("read ledger")
                .is_some(),
            "drift evidence must remain for explicit resolution"
        );
        assert!(destination.join("changed.txt").is_file());
    }

    #[test]
    #[serial_test::serial]
    fn discovery_rejects_a_manifest_without_pinned_required_description() {
        struct EnvGuard(Option<std::ffi::OsString>);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("PI_CODING_AGENT_DIR", value),
                    None => std::env::remove_var("PI_CODING_AGENT_DIR"),
                }
            }
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let _guard = EnvGuard(std::env::var_os("PI_CODING_AGENT_DIR"));
        std::env::set_var("PI_CODING_AGENT_DIR", temp.path());
        let manifest = temp.path().join("skills").join("invalid").join("SKILL.md");
        fs::create_dir_all(manifest.parent().expect("manifest parent")).expect("skills dir");
        fs::write(&manifest, "---\nname: invalid\n---\n").expect("manifest");

        let discovery = scan_pi_discovery().expect("scan");
        assert_eq!(
            discovery.by_manifest[&manifest].0,
            PiSkillDiscovery::Invalid
        );
        assert!(discovery.by_manifest[&manifest]
            .1
            .as_deref()
            .is_some_and(|issue| issue.contains("description")));
    }

    #[test]
    #[serial_test::serial]
    fn agent_root_relocation_deploys_new_destination_before_cleaning_old_ownership() {
        struct EnvGuard {
            key: &'static str,
            previous: Option<std::ffi::OsString>,
        }
        impl EnvGuard {
            fn set(key: &'static str, value: &Path) -> Self {
                let previous = std::env::var_os(key);
                std::env::set_var(key, value);
                Self { key, previous }
            }
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.previous.take() {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
                if self.key == "CC_SWITCH_TEST_HOME" {
                    let _ = crate::settings::reload_settings();
                }
            }
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let _home = EnvGuard::set("CC_SWITCH_TEST_HOME", temp.path());
        crate::settings::reload_settings().expect("reload settings");
        let old_root = temp.path().join("old-pi");
        let new_root = temp.path().join("new-pi");
        let _pi_root = EnvGuard::set("PI_CODING_AGENT_DIR", &old_root);

        let source = SkillService::get_ssot_dir()
            .expect("SSOT")
            .join("relocated");
        fs::create_dir_all(&source).expect("source");
        fs::write(
            source.join("SKILL.md"),
            "---\nname: relocated\ndescription: relocation test\n---\n",
        )
        .expect("manifest");
        let skill = InstalledSkill {
            id: "local:relocated".to_string(),
            name: "Relocated".to_string(),
            description: Some("relocation test".to_string()),
            directory: "relocated".to_string(),
            repo_owner: None,
            repo_name: None,
            repo_branch: None,
            readme_url: None,
            apps: SkillApps::only(&AppType::Pi),
            installed_at: 1,
            content_hash: None,
            updated_at: 1,
        };
        let db = Arc::new(Database::memory().expect("database"));
        db.save_skill(&skill).expect("save skill");
        PiSkillDeploymentService::reconcile_skill(&db, &skill).expect("old deployment");
        let old_destination = old_root.join("skills").join("relocated");
        assert!(fs::symlink_metadata(&old_destination).is_ok());

        std::env::set_var("PI_CODING_AGENT_DIR", &new_root);
        PiSkillDeploymentService::reconcile_skill(&db, &skill).expect("relocate deployment");
        let new_destination = new_root.join("skills").join("relocated");
        assert!(fs::symlink_metadata(&new_destination).is_ok());
        assert!(
            fs::symlink_metadata(&old_destination).is_err(),
            "the verified old deployment must be cleaned only after new publication"
        );
        let deployments = db
            .get_pi_skill_deployments(&skill.id)
            .expect("deployment ledger");
        assert_eq!(deployments.len(), 1);
        assert_eq!(
            deployments[0].destination_key,
            destination_key(&new_destination)
        );
    }

    #[test]
    #[serial_test::serial]
    fn drifted_old_root_is_post_commit_status_not_a_failed_new_publication() {
        struct EnvGuard {
            key: &'static str,
            previous: Option<std::ffi::OsString>,
        }
        impl EnvGuard {
            fn set(key: &'static str, value: &Path) -> Self {
                let previous = std::env::var_os(key);
                std::env::set_var(key, value);
                Self { key, previous }
            }
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.previous.take() {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
                if self.key == "CC_SWITCH_TEST_HOME" {
                    let _ = crate::settings::reload_settings();
                }
            }
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let _home = EnvGuard::set("CC_SWITCH_TEST_HOME", temp.path());
        crate::settings::reload_settings().expect("reload settings");
        let old_root = temp.path().join("old-pi");
        let new_root = temp.path().join("new-pi");
        let _pi_root = EnvGuard::set("PI_CODING_AGENT_DIR", &old_root);
        let source = SkillService::get_ssot_dir()
            .expect("SSOT")
            .join("drifted-relocation");
        fs::create_dir_all(&source).expect("source");
        fs::write(
            source.join("SKILL.md"),
            "---\nname: drifted-relocation\ndescription: relocation test\n---\n",
        )
        .expect("manifest");
        let mut skill = InstalledSkill {
            id: "local:drifted-relocation".to_string(),
            name: "Drifted relocation".to_string(),
            description: Some("relocation test".to_string()),
            directory: "drifted-relocation".to_string(),
            repo_owner: None,
            repo_name: None,
            repo_branch: None,
            readme_url: None,
            apps: SkillApps::only(&AppType::Pi),
            installed_at: 1,
            content_hash: None,
            updated_at: 1,
        };
        let db = Arc::new(Database::memory().expect("database"));
        db.save_skill(&skill).expect("save skill");
        PiSkillDeploymentService::reconcile_skill(&db, &skill).expect("old deployment");
        let old_destination = old_root.join("skills").join(&skill.directory);
        remove_path(&old_destination).expect("replace old deployment");
        fs::create_dir_all(&old_destination).expect("foreign old destination");
        fs::write(old_destination.join("external.txt"), "external").expect("external drift");

        std::env::set_var("PI_CODING_AGENT_DIR", &new_root);
        PiSkillDeploymentService::toggle(&db, &mut skill, true)
            .expect("the new publication is already committed");

        let new_destination = new_root.join("skills").join(&skill.directory);
        assert!(fs::symlink_metadata(&new_destination).is_ok());
        assert_eq!(
            fs::read_to_string(old_destination.join("external.txt")).expect("external survives"),
            "external"
        );
        let stored = db
            .get_installed_skill(&skill.id)
            .expect("read desired state")
            .expect("skill remains");
        assert!(stored.apps.pi);
        assert_eq!(
            db.get_pi_skill_deployments(&skill.id)
                .expect("ledger")
                .len(),
            2,
            "old drift evidence and the committed new deployment must both remain"
        );
        let status =
            PiSkillDeploymentService::inspect_all(&db).expect("inspect")[&skill.id].clone();
        assert_eq!(status.ownership, PiSkillOwnership::Stale);
        assert!(status.effectively_discovered);
        assert!(status.issue.is_some_and(|issue| issue.contains("previous")));
    }

    #[test]
    #[serial_test::serial]
    fn reconcile_all_handles_independent_and_orphaned_skills() {
        struct EnvGuard {
            key: &'static str,
            previous: Option<std::ffi::OsString>,
        }
        impl EnvGuard {
            fn set(key: &'static str, value: &Path) -> Self {
                let previous = std::env::var_os(key);
                std::env::set_var(key, value);
                Self { key, previous }
            }
        }
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match self.previous.take() {
                    Some(value) => std::env::set_var(self.key, value),
                    None => std::env::remove_var(self.key),
                }
                if self.key == "CC_SWITCH_TEST_HOME" {
                    let _ = crate::settings::reload_settings();
                }
            }
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let _home = EnvGuard::set("CC_SWITCH_TEST_HOME", temp.path());
        crate::settings::reload_settings().expect("reload settings");
        let pi_root = temp.path().join("pi");
        let _pi_root = EnvGuard::set("PI_CODING_AGENT_DIR", &pi_root);
        let ssot = SkillService::get_ssot_dir().expect("SSOT");
        for directory in ["good", "orphan"] {
            let source = ssot.join(directory);
            fs::create_dir_all(&source).expect("skill source");
            fs::write(
                source.join("SKILL.md"),
                format!("---\nname: {directory}\ndescription: reconciliation test\n---\n"),
            )
            .expect("manifest");
        }

        let db = Arc::new(Database::memory().expect("database"));
        for (id, directory) in [
            ("local:bad", "missing"),
            ("local:good", "good"),
            ("local:orphan", "orphan"),
        ] {
            db.save_skill(&InstalledSkill {
                id: id.to_string(),
                name: directory.to_string(),
                description: Some("reconciliation test".to_string()),
                directory: directory.to_string(),
                repo_owner: None,
                repo_name: None,
                repo_branch: None,
                readme_url: None,
                apps: SkillApps::only(&AppType::Pi),
                installed_at: 1,
                content_hash: None,
                updated_at: 1,
            })
            .expect("save skill");
        }

        let error = PiSkillDeploymentService::reconcile_all(&db)
            .expect_err("missing source must remain visible");
        assert!(error.to_string().contains("local:bad"));
        assert!(
            fs::symlink_metadata(pi_root.join("skills").join("good")).is_ok(),
            "one invalid Skill must not hide an independent valid deployment"
        );

        // Portable import can remove a catalog row and replace its SSOT while
        // retaining this device's ownership receipt. Reconciliation consumes
        // that receipt to remove the now-orphaned native destination safely.
        db.delete_skill("local:orphan")
            .expect("remove portable row");
        remove_path(&ssot.join("orphan")).expect("replace portable SSOT");
        PiSkillDeploymentService::reconcile_all(&db)
            .expect_err("the unrelated missing source remains visible");
        assert!(
            fs::symlink_metadata(pi_root.join("skills").join("orphan")).is_err(),
            "owned native deployment must follow a portable catalog deletion"
        );
        assert!(
            db.get_all_pi_skill_deployments()
                .expect("deployment ledger")
                .iter()
                .all(|deployment| deployment.skill_id != "local:orphan"),
            "the consumed orphan receipt must not remain as shadow state"
        );
    }
}

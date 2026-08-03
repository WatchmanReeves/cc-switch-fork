//! Reusable safety boundary for exact Pi-owned/shared files.
//!
//! Callers choose an exact path and size limit. This layer supplies bounded
//! regular-file reads, symlink rejection, optimistic revisions, per-path
//! process locking, and OS-backed compare/exchange replacement. The latter is
//! deliberately stronger than "read, compare, rename": the displaced path is
//! inspected after one atomic namespace operation, so an external Pi/user
//! rename in the commit window is restored instead of overwritten.

use crate::error::AppError;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Take, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

static FILE_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
type CompareExchangeHooks = HashMap<PathBuf, Vec<u8>>;
#[cfg(test)]
static BEFORE_COMPARE_EXCHANGE: LazyLock<Mutex<CompareExchangeHooks>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
#[cfg(test)]
static BEFORE_ROLLBACK_EXCHANGE: LazyLock<Mutex<CompareExchangeHooks>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
#[cfg(test)]
static FAIL_NEXT_ROLLBACK_RESTORE: LazyLock<Mutex<HashMap<PathBuf, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
#[cfg(test)]
static FAIL_AFTER_ATOMIC_ROLLBACK_SWAP: LazyLock<Mutex<HashMap<PathBuf, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
#[cfg(all(test, unix))]
static FAIL_NEXT_PARENT_SYNC: LazyLock<Mutex<HashMap<PathBuf, usize>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug)]
struct StagedReplacement {
    path: PathBuf,
    identity: FileIdentity,
}

/// Stable-enough namespace identity for conditional cleanup.
///
/// Installed-file classification pairs it with exact bytes; cleanup at a
/// private random path may use identity alone. Windows obtains the value from
/// an open handle instead of unstable `MetadataExt` APIs, keeping the pinned
/// Rust toolchain buildable without weakening identity to timestamps or
/// content alone.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume: u32,
    #[cfg(windows)]
    index: u64,
    #[cfg(not(any(unix, windows)))]
    len: u64,
    #[cfg(not(any(unix, windows)))]
    modified: Option<std::time::SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SharedFileSnapshot {
    pub revision: String,
    pub bytes: Option<Vec<u8>>,
}

impl SharedFileSnapshot {
    pub(crate) fn exists(&self) -> bool {
        self.bytes.is_some()
    }
}

pub(crate) fn read_shared_file(
    path: &Path,
    max_bytes: u64,
    label: &str,
) -> Result<SharedFileSnapshot, AppError> {
    let bytes = read_regular_bytes(path, max_bytes, label)?;
    Ok(SharedFileSnapshot {
        revision: revision(bytes.as_deref()),
        bytes,
    })
}

pub(crate) fn replace_shared_file(
    path: &Path,
    expected_revision: &str,
    bytes: &[u8],
    max_bytes: u64,
    new_file_mode: Option<u32>,
    label: &str,
) -> Result<SharedFileSnapshot, AppError> {
    if bytes.len() as u64 > max_bytes {
        return Err(AppError::InvalidInput(format!(
            "{label} exceeds the {max_bytes}-byte limit"
        )));
    }
    let lock = path_lock(path)?;
    let _guard = lock
        .lock()
        .map_err(|error| AppError::Config(format!("Pi file lock is poisoned: {error}")))?;
    let current = read_shared_file(path, max_bytes, label)?;
    ensure_revision(path, expected_revision, &current.revision)?;
    compare_exchange_under_lock(
        path,
        current.bytes.as_deref(),
        Some(bytes),
        max_bytes,
        new_file_mode,
        label,
    )
}

pub(crate) fn delete_shared_file(
    path: &Path,
    expected_revision: &str,
    max_bytes: u64,
    label: &str,
) -> Result<bool, AppError> {
    let lock = path_lock(path)?;
    let _guard = lock
        .lock()
        .map_err(|error| AppError::Config(format!("Pi file lock is poisoned: {error}")))?;
    let current = read_shared_file(path, max_bytes, label)?;
    ensure_revision(path, expected_revision, &current.revision)?;
    if !current.exists() {
        return Ok(false);
    }
    compare_exchange_under_lock(path, current.bytes.as_deref(), None, max_bytes, None, label)?;
    Ok(true)
}

/// Atomically replace the exact bytes a caller parsed.
///
/// This is the common commit primitive for shared Pi documents. Callers may
/// retry `Conflict` after reparsing, but other failures are fail-closed. The
/// target never gets replaced merely because a pre-rename fingerprint happened
/// to match.
pub(crate) fn compare_exchange_shared_file_bytes(
    path: &Path,
    expected: Option<&[u8]>,
    replacement: &[u8],
    max_bytes: u64,
    new_file_mode: Option<u32>,
    label: &str,
) -> Result<SharedFileSnapshot, AppError> {
    if replacement.len() as u64 > max_bytes {
        return Err(AppError::InvalidInput(format!(
            "{label} exceeds the {max_bytes}-byte limit"
        )));
    }
    let lock = path_lock(path)?;
    let _guard = lock
        .lock()
        .map_err(|error| AppError::Config(format!("Pi file lock is poisoned: {error}")))?;
    compare_exchange_under_lock(
        path,
        expected,
        Some(replacement),
        max_bytes,
        new_file_mode,
        label,
    )
}

/// Retry durability for a namespace state which was observed after a
/// compare/exchange returned an error. The caller is still responsible for
/// verifying the exact live value before treating the mutation as committed
/// or compensated.
pub(crate) fn sync_shared_file_parent(path: &Path) -> Result<(), AppError> {
    let parent = path.parent().ok_or_else(|| {
        AppError::InvalidInput(format!(
            "Pi shared-file path has no parent: {}",
            path.display()
        ))
    })?;
    sync_parent(parent)
}

fn compare_exchange_under_lock(
    path: &Path,
    expected: Option<&[u8]>,
    replacement: Option<&[u8]>,
    max_bytes: u64,
    new_file_mode: Option<u32>,
    label: &str,
) -> Result<SharedFileSnapshot, AppError> {
    if replacement.is_some_and(|bytes| bytes.len() as u64 > max_bytes) {
        return Err(AppError::InvalidInput(format!(
            "{label} exceeds the {max_bytes}-byte limit"
        )));
    }
    let parent = path.parent().ok_or_else(|| {
        AppError::InvalidInput(format!("{label} path has no parent: {}", path.display()))
    })?;
    fs::create_dir_all(parent).map_err(|error| AppError::io(parent, error))?;

    // This preflight rejects symlinks/non-regular files and avoids a namespace
    // operation when the conflict is already visible. Correctness still rests
    // on inspecting the displaced file after the atomic operation below.
    let before = read_regular_bytes(path, max_bytes, label)?;
    if before.as_deref() != expected {
        return Err(concurrent_change(path, label));
    }

    let staged = replacement
        .map(|bytes| stage_replacement(path, bytes, before.is_some(), new_file_mode))
        .transpose()?;
    run_before_compare_exchange_hook(path)?;

    let result = match (expected, replacement, staged.as_ref()) {
        (None, Some(bytes), Some(staged)) => match rename_noreplace(&staged.path, path) {
            Ok(()) => {
                match sync_parent(parent) {
                    Ok(()) => Ok(snapshot(Some(bytes))),
                    Err(publish_error) => {
                        match rollback_installed_file(
                            path,
                            &staged.identity,
                            bytes,
                            None,
                            parent,
                            max_bytes,
                            label,
                        ) {
                            Ok(InstalledRollback::Restored) => Err(publish_error),
                            Ok(InstalledRollback::Superseded) => Err(AppError::Config(format!(
                                "{label} create lost its durability barrier ({publish_error}); \
                             a concurrent external state won and was preserved"
                            ))),
                            Err(rollback_error) => {
                                if path_is_installed(
                                    path,
                                    &staged.identity,
                                    bytes,
                                    max_bytes,
                                    label,
                                ) && sync_parent(parent).is_ok()
                                {
                                    // A rollback can fail for reasons unrelated to
                                    // the now-visible canonical file. A successful
                                    // second durability barrier makes the only
                                    // honest outcome a committed success.
                                    Ok(snapshot(Some(bytes)))
                                } else {
                                    Err(ambiguous_publication(
                                        label,
                                        &publish_error,
                                        &rollback_error,
                                    ))
                                }
                            }
                        }
                    }
                }
            }
            Err(error) if is_destination_exists(&error) => Err(concurrent_change(path, label)),
            Err(error) => Err(rename_error("create", &staged.path, path, error)),
        },
        (Some(expected), Some(replacement), Some(staged)) => {
            replace_existing_if_equal(path, staged, expected, replacement, max_bytes, label)
        }
        (Some(expected), None, None) => delete_existing_if_equal(path, expected, max_bytes, label),
        (None, None, None) => Ok(snapshot(None)),
        _ => Err(AppError::Config(
            "invalid Pi shared-file compare/exchange state".to_string(),
        )),
    };

    if let Some(staged) = staged {
        // Never use byte equality as writer identity: an external writer may
        // independently publish the same bytes with different ownership.
        // Staging cleanup is safe only while the original staged inode/file-id
        // remains at this private path.
        let _ = remove_file_if_identity(&staged.path, &staged.identity);
    }
    result
}

fn replace_existing_if_equal(
    path: &Path,
    staged: &StagedReplacement,
    expected: &[u8],
    replacement: &[u8],
    max_bytes: u64,
    label: &str,
) -> Result<SharedFileSnapshot, AppError> {
    let parent = path
        .parent()
        .expect("a path accepted by compare/exchange has a parent");
    let displaced = match install_over_existing(&staged.path, path) {
        Ok(displaced) => displaced,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::AlreadyExists
            ) =>
        {
            return Err(concurrent_change(path, label));
        }
        Err(error) => return Err(rename_error("exchange", &staged.path, path, error)),
    };
    let displaced_bytes = read_regular_bytes(&displaced, max_bytes, label);
    if matches!(
        displaced_bytes,
        Ok(ref bytes) if bytes.as_deref() == Some(expected)
    ) {
        return match sync_parent(parent) {
            Ok(()) => {
                finalize_recovery_artifact(&displaced, parent, label);
                Ok(snapshot(Some(replacement)))
            }
            Err(publish_error) => match rollback_installed_file(
                path,
                &staged.identity,
                replacement,
                Some(&displaced),
                parent,
                max_bytes,
                label,
            ) {
                Ok(InstalledRollback::Restored) => Err(publish_error),
                Ok(InstalledRollback::Superseded) => Err(AppError::Config(format!(
                    "{label} replacement lost its durability barrier ({publish_error}); \
                     a concurrent external state won and was preserved"
                ))),
                Err(rollback_error) => {
                    if path_is_installed(path, &staged.identity, replacement, max_bytes, label)
                        && sync_parent(parent).is_ok()
                    {
                        finalize_recovery_artifact(&displaced, parent, label);
                        Ok(snapshot(Some(replacement)))
                    } else {
                        Err(ambiguous_publication(
                            label,
                            &publish_error,
                            &rollback_error,
                        ))
                    }
                }
            },
        };
    }

    match rollback_installed_file(
        path,
        &staged.identity,
        replacement,
        Some(&displaced),
        parent,
        max_bytes,
        label,
    ) {
        Ok(InstalledRollback::Restored) => match displaced_bytes {
            Ok(_) => Err(concurrent_change(path, label)),
            Err(error) => Err(AppError::Conflict(format!(
                "{label} became unsafe during atomic replacement and was restored: {error}"
            ))),
        },
        Ok(InstalledRollback::Superseded) => Err(AppError::Config(format!(
            "{label} changed again during rollback; all external bytes were preserved \
             and require explicit recovery/reconciliation"
        ))),
        Err(error) => Err(AppError::Config(format!(
            "{label} changed during atomic replacement and could not be restored safely; \
             the displaced bytes remain at {}: {error}",
            displaced.display()
        ))),
    }
}

fn delete_existing_if_equal(
    path: &Path,
    expected: &[u8],
    max_bytes: u64,
    label: &str,
) -> Result<SharedFileSnapshot, AppError> {
    let quarantine = sibling_temp_path(path, "delete");
    rename_noreplace(path, &quarantine)
        .map_err(|error| rename_error("quarantine", path, &quarantine, error))?;
    let quarantined = read_regular_bytes(&quarantine, max_bytes, label);
    if matches!(
        quarantined,
        Ok(ref bytes) if bytes.as_deref() == Some(expected)
    ) {
        let parent = path
            .parent()
            .expect("a path accepted by compare/exchange has a parent");
        return match sync_parent(parent) {
            Ok(()) => {
                finalize_recovery_artifact(&quarantine, parent, label);
                Ok(snapshot(None))
            }
            Err(publish_error) => {
                match restore_quarantined_file(&quarantine, path, parent, label) {
                    Ok(InstalledRollback::Restored) => Err(publish_error),
                    Ok(InstalledRollback::Superseded) => Err(AppError::Config(format!(
                        "{label} delete lost its durability barrier ({publish_error}); \
                     a concurrent external state won and was preserved"
                    ))),
                    Err(rollback_error) => {
                        if path_is_missing(path) && sync_parent(parent).is_ok() {
                            finalize_recovery_artifact(&quarantine, parent, label);
                            Ok(snapshot(None))
                        } else {
                            Err(ambiguous_publication(
                                label,
                                &publish_error,
                                &rollback_error,
                            ))
                        }
                    }
                }
            }
        };
    }

    let parent = path
        .parent()
        .expect("a path accepted by compare/exchange has a parent");
    match restore_quarantined_file(&quarantine, path, parent, label) {
        Ok(InstalledRollback::Restored) => match quarantined {
            Ok(_) => Err(concurrent_change(path, label)),
            Err(error) => Err(AppError::Conflict(format!(
                "{label} became unsafe during delete and was restored: {error}"
            ))),
        },
        Ok(InstalledRollback::Superseded) => Err(AppError::Config(format!(
            "{label} changed again while a delete was being restored; the newer \
             canonical state and displaced bytes at {} were both preserved",
            quarantine.display()
        ))),
        Err(error) => Err(AppError::Config(format!(
            "{label} changed during delete and the displaced bytes remain at {}: {error}",
            quarantine.display()
        ))),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InstalledRollback {
    Restored,
    Superseded,
}

/// Remove an installed replacement without ever identifying it by content alone.
///
/// The canonical path is first moved to a private recovery name and its
/// inode/file-id and exact bytes are compared with the staged witness. If an
/// external writer won between the failed durability barrier and this rollback,
/// that writer is restored (or retained at a recovery path) instead of being
/// overwritten.
fn rollback_installed_file(
    path: &Path,
    installed_identity: &FileIdentity,
    installed_bytes: &[u8],
    displaced: Option<&Path>,
    parent: &Path,
    max_bytes: u64,
    label: &str,
) -> Result<InstalledRollback, AppError> {
    if let Err(error) = run_before_rollback_restore_hook(path) {
        // The hook models a transient namespace rollback failure. Retrying the
        // actual no-replace operation is part of the production guarantee.
        log::warn!("retrying {label} rollback after transient failure: {error}");
    }
    if let Some(displaced) = displaced {
        return rollback_replacement_atomically(
            path,
            installed_identity,
            installed_bytes,
            displaced,
            parent,
            max_bytes,
            label,
        );
    }

    // A failed create has no before-image to exchange back into place. Isolate
    // the canonical entry and identify it by file-id plus exact bytes before
    // deleting it. If another writer won, restore that writer instead.
    let rejected = sibling_temp_path(path, "rejected");
    match rename_noreplace(path, &rejected) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            sync_parent(parent)?;
            return Ok(InstalledRollback::Superseded);
        }
        Err(error) => {
            return Err(rename_error(
                "isolate failed publication",
                path,
                &rejected,
                error,
            ));
        }
    }

    let rejected_identity =
        file_identity_from_path(&rejected).map_err(|error| AppError::io(&rejected, error))?;
    let rejected_is_installed = installed_identity == &rejected_identity
        && read_regular_bytes(&rejected, max_bytes, label)
            .ok()
            .flatten()
            .as_deref()
            == Some(installed_bytes);
    if !rejected_is_installed {
        let restored = match rename_noreplace(&rejected, path) {
            Ok(()) => InstalledRollback::Superseded,
            Err(error) if is_destination_exists(&error) => InstalledRollback::Superseded,
            Err(error) => {
                return Err(rename_error(
                    "restore external winner",
                    &rejected,
                    path,
                    error,
                ));
            }
        };
        sync_parent(parent)?;
        return Ok(restored);
    }

    sync_parent(parent)?;
    remove_file_if_identity(&rejected, installed_identity)?;
    sync_parent(parent)?;
    Ok(InstalledRollback::Restored)
}

/// Restore a replacement with one canonical-preserving namespace operation.
///
/// The displaced before-image becomes canonical atomically and the value which
/// occupied the canonical path moves to a private recovery path. If that value
/// is not our staged file, a second external writer won; atomically put it back
/// and retain the older external value as a recovery artifact. A crash at any
/// point leaves a real file at the canonical path rather than a gap between two
/// renames.
fn rollback_replacement_atomically(
    path: &Path,
    installed_identity: &FileIdentity,
    installed_bytes: &[u8],
    displaced: &Path,
    parent: &Path,
    max_bytes: u64,
    label: &str,
) -> Result<InstalledRollback, AppError> {
    if path_is_missing(path) {
        // An external delete superseded the failed publication. Do not
        // resurrect the older displaced value; retain it for reconciliation.
        sync_parent(parent)?;
        return Ok(InstalledRollback::Superseded);
    }
    if !path_is_installed(path, installed_identity, installed_bytes, max_bytes, label) {
        // A newer external value is already canonical. Leave it there; the
        // older displaced value is already a recovery artifact. The identity
        // check cannot be made atomic with an uncooperative writer, but it
        // removes the broad two-exchange window from the normal supersession
        // path.
        sync_parent(parent)?;
        return Ok(InstalledRollback::Superseded);
    }
    run_before_rollback_exchange_hook(path)?;
    // Namespace identity, rather than readability or content type, is the
    // rollback witness. A symlink/directory which appeared in the commit
    // window is unsafe to consume, but it still belongs back at the canonical
    // path instead of being overwritten by our staged regular file.
    let displaced_identity =
        file_identity_from_path(displaced).map_err(|error| AppError::io(displaced, error))?;

    let swapped_out = match restore_displaced_atomically(displaced, path) {
        Ok(swapped_out) => swapped_out,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && path_is_missing(path)
                && !path_is_missing(displaced) =>
        {
            sync_parent(parent)?;
            return Ok(InstalledRollback::Superseded);
        }
        Err(error) => {
            return Err(rename_error(
                "atomically restore displaced file",
                displaced,
                path,
                error,
            ));
        }
    };
    // Make the canonical-preserving recovery durable before inspecting or
    // cleaning either recovery name. A crash after this barrier may require
    // reconciliation, but it cannot replay a canonical-path gap.
    sync_parent(parent)?;

    #[cfg(test)]
    fail_after_atomic_rollback_swap_for_test(path)?;

    let swapped_is_installed = path_is_installed(
        &swapped_out,
        installed_identity,
        installed_bytes,
        max_bytes,
        label,
    );
    if swapped_is_installed {
        remove_file_if_identity(&swapped_out, installed_identity)?;
        sync_parent(parent)?;
        return Ok(if path_has_identity(path, &displaced_identity) {
            InstalledRollback::Restored
        } else {
            // The rollback itself succeeded, then an external writer changed
            // or deleted the canonical value. That newer state is authority.
            InstalledRollback::Superseded
        });
    }

    // The canonical path changed again after our original exchange. The first
    // atomic restore placed the older displaced value at the canonical path
    // and preserved the newer value at `swapped_out`. Put that newer value
    // back with another atomic operation; even a crash between the two swaps
    // leaves a valid external value at the canonical path.
    if !path_has_identity(path, &displaced_identity) {
        sync_parent(parent)?;
        return Ok(InstalledRollback::Superseded);
    }
    let older_recovery = match restore_displaced_atomically(&swapped_out, path) {
        Ok(older_recovery) => older_recovery,
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                && path_is_missing(path)
                && !path_is_missing(&swapped_out) =>
        {
            sync_parent(parent)?;
            return Ok(InstalledRollback::Superseded);
        }
        Err(error) => {
            let _ = sync_parent(parent);
            return Err(AppError::Config(format!(
                "{label} preserved a newer external value at {} but could not atomically \
                 restore it to {}: {error}",
                swapped_out.display(),
                path.display()
            )));
        }
    };
    sync_parent(parent)?;
    log::warn!(
        "{label} changed again during rollback; an external value remains canonical and another \
         external value is preserved at {}",
        older_recovery.display()
    );
    Ok(InstalledRollback::Superseded)
}

fn restore_quarantined_file(
    quarantine: &Path,
    path: &Path,
    parent: &Path,
    label: &str,
) -> Result<InstalledRollback, AppError> {
    if let Err(error) = run_before_rollback_restore_hook(path) {
        log::warn!("retrying {label} delete rollback after transient failure: {error}");
    }
    match rename_noreplace(quarantine, path) {
        Ok(()) => {
            sync_parent(parent)?;
            Ok(InstalledRollback::Restored)
        }
        Err(error) if is_destination_exists(&error) => {
            sync_parent(parent)?;
            Ok(InstalledRollback::Superseded)
        }
        Err(error) => Err(rename_error(
            "restore quarantined file",
            quarantine,
            path,
            error,
        )),
    }
}

fn path_is_installed(
    path: &Path,
    expected_identity: &FileIdentity,
    expected_bytes: &[u8],
    max_bytes: u64,
    label: &str,
) -> bool {
    file_identity_from_path(path)
        .ok()
        .is_some_and(|actual| &actual == expected_identity)
        && read_regular_bytes(path, max_bytes, label)
            .ok()
            .flatten()
            .as_deref()
            == Some(expected_bytes)
}

fn path_is_missing(path: &Path) -> bool {
    matches!(
        fs::symlink_metadata(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

fn path_has_identity(path: &Path, expected: &FileIdentity) -> bool {
    file_identity_from_path(path)
        .ok()
        .is_some_and(|actual| &actual == expected)
}

fn remove_file_if_identity(path: &Path, expected: &FileIdentity) -> Result<bool, AppError> {
    let actual = match fs::symlink_metadata(path) {
        Ok(actual) => actual,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(AppError::io(path, error)),
    };
    if !actual.file_type().is_file()
        || file_identity_from_path(path).map_err(|error| AppError::io(path, error))? != *expected
    {
        return Ok(false);
    }
    fs::remove_file(path).map_err(|error| AppError::io(path, error))?;
    Ok(true)
}

fn finalize_recovery_artifact(path: &Path, parent: &Path, label: &str) {
    match fs::remove_file(path) {
        Ok(()) => {
            if sync_parent(parent).is_err() {
                if let Err(error) = sync_parent(parent) {
                    // The canonical mutation already passed its own barrier.
                    // This cleanup barrier only governs whether a private
                    // recovery name can reappear after a crash, so it must not
                    // turn a committed operation into a false failure.
                    log::warn!(
                        "{label} committed, but recovery-artifact cleanup durability is uncertain at {}: {error}",
                        path.display()
                    );
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            log::warn!(
                "{label} committed, but its private recovery artifact remains at {}: {error}",
                path.display()
            );
        }
    }
}

fn ambiguous_publication(
    label: &str,
    publish_error: &AppError,
    rollback_error: &AppError,
) -> AppError {
    AppError::Config(format!(
        "{label} lost its durability barrier ({publish_error}) and neither conditional rollback \
         nor a second durability barrier established a safe outcome: {rollback_error}"
    ))
}

fn stage_replacement(
    path: &Path,
    bytes: &[u8],
    preserve_mode: bool,
    new_file_mode: Option<u32>,
) -> Result<StagedReplacement, AppError> {
    #[cfg(not(unix))]
    let _ = (preserve_mode, new_file_mode);
    let staged = sibling_temp_path(path, "cas");
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    let requested_mode = {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mode = if preserve_mode {
            Some(
                fs::metadata(path)
                    .map(|metadata| metadata.permissions().mode())
                    .unwrap_or_else(|_| new_file_mode.unwrap_or(0o666)),
            )
        } else {
            new_file_mode
        };
        options.mode(mode.unwrap_or(0o666));
        mode.map(|mode| mode & 0o7777)
    };
    let mut file = options
        .open(&staged)
        .map_err(|error| AppError::io(&staged, error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // open(2) always applies umask, including when OpenOptionsExt::mode is
        // used. Restore the exact requested bits before the durable commit.
        if let Some(requested_mode) = requested_mode {
            file.set_permissions(fs::Permissions::from_mode(requested_mode))
                .map_err(|error| AppError::io(&staged, error))?;
        }
    }
    file.write_all(bytes)
        .map_err(|error| AppError::io(&staged, error))?;
    file.flush()
        .and_then(|_| file.sync_all())
        .map_err(|error| AppError::io(&staged, error))?;
    let identity = file_identity_from_file(&file).map_err(|error| AppError::io(&staged, error))?;
    drop(file);
    Ok(StagedReplacement {
        path: staged,
        identity,
    })
}

fn sibling_temp_path(path: &Path, purpose: &str) -> PathBuf {
    let parent = path
        .parent()
        .expect("a path accepted by compare/exchange has a parent");
    let name = path
        .file_name()
        .expect("a path accepted by compare/exchange has a file name")
        .to_string_lossy();
    parent.join(format!(
        ".{name}.{purpose}.{}",
        uuid::Uuid::new_v4().simple()
    ))
}

fn snapshot(bytes: Option<&[u8]>) -> SharedFileSnapshot {
    SharedFileSnapshot {
        revision: revision(bytes),
        bytes: bytes.map(ToOwned::to_owned),
    }
}

fn concurrent_change(path: &Path, label: &str) -> AppError {
    AppError::Conflict(format!(
        "{label} changed since it was read: {}",
        path.display()
    ))
}

fn rename_error(
    operation: &str,
    source: &Path,
    destination: &Path,
    error: std::io::Error,
) -> AppError {
    AppError::IoContext {
        context: format!(
            "Pi shared-file {operation} failed: {} -> {}",
            source.display(),
            destination.display()
        ),
        source: error,
    }
}

#[cfg(unix)]
fn sync_parent(parent: &Path) -> Result<(), AppError> {
    #[cfg(test)]
    if fail_parent_sync_for_test(parent)? {
        return Err(AppError::io(
            parent,
            std::io::Error::other("injected Pi parent-directory sync failure"),
        ));
    }
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| AppError::io(parent, error))
}

#[cfg(not(unix))]
fn sync_parent(_parent: &Path) -> Result<(), AppError> {
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn c_path(path: &Path) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))
}

#[cfg(target_os = "linux")]
fn rename_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    let source = c_path(source)?;
    let destination = c_path(destination)?;
    // SAFETY: both C strings remain alive and renameat2 performs one
    // synchronous namespace operation.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn exchange_paths(left: &Path, right: &Path) -> std::io::Result<()> {
    let left = c_path(left)?;
    let right = c_path(right)?;
    // SAFETY: both C strings remain alive and renameat2 performs one
    // synchronous namespace operation.
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            left.as_ptr(),
            libc::AT_FDCWD,
            right.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn rename_with_flags(source: &Path, destination: &Path, flags: u32) -> std::io::Result<()> {
    let source = c_path(source)?;
    let destination = c_path(destination)?;
    // SAFETY: both C strings remain alive and renamex_np performs one
    // synchronous namespace operation.
    let result = unsafe { libc::renamex_np(source.as_ptr(), destination.as_ptr(), flags) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "macos")]
fn rename_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    rename_with_flags(source, destination, libc::RENAME_EXCL)
}

#[cfg(target_os = "macos")]
fn exchange_paths(left: &Path, right: &Path) -> std::io::Result<()> {
    rename_with_flags(left, right, libc::RENAME_SWAP)
}

#[cfg(windows)]
fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

#[cfg(windows)]
fn rename_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
    let source = wide_path(source);
    let destination = wide_path(destination);
    // SAFETY: both buffers are NUL-terminated and remain alive during the
    // synchronous Win32 call. No REPLACE_EXISTING flag is supplied.
    let result = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if result != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn replace_file_with_backup(path: &Path, replacement: &Path, backup: &Path) -> std::io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::{ReplaceFileW, REPLACEFILE_WRITE_THROUGH};
    let path_wide = wide_path(path);
    let replacement_wide = wide_path(replacement);
    let backup_wide = wide_path(backup);
    // SAFETY: all buffers are NUL-terminated and remain alive during the
    // synchronous Win32 call.
    let result = unsafe {
        ReplaceFileW(
            path_wide.as_ptr(),
            replacement_wide.as_ptr(),
            backup_wide.as_ptr(),
            REPLACEFILE_WRITE_THROUGH,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if result != 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(1177) {
            // ERROR_UNABLE_TO_MOVE_REPLACEMENT_2 is a documented partial
            // success: `path` moved to `backup`, `replacement` kept its name,
            // and the canonical name may be absent. Restore the backup without
            // overwriting a concurrent writer before surfacing the failure.
            return Err(recover_partial_replace_backup(path, backup, error));
        }
        Err(error)
    }
}

#[cfg(windows)]
fn recover_partial_replace_backup(
    path: &Path,
    backup: &Path,
    replace_error: std::io::Error,
) -> std::io::Error {
    match rename_noreplace(backup, path) {
        Ok(()) => std::io::Error::new(
            replace_error.kind(),
            format!("{replace_error}; restored the partial backup to the canonical path"),
        ),
        Err(recovery_error) if is_destination_exists(&recovery_error) => std::io::Error::new(
            replace_error.kind(),
            format!(
                "{replace_error}; a concurrent canonical value won and the partial backup remains at {}",
                backup.display()
            ),
        ),
        Err(recovery_error) => std::io::Error::new(
            replace_error.kind(),
            format!(
                "{replace_error}; the partial backup remains at {} and could not be restored: {recovery_error}",
                backup.display()
            ),
        ),
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn install_over_existing(staged: &Path, path: &Path) -> std::io::Result<PathBuf> {
    exchange_paths(staged, path)?;
    Ok(staged.to_path_buf())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn restore_displaced_atomically(displaced: &Path, path: &Path) -> std::io::Result<PathBuf> {
    exchange_paths(displaced, path)?;
    Ok(displaced.to_path_buf())
}

#[cfg(windows)]
fn install_over_existing(staged: &Path, path: &Path) -> std::io::Result<PathBuf> {
    let backup = sibling_temp_path(path, "displaced");
    replace_file_with_backup(path, staged, &backup)?;
    Ok(backup)
}

#[cfg(windows)]
fn restore_displaced_atomically(displaced: &Path, path: &Path) -> std::io::Result<PathBuf> {
    let backup = sibling_temp_path(path, "rejected");
    replace_file_with_backup(path, displaced, &backup)?;
    Ok(backup)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn rename_noreplace(_source: &Path, _destination: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic no-replace rename is unsupported on this platform",
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn install_over_existing(_staged: &Path, _path: &Path) -> std::io::Result<PathBuf> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic file exchange is unsupported on this platform",
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn restore_displaced_atomically(_displaced: &Path, _path: &Path) -> std::io::Result<PathBuf> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "atomic file recovery is unsupported on this platform",
    ))
}

fn is_destination_exists(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::AlreadyExists
        || matches!(error.raw_os_error(), Some(libc::EEXIST))
}

/// Publish a staged file or directory without replacing a path created by a
/// concurrent writer.
pub(crate) fn publish_path_noreplace(source: &Path, destination: &Path) -> std::io::Result<()> {
    rename_noreplace(source, destination)
}

#[cfg(test)]
pub(crate) fn replace_before_next_compare_exchange(path: &Path, bytes: &[u8]) {
    BEFORE_COMPARE_EXCHANGE
        .lock()
        .expect("compare/exchange test hook lock")
        .insert(path.to_path_buf(), bytes.to_vec());
}

#[cfg(all(test, unix))]
pub(crate) fn fail_next_parent_sync_for_test(path: &Path) {
    let parent = path
        .parent()
        .expect("a shared-file test path must have a parent")
        .to_path_buf();
    let mut failures = FAIL_NEXT_PARENT_SYNC
        .lock()
        .expect("parent sync test hook lock");
    failures
        .entry(parent)
        .and_modify(|remaining| *remaining = remaining.saturating_add(1))
        .or_insert(1);
}

#[cfg(all(test, unix))]
fn fail_parent_sync_for_test(parent: &Path) -> Result<bool, AppError> {
    let mut failures = FAIL_NEXT_PARENT_SYNC
        .lock()
        .map_err(|error| AppError::Config(format!("Pi sync test hook is poisoned: {error}")))?;
    let Some(remaining) = failures.get_mut(parent) else {
        return Ok(false);
    };
    *remaining = remaining.saturating_sub(1);
    if *remaining == 0 {
        failures.remove(parent);
    }
    Ok(true)
}

#[cfg(test)]
fn run_file_replacement_hook(
    hooks: &Mutex<CompareExchangeHooks>,
    path: &Path,
) -> Result<(), AppError> {
    let replacement = {
        let mut hook = hooks
            .lock()
            .map_err(|error| AppError::Config(format!("Pi CAS test hook is poisoned: {error}")))?;
        hook.remove(path)
    };
    if let Some(bytes) = replacement {
        crate::config::atomic_write_durable(path, &bytes, None)?;
    }
    Ok(())
}

#[cfg(test)]
fn run_before_compare_exchange_hook(path: &Path) -> Result<(), AppError> {
    run_file_replacement_hook(&BEFORE_COMPARE_EXCHANGE, path)
}

#[cfg(not(test))]
fn run_before_compare_exchange_hook(_path: &Path) -> Result<(), AppError> {
    Ok(())
}

#[cfg(test)]
fn replace_before_next_rollback_exchange(path: &Path, bytes: &[u8]) {
    BEFORE_ROLLBACK_EXCHANGE
        .lock()
        .expect("rollback exchange test hook lock")
        .insert(path.to_path_buf(), bytes.to_vec());
}

#[cfg(test)]
pub(crate) fn fail_next_rollback_restore_for_test(path: &Path) {
    let mut failures = FAIL_NEXT_ROLLBACK_RESTORE
        .lock()
        .expect("rollback restore test hook lock");
    failures
        .entry(path.to_path_buf())
        .and_modify(|remaining| *remaining = remaining.saturating_add(1))
        .or_insert(1);
}

#[cfg(test)]
fn fail_next_after_atomic_rollback_swap_for_test(path: &Path) {
    let mut failures = FAIL_AFTER_ATOMIC_ROLLBACK_SWAP
        .lock()
        .expect("atomic rollback-swap test hook lock");
    failures
        .entry(path.to_path_buf())
        .and_modify(|remaining| *remaining = remaining.saturating_add(1))
        .or_insert(1);
}

#[cfg(test)]
fn fail_after_atomic_rollback_swap_for_test(path: &Path) -> Result<(), AppError> {
    let mut failures = FAIL_AFTER_ATOMIC_ROLLBACK_SWAP.lock().map_err(|error| {
        AppError::Config(format!("Pi atomic rollback-swap hook is poisoned: {error}"))
    })?;
    let Some(remaining) = failures.get_mut(path) else {
        return Ok(());
    };
    *remaining = remaining.saturating_sub(1);
    if *remaining == 0 {
        failures.remove(path);
    }
    Err(AppError::Config(
        "injected stop after canonical-preserving rollback swap".to_string(),
    ))
}

#[cfg(test)]
fn run_before_rollback_restore_hook(path: &Path) -> std::io::Result<()> {
    let mut failures = FAIL_NEXT_ROLLBACK_RESTORE
        .lock()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let Some(remaining) = failures.get_mut(path) else {
        return Ok(());
    };
    *remaining = remaining.saturating_sub(1);
    if *remaining == 0 {
        failures.remove(path);
    }
    Err(std::io::Error::other(
        "injected Pi rollback-restore failure",
    ))
}

#[cfg(not(test))]
fn run_before_rollback_restore_hook(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
fn run_before_rollback_exchange_hook(path: &Path) -> Result<(), AppError> {
    run_file_replacement_hook(&BEFORE_ROLLBACK_EXCHANGE, path)
}

#[cfg(not(test))]
fn run_before_rollback_exchange_hook(_path: &Path) -> Result<(), AppError> {
    Ok(())
}

fn ensure_revision(path: &Path, expected: &str, actual: &str) -> Result<(), AppError> {
    if expected == actual {
        Ok(())
    } else {
        Err(AppError::Conflict(format!(
            "Pi file changed since it was read: {}",
            path.display()
        )))
    }
}

fn path_lock(path: &Path) -> Result<Arc<Mutex<()>>, AppError> {
    let mut locks = FILE_LOCKS
        .lock()
        .map_err(|error| AppError::Config(format!("Pi file-lock registry is poisoned: {error}")))?;
    Ok(locks
        .entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone())
}

fn revision(bytes: Option<&[u8]>) -> String {
    bytes.map_or_else(
        || "missing".to_string(),
        |bytes| format!("sha256:{:x}", Sha256::digest(bytes)),
    )
}

#[cfg(unix)]
fn open_read_only(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(windows)]
fn open_read_only(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
}

#[cfg(not(any(unix, windows)))]
fn open_read_only(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().read(true).open(path)
}

#[cfg(unix)]
fn file_identity_from_metadata(metadata: &Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    }
}

#[cfg(unix)]
fn file_identity_from_file(file: &File) -> std::io::Result<FileIdentity> {
    file.metadata()
        .map(|metadata| file_identity_from_metadata(&metadata))
}

#[cfg(unix)]
fn file_identity_from_path(path: &Path) -> std::io::Result<FileIdentity> {
    fs::symlink_metadata(path).map(|metadata| file_identity_from_metadata(&metadata))
}

#[cfg(windows)]
fn file_identity_from_file(file: &File) -> std::io::Result<FileIdentity> {
    use std::mem::MaybeUninit;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut information = MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::zeroed();
    // SAFETY: `file` owns a valid handle for the duration of the synchronous
    // call and `information` points to writable, correctly sized storage.
    let result =
        unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) };
    if result == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a successful GetFileInformationByHandle initializes the entire
    // BY_HANDLE_FILE_INFORMATION structure.
    let information = unsafe { information.assume_init() };
    Ok(FileIdentity {
        volume: information.dwVolumeSerialNumber,
        index: ((information.nFileIndexHigh as u64) << 32) | information.nFileIndexLow as u64,
    })
}

#[cfg(windows)]
fn file_identity_from_path(path: &Path) -> std::io::Result<FileIdentity> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    };

    let file = OpenOptions::new()
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    file_identity_from_file(&file)
}

#[cfg(not(any(unix, windows)))]
fn file_identity_from_metadata(metadata: &Metadata) -> FileIdentity {
    FileIdentity {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    }
}

#[cfg(not(any(unix, windows)))]
fn file_identity_from_file(file: &File) -> std::io::Result<FileIdentity> {
    file.metadata()
        .map(|metadata| file_identity_from_metadata(&metadata))
}

#[cfg(not(any(unix, windows)))]
fn file_identity_from_path(path: &Path) -> std::io::Result<FileIdentity> {
    fs::symlink_metadata(path).map(|metadata| file_identity_from_metadata(&metadata))
}

#[cfg(windows)]
fn is_reparse_point(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &Metadata) -> bool {
    false
}

fn read_limited(
    mut reader: Take<&mut File>,
    path: &Path,
    max_bytes: u64,
) -> Result<Vec<u8>, AppError> {
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|error| AppError::io(path, error))?;
    if bytes.len() as u64 > max_bytes {
        return Err(AppError::InvalidInput(format!(
            "Pi file exceeds the {max_bytes}-byte limit: {}",
            path.display()
        )));
    }
    Ok(bytes)
}

fn read_regular_bytes(
    path: &Path,
    max_bytes: u64,
    label: &str,
) -> Result<Option<Vec<u8>>, AppError> {
    let initial = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AppError::io(path, error)),
    };
    if !initial.file_type().is_file() || is_reparse_point(&initial) || initial.len() > max_bytes {
        return Err(AppError::InvalidInput(format!(
            "{label} must be a bounded regular file: {}",
            path.display()
        )));
    }
    let mut file = open_read_only(path).map_err(|error| AppError::io(path, error))?;
    let opened = file.metadata().map_err(|error| AppError::io(path, error))?;
    let opened_identity =
        file_identity_from_file(&file).map_err(|error| AppError::io(path, error))?;
    let bytes = read_limited(Read::by_ref(&mut file).take(max_bytes + 1), path, max_bytes)?;
    let completed = file.metadata().map_err(|error| AppError::io(path, error))?;
    let completed_identity =
        file_identity_from_file(&file).map_err(|error| AppError::io(path, error))?;
    let current = fs::symlink_metadata(path).map_err(|error| AppError::io(path, error))?;
    if !current.file_type().is_file() || is_reparse_point(&current) {
        return Err(AppError::Conflict(format!(
            "{label} changed during read: {}",
            path.display()
        )));
    }
    let current_identity =
        file_identity_from_path(path).map_err(|error| AppError::io(path, error))?;
    if !opened.file_type().is_file()
        || is_reparse_point(&opened)
        || !completed.file_type().is_file()
        || is_reparse_point(&completed)
        || opened_identity != completed_identity
        || completed_identity != current_identity
        || opened.len() != bytes.len() as u64
        || completed.len() != bytes.len() as u64
        || current.len() != bytes.len() as u64
        || opened.modified().ok() != completed.modified().ok()
    {
        return Err(AppError::Conflict(format!(
            "{label} changed during read: {}",
            path.display()
        )));
    }
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compare_and_replace_distinguishes_missing_and_content_revisions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("shared.md");
        let missing = read_shared_file(&path, 1024, "test").expect("missing");
        assert_eq!(missing.revision, "missing");
        let written = replace_shared_file(&path, "missing", b"one", 1024, Some(0o600), "test")
            .expect("create");
        assert!(written.revision.starts_with("sha256:"));
        assert!(replace_shared_file(&path, "missing", b"two", 1024, None, "test").is_err());
        let replaced = replace_shared_file(&path, &written.revision, b"two", 1024, None, "test")
            .expect("replace");
        assert_eq!(replaced.bytes.as_deref(), Some(b"two".as_slice()));
        assert!(delete_shared_file(&path, &written.revision, 1024, "test").is_err());
        assert!(delete_shared_file(&path, &replaced.revision, 1024, "test").expect("delete"));
    }

    #[test]
    fn external_rename_in_the_commit_window_is_restored_without_data_loss() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("shared.md");
        fs::write(&path, b"observed").expect("seed");
        let observed = read_shared_file(&path, 1024, "test").expect("snapshot");

        replace_before_next_compare_exchange(&path, b"external replacement");
        let replace = replace_shared_file(&path, &observed.revision, b"ours", 1024, None, "test");
        assert!(matches!(replace, Err(AppError::Conflict(_))));
        assert_eq!(
            fs::read(&path).expect("external bytes restored"),
            b"external replacement"
        );

        let observed = read_shared_file(&path, 1024, "test").expect("snapshot");
        replace_before_next_compare_exchange(&path, b"external before delete");
        let delete = delete_shared_file(&path, &observed.revision, 1024, "test");
        assert!(matches!(delete, Err(AppError::Conflict(_))));
        assert_eq!(
            fs::read(&path).expect("external bytes restored"),
            b"external before delete"
        );
    }

    #[test]
    fn second_external_rename_surfaces_the_recovery_artifact() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("shared.md");
        fs::write(&path, b"observed").expect("seed");
        let observed = read_shared_file(&path, 1024, "test").expect("snapshot");

        replace_before_next_compare_exchange(&path, b"external-a");
        replace_before_next_rollback_exchange(&path, b"external-b");
        let error = replace_shared_file(&path, &observed.revision, b"ours", 1024, None, "test")
            .expect_err("the second race needs explicit recovery");
        let message = error.to_string();
        assert!(
            matches!(error, AppError::Config(_)),
            "recovery conflicts must not be auto-retried: {message}"
        );
        assert!(message.contains("explicit recovery"));
        assert_eq!(
            fs::read(&path).expect("newest external version restored"),
            b"external-b"
        );
        assert!(
            fs::read_dir(temp.path())
                .expect("recovery directory")
                .filter_map(Result::ok)
                .any(|entry| fs::read(entry.path()).ok().as_deref() == Some(b"external-a")),
            "the displaced external bytes must remain in a named recovery artifact"
        );
    }

    #[test]
    fn atomic_rollback_stop_keeps_a_canonical_external_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("shared.md");
        fs::write(&path, b"observed").expect("seed");
        let observed = read_shared_file(&path, 1024, "test").expect("snapshot");

        replace_before_next_compare_exchange(&path, b"external-a");
        replace_before_next_rollback_exchange(&path, b"external-b");
        fail_next_after_atomic_rollback_swap_for_test(&path);
        replace_shared_file(&path, &observed.revision, b"ours", 1024, None, "test")
            .expect_err("the injected stop interrupts rollback cleanup");

        assert_eq!(
            fs::read(&path).expect("canonical path remains present"),
            b"external-a",
            "the first atomic recovery step must never create a canonical-path gap"
        );
        assert!(
            fs::read_dir(temp.path())
                .expect("recovery directory")
                .filter_map(Result::ok)
                .any(|entry| fs::read(entry.path()).ok().as_deref() == Some(b"external-b")),
            "the newer external value must remain recoverable if cleanup never runs"
        );
    }

    #[test]
    fn external_delete_wins_over_replacement_rollback() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("shared.md");
        let staged = temp.path().join("staged");
        let displaced = temp.path().join("displaced");
        fs::write(&staged, b"ours").expect("staged witness");
        fs::write(&displaced, b"external-before").expect("displaced value");
        let identity = file_identity_from_path(&staged).expect("staged identity");

        let outcome = rollback_replacement_atomically(
            &path,
            &identity,
            b"ours",
            &displaced,
            temp.path(),
            1024,
            "test",
        )
        .expect("external deletion is a safe superseding state");

        assert_eq!(outcome, InstalledRollback::Superseded);
        assert!(
            !path.exists(),
            "rollback must not resurrect the deleted path"
        );
        assert_eq!(
            fs::read(&displaced).expect("older value retained"),
            b"external-before"
        );
    }

    #[test]
    fn visible_external_winner_is_not_exchanged_during_rollback() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("shared.md");
        let installed = temp.path().join("installed-witness");
        let displaced = temp.path().join("displaced");
        fs::write(&path, b"external-newer").expect("canonical external winner");
        fs::write(&installed, b"ours").expect("installed witness");
        fs::write(&displaced, b"external-older").expect("older displaced value");
        let identity = file_identity_from_path(&installed).expect("installed identity");

        let outcome = rollback_replacement_atomically(
            &path,
            &identity,
            b"ours",
            &displaced,
            temp.path(),
            1024,
            "test",
        )
        .expect("visible external authority is a safe superseding state");

        assert_eq!(outcome, InstalledRollback::Superseded);
        assert_eq!(
            fs::read(&path).expect("newer value stays canonical"),
            b"external-newer"
        );
        assert_eq!(
            fs::read(&displaced).expect("older value stays recoverable"),
            b"external-older"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unsafe_displaced_entry_is_restored_by_identity_without_being_followed() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("shared.md");
        let displaced = temp.path().join("displaced");
        let target = temp.path().join("target");
        fs::write(&path, b"ours").expect("installed value");
        fs::write(&target, b"external-target").expect("external target");
        symlink(&target, &displaced).expect("unsafe displaced entry");
        let identity = file_identity_from_path(&path).expect("installed identity");

        let outcome = rollback_replacement_atomically(
            &path,
            &identity,
            b"ours",
            &displaced,
            temp.path(),
            1024,
            "test",
        )
        .expect("namespace identity is sufficient to restore an unsafe entry");

        assert_eq!(outcome, InstalledRollback::Restored);
        assert!(
            fs::symlink_metadata(&path)
                .expect("canonical entry")
                .file_type()
                .is_symlink(),
            "the external namespace entry must be restored instead of consumed"
        );
        assert_eq!(fs::read_link(&path).expect("symlink target"), target);
        assert_eq!(
            fs::read(&target).expect("target remains untouched"),
            b"external-target"
        );
        assert!(!displaced.exists(), "our rejected file must be removed");
    }

    #[cfg(windows)]
    #[test]
    fn windows_partial_replace_restores_backup_to_missing_canonical() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("shared.md");
        let backup = temp.path().join("backup");
        fs::write(&backup, b"external-before").expect("partial backup");

        let error =
            recover_partial_replace_backup(&path, &backup, std::io::Error::from_raw_os_error(1177));

        assert!(error.to_string().contains("restored"));
        assert_eq!(
            fs::read(&path).expect("canonical restored"),
            b"external-before"
        );
        assert!(!backup.exists());
    }

    #[cfg(unix)]
    #[test]
    fn create_sync_failure_returns_error_only_after_removing_its_file_identity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("shared.md");
        fail_next_parent_sync_for_test(&path);

        let error = replace_shared_file(&path, "missing", b"created", 1024, None, "test")
            .expect_err("the injected durability failure remains visible");
        assert!(error.to_string().contains("injected"));
        assert!(
            !path.exists(),
            "Err must not leave the attempted create live"
        );
    }

    #[cfg(unix)]
    #[test]
    fn replace_sync_failure_returns_error_only_after_restoring_before_image() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("shared.md");
        fs::write(&path, b"before").expect("seed");
        let before = read_shared_file(&path, 1024, "test").expect("snapshot");
        fail_next_parent_sync_for_test(&path);

        let error = replace_shared_file(&path, &before.revision, b"after", 1024, None, "test")
            .expect_err("the injected durability failure remains visible");
        assert!(error.to_string().contains("injected"));
        assert_eq!(fs::read(&path).expect("before restored"), b"before");
    }

    #[cfg(unix)]
    #[test]
    fn delete_sync_failure_returns_error_only_after_restoring_before_image() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("shared.md");
        fs::write(&path, b"before").expect("seed");
        let before = read_shared_file(&path, 1024, "test").expect("snapshot");
        fail_next_parent_sync_for_test(&path);

        let error = delete_shared_file(&path, &before.revision, 1024, "test")
            .expect_err("the injected durability failure remains visible");
        assert!(error.to_string().contains("injected"));
        assert_eq!(fs::read(&path).expect("before restored"), b"before");
    }

    #[cfg(unix)]
    #[test]
    fn replacement_preserves_exact_existing_permissions_despite_umask() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("shared.md");
        fs::write(&path, b"before").expect("seed");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o764)).expect("set mode");
        let before = read_shared_file(&path, 1024, "test").expect("snapshot");

        replace_shared_file(&path, &before.revision, b"after", 1024, None, "test")
            .expect("replace");
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o7777,
            0o764
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_targets_fail_closed() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("target");
        let path = temp.path().join("shared");
        fs::write(&target, b"secret").expect("target");
        symlink(&target, &path).expect("symlink");
        assert!(read_shared_file(&path, 1024, "test").is_err());
        assert!(replace_shared_file(&path, "missing", b"overwrite", 1024, None, "test").is_err());
        assert_eq!(fs::read(&target).expect("target remains"), b"secret");
    }
}

use indexmap::IndexMap;

use crate::app_config::AppType;
use crate::config::write_text_file;
use crate::database::Database;
use crate::error::AppError;
use crate::prompt::Prompt;
use crate::prompt_files::prompt_file_path;
use crate::services::pi_prompt_files::{
    lock_instruction_files, PiInstructionFileGuard, PiPromptFileKind, PiPromptFileService,
    PiPromptFileSnapshot,
};
use crate::store::AppState;
use serde::Serialize;
use sha2::{Digest, Sha256};

/// 安全地获取当前 Unix 时间戳
fn get_unix_timestamp() -> Result<i64, AppError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .map_err(|e| AppError::Message(format!("Failed to get system time: {e}")))
}

pub struct PromptService;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PiPromptLibraryStatus {
    pub native_exists: bool,
    pub native_revision: String,
    pub matched_prompt_id: Option<String>,
    pub needs_reconciliation: bool,
}

impl PromptService {
    pub fn get_prompts(
        state: &AppState,
        app: AppType,
    ) -> Result<IndexMap<String, Prompt>, AppError> {
        if matches!(app, AppType::Pi) {
            let guard = lock_instruction_files()?;
            return Self::inspect_pi_library_under_guard(state.db.as_ref(), &guard)
                .map(|(prompts, _)| prompts);
        }
        state.db.get_prompts(app.as_str())
    }

    /// Inspect Pi's live AGENTS.md without adopting it into the portable
    /// library. File presence and exact bytes are the effective active truth;
    /// persisted `enabled` flags are only a projection that explicit
    /// reconciliation may repair.
    pub fn get_pi_library_status(state: &AppState) -> Result<PiPromptLibraryStatus, AppError> {
        let guard = lock_instruction_files()?;
        Self::inspect_pi_library_under_guard(state.db.as_ref(), &guard).map(|(_, status)| status)
    }

    pub fn reconcile_pi_library(state: &AppState) -> Result<(), AppError> {
        Self::reconcile_pi_portable_import(state)
    }

    pub fn upsert_prompt(
        state: &AppState,
        app: AppType,
        _id: &str,
        prompt: Prompt,
    ) -> Result<(), AppError> {
        if matches!(app, AppType::Pi) {
            return Self::upsert_pi_prompt(state, prompt);
        }
        // 检查是否为已启用的提示词
        let is_enabled = prompt.enabled;

        state.db.save_prompt(app.as_str(), &prompt)?;

        if is_enabled {
            // 启用提示词：写入内容到文件
            let target_path = prompt_file_path(&app)?;
            write_text_file(&target_path, &prompt.content)?;
        } else {
            // 禁用提示词：检查是否还有其他已启用的提示词
            let prompts = state.db.get_prompts(app.as_str())?;
            let any_enabled = prompts.values().any(|p| p.enabled);

            if !any_enabled {
                // 所有提示词都已禁用，清空文件
                let target_path = prompt_file_path(&app)?;
                if target_path.exists() {
                    write_text_file(&target_path, "")?;
                }
            }
        }

        Ok(())
    }

    pub fn delete_prompt(state: &AppState, app: AppType, id: &str) -> Result<(), AppError> {
        if matches!(app, AppType::Pi) {
            let _switch_guard = futures::executor::block_on(
                state
                    .proxy_service
                    .lock_switch_for_app(AppType::Pi.as_str()),
            );
            let guard = lock_instruction_files()?;
            let prompts = state.db.get_prompts(AppType::Pi.as_str())?;
            let snapshot =
                PiPromptFileService::read_under_guard(&guard, PiPromptFileKind::GlobalContext)?;
            reject_pi_prompt_mutation_during_drift(&prompts, &snapshot, id, None)?;
            if prompts.get(id).is_some_and(|prompt| prompt.enabled)
                || preferred_pi_prompt_match(&prompts, &snapshot) == Some(id)
            {
                return Err(AppError::InvalidInput(
                    "无法删除 Pi 当前生效的提示词".to_string(),
                ));
            }
            let mut after = prompts.clone();
            after.shift_remove(id);
            return state.db.compare_exchange_prompt_selection(
                AppType::Pi.as_str(),
                &prompts,
                &after,
            );
        }
        let prompts = state.db.get_prompts(app.as_str())?;

        if let Some(prompt) = prompts.get(id) {
            if prompt.enabled {
                return Err(AppError::InvalidInput("无法删除已启用的提示词".to_string()));
            }
        }

        state.db.delete_prompt(app.as_str(), id)?;
        Ok(())
    }

    pub fn enable_prompt(state: &AppState, app: AppType, id: &str) -> Result<(), AppError> {
        if matches!(app, AppType::Pi) {
            return Self::enable_pi_prompt(state, id);
        }
        // 回填当前 live 文件内容到已启用的提示词，或创建备份
        let target_path = prompt_file_path(&app)?;
        if target_path.exists() {
            if let Ok(live_content) = std::fs::read_to_string(&target_path) {
                if !live_content.trim().is_empty() {
                    let mut prompts = state.db.get_prompts(app.as_str())?;

                    // 尝试回填到当前已启用的提示词
                    if let Some((enabled_id, enabled_prompt)) = prompts
                        .iter_mut()
                        .find(|(_, p)| p.enabled)
                        .map(|(id, p)| (id.clone(), p))
                    {
                        let timestamp = get_unix_timestamp()?;
                        enabled_prompt.content = live_content.clone();
                        enabled_prompt.updated_at = Some(timestamp);
                        log::info!("回填 live 提示词内容到已启用项: {enabled_id}");
                        state.db.save_prompt(app.as_str(), enabled_prompt)?;
                    } else {
                        // 没有已启用的提示词，则创建一次备份（避免重复备份）
                        let content_exists = prompts
                            .values()
                            .any(|p| p.content.trim() == live_content.trim());
                        if !content_exists {
                            let timestamp = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs() as i64;
                            let backup_id = format!("backup-{timestamp}");
                            let backup_prompt = Prompt {
                                id: backup_id.clone(),
                                name: format!(
                                    "原始提示词 {}",
                                    chrono::Local::now().format("%Y-%m-%d %H:%M")
                                ),
                                content: live_content,
                                description: Some("自动备份的原始提示词".to_string()),
                                enabled: false,
                                created_at: Some(timestamp),
                                updated_at: Some(timestamp),
                            };
                            log::info!("回填 live 提示词内容，创建备份: {backup_id}");
                            state.db.save_prompt(app.as_str(), &backup_prompt)?;
                        }
                    }
                }
            }
        }

        // 启用目标提示词并写入文件
        let mut prompts = state.db.get_prompts(app.as_str())?;

        for prompt in prompts.values_mut() {
            prompt.enabled = false;
        }

        if let Some(prompt) = prompts.get_mut(id) {
            prompt.enabled = true;
            write_text_file(&target_path, &prompt.content)?; // 原子写入
            state.db.save_prompt(app.as_str(), prompt)?;
        } else {
            return Err(AppError::InvalidInput(format!("提示词 {id} 不存在")));
        }

        // Save all prompts to disable others
        for (_, prompt) in prompts.iter() {
            state.db.save_prompt(app.as_str(), prompt)?;
        }

        Ok(())
    }

    pub fn import_from_file(state: &AppState, app: AppType) -> Result<String, AppError> {
        if matches!(app, AppType::Pi) {
            let _switch_guard = futures::executor::block_on(
                state
                    .proxy_service
                    .lock_switch_for_app(AppType::Pi.as_str()),
            );
            let guard = lock_instruction_files()?;
            let snapshot =
                PiPromptFileService::read_under_guard(&guard, PiPromptFileKind::GlobalContext)?;
            if !snapshot.exists {
                return Err(AppError::Message("Pi AGENTS.md does not exist".to_string()));
            }
            let before = state.db.get_prompts(AppType::Pi.as_str())?;
            let timestamp = get_unix_timestamp()?;
            let id = format!("imported-{timestamp}");
            let prompt = Prompt {
                id: id.clone(),
                name: format!(
                    "导入的提示词 {}",
                    chrono::Local::now().format("%Y-%m-%d %H:%M")
                ),
                content: snapshot.content.clone(),
                description: Some("从 Pi AGENTS.md 导入".to_string()),
                enabled: true,
                created_at: Some(timestamp),
                updated_at: Some(timestamp),
            };
            Self::import_pi_snapshot(state, &guard, &snapshot, &before, prompt)?;
            return Ok(id);
        }
        let file_path = prompt_file_path(&app)?;

        if !file_path.exists() {
            return Err(AppError::Message("提示词文件不存在".to_string()));
        }

        let content =
            std::fs::read_to_string(&file_path).map_err(|e| AppError::io(&file_path, e))?;
        let timestamp = get_unix_timestamp()?;

        let id = format!("imported-{timestamp}");
        let prompt = Prompt {
            id: id.clone(),
            name: format!(
                "导入的提示词 {}",
                chrono::Local::now().format("%Y-%m-%d %H:%M")
            ),
            content,
            description: Some("从现有配置文件导入".to_string()),
            enabled: false,
            created_at: Some(timestamp),
            updated_at: Some(timestamp),
        };

        Self::upsert_prompt(state, app, &id, prompt)?;
        Ok(id)
    }

    pub fn get_current_file_content(app: AppType) -> Result<Option<String>, AppError> {
        if matches!(app, AppType::Pi) {
            let guard = lock_instruction_files()?;
            let snapshot =
                PiPromptFileService::read_under_guard(&guard, PiPromptFileKind::GlobalContext)?;
            return Ok(snapshot.exists.then_some(snapshot.content));
        }
        let file_path = prompt_file_path(&app)?;
        if !file_path.exists() {
            return Ok(None);
        }
        let content =
            std::fs::read_to_string(&file_path).map_err(|e| AppError::io(&file_path, e))?;
        Ok(Some(content))
    }

    /// 首次启动时从现有提示词文件自动导入（如果存在）
    /// 返回导入的数量
    pub fn import_from_file_on_first_launch(
        state: &AppState,
        app: AppType,
    ) -> Result<usize, AppError> {
        if matches!(app, AppType::Pi) {
            let _switch_guard = futures::executor::block_on(
                state
                    .proxy_service
                    .lock_switch_for_app(AppType::Pi.as_str()),
            );
            let guard = lock_instruction_files()?;
            let existing = state.db.get_prompts(app.as_str())?;
            if !existing.is_empty() {
                return Ok(0);
            }
            let snapshot =
                PiPromptFileService::read_under_guard(&guard, PiPromptFileKind::GlobalContext)?;
            if !snapshot.exists {
                return Ok(0);
            }
            let timestamp = get_unix_timestamp()?;
            let id = format!("auto-imported-{timestamp}");
            let prompt = Prompt {
                id: id.clone(),
                name: format!(
                    "Auto-imported Prompt {}",
                    chrono::Local::now().format("%Y-%m-%d %H:%M")
                ),
                content: snapshot.content.clone(),
                description: Some("Automatically imported on first launch".to_string()),
                enabled: true,
                created_at: Some(timestamp),
                updated_at: Some(timestamp),
            };
            Self::import_pi_snapshot(state, &guard, &snapshot, &existing, prompt)?;
            return Ok(1);
        }

        // 幂等性保护：该应用已有提示词则跳过
        let existing = state.db.get_prompts(app.as_str())?;
        if !existing.is_empty() {
            return Ok(0);
        }

        let file_path = prompt_file_path(&app)?;

        // 检查文件是否存在
        if !file_path.exists() {
            return Ok(0);
        }

        // 读取文件内容
        let content = match std::fs::read_to_string(&file_path) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("读取提示词文件失败: {file_path:?}, 错误: {e}");
                return Ok(0);
            }
        };

        // 检查内容是否为空
        if content.trim().is_empty() {
            return Ok(0);
        }

        log::info!("发现提示词文件，自动导入: {file_path:?}");

        // 创建提示词对象
        let timestamp = get_unix_timestamp()?;
        let id = format!("auto-imported-{timestamp}");
        let prompt = Prompt {
            id: id.clone(),
            name: format!(
                "Auto-imported Prompt {}",
                chrono::Local::now().format("%Y-%m-%d %H:%M")
            ),
            content,
            description: Some("Automatically imported on first launch".to_string()),
            enabled: true, // 首次导入时自动启用
            created_at: Some(timestamp),
            updated_at: Some(timestamp),
        };

        // 保存到数据库
        state.db.save_prompt(app.as_str(), &prompt)?;

        log::info!("自动导入完成: {}", app.as_str());
        Ok(1)
    }

    /// Reconcile portable Pi prompt rows to this device's native AGENTS.md.
    ///
    /// Prompt content is portable, but native instruction files are not. The
    /// live file therefore decides which library row is active after an
    /// import: exact content adopts an existing row, otherwise a local
    /// counterpart is added. A missing file disables every row. The file is
    /// never created, replaced, or deleted by portable reconciliation.
    pub(crate) fn reconcile_pi_portable_import(state: &AppState) -> Result<(), AppError> {
        let _switch_guard = futures::executor::block_on(
            state
                .proxy_service
                .lock_switch_for_app(AppType::Pi.as_str()),
        );
        let guard = lock_instruction_files()?;
        Self::reconcile_pi_native_under_guard(state.db.as_ref(), &guard)
    }

    pub(crate) fn reconcile_pi_native_under_guard(
        db: &Database,
        guard: &PiInstructionFileGuard,
    ) -> Result<(), AppError> {
        const MAX_EXTERNAL_RETRIES: usize = 3;
        let original = db.get_prompts(AppType::Pi.as_str())?;

        for _ in 0..MAX_EXTERNAL_RETRIES {
            let snapshot =
                PiPromptFileService::read_under_guard(guard, PiPromptFileKind::GlobalContext)?;
            let prompts = build_pi_reconciled_library(&original, &snapshot);
            let precommit =
                PiPromptFileService::read_under_guard(guard, PiPromptFileKind::GlobalContext)?;
            if precommit.revision != snapshot.revision {
                continue;
            }

            db.compare_exchange_prompt_selection(AppType::Pi.as_str(), &original, &prompts)?;
            #[cfg(test)]
            apply_pi_native_binding_after_save_hooks_for_test(db, &snapshot.path)?;
            let verified =
                match PiPromptFileService::read_under_guard(guard, PiPromptFileKind::GlobalContext)
                {
                    Ok(verified) => verified,
                    Err(error) => {
                        restore_pi_library_after_failed_native_binding(
                            db, &original, &prompts, &error,
                        )?;
                        return Err(error);
                    }
                };
            if verified.revision == snapshot.revision {
                return Ok(());
            }
            restore_pi_library_after_failed_native_binding(
                db,
                &original,
                &prompts,
                &AppError::Conflict(
                    "Pi AGENTS.md changed after prompt-library publication".to_string(),
                ),
            )?;
        }

        Err(AppError::Conflict(
            "Pi AGENTS.md kept changing during portable prompt reconciliation".to_string(),
        ))
    }

    fn inspect_pi_library_under_guard(
        db: &Database,
        guard: &PiInstructionFileGuard,
    ) -> Result<(IndexMap<String, Prompt>, PiPromptLibraryStatus), AppError> {
        let snapshot =
            PiPromptFileService::read_under_guard(guard, PiPromptFileKind::GlobalContext)?;
        let mut prompts = db.get_prompts(AppType::Pi.as_str())?;
        let persisted_enabled = prompts
            .iter()
            .filter_map(|(id, prompt)| prompt.enabled.then_some(id.clone()))
            .collect::<Vec<_>>();
        let matched_prompt_id =
            preferred_pi_prompt_match(&prompts, &snapshot).map(ToOwned::to_owned);
        let expected_enabled = matched_prompt_id.iter().cloned().collect::<Vec<_>>();
        let needs_reconciliation = persisted_enabled != expected_enabled
            || (snapshot.exists && matched_prompt_id.is_none());

        for (id, prompt) in &mut prompts {
            prompt.enabled = matched_prompt_id.as_deref() == Some(id.as_str());
        }

        Ok((
            prompts,
            PiPromptLibraryStatus {
                native_exists: snapshot.exists,
                native_revision: snapshot.revision,
                matched_prompt_id,
                needs_reconciliation,
            },
        ))
    }

    fn upsert_pi_prompt(state: &AppState, prompt: Prompt) -> Result<(), AppError> {
        let _switch_guard = futures::executor::block_on(
            state
                .proxy_service
                .lock_switch_for_app(AppType::Pi.as_str()),
        );
        let guard = lock_instruction_files()?;
        let before = state.db.get_prompts(AppType::Pi.as_str())?;
        let snapshot =
            PiPromptFileService::read_under_guard(&guard, PiPromptFileKind::GlobalContext)?;
        reject_pi_prompt_mutation_during_drift(&before, &snapshot, &prompt.id, Some(&prompt))?;
        let mut prompts = before.clone();
        let previous = prompts.insert(prompt.id.clone(), prompt.clone());
        let current_enabled = previous
            .as_ref()
            .filter(|candidate| candidate.enabled)
            .or_else(|| {
                before
                    .values()
                    .find(|candidate| candidate.id != prompt.id && candidate.enabled)
            });

        if prompt.enabled {
            ensure_pi_library_projection_matches(&snapshot, current_enabled)?;
            for candidate in prompts.values_mut() {
                candidate.enabled = candidate.id == prompt.id;
            }
            let published = PiPromptFileService::replace_under_guard(
                &guard,
                PiPromptFileKind::GlobalContext,
                &snapshot.revision,
                &prompt.content,
            )?;
            if let Err(error) =
                state
                    .db
                    .compare_exchange_prompt_selection(AppType::Pi.as_str(), &before, &prompts)
            {
                restore_pi_prompt_file(&guard, &published, &snapshot)?;
                return Err(error);
            }
            return Ok(());
        }

        if previous.as_ref().is_some_and(|value| value.enabled) {
            ensure_pi_library_projection_matches(&snapshot, previous.as_ref())?;
            let removed = PiPromptFileService::delete_under_guard(
                &guard,
                PiPromptFileKind::GlobalContext,
                &snapshot.revision,
            )?;
            if let Err(error) =
                state
                    .db
                    .compare_exchange_prompt_selection(AppType::Pi.as_str(), &before, &prompts)
            {
                if removed {
                    let missing = PiPromptFileService::read_under_guard(
                        &guard,
                        PiPromptFileKind::GlobalContext,
                    )?;
                    PiPromptFileService::replace_under_guard(
                        &guard,
                        PiPromptFileKind::GlobalContext,
                        &missing.revision,
                        &snapshot.content,
                    )?;
                }
                return Err(error);
            }
            return Ok(());
        }

        state
            .db
            .compare_exchange_prompt_selection(AppType::Pi.as_str(), &before, &prompts)
    }

    fn enable_pi_prompt(state: &AppState, id: &str) -> Result<(), AppError> {
        let _switch_guard = futures::executor::block_on(
            state
                .proxy_service
                .lock_switch_for_app(AppType::Pi.as_str()),
        );
        let guard = lock_instruction_files()?;
        let before = state.db.get_prompts(AppType::Pi.as_str())?;
        let target = before
            .get(id)
            .cloned()
            .ok_or_else(|| AppError::InvalidInput(format!("提示词 {id} 不存在")))?;
        let snapshot =
            PiPromptFileService::read_under_guard(&guard, PiPromptFileKind::GlobalContext)?;
        ensure_pi_library_projection_matches(
            &snapshot,
            before.values().find(|candidate| candidate.enabled),
        )?;
        let published = PiPromptFileService::replace_under_guard(
            &guard,
            PiPromptFileKind::GlobalContext,
            &snapshot.revision,
            &target.content,
        )?;

        let mut after = before.clone();
        for prompt in after.values_mut() {
            prompt.enabled = prompt.id == id;
        }
        if let Err(error) =
            state
                .db
                .compare_exchange_prompt_selection(AppType::Pi.as_str(), &before, &after)
        {
            restore_pi_prompt_file(&guard, &published, &snapshot)?;
            return Err(error);
        }
        Ok(())
    }

    fn import_pi_snapshot(
        state: &AppState,
        guard: &PiInstructionFileGuard,
        snapshot: &PiPromptFileSnapshot,
        before: &IndexMap<String, Prompt>,
        prompt: Prompt,
    ) -> Result<(), AppError> {
        let mut prompts = before.clone();
        for prompt in prompts.values_mut() {
            prompt.enabled = false;
        }
        // Import is an explicit reconciliation action. The native file is
        // already active by presence, so its exact DB counterpart becomes the
        // sole enabled library entry without rewriting the user-owned file.
        prompts.insert(prompt.id.clone(), prompt);

        let precommit =
            PiPromptFileService::read_under_guard(guard, PiPromptFileKind::GlobalContext)?;
        if precommit.revision != snapshot.revision {
            return Err(AppError::Conflict(
                "Pi AGENTS.md changed before prompt import publication".to_string(),
            ));
        }
        state
            .db
            .compare_exchange_prompt_selection(AppType::Pi.as_str(), before, &prompts)?;
        #[cfg(test)]
        apply_pi_native_binding_after_save_hooks_for_test(state.db.as_ref(), &snapshot.path)?;
        let verified =
            match PiPromptFileService::read_under_guard(guard, PiPromptFileKind::GlobalContext) {
                Ok(verified) => verified,
                Err(error) => {
                    restore_pi_library_after_failed_native_binding(
                        state.db.as_ref(),
                        before,
                        &prompts,
                        &error,
                    )?;
                    return Err(error);
                }
            };
        if verified.revision != snapshot.revision {
            let error = AppError::Conflict("Pi AGENTS.md changed during prompt import".to_string());
            restore_pi_library_after_failed_native_binding(
                state.db.as_ref(),
                before,
                &prompts,
                &error,
            )?;
            return Err(error);
        }
        Ok(())
    }
}

fn preferred_pi_prompt_match<'a>(
    prompts: &'a IndexMap<String, Prompt>,
    snapshot: &PiPromptFileSnapshot,
) -> Option<&'a str> {
    if !snapshot.exists {
        return None;
    }
    prompts
        .iter()
        .find_map(|(id, prompt)| {
            (prompt.enabled && prompt.content == snapshot.content).then_some(id.as_str())
        })
        .or_else(|| {
            prompts.iter().find_map(|(id, prompt)| {
                (prompt.content == snapshot.content).then_some(id.as_str())
            })
        })
}

fn pi_library_needs_reconciliation(
    prompts: &IndexMap<String, Prompt>,
    snapshot: &PiPromptFileSnapshot,
) -> bool {
    let persisted_enabled = prompts
        .iter()
        .filter_map(|(id, prompt)| prompt.enabled.then_some(id.as_str()))
        .collect::<Vec<_>>();
    let matched = preferred_pi_prompt_match(prompts, snapshot);
    persisted_enabled != matched.into_iter().collect::<Vec<_>>()
        || (snapshot.exists && matched.is_none())
}

fn reject_pi_prompt_mutation_during_drift(
    prompts: &IndexMap<String, Prompt>,
    snapshot: &PiPromptFileSnapshot,
    target_id: &str,
    replacement: Option<&Prompt>,
) -> Result<(), AppError> {
    if !pi_library_needs_reconciliation(prompts, snapshot) {
        return Ok(());
    }
    let target_is_persisted_active = prompts.get(target_id).is_some_and(|prompt| prompt.enabled);
    let target_is_live_match = preferred_pi_prompt_match(prompts, snapshot) == Some(target_id);
    let replacement_claims_live = replacement.is_some_and(|prompt| {
        prompt.enabled || (snapshot.exists && prompt.content == snapshot.content)
    });
    if target_is_persisted_active || target_is_live_match || replacement_claims_live {
        return Err(AppError::Conflict(
            "Pi AGENTS.md and the prompt library disagree; reconcile native truth before changing \
             an active prompt"
                .to_string(),
        ));
    }
    Ok(())
}

fn build_pi_reconciled_library(
    original: &IndexMap<String, Prompt>,
    snapshot: &PiPromptFileSnapshot,
) -> IndexMap<String, Prompt> {
    let mut prompts = original.clone();
    for prompt in prompts.values_mut() {
        prompt.enabled = false;
    }
    if !snapshot.exists {
        return prompts;
    }

    let active_id = preferred_pi_prompt_match(original, snapshot)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            let digest = format!("{:x}", Sha256::digest(snapshot.content.as_bytes()));
            let base = format!("native-{digest}");
            let mut id = base.clone();
            let mut suffix = 1_u32;
            while prompts.contains_key(&id) {
                id = format!("{base}-{suffix}");
                suffix += 1;
            }
            let timestamp = chrono::Utc::now().timestamp();
            prompts.insert(
                id.clone(),
                Prompt {
                    id: id.clone(),
                    name: "Imported from Pi AGENTS.md".to_string(),
                    content: snapshot.content.clone(),
                    description: Some(
                        "Device-local native state preserved during portable import".to_string(),
                    ),
                    enabled: false,
                    created_at: Some(timestamp),
                    updated_at: Some(timestamp),
                },
            );
            id
        });
    prompts
        .get_mut(&active_id)
        .expect("selected Pi prompt is present")
        .enabled = true;
    prompts
}

fn restore_pi_library_after_failed_native_binding(
    db: &Database,
    original: &IndexMap<String, Prompt>,
    attempted: &IndexMap<String, Prompt>,
    cause: &AppError,
) -> Result<(), AppError> {
    db.restore_prompt_selection_if_attempted(AppType::Pi.as_str(), attempted, original)
        .map_err(|restore_error| {
            AppError::Config(format!(
                "Pi prompt-library publication lost its native revision ({cause}) and failed to \
                 restore the previous portable library without overwriting a newer database \
                 revision: {restore_error}"
            ))
        })
}

#[cfg(test)]
static PI_NATIVE_BINDING_AFTER_SAVE_REPLACEMENTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::VecDeque<String>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::VecDeque::new()));

#[cfg(test)]
fn apply_pi_native_binding_after_save_hooks_for_test(
    db: &Database,
    path: &str,
) -> Result<(), AppError> {
    if let Some(replacement) = PI_NATIVE_BINDING_AFTER_SAVE_DB_REPLACEMENTS
        .lock()
        .map_err(|error| AppError::Lock(error.to_string()))?
        .pop_front()
    {
        db.save_prompt_selection(AppType::Pi.as_str(), &replacement)?;
    }
    let replacement = PI_NATIVE_BINDING_AFTER_SAVE_REPLACEMENTS
        .lock()
        .map_err(|error| AppError::Lock(error.to_string()))?
        .pop_front();
    if let Some(replacement) = replacement {
        std::fs::write(path, replacement).map_err(|error| AppError::io(path, error))?;
    }
    Ok(())
}

#[cfg(test)]
static PI_NATIVE_BINDING_AFTER_SAVE_DB_REPLACEMENTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::VecDeque<IndexMap<String, Prompt>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::VecDeque::new()));

#[cfg(test)]
fn replace_pi_library_after_next_reconcile_save_for_test(replacement: IndexMap<String, Prompt>) {
    PI_NATIVE_BINDING_AFTER_SAVE_DB_REPLACEMENTS
        .lock()
        .expect("Pi reconcile DB hook lock")
        .push_back(replacement);
}

#[cfg(test)]
fn replace_pi_agents_after_each_native_binding_save_for_test(
    replacements: impl IntoIterator<Item = &'static str>,
) {
    *PI_NATIVE_BINDING_AFTER_SAVE_REPLACEMENTS
        .lock()
        .expect("Pi reconcile hook lock") =
        replacements.into_iter().map(ToOwned::to_owned).collect();
}

fn ensure_pi_library_projection_matches(
    snapshot: &PiPromptFileSnapshot,
    current_enabled: Option<&Prompt>,
) -> Result<(), AppError> {
    match current_enabled {
        Some(prompt) if snapshot.exists && snapshot.content == prompt.content => Ok(()),
        Some(_) => Err(AppError::Conflict(
            "Pi AGENTS.md changed outside CC Switch; import or reconcile it before switching prompts"
                .to_string(),
        )),
        None if !snapshot.exists => Ok(()),
        None => Err(AppError::Conflict(
            "Pi AGENTS.md is user-owned; import it before enabling a library prompt".to_string(),
        )),
    }
}

fn restore_pi_prompt_file(
    guard: &PiInstructionFileGuard,
    published: &PiPromptFileSnapshot,
    previous: &PiPromptFileSnapshot,
) -> Result<(), AppError> {
    if previous.exists {
        PiPromptFileService::replace_under_guard(
            guard,
            PiPromptFileKind::GlobalContext,
            &published.revision,
            &previous.content,
        )?;
    } else {
        PiPromptFileService::delete_under_guard(
            guard,
            PiPromptFileKind::GlobalContext,
            &published.revision,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use serial_test::serial;
    use std::ffi::OsString;
    use std::sync::Arc;

    struct EnvRestore {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvRestore {
        fn set(key: &'static str, value: &std::path::Path) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvRestore {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn prompt(id: &str, content: &str, enabled: bool, timestamp: i64) -> Prompt {
        Prompt {
            id: id.to_string(),
            name: id.to_string(),
            content: content.to_string(),
            description: None,
            enabled,
            created_at: Some(timestamp),
            updated_at: Some(timestamp),
        }
    }

    #[test]
    #[serial]
    fn public_pi_import_reconciles_an_active_empty_agents_file() {
        // Pinned-Pi provenance: scripts/pi-transport-capture.mjs executes
        // DefaultResourceLoader at ab366ebe94cacd419d986be454f12b1b9913aaca
        // and records an existing zero-byte AGENTS.md as an active resource.
        let temp = tempfile::tempdir().expect("tempdir");
        let _restore = EnvRestore::set("PI_CODING_AGENT_DIR", temp.path());
        std::fs::write(temp.path().join("AGENTS.md"), "").expect("seed AGENTS.md");
        let state = AppState::new(Arc::new(Database::memory().expect("database")));

        let imported =
            PromptService::import_from_file(&state, AppType::Pi).expect("import empty AGENTS.md");
        let prompts = PromptService::get_prompts(&state, AppType::Pi).expect("read prompts");
        let active = prompts.get(&imported).expect("imported prompt");
        assert!(active.enabled);
        assert_eq!(active.content, "");
        assert_eq!(
            prompts.values().filter(|prompt| prompt.enabled).count(),
            1,
            "the native active file must have exactly one active DB owner"
        );
        assert_eq!(
            PromptService::get_current_file_content(AppType::Pi).expect("read live"),
            Some(String::new())
        );
    }

    #[test]
    #[serial]
    fn public_pi_import_rolls_back_portable_state_if_native_revision_changes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _restore = EnvRestore::set("PI_CODING_AGENT_DIR", temp.path());
        std::fs::write(temp.path().join("AGENTS.md"), "native-before").expect("seed AGENTS.md");
        let state = AppState::new(Arc::new(Database::memory().expect("database")));
        replace_pi_agents_after_each_native_binding_save_for_test(["native-after"]);

        let error = PromptService::import_from_file(&state, AppType::Pi)
            .expect_err("a stale native snapshot must not become active portable state");
        assert!(matches!(error, AppError::Conflict(_)));
        assert!(
            state
                .db
                .get_prompts(AppType::Pi.as_str())
                .expect("restored portable library")
                .is_empty(),
            "failed import must conditionally restore its exact DB before-image"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("AGENTS.md")).expect("external native edit"),
            "native-after"
        );
    }

    #[test]
    #[serial]
    fn first_launch_pi_import_rolls_back_if_native_revision_changes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _restore = EnvRestore::set("PI_CODING_AGENT_DIR", temp.path());
        std::fs::write(temp.path().join("AGENTS.md"), "native-before").expect("seed AGENTS.md");
        let state = AppState::new(Arc::new(Database::memory().expect("database")));
        replace_pi_agents_after_each_native_binding_save_for_test(["native-after"]);

        let error = PromptService::import_from_file_on_first_launch(&state, AppType::Pi)
            .expect_err("first-launch import must bind its DB row to one native revision");
        assert!(matches!(error, AppError::Conflict(_)));
        assert!(state
            .db
            .get_prompts(AppType::Pi.as_str())
            .expect("restored portable library")
            .is_empty());
        assert_eq!(
            std::fs::read_to_string(temp.path().join("AGENTS.md")).expect("external native edit"),
            "native-after"
        );
    }

    #[test]
    fn unowned_empty_agents_file_is_not_treated_as_absent() {
        let snapshot = PiPromptFileSnapshot {
            kind: PiPromptFileKind::GlobalContext,
            path: "AGENTS.md".to_string(),
            exists: true,
            revision: "present-empty".to_string(),
            content: String::new(),
        };
        assert!(matches!(
            ensure_pi_library_projection_matches(&snapshot, None),
            Err(AppError::Conflict(_))
        ));
    }

    #[test]
    #[serial]
    fn portable_prompt_reconciliation_preserves_native_bytes_and_rebuilds_active_truth() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _restore = EnvRestore::set("PI_CODING_AGENT_DIR", temp.path());
        let native_content = "device-local AGENTS";
        std::fs::write(temp.path().join("AGENTS.md"), native_content).expect("seed AGENTS.md");
        let state = AppState::new(Arc::new(Database::memory().expect("database")));
        state
            .db
            .save_prompt(
                AppType::Pi.as_str(),
                &Prompt {
                    id: "portable-active".to_string(),
                    name: "Portable active".to_string(),
                    content: "incoming portable content".to_string(),
                    description: None,
                    enabled: true,
                    created_at: Some(1),
                    updated_at: Some(1),
                },
            )
            .expect("seed portable prompt");

        PromptService::reconcile_pi_portable_import(&state).expect("reconcile");

        assert_eq!(
            std::fs::read_to_string(temp.path().join("AGENTS.md")).expect("read native"),
            native_content,
            "portable import must not overwrite the device-local native file"
        );
        let prompts = state
            .db
            .get_prompts(AppType::Pi.as_str())
            .expect("read prompts");
        let active = prompts
            .values()
            .filter(|prompt| prompt.enabled)
            .collect::<Vec<_>>();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].content, native_content);
        assert!(!prompts["portable-active"].enabled);
    }

    #[test]
    #[serial]
    fn portable_prompt_reconciliation_disables_shadow_state_when_agents_is_absent() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _restore = EnvRestore::set("PI_CODING_AGENT_DIR", temp.path());
        let state = AppState::new(Arc::new(Database::memory().expect("database")));
        state
            .db
            .save_prompt(
                AppType::Pi.as_str(),
                &Prompt {
                    id: "portable-active".to_string(),
                    name: "Portable active".to_string(),
                    content: "incoming portable content".to_string(),
                    description: None,
                    enabled: true,
                    created_at: Some(1),
                    updated_at: Some(1),
                },
            )
            .expect("seed portable prompt");

        PromptService::reconcile_pi_portable_import(&state).expect("reconcile");

        let prompts = state
            .db
            .get_prompts(AppType::Pi.as_str())
            .expect("read prompts");
        assert!(prompts.values().all(|prompt| !prompt.enabled));
        assert!(!temp.path().join("AGENTS.md").exists());
    }

    #[test]
    #[serial]
    fn reading_pi_prompts_does_not_adopt_external_agents_drift() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _restore = EnvRestore::set("PI_CODING_AGENT_DIR", temp.path());
        std::fs::write(temp.path().join("AGENTS.md"), "managed-before").expect("seed AGENTS.md");
        let state = AppState::new(Arc::new(Database::memory().expect("database")));
        state
            .db
            .save_prompt(
                AppType::Pi.as_str(),
                &Prompt {
                    id: "managed".to_string(),
                    name: "Managed".to_string(),
                    content: "managed-before".to_string(),
                    description: None,
                    enabled: true,
                    created_at: Some(1),
                    updated_at: Some(1),
                },
            )
            .expect("seed prompt");
        state
            .db
            .save_prompt(
                AppType::Pi.as_str(),
                &Prompt {
                    id: "other".to_string(),
                    name: "Other".to_string(),
                    content: "other-content".to_string(),
                    description: None,
                    enabled: false,
                    created_at: Some(2),
                    updated_at: Some(2),
                },
            )
            .expect("seed alternate prompt");

        std::fs::write(temp.path().join("AGENTS.md"), "external-after").expect("external edit");
        let prompts = PromptService::get_prompts(&state, AppType::Pi).expect("read prompt list");
        assert_eq!(prompts["managed"].content, "managed-before");
        assert!(
            prompts.values().all(|prompt| !prompt.enabled),
            "read-only inspection must report native truth instead of the stale DB projection"
        );
        assert!(
            state
                .db
                .get_prompts(AppType::Pi.as_str())
                .expect("read persisted projection")["managed"]
                .enabled,
            "inspection must not silently adopt or rewrite the portable library"
        );
        assert!(
            PromptService::enable_prompt(&state, AppType::Pi, "other").is_err(),
            "the write boundary must still report the external drift conflict"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("AGENTS.md")).expect("live AGENTS.md"),
            "external-after"
        );
    }

    #[test]
    #[serial]
    fn explicit_pi_library_reconciliation_adopts_external_native_truth() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _restore = EnvRestore::set("PI_CODING_AGENT_DIR", temp.path());
        std::fs::write(temp.path().join("AGENTS.md"), "external-after").expect("seed AGENTS.md");
        let state = AppState::new(Arc::new(Database::memory().expect("database")));
        state
            .db
            .save_prompt(
                AppType::Pi.as_str(),
                &Prompt {
                    id: "stale".to_string(),
                    name: "Stale".to_string(),
                    content: "managed-before".to_string(),
                    description: None,
                    enabled: true,
                    created_at: Some(1),
                    updated_at: Some(1),
                },
            )
            .expect("seed stale projection");

        let status = PromptService::get_pi_library_status(&state).expect("inspect");
        assert!(status.native_exists);
        assert!(status.matched_prompt_id.is_none());
        assert!(status.needs_reconciliation);

        PromptService::reconcile_pi_library(&state).expect("explicit reconcile");
        let prompts = PromptService::get_prompts(&state, AppType::Pi).expect("read reconciled");
        let active = prompts
            .values()
            .filter(|prompt| prompt.enabled)
            .collect::<Vec<_>>();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].content, "external-after");
        assert!(
            !PromptService::get_pi_library_status(&state)
                .expect("reinspect")
                .needs_reconciliation
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("AGENTS.md")).expect("native remains"),
            "external-after"
        );
    }

    #[test]
    #[serial]
    fn missing_agents_file_is_effectively_inactive_until_explicit_reconciliation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _restore = EnvRestore::set("PI_CODING_AGENT_DIR", temp.path());
        let state = AppState::new(Arc::new(Database::memory().expect("database")));
        state
            .db
            .save_prompt(
                AppType::Pi.as_str(),
                &Prompt {
                    id: "shadow".to_string(),
                    name: "Shadow".to_string(),
                    content: "not live".to_string(),
                    description: None,
                    enabled: true,
                    created_at: Some(1),
                    updated_at: Some(1),
                },
            )
            .expect("seed shadow");

        assert!(PromptService::get_prompts(&state, AppType::Pi)
            .expect("read effective")
            .values()
            .all(|prompt| !prompt.enabled));
        let status = PromptService::get_pi_library_status(&state).expect("inspect");
        assert!(!status.native_exists);
        assert!(status.needs_reconciliation);

        PromptService::reconcile_pi_library(&state).expect("explicit reconcile");
        assert!(state
            .db
            .get_prompts(AppType::Pi.as_str())
            .expect("read persisted")
            .values()
            .all(|prompt| !prompt.enabled));
        assert!(!temp.path().join("AGENTS.md").exists());
    }

    #[test]
    #[serial]
    fn duplicate_prompt_content_prefers_the_persisted_enabled_identity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _restore = EnvRestore::set("PI_CODING_AGENT_DIR", temp.path());
        std::fs::write(temp.path().join("AGENTS.md"), "same-content").expect("seed AGENTS.md");
        let state = AppState::new(Arc::new(Database::memory().expect("database")));
        state
            .db
            .save_prompt(
                AppType::Pi.as_str(),
                &prompt("first", "same-content", false, 1),
            )
            .expect("first duplicate");
        state
            .db
            .save_prompt(
                AppType::Pi.as_str(),
                &prompt("persisted-active", "same-content", true, 2),
            )
            .expect("active duplicate");

        let effective = PromptService::get_prompts(&state, AppType::Pi).expect("effective prompts");
        assert!(!effective["first"].enabled);
        assert!(effective["persisted-active"].enabled);
        let status = PromptService::get_pi_library_status(&state).expect("status");
        assert_eq!(
            status.matched_prompt_id.as_deref(),
            Some("persisted-active")
        );
        assert!(!status.needs_reconciliation);
    }

    #[test]
    #[serial]
    fn external_prompt_drift_blocks_deleting_or_disabling_the_live_match() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _restore = EnvRestore::set("PI_CODING_AGENT_DIR", temp.path());
        std::fs::write(temp.path().join("AGENTS.md"), "external-live").expect("seed AGENTS.md");
        let state = AppState::new(Arc::new(Database::memory().expect("database")));
        state
            .db
            .save_prompt(
                AppType::Pi.as_str(),
                &prompt("stale-active", "stale-content", true, 1),
            )
            .expect("stale prompt");
        state
            .db
            .save_prompt(
                AppType::Pi.as_str(),
                &prompt("live-match", "external-live", false, 2),
            )
            .expect("live match");
        let before = serde_json::to_value(
            state
                .db
                .get_prompts(AppType::Pi.as_str())
                .expect("before prompts"),
        )
        .expect("serialize before");

        assert!(matches!(
            PromptService::delete_prompt(&state, AppType::Pi, "live-match"),
            Err(AppError::Conflict(_))
        ));
        assert!(matches!(
            PromptService::upsert_prompt(
                &state,
                AppType::Pi,
                "live-match",
                prompt("live-match", "external-live", false, 3),
            ),
            Err(AppError::Conflict(_))
        ));
        assert_eq!(
            serde_json::to_value(
                state
                    .db
                    .get_prompts(AppType::Pi.as_str())
                    .expect("after prompts")
            )
            .expect("serialize after"),
            before
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("AGENTS.md")).expect("live remains"),
            "external-live"
        );
    }

    #[test]
    #[serial]
    fn repeated_external_reconcile_races_restore_the_original_library() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _restore = EnvRestore::set("PI_CODING_AGENT_DIR", temp.path());
        std::fs::write(temp.path().join("AGENTS.md"), "native-0").expect("seed AGENTS.md");
        let state = AppState::new(Arc::new(Database::memory().expect("database")));
        state
            .db
            .save_prompt(
                AppType::Pi.as_str(),
                &prompt("portable", "portable-content", true, 1),
            )
            .expect("portable prompt");
        let before = serde_json::to_value(
            state
                .db
                .get_prompts(AppType::Pi.as_str())
                .expect("before prompts"),
        )
        .expect("serialize before");
        replace_pi_agents_after_each_native_binding_save_for_test([
            "native-1", "native-2", "native-3",
        ]);

        assert!(matches!(
            PromptService::reconcile_pi_library(&state),
            Err(AppError::Conflict(_))
        ));
        assert_eq!(
            serde_json::to_value(
                state
                    .db
                    .get_prompts(AppType::Pi.as_str())
                    .expect("restored prompts")
            )
            .expect("serialize restored"),
            before,
            "a failed reconcile must leave no imported rows or selection changes"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("AGENTS.md")).expect("latest native"),
            "native-3"
        );
    }

    #[test]
    #[serial]
    fn reconcile_compensation_preserves_a_concurrent_portable_library() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _restore = EnvRestore::set("PI_CODING_AGENT_DIR", temp.path());
        std::fs::write(temp.path().join("AGENTS.md"), "native-0").expect("seed AGENTS.md");
        let state = AppState::new(Arc::new(Database::memory().expect("database")));
        state
            .db
            .save_prompt(
                AppType::Pi.as_str(),
                &prompt("before", "portable-before", true, 1),
            )
            .expect("seed original library");

        let imported = IndexMap::from([(
            "restored-import".to_string(),
            prompt("restored-import", "portable-restored", true, 2),
        )]);
        replace_pi_library_after_next_reconcile_save_for_test(imported.clone());
        replace_pi_agents_after_each_native_binding_save_for_test(["native-1"]);

        let error = PromptService::reconcile_pi_library(&state)
            .expect_err("stale compensation must not overwrite a concurrent import");
        assert!(
            error.to_string().contains("newer database revision"),
            "the conflict must explain why compensation stopped: {error}"
        );
        assert_eq!(
            state
                .db
                .get_prompts(AppType::Pi.as_str())
                .expect("preserved imported library"),
            imported
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("AGENTS.md")).expect("latest native"),
            "native-1"
        );
    }

    #[test]
    fn prompt_selection_compensation_can_restore_an_exact_legacy_before_image() {
        let db = Database::memory().expect("database");
        db.save_prompt(AppType::Pi.as_str(), &prompt("legacy-a", "a", true, 1))
            .expect("first legacy row");
        db.save_prompt(AppType::Pi.as_str(), &prompt("legacy-b", "b", true, 2))
            .expect("second legacy row");
        let before = db
            .get_prompts(AppType::Pi.as_str())
            .expect("legacy before-image");
        let mut attempted = before.clone();
        attempted.get_mut("legacy-a").expect("first row").enabled = false;

        db.compare_exchange_prompt_selection(AppType::Pi.as_str(), &before, &attempted)
            .expect("publish valid projection");
        db.restore_prompt_selection_if_attempted(AppType::Pi.as_str(), &attempted, &before)
            .expect("restore exact legacy image");

        assert_eq!(
            db.get_prompts(AppType::Pi.as_str())
                .expect("restored library"),
            before
        );
    }
}

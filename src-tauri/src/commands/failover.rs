//! 故障转移队列命令
//!
//! 管理代理模式下的故障转移队列（基于 providers 表的 in_failover_queue 字段）

use crate::app_config::AppType;
use crate::database::FailoverQueueItem;
use crate::provider::Provider;
use crate::store::AppState;
use std::str::FromStr;
use tauri::Emitter;

/// 获取故障转移队列
#[tauri::command]
pub async fn get_failover_queue(
    state: tauri::State<'_, AppState>,
    app_type: String,
) -> Result<Vec<FailoverQueueItem>, String> {
    state
        .db
        .get_failover_queue(&app_type)
        .map_err(|e| e.to_string())
}

/// 获取可添加到故障转移队列的供应商（不在队列中的）
#[tauri::command]
pub async fn get_available_providers_for_failover(
    state: tauri::State<'_, AppState>,
    app_type: String,
) -> Result<Vec<Provider>, String> {
    state
        .db
        .get_available_providers_for_failover(&app_type)
        .map_err(|e| e.to_string())
}

/// 添加供应商到故障转移队列
#[tauri::command]
pub async fn add_to_failover_queue(
    state: tauri::State<'_, AppState>,
    app_type: String,
    provider_id: String,
) -> Result<(), String> {
    if app_type == "pi" {
        let _guard = state
            .proxy_service
            .lock_switch_for_app(AppType::Pi.as_str())
            .await;
        if state
            .db
            .get_provider_aggregate("pi", &provider_id)
            .map_err(|error| error.to_string())?
            .is_none()
        {
            return Err(format!("Pi provider does not exist: {provider_id}"));
        }
        let was_member = state
            .db
            .is_in_failover_queue("pi", &provider_id)
            .map_err(|error| error.to_string())?;
        let epoch = state.proxy_service.begin_pi_catalog_mutation().await;
        if let Err(error) = state.db.add_to_failover_queue("pi", &provider_id) {
            let _ = state
                .proxy_service
                .reconcile_pi_runtime_at_epoch(epoch)
                .await;
            return Err(error.to_string());
        }
        if let Err(error) = state
            .proxy_service
            .reconcile_pi_runtime_at_epoch(epoch)
            .await
        {
            if !was_member {
                let _ = state.db.remove_from_failover_queue("pi", &provider_id);
            }
            let rollback_epoch = state.proxy_service.begin_pi_catalog_mutation().await;
            let _ = state
                .proxy_service
                .reconcile_pi_runtime_at_epoch(rollback_epoch)
                .await;
            return Err(format!(
                "Pi failover queue changed but runtime publication failed: {error}"
            ));
        }
        return Ok(());
    }
    state
        .db
        .add_to_failover_queue(&app_type, &provider_id)
        .map_err(|e| e.to_string())
}

/// 从故障转移队列移除供应商
#[tauri::command]
pub async fn remove_from_failover_queue(
    state: tauri::State<'_, AppState>,
    app_type: String,
    provider_id: String,
) -> Result<(), String> {
    if app_type == "pi" {
        let _guard = state
            .proxy_service
            .lock_switch_for_app(AppType::Pi.as_str())
            .await;
        let was_member = state
            .db
            .is_in_failover_queue("pi", &provider_id)
            .map_err(|error| error.to_string())?;
        let epoch = state.proxy_service.begin_pi_catalog_mutation().await;
        if let Err(error) = state.db.remove_from_failover_queue("pi", &provider_id) {
            let _ = state
                .proxy_service
                .reconcile_pi_runtime_at_epoch(epoch)
                .await;
            return Err(error.to_string());
        }
        if let Err(error) = state
            .proxy_service
            .reconcile_pi_runtime_at_epoch(epoch)
            .await
        {
            if was_member {
                let _ = state.db.add_to_failover_queue("pi", &provider_id);
            }
            let rollback_epoch = state.proxy_service.begin_pi_catalog_mutation().await;
            let _ = state
                .proxy_service
                .reconcile_pi_runtime_at_epoch(rollback_epoch)
                .await;
            return Err(format!(
                "Pi failover queue changed but runtime publication failed: {error}"
            ));
        }
        return Ok(());
    }
    state
        .db
        .remove_from_failover_queue(&app_type, &provider_id)
        .map_err(|e| e.to_string())
}

/// 获取指定应用的自动故障转移开关状态（从 proxy_config 表读取）
#[tauri::command]
pub async fn get_auto_failover_enabled(
    state: tauri::State<'_, AppState>,
    app_type: String,
) -> Result<bool, String> {
    if app_type == "pi" {
        return Ok(crate::settings::get_pi_proxy_settings().auto_failover_enabled);
    }
    state
        .db
        .get_proxy_config_for_app(&app_type)
        .await
        .map(|config| config.auto_failover_enabled)
        .map_err(|e| e.to_string())
}

/// 设置指定应用的自动故障转移开关状态（写入 proxy_config 表）
///
/// 注意：关闭故障转移时不会清除队列，队列内容会保留供下次开启时使用
#[tauri::command]
pub async fn set_auto_failover_enabled(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    app_type: String,
    enabled: bool,
) -> Result<(), String> {
    log::info!(
        "[Failover] Setting auto_failover_enabled: app_type='{app_type}', enabled={enabled}"
    );

    if app_type == "pi" {
        return set_pi_auto_failover_enabled(&app, state.inner(), enabled).await;
    }

    // 读取当前配置
    let mut config = state
        .db
        .get_proxy_config_for_app(&app_type)
        .await
        .map_err(|e| e.to_string())?;

    if enabled && !config.enabled {
        return Err("需要先启用该应用的代理接管，再开启故障转移".to_string());
    }

    // 队列为空时把当前供应商自动加入作为 P1，避免用户陷入"必须先加队列才能开启"的死锁
    let mut auto_added_provider_id: Option<String> = None;
    let p1_provider_id = if enabled {
        let mut queue = state
            .db
            .get_failover_queue(&app_type)
            .map_err(|e| e.to_string())?;

        if queue.is_empty() {
            let app_enum = crate::app_config::AppType::from_str(&app_type)
                .map_err(|_| format!("无效的应用类型: {app_type}"))?;

            let current_id = crate::settings::get_effective_current_provider(&state.db, &app_enum)
                .map_err(|e| e.to_string())?;

            let Some(current_id) = current_id else {
                return Err("故障转移队列为空，且未设置当前供应商，无法开启故障转移".to_string());
            };

            state
                .db
                .add_to_failover_queue(&app_type, &current_id)
                .map_err(|e| e.to_string())?;
            auto_added_provider_id = Some(current_id);

            queue = state
                .db
                .get_failover_queue(&app_type)
                .map_err(|e| e.to_string())?;
        }

        queue
            .first()
            .map(|item| item.provider_id.clone())
            .ok_or_else(|| "故障转移队列为空，无法开启故障转移".to_string())?
    } else {
        String::new()
    };

    // 开启前先切到 P1。只有切换成功后才写入 auto_failover_enabled=true，
    // 避免 P1 不可切换（例如 official provider）时留下“开关已开但目标未切”的脏状态。
    if enabled {
        if let Err(e) = state
            .proxy_service
            .switch_proxy_target(&app_type, &p1_provider_id)
            .await
        {
            if let Some(provider_id) = auto_added_provider_id {
                let _ = state.db.remove_from_failover_queue(&app_type, &provider_id);
            }
            return Err(e);
        }
    }

    // 更新 auto_failover_enabled 字段
    config.auto_failover_enabled = enabled;

    // 写回数据库
    state
        .db
        .update_proxy_config_for_app(config)
        .await
        .map_err(|e| e.to_string())?;

    if enabled {
        // 发射 provider-switched 事件（让前端刷新当前供应商）
        let event_data = serde_json::json!({
            "appType": app_type,
            "providerId": p1_provider_id,
            "source": "failoverEnabled"
        });
        let _ = app.emit("provider-switched", event_data);
    }

    // 刷新托盘菜单，确保状态同步
    if let Ok(new_menu) = crate::tray::create_tray_menu(&app, &state) {
        if let Some(tray) = app.tray_by_id(crate::tray::TRAY_ID) {
            let _ = tray.set_menu(Some(new_menu));
        }
    }

    Ok(())
}

async fn set_pi_auto_failover_enabled(
    app: &tauri::AppHandle,
    state: &AppState,
    enabled: bool,
) -> Result<(), String> {
    let selected_provider = set_pi_auto_failover_enabled_inner(state, enabled).await?;

    let _ = app.emit(
        "provider-switched",
        serde_json::json!({
            "appType": "pi",
            "providerId": selected_provider,
            "source": "failoverPreferenceChanged"
        }),
    );
    if let Ok(new_menu) = crate::tray::create_tray_menu(app, state) {
        if let Some(tray) = app.tray_by_id(crate::tray::TRAY_ID) {
            let _ = tray.set_menu(Some(new_menu));
        }
    }
    Ok(())
}

async fn set_pi_auto_failover_enabled_inner(
    state: &AppState,
    enabled: bool,
) -> Result<Option<String>, String> {
    let guard = state
        .proxy_service
        .lock_switch_for_app(AppType::Pi.as_str())
        .await;
    let previous_config = crate::settings::get_pi_proxy_settings();
    if enabled && !crate::settings::pi_takeover_enabled() {
        return Err("Pi gateway takeover must be enabled before failover".to_string());
    }

    let previous_provider =
        crate::services::pi_catalog::PiCatalogCoordinator::current_native_provider(state)
            .map_err(|error| error.to_string())?;
    let mut auto_added = None;
    let mut switched_primary = false;
    let selected_provider = if enabled {
        let previous_provider = previous_provider.clone().ok_or_else(|| {
            "Pi has no current provider, so failover cannot select queue P1".to_string()
        })?;
        let mut queue = state
            .db
            .get_failover_queue("pi")
            .map_err(|error| error.to_string())?;
        if queue.is_empty() {
            state
                .db
                .add_to_failover_queue("pi", &previous_provider)
                .map_err(|error| error.to_string())?;
            auto_added = Some(previous_provider.clone());
            queue = state
                .db
                .get_failover_queue("pi")
                .map_err(|error| error.to_string())?;
        }
        let primary = queue
            .first()
            .map(|item| item.provider_id.clone())
            .ok_or_else(|| "Pi failover queue is empty".to_string())?;
        if primary != previous_provider {
            if let Err(error) = set_pi_default_under_switch_guard(state, &guard, &primary) {
                if let Some(provider_id) = auto_added.take() {
                    let _ = state.db.remove_from_failover_queue("pi", &provider_id);
                }
                return Err(error);
            }
            switched_primary = true;
        }
        Some(primary)
    } else {
        previous_provider.clone()
    };

    let mut next = previous_config.clone();
    next.auto_failover_enabled = enabled;
    let epoch = state.proxy_service.begin_pi_catalog_mutation().await;
    if let Err(error) = crate::settings::update_pi_proxy_settings(next) {
        if let Some(provider_id) = auto_added.take() {
            let _ = state.db.remove_from_failover_queue("pi", &provider_id);
        }
        let rollback = rollback_pi_failover_primary(
            state,
            &guard,
            previous_provider.as_deref(),
            switched_primary,
            Some(epoch),
        )
        .await;
        return Err(with_pi_failover_rollback(error.to_string(), rollback));
    }
    if let Err(error) = state
        .proxy_service
        .reconcile_pi_runtime_at_epoch(epoch)
        .await
    {
        let settings_rollback = crate::settings::update_pi_proxy_settings(previous_config)
            .err()
            .map(|rollback_error| rollback_error.to_string());
        if let Some(provider_id) = auto_added.take() {
            let _ = state.db.remove_from_failover_queue("pi", &provider_id);
        }
        let primary_rollback = rollback_pi_failover_primary(
            state,
            &guard,
            previous_provider.as_deref(),
            switched_primary,
            None,
        )
        .await;
        let rollback = settings_rollback.or(primary_rollback);
        return Err(with_pi_failover_rollback(
            format!("Pi failover preference changed but runtime publication failed: {error}"),
            rollback,
        ));
    }

    Ok(selected_provider)
}

fn set_pi_default_under_switch_guard(
    state: &AppState,
    guard: &tokio::sync::OwnedMutexGuard<()>,
    provider_id: &str,
) -> Result<(), String> {
    let aggregate = state
        .db
        .get_provider_aggregate(AppType::Pi.as_str(), provider_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("Pi provider does not exist: {provider_id}"))?;
    let config: crate::pi_config::model::PiManagedProviderConfig =
        serde_json::from_value(aggregate.provider.settings_config)
            .map_err(|error| format!("managed Pi provider '{provider_id}' is invalid: {error}"))?;
    let model_id = config
        .models
        .first()
        .map(|model| model.id.clone())
        .ok_or_else(|| format!("Pi provider '{provider_id}' has no selectable models"))?;
    crate::services::pi_catalog::PiCatalogCoordinator::apply_under_switch_guard(
        state,
        guard,
        crate::services::pi_catalog::PiCatalogMutation::SetDefault {
            provider_id: provider_id.to_string(),
            model_id,
        },
    )
    .map(|_| ())
    .map_err(|error| error.to_string())
}

async fn rollback_pi_failover_primary(
    state: &AppState,
    guard: &tokio::sync::OwnedMutexGuard<()>,
    previous_provider: Option<&str>,
    switched_primary: bool,
    pending_epoch: Option<u64>,
) -> Option<String> {
    if switched_primary {
        let previous_provider =
            previous_provider.expect("switching P1 requires a previous Pi provider");
        return set_pi_default_under_switch_guard(state, guard, previous_provider)
            .err()
            .map(|error| format!("primary rollback failed: {error}"));
    }

    let epoch = match pending_epoch {
        Some(epoch) => epoch,
        None => state.proxy_service.begin_pi_catalog_mutation().await,
    };
    state
        .proxy_service
        .reconcile_pi_runtime_at_epoch(epoch)
        .await
        .err()
        .map(|error| format!("runtime rollback failed: {error}"))
}

fn with_pi_failover_rollback(error: String, rollback: Option<String>) -> String {
    match rollback {
        Some(rollback) => format!("{error}; {rollback}"),
        None => error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::provider::ProviderMutationInput;
    use crate::services::pi_catalog::{PiCatalogCoordinator, PiCatalogMutation};
    use serde_json::json;
    use std::sync::Arc;

    struct TestHome(Option<std::ffi::OsString>);

    impl TestHome {
        fn install(path: &std::path::Path) -> Result<Self, crate::error::AppError> {
            let previous = std::env::var_os("CC_SWITCH_TEST_HOME");
            std::env::set_var("CC_SWITCH_TEST_HOME", path);
            crate::settings::reload_settings()?;
            Ok(Self(previous))
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
                None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
            }
            let _ = crate::settings::reload_settings();
        }
    }

    fn managed_input(id: &str) -> ProviderMutationInput {
        ProviderMutationInput {
            id: id.to_string(),
            name: id.to_string(),
            settings_config: json!({
                "name": id,
                "api": "openai-responses",
                "baseUrl": format!("https://{id}.example/v1"),
                "apiKey": "literal-key",
                "models": [{"id": format!("{id}-model"), "name": id}]
            }),
            website_url: None,
            category: None,
            created_at: Some(1),
            sort_index: Some(0),
            notes: None,
            meta: None,
            icon: Some("pi".to_string()),
            icon_color: None,
            in_failover_queue: false,
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn enabling_pi_failover_selects_queue_p1() -> Result<(), crate::error::AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        let mut settings = crate::settings::get_settings();
        settings.pi_config_dir = Some(temp.path().join("pi-agent").to_string_lossy().into_owned());
        settings.pi_takeover_enabled = false;
        crate::settings::update_settings(settings)?;

        let state = AppState::new(Arc::new(Database::memory()?));
        for (provider_id, activate_if_first) in [("provider-a", true), ("provider-b", false)] {
            PiCatalogCoordinator::apply(
                &state,
                PiCatalogMutation::CreateProvider {
                    input: managed_input(provider_id),
                    provider_key: provider_id.to_string(),
                    activate_if_first,
                },
            )?;
        }
        state.db.add_to_failover_queue("pi", "provider-b")?;
        let mut proxy_config = state.db.get_global_proxy_config().await?;
        proxy_config.listen_port = 0;
        state.db.update_global_proxy_config(proxy_config).await?;
        state
            .proxy_service
            .set_takeover_for_app("pi", true)
            .await
            .map_err(crate::error::AppError::Message)?;

        let selected = set_pi_auto_failover_enabled_inner(&state, true)
            .await
            .map_err(crate::error::AppError::Message)?;

        assert_eq!(selected.as_deref(), Some("provider-b"));
        assert_eq!(
            PiCatalogCoordinator::current_native_provider(&state)?.as_deref(),
            Some("provider-b")
        );
        assert!(crate::settings::get_pi_proxy_settings().auto_failover_enabled);
        state
            .proxy_service
            .set_takeover_for_app("pi", false)
            .await
            .map_err(crate::error::AppError::Message)?;
        Ok(())
    }
}

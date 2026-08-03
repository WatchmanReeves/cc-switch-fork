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
    let _guard = state
        .proxy_service
        .lock_switch_for_app(AppType::Pi.as_str())
        .await;
    let previous_config = crate::settings::get_pi_proxy_settings();
    if enabled && !crate::settings::pi_takeover_enabled() {
        return Err("Pi gateway takeover must be enabled before failover".to_string());
    }

    let mut auto_added = None;
    if enabled
        && state
            .db
            .get_failover_queue("pi")
            .map_err(|error| error.to_string())?
            .is_empty()
    {
        let current =
            crate::services::pi_catalog::PiCatalogCoordinator::current_native_provider(state)
                .map_err(|error| error.to_string())?
                .ok_or_else(|| {
                    "Pi failover queue is empty and no current provider is selected".to_string()
                })?;
        state
            .db
            .add_to_failover_queue("pi", &current)
            .map_err(|error| error.to_string())?;
        auto_added = Some(current);
    }

    let mut next = previous_config.clone();
    next.auto_failover_enabled = enabled;
    let epoch = state.proxy_service.begin_pi_catalog_mutation().await;
    if let Err(error) = crate::settings::update_pi_proxy_settings(next) {
        if let Some(provider_id) = auto_added {
            let _ = state.db.remove_from_failover_queue("pi", &provider_id);
        }
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
        let _ = crate::settings::update_pi_proxy_settings(previous_config);
        if let Some(provider_id) = auto_added {
            let _ = state.db.remove_from_failover_queue("pi", &provider_id);
        }
        let rollback_epoch = state.proxy_service.begin_pi_catalog_mutation().await;
        let _ = state
            .proxy_service
            .reconcile_pi_runtime_at_epoch(rollback_epoch)
            .await;
        return Err(format!(
            "Pi failover preference changed but runtime publication failed: {error}"
        ));
    }

    let _ = app.emit(
        "provider-switched",
        serde_json::json!({
            "appType": "pi",
            "providerId":
                crate::services::pi_catalog::PiCatalogCoordinator::current_native_provider(state)
                    .map_err(|error| error.to_string())?,
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

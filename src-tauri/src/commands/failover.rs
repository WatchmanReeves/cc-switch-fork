//! 故障转移队列命令
//!
//! 管理代理模式下的故障转移队列（基于 providers 表的 in_failover_queue 字段）

use crate::app_config::AppType;
use crate::database::FailoverQueueItem;
use crate::pi_config::native_settings::PiNativeDefaultsReceipt;
use crate::provider::Provider;
use crate::services::pi_catalog::{PiActiveRoute, PiCatalogCoordinator};
use crate::store::AppState;
use std::str::FromStr;
use tauri::Emitter;

/// 获取故障转移队列
#[tauri::command]
pub async fn get_failover_queue(
    state: tauri::State<'_, AppState>,
    app_type: String,
) -> Result<Vec<FailoverQueueItem>, String> {
    if app_type == AppType::Pi.as_str() {
        let _guard = state
            .proxy_service
            .lock_switch_for_app(AppType::Pi.as_str())
            .await;
        return PiCatalogCoordinator::failover_queue_with_admission(state.inner())
            .map_err(|error| error.to_string());
    }
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
    if app_type == AppType::Pi.as_str() {
        let _guard = state
            .proxy_service
            .lock_switch_for_app(AppType::Pi.as_str())
            .await;
        let providers = state
            .db
            .get_available_providers_for_failover(&app_type)
            .map_err(|error| error.to_string())?;
        return filter_proxyable_pi_providers(state.inner(), providers);
    }
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
        let guard = state
            .proxy_service
            .lock_switch_for_app(AppType::Pi.as_str())
            .await;
        set_pi_failover_membership_under_switch_guard(state.inner(), &guard, &provider_id, true)?;
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
        let guard = state
            .proxy_service
            .lock_switch_for_app(AppType::Pi.as_str())
            .await;
        set_pi_failover_membership_under_switch_guard(state.inner(), &guard, &provider_id, false)?;
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
    let mut switched_defaults_receipt = None;
    let selected_provider = if enabled {
        // Persisted queue membership is the ownership boundary. An empty
        // runtime-admission view can also mean that an existing P1 was fenced
        // by native OAuth or external drift, so it must never trigger seeding.
        let current = PiCatalogCoordinator::current_state_under_switch_guard(state, &guard)
            .map_err(|error| error.to_string())?;
        let queue = PiCatalogCoordinator::failover_queue_with_admission(state)
            .map_err(|error| error.to_string())?;
        let primary = if let Some(primary) = queue.first() {
            if primary.gateway_ready != Some(true) {
                return Err(format!(
                    "Pi failover queue primary '{}' is not gateway-ready; remove or repair it before enabling automatic failover",
                    primary.provider_id
                ));
            }
            primary.provider_id.clone()
        } else {
            let seed_provider = previous_provider.clone().ok_or_else(|| {
                "Pi has no managed current provider and the failover queue is empty".to_string()
            })?;
            if !PiCatalogCoordinator::gateway_admission_ready(state, &seed_provider)
                .map_err(|error| error.to_string())?
            {
                return Err(
                    "Pi's current provider is direct-only and cannot seed failover".to_string(),
                );
            }
            set_pi_failover_membership_under_switch_guard(state, &guard, &seed_provider, true)?;
            auto_added = Some(seed_provider.clone());
            seed_provider
        };
        let selection_needs_repair = current.managed_provider_id.as_deref() != Some(&primary)
            || current.active_route == PiActiveRoute::Unavailable;
        if previous_provider.as_deref() != Some(primary.as_str()) || selection_needs_repair {
            match set_pi_default_under_switch_guard(state, &guard, &primary) {
                Ok(receipt) => switched_defaults_receipt = Some(receipt),
                Err(error) => {
                    let queue_rollback =
                        rollback_auto_added_pi_failover_member(state, &guard, auto_added.take());
                    return Err(with_pi_failover_rollback(error, queue_rollback));
                }
            }
        }
        Some(primary)
    } else {
        previous_provider.clone()
    };

    let mut next = previous_config.clone();
    next.auto_failover_enabled = enabled;
    let native_auth_owns_claim = !enabled
        && PiCatalogCoordinator::native_auth_owns_managed_claim(state)
            .map_err(|error| error.to_string())?;
    let epoch = state.proxy_service.begin_pi_catalog_mutation().await;
    if let Err(error) = crate::settings::update_pi_proxy_settings(next) {
        let primary_rollback = rollback_pi_failover_primary(
            state,
            &guard,
            switched_defaults_receipt.as_ref(),
            Some(epoch),
        )
        .await;
        let queue_rollback =
            rollback_auto_added_pi_failover_member(state, &guard, auto_added.take());
        return Err(with_pi_failover_rollbacks(
            error.to_string(),
            [primary_rollback, queue_rollback],
        ));
    }
    let reconcile = if native_auth_owns_claim {
        state
            .proxy_service
            .close_pi_runtime_at_epoch(epoch)
            .await
            .map(|_| Vec::new())
    } else {
        state
            .proxy_service
            .reconcile_pi_runtime_at_epoch(epoch)
            .await
    };
    if let Err(error) = reconcile {
        let settings_rollback = crate::settings::update_pi_proxy_settings(previous_config)
            .err()
            .map(|rollback_error| rollback_error.to_string());
        let primary_rollback =
            rollback_pi_failover_primary(state, &guard, switched_defaults_receipt.as_ref(), None)
                .await;
        let queue_rollback =
            rollback_auto_added_pi_failover_member(state, &guard, auto_added.take());
        return Err(with_pi_failover_rollbacks(
            format!("Pi failover preference changed but runtime publication failed: {error}"),
            [settings_rollback, primary_rollback, queue_rollback],
        ));
    }

    Ok(selected_provider)
}

fn set_pi_default_under_switch_guard(
    state: &AppState,
    guard: &tokio::sync::OwnedMutexGuard<()>,
    provider_id: &str,
) -> Result<PiNativeDefaultsReceipt, String> {
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
    .map_err(|error| error.to_string())?
    .into_native_defaults_receipt()
    .ok_or_else(|| "Pi default switch did not return a native rollback receipt".to_string())
}

async fn rollback_pi_failover_primary(
    state: &AppState,
    guard: &tokio::sync::OwnedMutexGuard<()>,
    defaults_receipt: Option<&PiNativeDefaultsReceipt>,
    pending_epoch: Option<u64>,
) -> Option<String> {
    let mut failures = Vec::new();
    if let Some(receipt) = defaults_receipt {
        if let Err(error) =
            PiCatalogCoordinator::rollback_native_defaults_under_switch_guard(state, guard, receipt)
        {
            failures.push(format!("native-default rollback failed: {error}"));
        }
    }

    let epoch = match pending_epoch {
        Some(epoch) => epoch,
        None => state.proxy_service.begin_pi_catalog_mutation().await,
    };
    if let Err(error) = state
        .proxy_service
        .reconcile_pi_runtime_at_epoch(epoch)
        .await
    {
        failures.push(format!("runtime rollback failed: {error}"));
    }
    (!failures.is_empty()).then(|| failures.join("; "))
}

fn with_pi_failover_rollback(error: String, rollback: Option<String>) -> String {
    match rollback {
        Some(rollback) => format!("{error}; {rollback}"),
        None => error,
    }
}

fn with_pi_failover_rollbacks<const N: usize>(
    error: String,
    rollbacks: [Option<String>; N],
) -> String {
    let failures = rollbacks.into_iter().flatten().collect::<Vec<_>>();
    if failures.is_empty() {
        error
    } else {
        format!("{error}; {}", failures.join("; "))
    }
}

fn set_pi_failover_membership_under_switch_guard(
    state: &AppState,
    guard: &tokio::sync::OwnedMutexGuard<()>,
    provider_id: &str,
    in_failover_queue: bool,
) -> Result<(), String> {
    PiCatalogCoordinator::apply_under_switch_guard(
        state,
        guard,
        crate::services::pi_catalog::PiCatalogMutation::SetFailoverMembership {
            provider_id: provider_id.to_string(),
            in_failover_queue,
        },
    )
    .map(|_| ())
    .map_err(|error| error.to_string())
}

fn rollback_auto_added_pi_failover_member(
    state: &AppState,
    guard: &tokio::sync::OwnedMutexGuard<()>,
    provider_id: Option<String>,
) -> Option<String> {
    provider_id.and_then(|provider_id| {
        set_pi_failover_membership_under_switch_guard(state, guard, &provider_id, false)
            .err()
            .map(|error| format!("queue rollback failed: {error}"))
    })
}

fn filter_proxyable_pi_providers(
    state: &AppState,
    providers: Vec<Provider>,
) -> Result<Vec<Provider>, String> {
    let provider_ids = providers
        .iter()
        .map(|provider| provider.id.clone())
        .collect::<Vec<_>>();
    let admission = PiCatalogCoordinator::gateway_admission_snapshot(state, &provider_ids)
        .map_err(|error| error.to_string())?;
    Ok(providers
        .into_iter()
        .filter(|provider| admission.get(&provider.id).copied().unwrap_or(false))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use crate::provider::ProviderMutationInput;
    use crate::services::pi_catalog::{PiCatalogCoordinator, PiCatalogMutation};
    use serde_json::{json, Value};
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
        let error = state
            .proxy_service
            .set_takeover_for_app("pi", false)
            .await
            .expect_err("takeover cannot be disabled while Pi failover owns queue P1");
        assert!(error.contains("disable Pi automatic failover"));
        set_pi_auto_failover_enabled_inner(&state, false)
            .await
            .map_err(crate::error::AppError::Message)?;
        state
            .proxy_service
            .set_takeover_for_app("pi", false)
            .await
            .map_err(crate::error::AppError::Message)?;
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn enabling_pi_failover_repairs_an_unavailable_current_model(
    ) -> Result<(), crate::error::AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        let mut settings = crate::settings::get_settings();
        settings.pi_config_dir = Some(temp.path().join("pi-agent").to_string_lossy().into_owned());
        settings.pi_takeover_enabled = false;
        crate::settings::update_settings(settings)?;

        let state = AppState::new(Arc::new(Database::memory()?));
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: managed_input("provider-a"),
                provider_key: "provider-a".to_string(),
                activate_if_first: true,
            },
        )?;
        let mut proxy_config = state.db.get_global_proxy_config().await?;
        proxy_config.listen_port = 0;
        state.db.update_global_proxy_config(proxy_config).await?;
        state
            .proxy_service
            .set_takeover_for_app("pi", true)
            .await
            .map_err(crate::error::AppError::Message)?;
        crate::pi_config::native_settings::set_pi_native_default_with_receipt(
            "provider-a",
            "missing-model",
        )?;

        set_pi_auto_failover_enabled_inner(&state, true)
            .await
            .map_err(crate::error::AppError::Message)?;

        let defaults = crate::pi_config::native_settings::read_pi_native_defaults()?;
        assert_eq!(defaults.default_provider.as_deref(), Some("provider-a"));
        assert_eq!(defaults.default_model.as_deref(), Some("provider-a-model"));
        assert!(crate::settings::get_pi_proxy_settings().auto_failover_enabled);
        set_pi_auto_failover_enabled_inner(&state, false)
            .await
            .map_err(crate::error::AppError::Message)?;
        state
            .proxy_service
            .set_takeover_for_app("pi", false)
            .await
            .map_err(crate::error::AppError::Message)?;
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn disabling_pi_failover_fences_runtime_after_native_oauth_takeover(
    ) -> Result<(), crate::error::AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        let pi_dir = temp.path().join("pi-agent");
        let mut settings = crate::settings::get_settings();
        settings.pi_config_dir = Some(pi_dir.to_string_lossy().into_owned());
        settings.pi_takeover_enabled = false;
        crate::settings::update_settings(settings)?;

        let state = AppState::new(Arc::new(Database::memory()?));
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: managed_input("provider"),
                provider_key: "provider".to_string(),
                activate_if_first: true,
            },
        )?;
        state.db.add_to_failover_queue("pi", "provider")?;
        let mut proxy_config = state.db.get_global_proxy_config().await?;
        proxy_config.listen_port = 0;
        state.db.update_global_proxy_config(proxy_config).await?;
        state
            .proxy_service
            .set_takeover_for_app("pi", true)
            .await
            .map_err(crate::error::AppError::Message)?;
        set_pi_auto_failover_enabled_inner(&state, true)
            .await
            .map_err(crate::error::AppError::Message)?;

        let oauth = json!({
            "api": "anthropic-messages",
            "baseUrl": "https://api.anthropic.com",
            "oauth": "radius",
            "models": [{"id": "claude"}]
        });
        std::fs::write(
            pi_dir.join("models.json"),
            serde_json::to_vec_pretty(&json!({
                "providers": {"provider": oauth.clone()}
            }))
            .expect("serialize Pi OAuth"),
        )
        .expect("replace managed projection");

        set_pi_auto_failover_enabled_inner(&state, false)
            .await
            .map_err(crate::error::AppError::Message)?;

        assert!(!crate::settings::get_pi_proxy_settings().auto_failover_enabled);
        assert_eq!(
            state
                .proxy_service
                .active_pi_gateway_projection("provider", "provider"),
            None,
            "native OAuth takeover must fence the local route"
        );
        let live: Value = serde_json::from_slice(
            &std::fs::read(pi_dir.join("models.json")).expect("read Pi models"),
        )
        .expect("parse Pi models");
        assert_eq!(live.pointer("/providers/provider"), Some(&oauth));
        state
            .proxy_service
            .set_takeover_for_app("pi", false)
            .await
            .map_err(crate::error::AppError::Message)?;
        assert!(!crate::settings::pi_takeover_enabled());
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_populated_queue_can_take_over_from_pi_native_login(
    ) -> Result<(), crate::error::AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        let pi_dir = temp.path().join("pi-agent");
        let mut settings = crate::settings::get_settings();
        settings.pi_config_dir = Some(pi_dir.to_string_lossy().into_owned());
        settings.pi_takeover_enabled = false;
        crate::settings::update_settings(settings)?;
        std::fs::create_dir_all(&pi_dir).expect("Pi directory");
        std::fs::write(
            pi_dir.join("models.json"),
            serde_json::to_vec_pretty(&json!({
                "providers": {
                    "anthropic": {
                        "api": "anthropic-messages",
                        "baseUrl": "https://api.anthropic.com",
                        "oauth": "radius",
                        "models": [{"id": "claude"}]
                    }
                }
            }))
            .expect("serialize Pi native provider"),
        )
        .expect("write Pi native provider");
        crate::pi_config::native_settings::set_pi_native_default_with_receipt(
            "anthropic",
            "claude",
        )?;

        let state = AppState::new(Arc::new(Database::memory()?));
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: managed_input("managed"),
                provider_key: "managed".to_string(),
                activate_if_first: true,
            },
        )?;
        state.db.add_to_failover_queue("pi", "managed")?;
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

        assert_eq!(selected.as_deref(), Some("managed"));
        assert_eq!(
            PiCatalogCoordinator::current_native_provider(&state)?.as_deref(),
            Some("managed")
        );
        set_pi_auto_failover_enabled_inner(&state, false)
            .await
            .map_err(crate::error::AppError::Message)?;
        state
            .proxy_service
            .set_takeover_for_app("pi", false)
            .await
            .map_err(crate::error::AppError::Message)?;
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn enabling_pi_failover_never_seeds_over_a_persisted_ineligible_primary(
    ) -> Result<(), crate::error::AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        let mut settings = crate::settings::get_settings();
        settings.pi_config_dir = Some(temp.path().join("pi-agent").to_string_lossy().into_owned());
        settings.pi_takeover_enabled = false;
        crate::settings::update_settings(settings)?;

        let state = AppState::new(Arc::new(Database::memory()?));
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: managed_input("a-current"),
                provider_key: "a-current".to_string(),
                activate_if_first: true,
            },
        )?;
        let mut direct_only = managed_input("z-blocked");
        direct_only.settings_config["api"] = json!("future-wire-v9");
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: direct_only,
                provider_key: "z-blocked".to_string(),
                activate_if_first: false,
            },
        )?;
        // Simulate a queue membership persisted by an older release before
        // gateway admission became authoritative.
        state.db.add_to_failover_queue("pi", "z-blocked")?;

        let mut proxy_config = state.db.get_global_proxy_config().await?;
        proxy_config.listen_port = 0;
        state.db.update_global_proxy_config(proxy_config).await?;
        state
            .proxy_service
            .set_takeover_for_app("pi", true)
            .await
            .map_err(crate::error::AppError::Message)?;

        let error = set_pi_auto_failover_enabled_inner(&state, true)
            .await
            .expect_err("a blocked persisted P1 must not be replaced by the current provider");

        assert!(error.contains("primary 'z-blocked' is not gateway-ready"));
        assert!(state.db.is_in_failover_queue("pi", "z-blocked")?);
        assert!(!state.db.is_in_failover_queue("pi", "a-current")?);
        assert!(!crate::settings::get_pi_proxy_settings().auto_failover_enabled);

        state
            .proxy_service
            .set_takeover_for_app("pi", false)
            .await
            .map_err(crate::error::AppError::Message)?;
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn failover_primary_rollback_restores_the_exact_previous_model(
    ) -> Result<(), crate::error::AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        let mut settings = crate::settings::get_settings();
        settings.pi_config_dir = Some(temp.path().join("pi-agent").to_string_lossy().into_owned());
        settings.pi_takeover_enabled = false;
        crate::settings::update_settings(settings)?;
        let state = AppState::new(Arc::new(Database::memory()?));
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: managed_input("primary"),
                provider_key: "primary".to_string(),
                activate_if_first: true,
            },
        )?;
        let mut previous = managed_input("previous");
        previous.settings_config["models"] = json!([
            {"id": "previous-first", "name": "First"},
            {"id": "previous-second", "name": "Second"}
        ]);
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: previous,
                provider_key: "previous".to_string(),
                activate_if_first: false,
            },
        )?;
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::SetDefault {
                provider_id: "previous".to_string(),
                model_id: "previous-second".to_string(),
            },
        )?;
        let guard = state.proxy_service.lock_switch_for_app("pi").await;
        let receipt = set_pi_default_under_switch_guard(&state, &guard, "primary")
            .expect("switch to failover P1");

        let rollback = rollback_pi_failover_primary(&state, &guard, Some(&receipt), None).await;

        assert_eq!(rollback, None);
        let defaults = crate::pi_config::native_settings::read_pi_native_defaults()?;
        assert_eq!(defaults.default_provider.as_deref(), Some("previous"));
        assert_eq!(defaults.default_model.as_deref(), Some("previous-second"));
        assert_eq!(
            PiCatalogCoordinator::current_native_provider(&state)?.as_deref(),
            Some("previous")
        );
        Ok(())
    }
}

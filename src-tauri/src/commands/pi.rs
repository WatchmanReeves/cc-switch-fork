use crate::pi_config::model::PiNativeDiagnostic;
use crate::pi_config::native_settings::{read_pi_native_defaults, PiNativeDefaults};
use crate::services::pi_catalog::{PiCatalogCoordinator, PiCatalogMutation};
use crate::session_manager::providers::pi::PiSessionDiscovery;
use crate::store::AppState;
use tauri::State;

/// Read-only diagnostics come exclusively from the Pre-C certified inspection
/// service. This command does not infer manageability or gateway status.
#[tauri::command]
pub(crate) fn get_pi_native_catalog(
    state: State<'_, AppState>,
) -> Result<Vec<PiNativeDiagnostic>, String> {
    PiCatalogCoordinator::inspect_native(state.inner()).map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn import_pi_native_provider(
    state: State<'_, AppState>,
    #[allow(non_snake_case)] providerKey: String,
    #[allow(non_snake_case)] expectedFingerprint: String,
) -> Result<String, String> {
    let result = PiCatalogCoordinator::apply(
        state.inner(),
        PiCatalogMutation::ImportNative {
            provider_key: providerKey,
            expected_fingerprint: expectedFingerprint,
        },
    )
    .map_err(|error| error.to_string())?;
    result
        .provider_id
        .ok_or_else(|| "Pi import did not return a provider id".to_string())
}

#[tauri::command]
pub(crate) fn set_pi_default_model(
    state: State<'_, AppState>,
    #[allow(non_snake_case)] providerId: String,
    #[allow(non_snake_case)] modelId: String,
) -> Result<bool, String> {
    PiCatalogCoordinator::apply(
        state.inner(),
        PiCatalogMutation::SetDefault {
            provider_id: providerId,
            model_id: modelId,
        },
    )
    .map(|_| true)
    .map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn get_pi_native_defaults() -> Result<PiNativeDefaults, String> {
    read_pi_native_defaults().map_err(|error| error.to_string())
}

#[tauri::command]
pub(crate) fn get_pi_session_discovery() -> PiSessionDiscovery {
    crate::session_manager::providers::pi::session_discovery()
}

/// Explicitly rotate the device-local gateway bearer and republish every
/// managed Pi projection. Existing Pi processes must restart because they
/// retain the previous projected credential in memory.
#[tauri::command]
pub(crate) async fn reset_pi_gateway_credential(
    state: State<'_, AppState>,
) -> Result<bool, String> {
    state
        .proxy_service
        .rotate_pi_gateway_token()
        .await
        .map(|()| true)
        .map_err(|error| error.to_string())
}

//! Ordered mutations for Pi's managed provider catalog.
//!
//! SQLite is the managed aggregate authority, `pi_provider_projections` owns
//! exact keys in Pi's shared `models.json`, and Pi's `settings.json` owns the
//! native default. Every public mutation acquires the same Pi switch boundary;
//! callers must not compose the database and native-file primitives directly.

use crate::app_config::AppType;
use crate::database::{
    FailoverQueueItem, NewEndpoint, NewProviderAggregate, PiProviderProjection, ProviderKey,
    ProviderRowUpdate,
};
use crate::error::AppError;
use crate::pi_config::document::{
    snapshot_pi_provider_values, PiProviderPatchReceipt, PiProviderValuesSnapshot,
};
use crate::pi_config::gateway::{
    assess_composition_for_runtime, parse_pi_gateway_endpoint, PiGatewayCapability,
};
use crate::pi_config::model::{
    effective_pi_model, validate_pi_managed_provider, value_uses_pi_owned_auth, PiGatewayStatus,
    PiManagedProviderConfig, PiManagementStatus,
};
use crate::pi_config::native::{
    apply_managed_pi_provider_patch_with_receipt, compose_managed_pi_provider, get_pi_models_path,
    inspect_pi_native_entry, is_pi_builtin_provider_key, is_pi_owned_native_entry,
    native_entry_contains_model, pi_owns_native_provider_value, PiNativeInspectionService,
};
use crate::pi_config::native_settings::{
    read_pi_native_defaults, set_pi_native_default_with_receipt, PiNativeDefaults,
    PiNativeDefaultsReceipt, PiNativeDefaultsRollback,
};
use crate::provider::{ProviderAggregate, ProviderMutationInput};
use crate::settings;
use crate::store::AppState;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

const PI_APP: &str = "pi";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PiCatalogAuthority {
    Published,
    PreviousRestored,
    MutatedDatabaseAuthoritative,
    ProjectionPending,
}

impl PiCatalogAuthority {
    fn as_str(self) -> &'static str {
        match self {
            Self::Published => "published",
            Self::PreviousRestored => "previous_restored",
            Self::MutatedDatabaseAuthoritative => "mutated_database_authoritative",
            Self::ProjectionPending => "projection_pending",
        }
    }
}

#[derive(Debug)]
pub(crate) enum PiCatalogMutation {
    CreateProvider {
        input: ProviderMutationInput,
        provider_key: String,
        activate_if_first: bool,
    },
    UpdateProvider {
        input: ProviderMutationInput,
    },
    DeleteProvider {
        provider_id: String,
    },
    AddEndpoint {
        provider_id: String,
        url: String,
    },
    RemoveEndpoint {
        provider_id: String,
        url: String,
    },
    ImportNative {
        provider_key: String,
        expected_fingerprint: String,
    },
    SetDefault {
        provider_id: String,
        model_id: String,
    },
    SetFailoverMembership {
        provider_id: String,
        in_failover_queue: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiCatalogMutationResult {
    pub authority: PiCatalogAuthority,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(skip)]
    native_defaults_receipt: Option<PiNativeDefaultsReceipt>,
    #[serde(skip)]
    native_patch_receipt: Option<PiProviderPatchReceipt>,
    #[serde(skip)]
    native_fingerprint_preconditions: IndexMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PiCurrentOwnership {
    Managed,
    PiNative,
    External,
    Unconfigured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PiActiveRoute {
    Gateway,
    Direct,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PiCurrentRouteReason {
    Unconfigured,
    NativeDirect,
    ManagedGateway,
    ManagedDirect,
    ManagedProjectionMismatch,
    FailoverPrimaryMismatch,
    NativeCatalogUnavailable,
    SelectionUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiCurrentState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub managed_provider_id: Option<String>,
    pub ownership: PiCurrentOwnership,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gateway_status: Option<PiGatewayStatus>,
    pub active_route: PiActiveRoute,
    pub route_reason: PiCurrentRouteReason,
}

pub(crate) struct PiCatalogCoordinator;

struct PiCatalogSnapshot {
    aggregates: IndexMap<String, ProviderAggregate>,
    projections: Vec<PiProviderProjection>,
    db_current: Option<String>,
    // Capture validates the complete shared projection before any DB write.
    // Exact compensation itself uses per-operation before/attempted receipts.
    native: PiProviderValuesSnapshot,
}

impl PiCatalogCoordinator {
    pub(crate) fn update_route_order(
        state: &AppState,
        updates: Vec<(ProviderKey, usize)>,
    ) -> Result<bool, AppError> {
        let _switch_guard = futures::executor::block_on(
            state
                .proxy_service
                .lock_switch_for_app(AppType::Pi.as_str()),
        );
        let aggregates = state.db.get_all_provider_aggregates(PI_APP)?;
        for (key, _) in &updates {
            if !aggregates.contains_key(key.id()) {
                return Err(AppError::NotFound(format!(
                    "Pi provider '{}' cannot be sorted because it does not exist",
                    key.id()
                )));
            }
        }
        Self::ensure_auto_failover_reorder_preserves_primary(state, &updates)?;
        let projections = state
            .db
            .get_pi_projection_manifest()?
            .into_values()
            .collect::<Vec<_>>();
        let db_current = state.db.get_current_provider(PI_APP)?;

        state.db.update_provider_sort_index(&updates)?;
        if let Err(error) =
            futures::executor::block_on(state.proxy_service.publish_pi_runtime_order())
        {
            let rollback = state.db.restore_pi_catalog_snapshot(
                &aggregates,
                &projections,
                db_current.as_deref(),
            );
            return Err(AppError::Config(format!(
                "failed to publish sorted Pi runtime: {error}; DB rollback={}",
                rollback
                    .err()
                    .map_or_else(|| "ok".to_string(), |value| value.to_string())
            )));
        }
        Ok(true)
    }

    pub(crate) fn apply(
        state: &AppState,
        mutation: PiCatalogMutation,
    ) -> Result<PiCatalogMutationResult, AppError> {
        let switch_guard = futures::executor::block_on(
            state
                .proxy_service
                .lock_switch_for_app(AppType::Pi.as_str()),
        );
        Self::apply_under_switch_guard(state, &switch_guard, mutation)
    }

    /// Compose a Pi catalog mutation with a caller-owned switch boundary.
    ///
    /// Multi-step control-plane operations (for example selecting failover P1
    /// and enabling its runtime policy) use this entry point so they do not
    /// release the shared ownership boundary or deadlock by acquiring it twice.
    pub(crate) fn apply_under_switch_guard(
        state: &AppState,
        switch_guard: &tokio::sync::OwnedMutexGuard<()>,
        mutation: PiCatalogMutation,
    ) -> Result<PiCatalogMutationResult, AppError> {
        let may_fence_for_native_auth = matches!(
            &mutation,
            PiCatalogMutation::SetFailoverMembership {
                in_failover_queue: false,
                ..
            }
        );
        let additional_native_key = match &mutation {
            PiCatalogMutation::CreateProvider { provider_key, .. }
            | PiCatalogMutation::ImportNative { provider_key, .. } => Some(provider_key.clone()),
            _ => None,
        };
        Self::run_with_runtime_reconcile_locked(
            state,
            switch_guard,
            additional_native_key.as_deref(),
            may_fence_for_native_auth,
            || match mutation {
                PiCatalogMutation::CreateProvider {
                    input,
                    provider_key,
                    activate_if_first,
                } => Self::create(state, input, provider_key, activate_if_first),
                PiCatalogMutation::UpdateProvider { input } => Self::update(state, input),
                PiCatalogMutation::DeleteProvider { provider_id } => {
                    Self::delete(state, &provider_id)
                }
                PiCatalogMutation::AddEndpoint { provider_id, url } => {
                    Self::add_endpoint(state, &provider_id, &url)
                }
                PiCatalogMutation::RemoveEndpoint { provider_id, url } => {
                    Self::remove_endpoint(state, &provider_id, &url)
                }
                PiCatalogMutation::ImportNative {
                    provider_key,
                    expected_fingerprint,
                } => Self::import_native(state, &provider_key, &expected_fingerprint),
                PiCatalogMutation::SetDefault {
                    provider_id,
                    model_id,
                } => Self::set_default(state, &provider_id, &model_id),
                PiCatalogMutation::SetFailoverMembership {
                    provider_id,
                    in_failover_queue,
                } => Self::set_failover_membership(state, &provider_id, in_failover_queue),
            },
        )
    }

    pub(crate) fn rollback_native_defaults_under_switch_guard(
        state: &AppState,
        _switch_guard: &tokio::sync::OwnedMutexGuard<()>,
        receipt: &PiNativeDefaultsReceipt,
    ) -> Result<(), AppError> {
        let outcome = receipt.rollback()?;
        Self::reconcile_current_indexes_from_native(state)?;
        match outcome {
            PiNativeDefaultsRollback::Restored => Ok(()),
            PiNativeDefaultsRollback::Superseded => Err(authority_error(
                PiCatalogAuthority::ProjectionPending,
                "Pi changed its native default while automatic failover was being updated; the newer Pi selection was preserved",
            )),
        }
    }

    /// Reconcile portable provider rows with this device's exact-key ledger.
    ///
    /// SQL/WebDAV/S3 exports intentionally omit device-local projection rows.
    /// Imported Pi providers use their immutable provider id as their native
    /// key. A missing native key can therefore be claimed and published
    /// without guessing; an existing unclaimed key is never overwritten.
    pub(crate) fn reconcile_portable_import(state: &AppState) -> Result<(), AppError> {
        Self::run_with_runtime_reconcile(state, None, || {
            let models_path = get_pi_models_path()?;
            let (native_defaults_receipt, native_patch_receipt) =
                Self::reconcile_portable_catalog_at(
                    state,
                    &models_path,
                    |provider_id, provider_key, config| {
                        state.proxy_service.project_pi_provider_value(
                            provider_id,
                            provider_key,
                            config,
                        )
                    },
                )?;
            Ok(success(None)
                .with_native_defaults_receipt(native_defaults_receipt)
                .with_native_patch_receipt(native_patch_receipt))
        })
        .map(|_| ())
    }

    fn run_with_runtime_reconcile(
        state: &AppState,
        additional_native_key: Option<&str>,
        operation: impl FnOnce() -> Result<PiCatalogMutationResult, AppError>,
    ) -> Result<PiCatalogMutationResult, AppError> {
        let switch_guard = futures::executor::block_on(
            state
                .proxy_service
                .lock_switch_for_app(AppType::Pi.as_str()),
        );
        Self::run_with_runtime_reconcile_locked(
            state,
            &switch_guard,
            additional_native_key,
            false,
            operation,
        )
    }

    fn run_with_runtime_reconcile_locked(
        state: &AppState,
        _switch_guard: &tokio::sync::OwnedMutexGuard<()>,
        additional_native_key: Option<&str>,
        may_fence_for_native_auth: bool,
        operation: impl FnOnce() -> Result<PiCatalogMutationResult, AppError>,
    ) -> Result<PiCatalogMutationResult, AppError> {
        Self::reconcile_current_indexes_from_native(state)?;
        let snapshot = PiCatalogSnapshot::capture(state, additional_native_key)?;
        let catalog_epoch =
            futures::executor::block_on(state.proxy_service.begin_pi_catalog_mutation());
        let result = operation();
        let native_defaults_receipt = result
            .as_ref()
            .ok()
            .and_then(|result| result.native_defaults_receipt.as_ref())
            .cloned();
        let native_patch_receipt = result
            .as_ref()
            .ok()
            .and_then(|result| result.native_patch_receipt.as_ref())
            .cloned();
        let native_fingerprint_preconditions = result
            .as_ref()
            .ok()
            .map(|result| result.native_fingerprint_preconditions.clone())
            .unwrap_or_default();
        let mut expected_native = snapshot.native.clone();
        if let Some(receipt) = native_patch_receipt.as_ref() {
            for (provider_key, attempted) in receipt.attempted_values() {
                expected_native
                    .values
                    .insert(provider_key.clone(), attempted.clone());
            }
        }
        let mut ownership_check_error = None;
        if may_fence_for_native_auth && result.is_ok() {
            match snapshot_contains_pi_owned_claim(state, &expected_native) {
                Ok(true)
                    if futures::executor::block_on(
                        state.proxy_service.close_pi_runtime_at_epoch(catalog_epoch),
                    )
                    .is_ok() =>
                {
                    // Removing stale queue ownership is a recovery operation.
                    // Pi's native OAuth value remains untouched and local
                    // admission stays closed until the user switches back to
                    // a managed provider.
                    return result;
                }
                Ok(_) => {}
                Err(error) => ownership_check_error = Some(error),
            }
        }
        let reconcile = match ownership_check_error {
            Some(error) => Err(error),
            None => futures::executor::block_on(
                state
                    .proxy_service
                    .reconcile_pi_runtime_at_epoch_with_native_claim_precondition(
                        catalog_epoch,
                        Some(&expected_native),
                        (!native_fingerprint_preconditions.is_empty())
                            .then_some(&native_fingerprint_preconditions),
                    ),
            ),
        };
        match (result, reconcile) {
            (Ok(result), Ok(_)) => Ok(result),
            (Ok(_), Err(error)) => {
                if let Err(rollback_error) = snapshot.restore(
                    state,
                    native_defaults_receipt.as_ref(),
                    native_patch_receipt.as_ref(),
                ) {
                    let _ = futures::executor::block_on(
                        state.proxy_service.close_pi_runtime_at_epoch(catalog_epoch),
                    );
                    return Err(authority_error(
                        PiCatalogAuthority::ProjectionPending,
                        format!(
                            "Pi catalog runtime publication failed ({error}); snapshot rollback failed ({rollback_error})"
                        ),
                    ));
                }
                match futures::executor::block_on(
                    state
                        .proxy_service
                        .reconcile_pi_runtime_at_epoch_with_native_precondition(
                            catalog_epoch,
                            Some(&snapshot.native),
                        ),
                ) {
                    Ok(_) => Err(authority_error(
                        PiCatalogAuthority::PreviousRestored,
                        format!(
                            "Pi catalog runtime publication failed and the previous catalog was restored: {error}"
                        ),
                    )),
                    Err(rollback_error) => {
                        let _ = futures::executor::block_on(
                            state.proxy_service.close_pi_runtime_at_epoch(catalog_epoch),
                        );
                        Err(authority_error(
                            PiCatalogAuthority::ProjectionPending,
                            format!(
                                "Pi catalog runtime publication failed ({error}); the database snapshot was restored but runtime recovery failed ({rollback_error})"
                            ),
                        ))
                    }
                }
            }
            (Err(error), Ok(_)) => Err(error),
            (Err(error), Err(reconcile_error)) => {
                let _ = futures::executor::block_on(
                    state.proxy_service.close_pi_runtime_at_epoch(catalog_epoch),
                );
                Err(authority_error(
                    PiCatalogAuthority::ProjectionPending,
                    format!(
                        "{error}; additionally failed to reconcile Pi admission: {reconcile_error}"
                    ),
                ))
            }
        }
    }

    fn reconcile_portable_catalog_at(
        state: &AppState,
        models_path: &std::path::Path,
        mut project: impl FnMut(&str, &str, &PiManagedProviderConfig) -> Result<Value, AppError>,
    ) -> Result<
        (
            Option<PiNativeDefaultsReceipt>,
            Option<PiProviderPatchReceipt>,
        ),
        AppError,
    > {
        let providers = state.db.get_all_providers(PI_APP)?;
        let manifest = state.db.get_pi_projection_manifest()?;
        let claimed_keys = manifest
            .values()
            .map(|projection| {
                (
                    projection.provider_key.clone(),
                    projection.provider_id.clone(),
                )
            })
            .collect::<BTreeMap<_, _>>();

        struct PlannedProjection {
            provider_id: String,
            provider_key: String,
            config: PiManagedProviderConfig,
            projected: Value,
            needs_claim: bool,
        }

        let mut plans = Vec::with_capacity(providers.len());
        let mut planned_key_owners = BTreeMap::<String, String>::new();
        for provider in providers.values() {
            let config: PiManagedProviderConfig =
                serde_json::from_value(provider.settings_config.clone()).map_err(|error| {
                    AppError::InvalidInput(format!(
                        "imported Pi provider '{}' cannot be decoded: {error}",
                        provider.id
                    ))
                })?;
            validate_pi_managed_provider(&config).map_err(|error| {
                AppError::InvalidInput(format!(
                    "imported Pi provider '{}' is invalid: {error}",
                    provider.id
                ))
            })?;
            let (provider_key, needs_claim) = match manifest.get(&provider.id) {
                Some(projection) => (projection.provider_key.clone(), false),
                None => (non_empty_native_key(&provider.id)?.to_string(), true),
            };
            ensure_managed_native_key(&provider_key)?;
            if let Some(owner) =
                planned_key_owners.insert(provider_key.clone(), provider.id.clone())
            {
                if owner != provider.id {
                    return Err(AppError::Conflict(format!(
                        "imported Pi providers '{owner}' and '{}' normalize to the same native key '{provider_key}'",
                        provider.id
                    )));
                }
            }
            if let Some(owner) = claimed_keys.get(&provider_key) {
                if owner != &provider.id {
                    return Err(AppError::Conflict(format!(
                        "cannot project imported Pi provider '{}': native key '{}' is owned by '{}'",
                        provider.id, provider_key, owner
                    )));
                }
            }
            let projected = project(&provider.id, &provider_key, &config)?;
            plans.push(PlannedProjection {
                provider_id: provider.id.clone(),
                provider_key,
                config,
                projected,
                needs_claim,
            });
        }

        let before_file = snapshot_pi_provider_values(
            models_path,
            plans.iter().map(|plan| plan.provider_key.clone()),
        )?;
        for plan in plans.iter().filter(|plan| plan.needs_claim) {
            if before_file
                .values
                .get(&plan.provider_key)
                .and_then(Option::as_ref)
                .is_some()
            {
                return Err(AppError::Conflict(format!(
                    "cannot project imported Pi provider '{}': unclaimed native key '{}' already exists",
                    plan.provider_id, plan.provider_key
                )));
            }
        }
        // Native defaults are another authority input to the reconciliation
        // plan. Read and validate them before changing either the exact-key
        // ownership ledger or models.json, so an unreadable settings.json is a
        // fail-before-write error rather than a partially compensated publish.
        let previous_defaults = read_pi_native_defaults()?;

        // A portable import can leave device-local exact-key claims behind
        // (for example, when the restore implementation preserves local-only
        // tables). Claims whose providers disappeared must be released before
        // publishing a runtime, otherwise the manifest and provider aggregate
        // sets can never converge. Their native values remain untouched and
        // become user-owned; without a retained provider aggregate there is no
        // safe expected value with which to delete them.
        let orphaned_claims = manifest
            .values()
            .filter(|projection| !providers.contains_key(&projection.provider_id))
            .map(|projection| {
                (
                    projection.provider_id.clone(),
                    projection.provider_key.clone(),
                )
            })
            .collect::<Vec<_>>();
        let mut released_claims = Vec::new();
        for (provider_id, provider_key) in &orphaned_claims {
            match state.db.delete_pi_projection_key(provider_id, provider_key) {
                Ok(true) => released_claims.push((provider_id.clone(), provider_key.clone())),
                Ok(false) => {
                    return Err(compensate_portable_projection_ledger(
                        state,
                        &[],
                        &released_claims,
                        AppError::Conflict(format!(
                            "Pi projection claim '{provider_id}' disappeared during import reconciliation"
                        )),
                    ));
                }
                Err(error) => {
                    return Err(compensate_portable_projection_ledger(
                        state,
                        &[],
                        &released_claims,
                        error,
                    ));
                }
            }
        }

        let mut newly_claimed = Vec::new();
        for plan in plans.iter().filter(|plan| plan.needs_claim) {
            match state
                .db
                .claim_pi_projection_key(&plan.provider_id, &plan.provider_key)
            {
                Ok(_) => newly_claimed.push((plan.provider_id.clone(), plan.provider_key.clone())),
                Err(error) => {
                    return Err(compensate_portable_projection_ledger(
                        state,
                        &newly_claimed,
                        &released_claims,
                        error,
                    ));
                }
            }
        }

        let patch = plans
            .iter()
            .map(|plan| (plan.provider_key.clone(), Some(plan.projected.clone())))
            .collect::<IndexMap<_, _>>();
        let native_patch_receipt = if patch.is_empty() {
            None
        } else {
            match apply_managed_pi_provider_patch_with_receipt(models_path, &before_file, &patch) {
                Ok(receipt) => Some(receipt),
                Err(error) => {
                    return Err(compensate_portable_projection_ledger(
                        state,
                        &newly_claimed,
                        &released_claims,
                        error,
                    ));
                }
            }
        };

        let native_selection = match (
            previous_defaults.default_provider.as_deref(),
            previous_defaults.default_model.as_deref(),
        ) {
            (Some(provider_key), Some(model_id)) => plans
                .iter()
                .find(|plan| {
                    plan.provider_key == provider_key
                        && effective_pi_model(&plan.config, model_id).is_ok()
                })
                .map(|plan| (plan.provider_id.clone(), model_id.to_string())),
            _ => None,
        };
        // Only use a DB/local fallback when Pi has no native selection at all.
        // A partial, invalid, or unowned native default is still real state and
        // must be surfaced rather than silently overwritten during import.
        let current_selection = if native_selection.is_some() {
            native_selection
        } else if previous_defaults.default_provider.is_none()
            && previous_defaults.default_model.is_none()
        {
            crate::settings::get_effective_current_provider(&state.db, &AppType::Pi)?
                .and_then(|provider_id| plans.iter().find(|plan| plan.provider_id == provider_id))
                .map(|plan| {
                    (
                        plan.provider_id.clone(),
                        plan.config
                            .models
                            .first()
                            .expect("validated Pi provider has at least one model")
                            .id
                            .clone(),
                    )
                })
        } else {
            None
        };

        let mut native_defaults_receipt = None;
        if let Some((current_provider, model_id)) = current_selection {
            match Self::set_default(state, &current_provider, &model_id) {
                Ok(result) => native_defaults_receipt = result.native_defaults_receipt,
                Err(error) => {
                    let file_restored = native_patch_receipt
                        .as_ref()
                        .map_or(Ok(()), PiProviderPatchReceipt::rollback);
                    let claims_restored = compensate_portable_projection_ledger(
                        state,
                        &newly_claimed,
                        &released_claims,
                        error,
                    );
                    if file_restored.is_err() {
                        return Err(authority_error(
                            PiCatalogAuthority::ProjectionPending,
                            format!(
                                "{claims_restored}; imported Pi default compensation was incomplete"
                            ),
                        ));
                    }
                    return Err(claims_restored);
                }
            }
        }
        Ok((native_defaults_receipt, native_patch_receipt))
    }

    pub(crate) fn inspect_native(
        state: &AppState,
    ) -> Result<Vec<crate::pi_config::model::PiNativeDiagnostic>, AppError> {
        PiNativeInspectionService::inspect_current(&managed_claims(state)?)
    }

    /// Resolve the provider that Pi itself will start with.
    ///
    /// `settings.json` is the live authority. The device-local and SQLite
    /// current markers are compensation/indexing aids and must never make the
    /// UI claim that a different provider is active after an external Pi edit.
    pub(crate) fn current_native_provider(state: &AppState) -> Result<Option<String>, AppError> {
        let defaults = read_pi_native_defaults()?;
        resolve_native_current_provider(state, &defaults)
    }

    pub(crate) fn current_state_under_switch_guard(
        state: &AppState,
        _switch_guard: &tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<PiCurrentState, AppError> {
        let defaults = read_pi_native_defaults()?;
        let (Some(provider_key), Some(model_id)) = (
            defaults.default_provider.clone(),
            defaults.default_model.clone(),
        ) else {
            return Ok(PiCurrentState {
                provider_key: defaults.default_provider,
                model_id: defaults.default_model,
                managed_provider_id: None,
                ownership: PiCurrentOwnership::Unconfigured,
                gateway_status: None,
                active_route: PiActiveRoute::Unavailable,
                route_reason: PiCurrentRouteReason::Unconfigured,
            });
        };

        let models_path = get_pi_models_path()?;
        let is_builtin = is_pi_builtin_provider_key(&provider_key);
        let inspection = if is_builtin {
            None
        } else {
            inspect_pi_native_entry(&models_path, &provider_key, &managed_claims(state)?)?
        };
        let pi_owned = is_builtin || inspection.as_ref().is_some_and(is_pi_owned_native_entry);
        if pi_owned {
            if is_builtin {
                return Ok(PiCurrentState {
                    provider_key: Some(provider_key),
                    model_id: Some(model_id),
                    managed_provider_id: None,
                    ownership: PiCurrentOwnership::PiNative,
                    gateway_status: None,
                    active_route: PiActiveRoute::Direct,
                    route_reason: PiCurrentRouteReason::NativeCatalogUnavailable,
                });
            }
            let selection_available = inspection
                .as_ref()
                .is_some_and(|entry| native_entry_contains_model(entry, &model_id));
            return Ok(PiCurrentState {
                provider_key: Some(provider_key),
                model_id: Some(model_id),
                managed_provider_id: None,
                ownership: PiCurrentOwnership::PiNative,
                gateway_status: None,
                active_route: if selection_available {
                    PiActiveRoute::Direct
                } else {
                    PiActiveRoute::Unavailable
                },
                route_reason: if selection_available {
                    PiCurrentRouteReason::NativeDirect
                } else {
                    PiCurrentRouteReason::SelectionUnavailable
                },
            });
        }

        let Some(projection) = state.db.get_pi_projection_for_key(&provider_key)? else {
            let selection_available = inspection
                .as_ref()
                .is_some_and(|entry| native_entry_contains_model(entry, &model_id));
            return Ok(PiCurrentState {
                provider_key: Some(provider_key.clone()),
                model_id: Some(model_id),
                managed_provider_id: None,
                ownership: PiCurrentOwnership::External,
                gateway_status: None,
                active_route: if selection_available {
                    PiActiveRoute::Direct
                } else {
                    PiActiveRoute::Unavailable
                },
                route_reason: if selection_available {
                    PiCurrentRouteReason::NativeDirect
                } else {
                    PiCurrentRouteReason::SelectionUnavailable
                },
            });
        };

        let aggregate = ensure_managed_provider(state, &projection.provider_id)?;
        let config: PiManagedProviderConfig =
            serde_json::from_value(aggregate.provider.settings_config).map_err(|error| {
                AppError::Config(format!(
                    "managed Pi provider '{}' is invalid: {error}",
                    projection.provider_id
                ))
            })?;
        let gateway_status = gateway_status_for_config(&provider_key, &config)?;
        if effective_pi_model(&config, &model_id).is_err() {
            return Ok(PiCurrentState {
                provider_key: Some(provider_key),
                model_id: Some(model_id),
                managed_provider_id: Some(projection.provider_id),
                ownership: PiCurrentOwnership::Managed,
                gateway_status: Some(gateway_status),
                active_route: PiActiveRoute::Unavailable,
                route_reason: PiCurrentRouteReason::SelectionUnavailable,
            });
        }
        if settings::get_pi_proxy_settings().auto_failover_enabled {
            let primary = Self::gateway_ready_failover_queue(state)?
                .first()
                .map(|item| item.provider_id.clone());
            if primary.as_deref() != Some(projection.provider_id.as_str()) {
                return Ok(PiCurrentState {
                    provider_key: Some(provider_key),
                    model_id: Some(model_id),
                    managed_provider_id: Some(projection.provider_id),
                    ownership: PiCurrentOwnership::Managed,
                    gateway_status: Some(gateway_status),
                    active_route: PiActiveRoute::Unavailable,
                    route_reason: PiCurrentRouteReason::FailoverPrimaryMismatch,
                });
            }
        }
        let actual = snapshot_pi_provider_values(&models_path, [provider_key.clone()])?
            .values
            .get(&provider_key)
            .cloned()
            .flatten();
        let direct =
            serde_json::to_value(&config).map_err(|source| AppError::JsonSerialize { source })?;
        let gateway = state
            .proxy_service
            .active_pi_gateway_projection(&projection.provider_id, &provider_key);
        let (active_route, route_reason) = if gateway.as_ref().is_some_and(|value| {
            gateway_status == PiGatewayStatus::Proxyable && actual.as_ref() == Some(value)
        }) {
            (PiActiveRoute::Gateway, PiCurrentRouteReason::ManagedGateway)
        } else if actual.as_ref() == Some(&direct) {
            (PiActiveRoute::Direct, PiCurrentRouteReason::ManagedDirect)
        } else {
            (
                PiActiveRoute::Unavailable,
                PiCurrentRouteReason::ManagedProjectionMismatch,
            )
        };

        Ok(PiCurrentState {
            provider_key: Some(provider_key),
            model_id: Some(model_id),
            managed_provider_id: Some(projection.provider_id),
            ownership: PiCurrentOwnership::Managed,
            gateway_status: Some(gateway_status),
            active_route,
            route_reason,
        })
    }

    /// One control-plane answer for every caller that needs to decide whether
    /// a managed Pi provider can enter the gateway or failover runtime.
    #[cfg(test)]
    pub(crate) fn gateway_status(
        state: &AppState,
        provider_id: &str,
    ) -> Result<PiGatewayStatus, AppError> {
        let aggregate = ensure_managed_provider(state, provider_id)?;
        let projection = state.db.get_pi_projection(provider_id)?.ok_or_else(|| {
            AppError::Conflict(format!(
                "Pi provider '{provider_id}' has no exact-key ownership claim"
            ))
        })?;
        let config: PiManagedProviderConfig =
            serde_json::from_value(aggregate.provider.settings_config).map_err(|error| {
                AppError::Config(format!(
                    "managed Pi provider '{provider_id}' is invalid: {error}"
                ))
            })?;
        gateway_status_for_config(&projection.provider_key, &config)
    }

    /// Gateway composition is necessary but not sufficient for admission.
    ///
    /// Pi may replace an exact native key with its own OAuth state while
    /// takeover is off. Keep the stored provider manageable, but exclude it
    /// from failover until the native ownership conflict is resolved.
    pub(crate) fn gateway_admission_ready(
        state: &AppState,
        provider_id: &str,
    ) -> Result<bool, AppError> {
        Self::gateway_admission_snapshot(state, &[provider_id.to_string()])?
            .get(provider_id)
            .copied()
            .ok_or_else(|| {
                AppError::Config(format!(
                    "Pi gateway admission omitted provider '{provider_id}'"
                ))
            })
    }

    /// Capture one native-file view for a set of managed providers.
    ///
    /// Composition answers whether a stored provider can be proxied; native
    /// ownership answers whether CC Switch may currently project that exact
    /// key. Keeping both checks in one batch prevents queue callers from
    /// observing a mixture of different `models.json` revisions.
    pub(crate) fn gateway_admission_snapshot(
        state: &AppState,
        provider_ids: &[String],
    ) -> Result<BTreeMap<String, bool>, AppError> {
        let manifest = state.db.get_pi_projection_manifest()?;
        let mut provider_keys = BTreeMap::new();
        for provider_id in provider_ids {
            let projection = manifest.get(provider_id).ok_or_else(|| {
                AppError::Conflict(format!(
                    "Pi provider '{provider_id}' has no exact-key ownership claim"
                ))
            })?;
            provider_keys.insert(provider_id.clone(), projection.provider_key.clone());
        }
        let native =
            snapshot_pi_provider_values(&get_pi_models_path()?, provider_keys.values().cloned())?;

        provider_keys
            .into_iter()
            .map(|(provider_id, provider_key)| {
                let observed = native.values.get(&provider_key).and_then(Option::as_ref);
                let aggregate = ensure_managed_provider(state, &provider_id)?;
                let config: PiManagedProviderConfig = serde_json::from_value(
                    aggregate.provider.settings_config,
                )
                .map_err(|error| {
                    AppError::Config(format!(
                        "managed Pi provider '{provider_id}' is invalid: {error}"
                    ))
                })?;
                let composition_ready = gateway_status_for_config(&provider_key, &config)?
                    == PiGatewayStatus::Proxyable;
                let projection_matches = if settings::pi_takeover_enabled() {
                    state
                        .proxy_service
                        .project_pi_provider_value(&provider_id, &provider_key, &config)
                        .is_ok_and(|expected| Some(&expected) == observed)
                } else {
                    let direct = serde_json::to_value(config)
                        .map_err(|source| AppError::JsonSerialize { source })?;
                    Some(&direct) == observed
                };
                Ok((
                    provider_id,
                    composition_ready
                        && projection_matches
                        && !pi_owns_native_provider_value(&provider_key, observed),
                ))
            })
            .collect()
    }

    /// Whether Pi currently owns authentication for any exact key still
    /// claimed by the managed catalog.
    pub(crate) fn native_auth_owns_managed_claim(state: &AppState) -> Result<bool, AppError> {
        let manifest = state.db.get_pi_projection_manifest()?;
        let native = snapshot_pi_provider_values(
            &get_pi_models_path()?,
            manifest
                .values()
                .map(|projection| projection.provider_key.clone()),
        )?;
        snapshot_contains_pi_owned_claim(state, &native)
    }

    /// Persisted queue membership is control-plane state and must remain
    /// visible even when live Pi authentication makes an entry temporarily
    /// ineligible for the gateway.
    pub(crate) fn failover_queue_with_admission(
        state: &AppState,
    ) -> Result<Vec<FailoverQueueItem>, AppError> {
        let mut queue = state.db.get_failover_queue(PI_APP)?;
        let provider_ids = queue
            .iter()
            .map(|item| item.provider_id.clone())
            .collect::<Vec<_>>();
        let admission = Self::gateway_admission_snapshot(state, &provider_ids)?;
        for item in &mut queue {
            item.gateway_ready = admission.get(&item.provider_id).copied();
        }
        Ok(queue)
    }

    pub(crate) fn gateway_ready_failover_queue(
        state: &AppState,
    ) -> Result<Vec<FailoverQueueItem>, AppError> {
        let queue = Self::failover_queue_with_admission(state)?;
        if queue
            .first()
            .is_some_and(|item| item.gateway_ready != Some(true))
        {
            // Queue position is ownership. Never silently promote P2 when the
            // persisted P1 becomes ineligible outside CC Switch.
            return Ok(Vec::new());
        }
        Ok(queue
            .into_iter()
            .filter(|item| item.gateway_ready == Some(true))
            .collect())
    }

    pub(crate) fn ensure_auto_failover_add_preserves_primary(
        state: &AppState,
        provider_id: &str,
    ) -> Result<(), AppError> {
        ensure_auto_failover_primary_stays(state, |queue| {
            if queue.iter().any(|item| item.provider_id == provider_id) {
                return Ok(());
            }
            let provider = ensure_managed_provider(state, provider_id)?.provider;
            queue.push(FailoverQueueItem {
                provider_id: provider.id,
                provider_name: provider.name,
                sort_index: provider.sort_index,
                provider_notes: provider.notes,
                gateway_ready: None,
            });
            Ok(())
        })
    }

    pub(crate) fn ensure_auto_failover_remove_preserves_primary(
        state: &AppState,
        provider_id: &str,
    ) -> Result<(), AppError> {
        ensure_auto_failover_primary_stays(state, |queue| {
            queue.retain(|item| item.provider_id != provider_id);
            Ok(())
        })
    }

    fn ensure_auto_failover_reorder_preserves_primary(
        state: &AppState,
        updates: &[(ProviderKey, usize)],
    ) -> Result<(), AppError> {
        let next_order = updates
            .iter()
            .map(|(key, sort_index)| (key.id().to_string(), *sort_index))
            .collect::<BTreeMap<_, _>>();
        ensure_auto_failover_primary_stays(state, |queue| {
            for item in queue {
                if let Some(sort_index) = next_order.get(&item.provider_id) {
                    item.sort_index = Some(*sort_index);
                }
            }
            Ok(())
        })
    }

    fn reconcile_current_indexes_from_native(state: &AppState) -> Result<(), AppError> {
        let defaults = read_pi_native_defaults()?;
        let native_current = resolve_native_current_provider(state, &defaults)?;
        let previous_local = settings::get_current_provider(&AppType::Pi);
        let previous_db = state.db.get_current_provider(PI_APP)?;
        if previous_local == native_current && previous_db == native_current {
            return Ok(());
        }

        settings::set_current_provider(&AppType::Pi, native_current.as_deref())?;
        if let Err(error) = restore_db_current(state, native_current.as_deref()) {
            let local_restored =
                settings::set_current_provider(&AppType::Pi, previous_local.as_deref()).is_ok();
            let db_restored = restore_db_current(state, previous_db.as_deref()).is_ok();
            return Err(authority_error(
                PiCatalogAuthority::ProjectionPending,
                format!(
                    "failed to align Pi current indexes with native settings: {error}; rollback: local={local_restored}, db={db_restored}"
                ),
            ));
        }
        Ok(())
    }

    fn create(
        state: &AppState,
        input: ProviderMutationInput,
        provider_key: String,
        activate_if_first: bool,
    ) -> Result<PiCatalogMutationResult, AppError> {
        if input.in_failover_queue {
            return Err(AppError::InvalidInput(
                "Pi provider creation cannot set failover membership; create it first, then use the failover queue"
                    .to_string(),
            ));
        }
        let catalog_was_empty = state.db.get_all_providers(PI_APP)?.is_empty();
        let provider_key = ensure_managed_native_key(&provider_key)?;
        let config = managed_config(&input)?;
        validate_pi_initial_endpoints(&input)?;
        let provider_id = input.id.clone();
        if state.db.get_pi_projection_for_key(provider_key)?.is_some() {
            return Err(AppError::Conflict(format!(
                "Pi native provider key '{provider_key}' is already managed"
            )));
        }

        let models_path = get_pi_models_path()?;
        let before_file = snapshot_pi_provider_values(&models_path, [provider_key.to_string()])?;
        if before_file
            .values
            .get(provider_key)
            .and_then(Option::as_ref)
            .is_some()
        {
            return Err(AppError::Conflict(format!(
                "Pi native provider key '{provider_key}' already exists; import it instead"
            )));
        }

        let previous_defaults = read_pi_native_defaults()?;
        let previous_local = settings::get_current_provider(&AppType::Pi);
        let previous_db = state.db.get_current_provider(PI_APP)?;
        let projected =
            state
                .proxy_service
                .project_pi_provider_value(&provider_id, provider_key, &config)?;
        state.db.create_pi_catalog_provider(
            NewProviderAggregate::from_input(PI_APP, input)?,
            provider_key,
        )?;

        let projection = IndexMap::from([(provider_key.to_string(), Some(projected))]);
        let native_patch_receipt = match apply_managed_pi_provider_patch_with_receipt(
            &models_path,
            &before_file,
            &projection,
        ) {
            Ok(receipt) => receipt,
            Err(projection_error) => {
                return Err(compensate_created_provider(
                    state,
                    &provider_id,
                    None,
                    projection_error,
                ));
            }
        };

        let no_selected_provider = previous_local.is_none() && previous_db.is_none();
        let native_defaults_empty = previous_defaults.default_provider.is_none()
            && previous_defaults.default_model.is_none();
        let mut result = success(Some(provider_id.clone()));
        if activate_if_first && catalog_was_empty && no_selected_provider && native_defaults_empty {
            let first_model = config
                .models
                .first()
                .expect("validated Pi config has at least one model")
                .id
                .clone();
            match Self::set_default(state, &provider_id, &first_model) {
                Ok(activation) => {
                    result.native_defaults_receipt = activation.native_defaults_receipt;
                }
                Err(error) => {
                    return Err(compensate_created_provider(
                        state,
                        &provider_id,
                        Some(&native_patch_receipt),
                        error,
                    ));
                }
            }
        }

        Ok(result.with_native_patch_receipt(Some(native_patch_receipt)))
    }

    fn set_failover_membership(
        state: &AppState,
        provider_id: &str,
        in_failover_queue: bool,
    ) -> Result<PiCatalogMutationResult, AppError> {
        let aggregate = state
            .db
            .get_provider_aggregate(PI_APP, provider_id)?
            .ok_or_else(|| AppError::NotFound(format!("Pi provider '{provider_id}'")))?;
        if aggregate.provider.in_failover_queue == in_failover_queue {
            return Ok(success(Some(provider_id.to_string())));
        }
        if in_failover_queue {
            if !Self::gateway_admission_ready(state, provider_id)? {
                return Err(AppError::Conflict(format!(
                    "Pi provider '{provider_id}' is not gateway-ready and cannot join failover"
                )));
            }
            Self::ensure_auto_failover_add_preserves_primary(state, provider_id)?;
        } else {
            Self::ensure_auto_failover_remove_preserves_primary(state, provider_id)?;
        }
        if in_failover_queue {
            state.db.add_to_failover_queue(PI_APP, provider_id)?;
        } else {
            state.db.remove_from_failover_queue(PI_APP, provider_id)?;
        }
        Ok(success(Some(provider_id.to_string())))
    }

    fn update(
        state: &AppState,
        input: ProviderMutationInput,
    ) -> Result<PiCatalogMutationResult, AppError> {
        let config = managed_config(&input)?;
        let provider_id = input.id.clone();
        let projection = state.db.get_pi_projection(&provider_id)?.ok_or_else(|| {
            AppError::Conflict(format!(
                "Pi provider '{provider_id}' has no exact-key ownership claim"
            ))
        })?;
        let previous = state
            .db
            .get_provider_aggregate(PI_APP, &provider_id)?
            .ok_or_else(|| AppError::NotFound(format!("Pi provider '{provider_id}'")))?;
        let previous_config: PiManagedProviderConfig =
            serde_json::from_value(previous.provider.settings_config.clone()).map_err(|error| {
                AppError::Config(format!(
                    "managed Pi provider '{provider_id}' is invalid: {error}"
                ))
            })?;
        let previous_gateway =
            gateway_status_for_config(&projection.provider_key, &previous_config)?;
        let next_gateway = gateway_status_for_config(&projection.provider_key, &config)?;
        let previous_defaults = read_pi_native_defaults()?;
        let previous_db = state.db.get_current_provider(PI_APP)?;
        let was_current = previous_db.as_deref() == Some(&provider_id);
        if was_current {
            ensure_auto_failover_target_is_gateway_ready(next_gateway)?;
        }
        if previous.provider.in_failover_queue
            && previous_gateway == PiGatewayStatus::Proxyable
            && next_gateway != PiGatewayStatus::Proxyable
        {
            return Err(AppError::Conflict(
                "remove this Pi provider from failover before making it direct-only".to_string(),
            ));
        }
        // Old versions could leave a direct-only provider marked as a queue
        // member. Clear that stale bit only while automatic failover is off.
        // When failover is on, persisted P1 membership is the ownership
        // boundary and repairing the provider must not silently replace it.
        let clear_stale_failover = previous.provider.in_failover_queue
            && previous_gateway != PiGatewayStatus::Proxyable
            && !settings::get_pi_proxy_settings().auto_failover_enabled;
        let models_path = get_pi_models_path()?;
        let before_file =
            snapshot_pi_provider_values(&models_path, [projection.provider_key.clone()])?;
        let projected = state.proxy_service.project_pi_provider_value(
            &provider_id,
            &projection.provider_key,
            &config,
        )?;

        let key = ProviderKey::new(PI_APP, provider_id.clone())?;
        let row = ProviderRowUpdate::from_input(&input)?;
        if clear_stale_failover {
            if let Err(error) = state.db.remove_from_failover_queue(PI_APP, &provider_id) {
                return Err(compensate_existing_provider(
                    state,
                    &previous,
                    was_current,
                    &projection,
                    None,
                    error,
                ));
            }
        }
        if let Err(error) = state.db.update_pi_catalog_provider(&key, &row) {
            if clear_stale_failover {
                return Err(compensate_existing_provider(
                    state,
                    &previous,
                    was_current,
                    &projection,
                    None,
                    error,
                ));
            }
            return Err(error);
        }
        let patch = IndexMap::from([(projection.provider_key.clone(), Some(projected))]);
        let native_patch_receipt = match apply_managed_pi_provider_patch_with_receipt(
            &models_path,
            &before_file,
            &patch,
        ) {
            Ok(receipt) => receipt,
            Err(error) => {
                return Err(compensate_existing_provider(
                    state,
                    &previous,
                    was_current,
                    &projection,
                    None,
                    error,
                ));
            }
        };
        let mut result = success(Some(provider_id.clone()));
        if previous_defaults.default_provider.as_deref() == Some(&projection.provider_key) {
            if let Some(default_model) = previous_defaults.default_model.as_deref() {
                if effective_pi_model(&config, default_model).is_err() {
                    let replacement = config
                        .models
                        .first()
                        .expect("validated Pi config has at least one model")
                        .id
                        .clone();
                    match Self::set_default(state, &provider_id, &replacement) {
                        Ok(default_update) => {
                            result.native_defaults_receipt = default_update.native_defaults_receipt;
                        }
                        Err(error) => {
                            return Err(compensate_existing_provider(
                                state,
                                &previous,
                                was_current,
                                &projection,
                                Some(&native_patch_receipt),
                                error,
                            ));
                        }
                    }
                }
            }
        }
        Ok(result.with_native_patch_receipt(Some(native_patch_receipt)))
    }

    fn delete(state: &AppState, provider_id: &str) -> Result<PiCatalogMutationResult, AppError> {
        let previous = state
            .db
            .get_provider_aggregate(PI_APP, provider_id)?
            .ok_or_else(|| AppError::NotFound(format!("Pi provider '{provider_id}'")))?;
        if previous.provider.in_failover_queue {
            Self::ensure_auto_failover_remove_preserves_primary(state, provider_id)?;
        }
        let Some(projection) = state.db.get_pi_projection(provider_id)? else {
            // A failed portable reconciliation can leave an unclaimed row.
            // It owns no native key, so deleting the row is a safe detach and
            // must not attempt to edit Pi's file.
            state.db.delete_pi_catalog_provider(provider_id)?;
            return Ok(success(Some(provider_id.to_string())));
        };
        let db_current = state.db.get_current_provider(PI_APP)?;
        let native_defaults = read_pi_native_defaults()?;
        let models_path = get_pi_models_path()?;
        let before_file =
            snapshot_pi_provider_values(&models_path, [projection.provider_key.clone()])?;
        let native_is_pi_owned = is_pi_builtin_provider_key(&projection.provider_key)
            || before_file
                .values
                .get(&projection.provider_key)
                .and_then(Option::as_ref)
                .is_some_and(value_uses_pi_owned_auth);
        if native_defaults.default_provider.as_deref() == Some(&projection.provider_key)
            && !native_is_pi_owned
        {
            return Err(AppError::Conflict(
                "the active Pi provider cannot be deleted".to_string(),
            ));
        }

        let was_current = db_current.as_deref() == Some(provider_id);
        state.db.delete_pi_catalog_provider(provider_id)?;
        if native_is_pi_owned {
            // Ownership changed outside CC Switch. Release the stale database
            // claim while preserving Pi's value byte-for-byte.
            return Ok(success(Some(provider_id.to_string())));
        }
        let patch = IndexMap::from([(projection.provider_key.clone(), None)]);
        let native_patch_receipt = match apply_managed_pi_provider_patch_with_receipt(
            &models_path,
            &before_file,
            &patch,
        ) {
            Ok(receipt) => receipt,
            Err(error) => {
                return Err(compensate_existing_provider(
                    state,
                    &previous,
                    was_current,
                    &projection,
                    None,
                    error,
                ));
            }
        };
        Ok(success(Some(provider_id.to_string()))
            .with_native_patch_receipt(Some(native_patch_receipt)))
    }

    fn add_endpoint(
        state: &AppState,
        provider_id: &str,
        url: &str,
    ) -> Result<PiCatalogMutationResult, AppError> {
        ensure_managed_provider(state, provider_id)?;
        let normalized = normalize_gateway_endpoint_for_write(url)?;
        state.db.add_provider_endpoint(
            &ProviderKey::new(PI_APP, provider_id)?,
            NewEndpoint::now(normalized)?,
        )?;
        Ok(success(Some(provider_id.to_string())))
    }

    fn remove_endpoint(
        state: &AppState,
        provider_id: &str,
        url: &str,
    ) -> Result<PiCatalogMutationResult, AppError> {
        ensure_managed_provider(state, provider_id)?;
        let normalized = normalize_endpoint_key(url)?;
        state
            .db
            .remove_provider_endpoint(&ProviderKey::new(PI_APP, provider_id)?, &normalized)?;
        Ok(success(Some(provider_id.to_string())))
    }

    fn import_native(
        state: &AppState,
        provider_key: &str,
        expected_fingerprint: &str,
    ) -> Result<PiCatalogMutationResult, AppError> {
        let provider_key = non_empty_native_key(provider_key)?;
        let claims = managed_claims(state)?;
        let inspection = inspect_pi_native_entry(&get_pi_models_path()?, provider_key, &claims)?
            .ok_or_else(|| AppError::NotFound(format!("Pi native provider '{provider_key}'")))?;
        if inspection.diagnostic.fingerprint != expected_fingerprint {
            return Err(AppError::Conflict(format!(
                "Pi native provider '{provider_key}' changed since inspection"
            )));
        }
        if inspection.diagnostic.management_status != PiManagementStatus::Importable {
            return Err(AppError::Conflict(format!(
                "Pi native provider '{provider_key}' is not importable"
            )));
        }
        let config = inspection.managed_config.ok_or_else(|| {
            AppError::Conflict(format!(
                "Pi native provider '{provider_key}' is not representable by the managed catalog"
            ))
        })?;
        validate_pi_managed_provider(&config)
            .map_err(|error| AppError::InvalidInput(error.to_string()))?;
        let display_name = config
            .name
            .clone()
            .unwrap_or_else(|| provider_key.to_string());
        let input = ProviderMutationInput {
            id: provider_key.to_string(),
            name: display_name,
            settings_config: serde_json::to_value(config).map_err(|source| {
                AppError::Config(format!(
                    "failed to serialize imported Pi provider: {source}"
                ))
            })?,
            website_url: None,
            category: Some("imported".to_string()),
            created_at: Some(chrono::Utc::now().timestamp_millis()),
            sort_index: state
                .db
                .get_all_providers(PI_APP)?
                .values()
                .filter_map(|provider| provider.sort_index)
                .max()
                .map_or(Some(0), |index| index.checked_add(1)),
            notes: None,
            meta: None,
            icon: Some("pi".to_string()),
            icon_color: None,
            in_failover_queue: false,
        };
        state.db.create_pi_catalog_provider(
            NewProviderAggregate::from_input(PI_APP, input)?,
            provider_key,
        )?;
        Ok(
            success(Some(provider_key.to_string())).with_native_fingerprint_precondition(
                provider_key.to_string(),
                inspection.diagnostic.fingerprint,
            ),
        )
    }

    fn set_default(
        state: &AppState,
        provider_id: &str,
        model_id: &str,
    ) -> Result<PiCatalogMutationResult, AppError> {
        let aggregate = ensure_managed_provider(state, provider_id)?;
        let config: PiManagedProviderConfig = serde_json::from_value(
            aggregate.provider.settings_config.clone(),
        )
        .map_err(|error| {
            AppError::Config(format!(
                "managed Pi provider '{provider_id}' is invalid: {error}"
            ))
        })?;
        effective_pi_model(&config, model_id)
            .map_err(|error| AppError::InvalidInput(error.to_string()))?;
        let projection = state.db.get_pi_projection(provider_id)?.ok_or_else(|| {
            AppError::Conflict(format!(
                "Pi provider '{provider_id}' has no exact-key ownership claim"
            ))
        })?;
        let native =
            snapshot_pi_provider_values(&get_pi_models_path()?, [projection.provider_key.clone()])?;
        let observed = native
            .values
            .get(&projection.provider_key)
            .and_then(Option::as_ref);
        if pi_owns_native_provider_value(&projection.provider_key, observed) {
            return Err(AppError::Conflict(format!(
                "Pi native provider key '{}' is no longer managed by CC Switch",
                projection.provider_key
            )));
        }
        let gateway_status = gateway_status_for_config(&projection.provider_key, &config)?;
        ensure_auto_failover_default(state, provider_id, gateway_status)?;

        let previous_local = settings::get_current_provider(&AppType::Pi);
        let previous_db = state.db.get_current_provider(PI_APP)?;
        let native_defaults_receipt =
            set_pi_native_default_with_receipt(&projection.provider_key, model_id)?;
        if let Err(error) = settings::set_current_provider(&AppType::Pi, Some(provider_id)) {
            return match native_defaults_receipt.rollback() {
                Ok(PiNativeDefaultsRollback::Restored) => Err(authority_error(
                    PiCatalogAuthority::PreviousRestored,
                    error.to_string(),
                )),
                Ok(PiNativeDefaultsRollback::Superseded) => {
                    let indexes = Self::reconcile_current_indexes_from_native(state).map_or_else(
                        |index_error| format!("; current-index reconcile failed: {index_error}"),
                        |()| "; external native defaults were preserved".to_string(),
                    );
                    Err(authority_error(
                        PiCatalogAuthority::ProjectionPending,
                        format!("{error}{indexes}"),
                    ))
                }
                Err(rollback_error) => Err(authority_error(
                    PiCatalogAuthority::ProjectionPending,
                    format!("{error}; native-default rollback failed: {rollback_error}"),
                )),
            };
        }
        if let Err(error) = state.db.set_current_provider(PI_APP, provider_id) {
            return match native_defaults_receipt.rollback() {
                Ok(PiNativeDefaultsRollback::Restored) => {
                    let local_restored =
                        settings::set_current_provider(&AppType::Pi, previous_local.as_deref());
                    let db_restored = restore_db_current(state, previous_db.as_deref());
                    let authority = if local_restored.is_ok() && db_restored.is_ok() {
                        PiCatalogAuthority::PreviousRestored
                    } else {
                        PiCatalogAuthority::ProjectionPending
                    };
                    Err(authority_error(authority, error.to_string()))
                }
                Ok(PiNativeDefaultsRollback::Superseded) => {
                    let indexes = Self::reconcile_current_indexes_from_native(state).map_or_else(
                        |index_error| format!("; current-index reconcile failed: {index_error}"),
                        |()| "; external native defaults were preserved".to_string(),
                    );
                    Err(authority_error(
                        PiCatalogAuthority::ProjectionPending,
                        format!("{error}{indexes}"),
                    ))
                }
                Err(rollback_error) => {
                    let indexes = Self::reconcile_current_indexes_from_native(state).map_or_else(
                        |index_error| format!("; current-index reconcile failed: {index_error}"),
                        |()| String::new(),
                    );
                    Err(authority_error(
                        PiCatalogAuthority::ProjectionPending,
                        format!(
                            "{error}; native-default rollback failed: {rollback_error}{indexes}"
                        ),
                    ))
                }
            };
        }
        Ok(success(Some(provider_id.to_string()))
            .with_native_defaults_receipt(Some(native_defaults_receipt)))
    }
}

fn managed_config(input: &ProviderMutationInput) -> Result<PiManagedProviderConfig, AppError> {
    let config: PiManagedProviderConfig = serde_json::from_value(input.settings_config.clone())
        .map_err(|error| {
            AppError::InvalidInput(format!("invalid managed Pi provider config: {error}"))
        })?;
    validate_pi_managed_provider(&config)
        .map_err(|error| AppError::InvalidInput(error.to_string()))?;
    Ok(config)
}

fn gateway_status_for_config(
    provider_key: &str,
    config: &PiManagedProviderConfig,
) -> Result<PiGatewayStatus, AppError> {
    let composition = compose_managed_pi_provider(provider_key, config)?;
    let assessment = assess_composition_for_runtime(&composition);
    Ok(match assessment.capability {
        PiGatewayCapability::Proxyable if assessment.plans.len() == composition.models.len() => {
            PiGatewayStatus::Proxyable
        }
        PiGatewayCapability::DirectOnly => PiGatewayStatus::DirectOnly,
        PiGatewayCapability::Proxyable | PiGatewayCapability::Unknown => PiGatewayStatus::Unknown,
    })
}

fn ensure_auto_failover_target_is_gateway_ready(
    gateway_status: PiGatewayStatus,
) -> Result<(), AppError> {
    if settings::get_pi_proxy_settings().auto_failover_enabled
        && gateway_status != PiGatewayStatus::Proxyable
    {
        return Err(AppError::Conflict(
            "disable Pi auto failover before selecting a direct-only provider".to_string(),
        ));
    }
    Ok(())
}

fn ensure_auto_failover_default(
    state: &AppState,
    provider_id: &str,
    gateway_status: PiGatewayStatus,
) -> Result<(), AppError> {
    ensure_auto_failover_target_is_gateway_ready(gateway_status)?;
    if !settings::get_pi_proxy_settings().auto_failover_enabled {
        return Ok(());
    }

    let primary = PiCatalogCoordinator::gateway_ready_failover_queue(state)?
        .first()
        .map(|item| item.provider_id.clone());
    if primary.as_deref() != Some(provider_id) {
        return Err(AppError::Conflict(
            "Pi auto failover is enabled; the failover queue primary owns the managed default"
                .to_string(),
        ));
    }
    Ok(())
}

fn ensure_auto_failover_primary_stays(
    state: &AppState,
    project: impl FnOnce(&mut Vec<FailoverQueueItem>) -> Result<(), AppError>,
) -> Result<(), AppError> {
    if !settings::get_pi_proxy_settings().auto_failover_enabled {
        return Ok(());
    }

    // Mutation ownership follows the persisted queue, even when external Pi
    // auth drift makes its current primary temporarily ineligible at runtime.
    // Otherwise that drift would silently remove the primary from this check
    // and allow callers to mutate the queue while auto failover still owns it.
    let mut queue = state.db.get_failover_queue(PI_APP)?;
    let previous_primary = queue.first().map(|item| item.provider_id.clone());
    project(&mut queue)?;
    queue.sort_by(|left, right| {
        left.sort_index
            .unwrap_or(999_999)
            .cmp(&right.sort_index.unwrap_or(999_999))
            .then_with(|| left.provider_id.cmp(&right.provider_id))
    });
    let next_primary = queue.first().map(|item| item.provider_id.clone());
    if previous_primary != next_primary {
        return Err(AppError::Conflict(
            "disable Pi auto failover before changing the failover queue primary".to_string(),
        ));
    }
    Ok(())
}

fn snapshot_contains_pi_owned_claim(
    state: &AppState,
    native: &PiProviderValuesSnapshot,
) -> Result<bool, AppError> {
    for projection in state.db.get_pi_projection_manifest()?.values() {
        let observed = native
            .values
            .get(&projection.provider_key)
            .ok_or_else(|| {
                AppError::Config(format!(
                    "Pi native snapshot omitted claimed key '{}'",
                    projection.provider_key
                ))
            })?
            .as_ref();
        if pi_owns_native_provider_value(&projection.provider_key, observed) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn ensure_managed_provider(
    state: &AppState,
    provider_id: &str,
) -> Result<ProviderAggregate, AppError> {
    if state.db.get_pi_projection(provider_id)?.is_none() {
        return Err(AppError::Conflict(format!(
            "Pi provider '{provider_id}' has no exact-key ownership claim"
        )));
    }
    state
        .db
        .get_provider_aggregate(PI_APP, provider_id)?
        .ok_or_else(|| AppError::NotFound(format!("Pi provider '{provider_id}'")))
}

fn managed_claims(state: &AppState) -> Result<BTreeMap<String, String>, AppError> {
    Ok(state
        .db
        .get_pi_projection_manifest()?
        .into_values()
        .map(|projection| (projection.provider_key, projection.provider_id))
        .collect())
}

fn non_empty_native_key(value: &str) -> Result<&str, AppError> {
    let value = value.trim();
    if value.is_empty() {
        Err(AppError::InvalidInput(
            "Pi native provider key cannot be empty".to_string(),
        ))
    } else {
        Ok(value)
    }
}

fn ensure_managed_native_key(value: &str) -> Result<&str, AppError> {
    let value = non_empty_native_key(value)?;
    if is_pi_builtin_provider_key(value) {
        Err(AppError::Conflict(format!(
            "Pi built-in provider key '{value}' is owned by Pi; use Pi native login instead"
        )))
    } else {
        Ok(value)
    }
}

fn normalize_endpoint_key(value: &str) -> Result<String, AppError> {
    let normalized = value.trim().trim_end_matches('/').to_string();
    if normalized.is_empty() {
        return Err(AppError::InvalidInput(
            "Pi endpoint URL cannot be empty".to_string(),
        ));
    }
    Ok(normalized)
}

fn normalize_gateway_endpoint_for_write(value: &str) -> Result<String, AppError> {
    let normalized = normalize_endpoint_key(value)?;
    parse_pi_gateway_endpoint(&normalized, "/customEndpoints").map_err(|_| {
        AppError::InvalidInput(
            "Pi endpoint must be an absolute HTTP(S) URL without embedded credentials".to_string(),
        )
    })?;
    Ok(normalized)
}

fn validate_pi_initial_endpoints(input: &ProviderMutationInput) -> Result<(), AppError> {
    if let Some(meta) = input.meta.as_ref() {
        for endpoint in meta.custom_endpoints.values() {
            normalize_gateway_endpoint_for_write(&endpoint.url)?;
        }
    }
    Ok(())
}

fn compensate_created_provider(
    state: &AppState,
    provider_id: &str,
    native_patch_receipt: Option<&PiProviderPatchReceipt>,
    cause: AppError,
) -> AppError {
    let db_restored = state.db.delete_pi_catalog_provider(provider_id);
    let file_restored = native_patch_receipt.map_or(Ok(()), PiProviderPatchReceipt::rollback);
    if db_restored.is_ok() && file_restored.is_ok() {
        authority_error(PiCatalogAuthority::PreviousRestored, cause.to_string())
    } else {
        authority_error(
            if db_restored.is_err() {
                PiCatalogAuthority::MutatedDatabaseAuthoritative
            } else {
                PiCatalogAuthority::ProjectionPending
            },
            format!("{}; create compensation was incomplete", cause),
        )
    }
}

fn compensate_portable_projection_ledger(
    state: &AppState,
    new_claims: &[(String, String)],
    released_claims: &[(String, String)],
    cause: AppError,
) -> AppError {
    let mut restored = true;
    for (provider_id, provider_key) in new_claims.iter().rev() {
        if state
            .db
            .delete_pi_projection_key(provider_id, provider_key)
            .is_err()
        {
            // Continue compensating the remaining claims even after one row
            // fails. A short-circuit here would strand every earlier claim.
            restored = false;
        }
    }
    for (provider_id, provider_key) in released_claims {
        if state
            .db
            .claim_pi_projection_key(provider_id, provider_key)
            .is_err()
        {
            restored = false;
        }
    }
    if restored {
        authority_error(PiCatalogAuthority::PreviousRestored, cause)
    } else {
        authority_error(
            PiCatalogAuthority::ProjectionPending,
            format!("{cause}; imported projection-ledger compensation was incomplete"),
        )
    }
}

fn compensate_existing_provider(
    state: &AppState,
    previous: &ProviderAggregate,
    was_current: bool,
    projection: &crate::database::PiProviderProjection,
    native_patch_receipt: Option<&PiProviderPatchReceipt>,
    cause: AppError,
) -> AppError {
    let db_restored = state
        .db
        .restore_pi_catalog_provider(previous, was_current, Some(projection));
    let file_restored = native_patch_receipt.map_or(Ok(()), PiProviderPatchReceipt::rollback);
    if db_restored.is_ok() && file_restored.is_ok() {
        authority_error(PiCatalogAuthority::PreviousRestored, cause.to_string())
    } else {
        authority_error(
            if db_restored.is_err() {
                PiCatalogAuthority::MutatedDatabaseAuthoritative
            } else {
                PiCatalogAuthority::ProjectionPending
            },
            format!("{}; catalog compensation was incomplete", cause),
        )
    }
}

fn restore_db_current(state: &AppState, previous: Option<&str>) -> Result<(), AppError> {
    match previous {
        Some(provider_id) => state.db.set_current_provider(PI_APP, provider_id),
        None => state.db.clear_current_provider_for_app(PI_APP),
    }
}

impl PiCatalogSnapshot {
    fn capture(state: &AppState, additional_native_key: Option<&str>) -> Result<Self, AppError> {
        let aggregates = state.db.get_all_provider_aggregates(PI_APP)?;
        let manifest = state.db.get_pi_projection_manifest()?;
        let projections = manifest.values().cloned().collect::<Vec<_>>();
        let mut native_keys = BTreeMap::<String, ()>::new();
        for projection in manifest.values() {
            native_keys.insert(projection.provider_key.clone(), ());
        }
        // Portable providers have no device-local claim yet and reconcile
        // under their normalized id. Include every possible new key in the
        // snapshot so a later runtime failure can remove it again.
        for provider_id in aggregates.keys() {
            native_keys.insert(non_empty_native_key(provider_id)?.to_string(), ());
        }
        if let Some(key) = additional_native_key {
            native_keys.insert(non_empty_native_key(key)?.to_string(), ());
        }
        let models_path = get_pi_models_path()?;
        let native = snapshot_pi_provider_values(&models_path, native_keys.into_keys())?;
        Ok(Self {
            aggregates,
            projections,
            db_current: state.db.get_current_provider(PI_APP)?,
            native,
        })
    }

    fn restore(
        &self,
        state: &AppState,
        native_defaults_receipt: Option<&PiNativeDefaultsReceipt>,
        native_patch_receipt: Option<&PiProviderPatchReceipt>,
    ) -> Result<(), AppError> {
        let mut failures = Vec::new();
        if let Err(error) = state.db.restore_pi_catalog_snapshot(
            &self.aggregates,
            &self.projections,
            self.db_current.as_deref(),
        ) {
            failures.push(format!("database={error}"));
        }
        if let Some(receipt) = native_patch_receipt {
            if let Err(error) = receipt.rollback() {
                failures.push(format!("models={error}"));
            }
        }
        if let Some(receipt) = native_defaults_receipt {
            if let Err(error) = receipt.rollback() {
                failures.push(format!("native_defaults={error}"));
            }
        }
        // Current markers are derived indexes. Re-read the native authority so
        // an external selection which superseded our receipt is preserved.
        if let Err(error) = PiCatalogCoordinator::reconcile_current_indexes_from_native(state) {
            failures.push(format!("current_indexes={error}"));
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(AppError::Config(failures.join(", ")))
        }
    }
}

fn resolve_native_current_provider(
    state: &AppState,
    defaults: &PiNativeDefaults,
) -> Result<Option<String>, AppError> {
    let Some(provider_key) = defaults.default_provider.as_deref() else {
        return Ok(None);
    };
    if is_pi_builtin_provider_key(provider_key) {
        return Ok(None);
    }
    let Some(projection) = state.db.get_pi_projection_for_key(provider_key)? else {
        return Ok(None);
    };
    let native = snapshot_pi_provider_values(&get_pi_models_path()?, [provider_key.to_string()])?;
    if native
        .values
        .get(provider_key)
        .and_then(Option::as_ref)
        .is_some_and(value_uses_pi_owned_auth)
    {
        return Ok(None);
    }
    Ok(state
        .db
        .get_provider_aggregate(PI_APP, &projection.provider_id)?
        .is_some()
        .then_some(projection.provider_id))
}

fn success(provider_id: Option<String>) -> PiCatalogMutationResult {
    PiCatalogMutationResult {
        authority: PiCatalogAuthority::Published,
        provider_id,
        native_defaults_receipt: None,
        native_patch_receipt: None,
        native_fingerprint_preconditions: IndexMap::new(),
    }
}

impl PiCatalogMutationResult {
    pub(crate) fn into_native_defaults_receipt(mut self) -> Option<PiNativeDefaultsReceipt> {
        self.native_defaults_receipt.take()
    }

    fn with_native_defaults_receipt(mut self, receipt: Option<PiNativeDefaultsReceipt>) -> Self {
        self.native_defaults_receipt = receipt;
        self
    }

    fn with_native_patch_receipt(mut self, receipt: Option<PiProviderPatchReceipt>) -> Self {
        self.native_patch_receipt = receipt;
        self
    }

    fn with_native_fingerprint_precondition(
        mut self,
        provider_key: String,
        fingerprint: String,
    ) -> Self {
        self.native_fingerprint_preconditions
            .insert(provider_key, fingerprint);
        self
    }
}

fn authority_error(authority: PiCatalogAuthority, message: impl std::fmt::Display) -> AppError {
    AppError::Message(format!(
        "{message} [authoritative_state={}]",
        authority.as_str()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::Database;
    use serde_json::json;
    use std::sync::Arc;

    struct TestHome(Option<std::ffi::OsString>);

    impl TestHome {
        fn install(path: &std::path::Path) -> Result<Self, AppError> {
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

    fn managed_input(id: &str, base_url: &str) -> ProviderMutationInput {
        ProviderMutationInput {
            id: id.to_string(),
            name: id.to_string(),
            settings_config: json!({
                "name": id,
                "api": "openai-responses",
                "baseUrl": base_url,
                "apiKey": "literal-key",
                "models": [{"id": "model-a", "name": "Model A"}]
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

    fn insert_portable_pi_provider(db: &Database, id: &str) -> Result<Value, AppError> {
        let config = json!({
            "name": "Portable Pi",
            "api": "openai-responses",
            "baseUrl": "https://portable.example/v1",
            "apiKey": "literal-key",
            "headers": {"x-capture": "present"},
            "models": [{
                "id": "portable-model",
                "name": "Portable model",
                "compat": {"supportsStore": true}
            }]
        });
        let conn = crate::database::lock_conn!(db.conn);
        conn.execute(
            "INSERT INTO providers
                (id, app_type, name, settings_config, meta, is_current)
             VALUES (?1, 'pi', 'Portable Pi', ?2, '{}', 0)",
            rusqlite::params![id, config.to_string()],
        )
        .map_err(|error| AppError::Database(error.to_string()))?;
        Ok(config)
    }

    fn configure_pi_directory(path: &std::path::Path) -> Result<(), AppError> {
        let mut app_settings = crate::settings::get_settings();
        app_settings.pi_config_dir = Some(path.to_string_lossy().into_owned());
        app_settings.pi_takeover_enabled = false;
        crate::settings::update_settings(app_settings)
    }

    #[test]
    #[serial_test::serial]
    fn managed_create_cannot_shadow_a_pi_builtin_provider_key() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        configure_pi_directory(&temp.path().join("pi-agent"))?;
        let state = AppState::new(Arc::new(Database::memory()?));

        let error = PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: managed_input("shadow", "https://example.com/v1"),
                provider_key: "anthropic".to_string(),
                activate_if_first: false,
            },
        )
        .expect_err("Pi owns built-in keys");

        assert!(error.to_string().contains("owned by Pi"));
        assert!(state.db.get_provider_aggregate(PI_APP, "shadow")?.is_none());
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn delete_detaches_unclaimed_rows_without_touching_pi_native_content() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        let pi_dir = temp.path().join("pi-agent");
        configure_pi_directory(&pi_dir)?;
        std::fs::create_dir_all(&pi_dir).expect("create Pi directory");
        let native = json!({
            "api": "anthropic-messages",
            "baseUrl": "https://api.anthropic.com",
            "oauth": "radius",
            "models": [{"id": "claude"}]
        });
        std::fs::write(
            pi_dir.join("models.json"),
            serde_json::to_vec_pretty(&json!({
                "providers": {"anthropic": native}
            }))
            .expect("serialize models"),
        )
        .expect("write models");
        let state = AppState::new(Arc::new(Database::memory()?));
        insert_portable_pi_provider(state.db.as_ref(), "anthropic")?;

        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::DeleteProvider {
                provider_id: "anthropic".to_string(),
            },
        )?;

        assert!(state
            .db
            .get_provider_aggregate(PI_APP, "anthropic")?
            .is_none());
        let live =
            snapshot_pi_provider_values(&pi_dir.join("models.json"), ["anthropic".to_string()])?;
        assert_eq!(
            live.values.get("anthropic").cloned().flatten(),
            Some(native)
        );
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn delete_releases_a_stale_claim_after_pi_takes_over_authentication() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        let pi_dir = temp.path().join("pi-agent");
        configure_pi_directory(&pi_dir)?;
        let state = AppState::new(Arc::new(Database::memory()?));
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: managed_input("provider", "https://managed.example/v1"),
                provider_key: "managed-provider".to_string(),
                activate_if_first: true,
            },
        )?;
        let native = json!({
            "api": "anthropic-messages",
            "baseUrl": "https://api.anthropic.com",
            "oauth": "radius",
            "models": [{"id": "claude"}]
        });
        std::fs::write(
            pi_dir.join("models.json"),
            serde_json::to_vec_pretty(&json!({
                "providers": {"managed-provider": native}
            }))
            .expect("serialize models"),
        )
        .expect("write models");
        set_pi_native_default_with_receipt("managed-provider", "claude")?;
        assert_eq!(
            PiCatalogCoordinator::current_native_provider(&state)?,
            None,
            "Pi-owned authentication must release the derived managed-current marker"
        );

        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::DeleteProvider {
                provider_id: "provider".to_string(),
            },
        )?;

        assert!(state
            .db
            .get_provider_aggregate(PI_APP, "provider")?
            .is_none());
        assert!(state.db.get_pi_projection("provider")?.is_none());
        let live = snapshot_pi_provider_values(
            &pi_dir.join("models.json"),
            ["managed-provider".to_string()],
        )?;
        assert_eq!(
            live.values.get("managed-provider").cloned().flatten(),
            Some(native)
        );
        let defaults = read_pi_native_defaults()?;
        assert_eq!(
            defaults.default_provider.as_deref(),
            Some("managed-provider")
        );
        assert_eq!(defaults.default_model.as_deref(), Some("claude"));
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn managed_default_rejects_a_claim_taken_over_by_pi_authentication() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        let pi_dir = temp.path().join("pi-agent");
        configure_pi_directory(&pi_dir)?;
        let state = AppState::new(Arc::new(Database::memory()?));
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: managed_input("provider", "https://managed.example/v1"),
                provider_key: "managed-provider".to_string(),
                activate_if_first: true,
            },
        )?;

        let oauth = json!({
            "api": "anthropic-messages",
            "baseUrl": "https://api.anthropic.com",
            "oauth": "radius",
            "models": [{"id": "native-model"}]
        });
        std::fs::write(
            pi_dir.join("models.json"),
            serde_json::to_vec_pretty(&json!({
                "providers": {"managed-provider": oauth}
            }))
            .expect("serialize models"),
        )
        .expect("write models");
        set_pi_native_default_with_receipt("managed-provider", "native-model")?;

        let error = PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::SetDefault {
                provider_id: "provider".to_string(),
                model_id: "model-a".to_string(),
            },
        )
        .expect_err("Pi-owned authentication must fence the stale managed picker entry");

        assert!(error.to_string().contains("no longer managed"));
        let defaults = read_pi_native_defaults()?;
        assert_eq!(
            defaults.default_provider.as_deref(),
            Some("managed-provider")
        );
        assert_eq!(
            defaults.default_model.as_deref(),
            Some("native-model"),
            "the rejected managed selection must preserve Pi's native model"
        );
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn enabled_auto_failover_blocks_deleting_a_stale_p1_claim() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        let pi_dir = temp.path().join("pi-agent");
        configure_pi_directory(&pi_dir)?;
        let state = AppState::new(Arc::new(Database::memory()?));
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: managed_input("provider", "https://managed.example/v1"),
                provider_key: "managed-provider".to_string(),
                activate_if_first: true,
            },
        )?;
        state.db.add_to_failover_queue(PI_APP, "provider")?;
        let mut proxy = settings::get_pi_proxy_settings();
        proxy.auto_failover_enabled = true;
        settings::update_pi_proxy_settings(proxy)?;

        let oauth = json!({
            "api": "anthropic-messages",
            "baseUrl": "https://api.anthropic.com",
            "oauth": "radius",
            "models": [{"id": "claude"}]
        });
        std::fs::write(
            pi_dir.join("models.json"),
            serde_json::to_vec_pretty(&json!({
                "providers": {"managed-provider": oauth}
            }))
            .expect("serialize models"),
        )
        .expect("write models");
        set_pi_native_default_with_receipt("managed-provider", "claude")?;

        let error = PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::DeleteProvider {
                provider_id: "provider".to_string(),
            },
        )
        .expect_err("enabled failover must retain its queue primary");

        assert!(error
            .to_string()
            .contains("changing the failover queue primary"));
        assert!(state
            .db
            .get_provider_aggregate(PI_APP, "provider")?
            .is_some());
        assert!(state.db.get_pi_projection("provider")?.is_some());
        assert!(state.db.is_in_failover_queue(PI_APP, "provider")?);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn enabled_auto_failover_owns_the_managed_default() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        configure_pi_directory(&temp.path().join("pi-agent"))?;
        let state = AppState::new(Arc::new(Database::memory()?));
        for provider_id in ["primary", "outside"] {
            PiCatalogCoordinator::apply(
                &state,
                PiCatalogMutation::CreateProvider {
                    input: managed_input(provider_id, "https://example.com/v1"),
                    provider_key: format!("managed-{provider_id}"),
                    activate_if_first: false,
                },
            )?;
        }
        let mut direct = managed_input("direct", "https://example.com");
        direct.settings_config["headers"] = json!({"Host": "example.com"});
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: direct,
                provider_key: "managed-direct".to_string(),
                activate_if_first: false,
            },
        )?;
        state.db.add_to_failover_queue(PI_APP, "primary")?;
        let mut proxy = settings::get_pi_proxy_settings();
        proxy.auto_failover_enabled = true;
        settings::update_pi_proxy_settings(proxy)?;

        let direct_error = PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::SetDefault {
                provider_id: "direct".to_string(),
                model_id: "model-a".to_string(),
            },
        )
        .expect_err("direct-only providers cannot bypass enabled failover");
        assert!(direct_error
            .to_string()
            .contains("disable Pi auto failover"));

        let outside_error = PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::SetDefault {
                provider_id: "outside".to_string(),
                model_id: "model-a".to_string(),
            },
        )
        .expect_err("the queue primary owns the default while failover is enabled");
        assert!(outside_error.to_string().contains("queue primary"));

        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::SetDefault {
                provider_id: "primary".to_string(),
                model_id: "model-a".to_string(),
            },
        )?;
        assert_eq!(
            read_pi_native_defaults()?.default_provider.as_deref(),
            Some("managed-primary")
        );
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn enabled_auto_failover_rejects_membership_changes_that_replace_p1() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        configure_pi_directory(&temp.path().join("pi-agent"))?;
        let state = AppState::new(Arc::new(Database::memory()?));
        for (provider_id, sort_index) in [("earlier", 0), ("primary", 1), ("later", 2)] {
            let mut input = managed_input(provider_id, "https://example.com/v1");
            input.sort_index = Some(sort_index);
            PiCatalogCoordinator::apply(
                &state,
                PiCatalogMutation::CreateProvider {
                    input,
                    provider_key: format!("managed-{provider_id}"),
                    activate_if_first: false,
                },
            )?;
        }
        state.db.add_to_failover_queue(PI_APP, "primary")?;
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::SetDefault {
                provider_id: "primary".to_string(),
                model_id: "model-a".to_string(),
            },
        )?;
        let mut proxy = settings::get_pi_proxy_settings();
        proxy.auto_failover_enabled = true;
        settings::update_pi_proxy_settings(proxy)?;

        let earlier =
            PiCatalogCoordinator::ensure_auto_failover_add_preserves_primary(&state, "earlier")
                .expect_err("an earlier-sorted member would replace P1");
        assert!(earlier
            .to_string()
            .contains("changing the failover queue primary"));

        PiCatalogCoordinator::ensure_auto_failover_add_preserves_primary(&state, "later")?;
        state.db.add_to_failover_queue(PI_APP, "later")?;
        let remove_primary =
            PiCatalogCoordinator::ensure_auto_failover_remove_preserves_primary(&state, "primary")
                .expect_err("removing P1 must be rejected while failover is enabled");
        assert!(remove_primary
            .to_string()
            .contains("changing the failover queue primary"));
        PiCatalogCoordinator::ensure_auto_failover_remove_preserves_primary(&state, "later")?;
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn failover_membership_runtime_failure_restores_the_exact_catalog_snapshot(
    ) -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        configure_pi_directory(&temp.path().join("pi-agent"))?;
        let state = AppState::new(Arc::new(Database::memory()?));
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: managed_input("provider", "https://example.com/v1"),
                provider_key: "managed-provider".to_string(),
                activate_if_first: false,
            },
        )?;

        state.proxy_service.fail_next_pi_reconcile_for_test();
        let add_error = PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::SetFailoverMembership {
                provider_id: "provider".to_string(),
                in_failover_queue: true,
            },
        )
        .expect_err("failed runtime publication must undo queue insertion");
        assert!(add_error
            .to_string()
            .contains("previous catalog was restored"));
        assert!(!state.db.is_in_failover_queue(PI_APP, "provider")?);

        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::SetFailoverMembership {
                provider_id: "provider".to_string(),
                in_failover_queue: true,
            },
        )?;
        state.proxy_service.fail_next_pi_reconcile_for_test();
        let remove_error = PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::SetFailoverMembership {
                provider_id: "provider".to_string(),
                in_failover_queue: false,
            },
        )
        .expect_err("failed runtime publication must undo queue removal");
        assert!(remove_error
            .to_string()
            .contains("previous catalog was restored"));
        assert!(state.db.is_in_failover_queue(PI_APP, "provider")?);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn pi_owned_native_auth_is_not_advertised_or_admitted_to_failover() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        configure_pi_directory(&temp.path().join("pi-agent"))?;
        let state = AppState::new(Arc::new(Database::memory()?));
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: managed_input("provider", "https://example.com/v1"),
                provider_key: "managed-provider".to_string(),
                activate_if_first: false,
            },
        )?;
        assert!(PiCatalogCoordinator::gateway_admission_ready(
            &state, "provider"
        )?);

        let models_path = get_pi_models_path()?;
        std::fs::write(
            &models_path,
            serde_json::to_vec_pretty(&json!({
                "providers": {
                    "managed-provider": {
                        "api": "anthropic-messages",
                        "baseUrl": "https://api.anthropic.com",
                        "oauth": "radius",
                        "models": [{"id": "model-a"}]
                    }
                }
            }))
            .expect("serialize Pi-owned entry"),
        )
        .expect("replace native entry");

        assert_eq!(
            PiCatalogCoordinator::gateway_status(&state, "provider")?,
            PiGatewayStatus::Proxyable,
            "stored composition capability remains independently inspectable"
        );
        assert!(!PiCatalogCoordinator::gateway_admission_ready(
            &state, "provider"
        )?);
        let error = PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::SetFailoverMembership {
                provider_id: "provider".to_string(),
                in_failover_queue: true,
            },
        )
        .expect_err("Pi-owned native authentication must block gateway admission");
        assert!(error.to_string().contains("not gateway-ready"));
        assert!(!state.db.is_in_failover_queue(PI_APP, "provider")?);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn oauth_drift_keeps_persisted_queue_visible_without_promoting_p2() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        let pi_dir = temp.path().join("pi-agent");
        configure_pi_directory(&pi_dir)?;
        let state = AppState::new(Arc::new(Database::memory()?));
        for provider_id in ["primary", "secondary"] {
            PiCatalogCoordinator::apply(
                &state,
                PiCatalogMutation::CreateProvider {
                    input: managed_input(provider_id, "https://example.com/v1"),
                    provider_key: provider_id.to_string(),
                    activate_if_first: false,
                },
            )?;
            PiCatalogCoordinator::apply(
                &state,
                PiCatalogMutation::SetFailoverMembership {
                    provider_id: provider_id.to_string(),
                    in_failover_queue: true,
                },
            )?;
        }

        let models_path = pi_dir.join("models.json");
        let mut document: Value =
            serde_json::from_slice(&std::fs::read(&models_path).expect("read models"))
                .expect("parse models");
        let oauth = json!({
            "api": "anthropic-messages",
            "baseUrl": "https://api.anthropic.com",
            "oauth": "radius",
            "models": [{"id": "claude"}]
        });
        document["providers"]["primary"] = oauth.clone();
        std::fs::write(
            &models_path,
            serde_json::to_vec_pretty(&document).expect("serialize models"),
        )
        .expect("replace primary with Pi OAuth");

        let queue = PiCatalogCoordinator::failover_queue_with_admission(&state)?;
        assert_eq!(
            queue
                .iter()
                .map(|item| (item.provider_id.as_str(), item.gateway_ready))
                .collect::<Vec<_>>(),
            vec![("primary", Some(false)), ("secondary", Some(true))]
        );
        assert!(
            PiCatalogCoordinator::gateway_ready_failover_queue(&state)?.is_empty(),
            "an ineligible persisted P1 must not silently promote P2"
        );

        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::SetFailoverMembership {
                provider_id: "primary".to_string(),
                in_failover_queue: false,
            },
        )?;
        assert!(!state.db.is_in_failover_queue(PI_APP, "primary")?);
        let live: Value =
            serde_json::from_slice(&std::fs::read(&models_path).expect("read preserved models"))
                .expect("parse preserved models");
        assert_eq!(live.pointer("/providers/primary"), Some(&oauth));
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn pi_create_cannot_bypass_the_failover_membership_authority() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        configure_pi_directory(&temp.path().join("pi-agent"))?;
        let state = AppState::new(Arc::new(Database::memory()?));
        let mut input = managed_input("provider", "https://example.com/v1");
        input.in_failover_queue = true;

        let error = PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input,
                provider_key: "managed-provider".to_string(),
                activate_if_first: true,
            },
        )
        .expect_err("Pi creation must not own failover membership");

        assert!(error.to_string().contains("create it first"));
        assert!(state
            .db
            .get_provider_aggregate(PI_APP, "provider")?
            .is_none());
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn enabled_auto_failover_rejects_reordering_a_new_provider_into_p1() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        configure_pi_directory(&temp.path().join("pi-agent"))?;
        let state = AppState::new(Arc::new(Database::memory()?));
        for (provider_id, sort_index) in [("primary", 0), ("secondary", 1)] {
            let mut input = managed_input(provider_id, "https://example.com/v1");
            input.sort_index = Some(sort_index);
            PiCatalogCoordinator::apply(
                &state,
                PiCatalogMutation::CreateProvider {
                    input,
                    provider_key: format!("managed-{provider_id}"),
                    activate_if_first: false,
                },
            )?;
            state.db.add_to_failover_queue(PI_APP, provider_id)?;
        }
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::SetDefault {
                provider_id: "primary".to_string(),
                model_id: "model-a".to_string(),
            },
        )?;
        let mut proxy = settings::get_pi_proxy_settings();
        proxy.auto_failover_enabled = true;
        settings::update_pi_proxy_settings(proxy)?;

        let error = PiCatalogCoordinator::update_route_order(
            &state,
            vec![
                (ProviderKey::new(PI_APP, "primary")?, 1),
                (ProviderKey::new(PI_APP, "secondary")?, 0),
            ],
        )
        .expect_err("reordering must not replace P1 while failover is enabled");

        assert!(error
            .to_string()
            .contains("changing the failover queue primary"));
        assert_eq!(
            state.db.get_failover_queue(PI_APP)?[0].provider_id,
            "primary"
        );
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn current_state_reports_unconfigured_from_one_locked_read() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        configure_pi_directory(&temp.path().join("pi-agent"))?;
        let state = AppState::new(Arc::new(Database::memory()?));
        let guard = state.proxy_service.lock_switch_for_app(PI_APP).await;

        let current = PiCatalogCoordinator::current_state_under_switch_guard(&state, &guard)?;

        assert_eq!(current.ownership, PiCurrentOwnership::Unconfigured);
        assert_eq!(current.active_route, PiActiveRoute::Unavailable);
        assert_eq!(current.route_reason, PiCurrentRouteReason::Unconfigured);
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn current_state_reports_builtin_ownership_as_direct_without_guessing_model_availability(
    ) -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        configure_pi_directory(&temp.path().join("pi-agent"))?;
        set_pi_native_default_with_receipt("anthropic", "unverified-model")?;
        let state = AppState::new(Arc::new(Database::memory()?));
        let guard = state.proxy_service.lock_switch_for_app(PI_APP).await;

        let current = PiCatalogCoordinator::current_state_under_switch_guard(&state, &guard)?;

        assert_eq!(current.ownership, PiCurrentOwnership::PiNative);
        assert_eq!(current.active_route, PiActiveRoute::Direct);
        assert_eq!(
            current.route_reason,
            PiCurrentRouteReason::NativeCatalogUnavailable
        );
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn current_state_distinguishes_managed_direct_from_gateway_intent() -> Result<(), AppError>
    {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        configure_pi_directory(&temp.path().join("pi-agent"))?;
        let state = AppState::new(Arc::new(Database::memory()?));
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: managed_input("provider", "https://example.com/v1"),
                provider_key: "managed-provider".to_string(),
                activate_if_first: true,
            },
        )?;
        let guard = state.proxy_service.lock_switch_for_app(PI_APP).await;

        let current = PiCatalogCoordinator::current_state_under_switch_guard(&state, &guard)?;

        assert_eq!(current.ownership, PiCurrentOwnership::Managed);
        assert_eq!(current.gateway_status, Some(PiGatewayStatus::Proxyable));
        assert_eq!(current.active_route, PiActiveRoute::Direct);
        assert_eq!(current.route_reason, PiCurrentRouteReason::ManagedDirect);

        drop(guard);
        crate::pi_config::native_settings::set_pi_native_default_with_receipt(
            "managed-provider",
            "missing-model",
        )?;
        let guard = state.proxy_service.lock_switch_for_app(PI_APP).await;
        let missing = PiCatalogCoordinator::current_state_under_switch_guard(&state, &guard)?;
        assert_eq!(missing.ownership, PiCurrentOwnership::Managed);
        assert_eq!(missing.active_route, PiActiveRoute::Unavailable);
        assert_eq!(
            missing.route_reason,
            PiCurrentRouteReason::SelectionUnavailable
        );
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn current_state_rejects_an_external_managed_switch_away_from_failover_p1(
    ) -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        configure_pi_directory(&temp.path().join("pi-agent"))?;
        let state = AppState::new(Arc::new(Database::memory()?));
        for provider_id in ["primary", "secondary"] {
            PiCatalogCoordinator::apply(
                &state,
                PiCatalogMutation::CreateProvider {
                    input: managed_input(provider_id, "https://example.com/v1"),
                    provider_key: format!("managed-{provider_id}"),
                    activate_if_first: false,
                },
            )?;
        }
        state.db.add_to_failover_queue(PI_APP, "primary")?;
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::SetDefault {
                provider_id: "secondary".to_string(),
                model_id: "model-a".to_string(),
            },
        )?;
        let mut proxy = settings::get_pi_proxy_settings();
        proxy.auto_failover_enabled = true;
        settings::update_pi_proxy_settings(proxy)?;
        let guard = state.proxy_service.lock_switch_for_app(PI_APP).await;

        let current = PiCatalogCoordinator::current_state_under_switch_guard(&state, &guard)?;

        assert_eq!(current.managed_provider_id.as_deref(), Some("secondary"));
        assert_eq!(current.active_route, PiActiveRoute::Unavailable);
        assert_eq!(
            current.route_reason,
            PiCurrentRouteReason::FailoverPrimaryMismatch
        );
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn current_state_uses_native_auth_ownership_and_checks_external_models(
    ) -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        let pi_dir = temp.path().join("pi-agent");
        configure_pi_directory(&pi_dir)?;
        std::fs::create_dir_all(&pi_dir).expect("create Pi directory");
        std::fs::write(
            pi_dir.join("models.json"),
            serde_json::to_vec_pretty(&json!({
                "providers": {
                    "subscription": {
                        "api": "anthropic-messages",
                        "baseUrl": "https://api.anthropic.com",
                        "oauth": "radius",
                        "models": [{"id": "claude"}]
                    }
                }
            }))
            .expect("serialize models"),
        )
        .expect("write models");
        crate::pi_config::native_settings::set_pi_native_default_with_receipt(
            "subscription",
            "claude",
        )?;
        let state = AppState::new(Arc::new(Database::memory()?));
        let guard = state.proxy_service.lock_switch_for_app(PI_APP).await;

        let current = PiCatalogCoordinator::current_state_under_switch_guard(&state, &guard)?;

        assert_eq!(current.ownership, PiCurrentOwnership::PiNative);
        assert_eq!(current.active_route, PiActiveRoute::Direct);

        drop(guard);
        crate::pi_config::native_settings::set_pi_native_default_with_receipt(
            "missing-external",
            "missing-model",
        )?;
        let guard = state.proxy_service.lock_switch_for_app(PI_APP).await;
        let missing = PiCatalogCoordinator::current_state_under_switch_guard(&state, &guard)?;
        assert_eq!(missing.ownership, PiCurrentOwnership::External);
        assert_eq!(missing.active_route, PiActiveRoute::Unavailable);
        assert_eq!(
            missing.route_reason,
            PiCurrentRouteReason::SelectionUnavailable
        );
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn live_pi_owned_auth_outranks_a_stale_managed_claim() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        let pi_dir = temp.path().join("pi-agent");
        configure_pi_directory(&pi_dir)?;
        let state = AppState::new(Arc::new(Database::memory()?));
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: managed_input("provider", "https://managed.example/v1"),
                provider_key: "managed-provider".to_string(),
                activate_if_first: true,
            },
        )?;

        let oauth = json!({
            "api": "anthropic-messages",
            "baseUrl": "https://api.anthropic.com",
            "oauth": "radius",
            "models": [{"id": "claude"}]
        });
        std::fs::write(
            pi_dir.join("models.json"),
            serde_json::to_vec_pretty(&json!({
                "providers": {"managed-provider": oauth}
            }))
            .expect("serialize models"),
        )
        .expect("write models");
        crate::pi_config::native_settings::set_pi_native_default_with_receipt(
            "managed-provider",
            "claude",
        )?;

        let guard = state.proxy_service.lock_switch_for_app(PI_APP).await;
        let current = PiCatalogCoordinator::current_state_under_switch_guard(&state, &guard)?;
        assert_eq!(current.ownership, PiCurrentOwnership::PiNative);
        assert_eq!(current.managed_provider_id, None);
        assert_eq!(current.active_route, PiActiveRoute::Direct);
        drop(guard);

        let error = PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::UpdateProvider {
                input: managed_input("provider", "https://updated.example/v1"),
            },
        )
        .expect_err("managed update must not replace Pi-owned authentication");
        assert!(error.to_string().contains("Pi-owned authentication"));

        let live = snapshot_pi_provider_values(
            &pi_dir.join("models.json"),
            ["managed-provider".to_string()],
        )?;
        assert_eq!(
            live.values.get("managed-provider").cloned().flatten(),
            Some(oauth)
        );
        assert_eq!(
            state
                .db
                .get_provider_aggregate(PI_APP, "provider")?
                .expect("managed aggregate")
                .provider
                .settings_config
                .get("baseUrl"),
            Some(&json!("https://managed.example/v1"))
        );
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn failover_membership_cannot_cross_the_gateway_capability_boundary() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        configure_pi_directory(&temp.path().join("pi-agent"))?;
        let state = AppState::new(Arc::new(Database::memory()?));
        let mut direct = managed_input("provider", "https://example.com/v1");
        direct.settings_config["api"] = json!("future-wire-v9");
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: direct,
                provider_key: "managed-provider".to_string(),
                activate_if_first: false,
            },
        )?;
        state.db.add_to_failover_queue(PI_APP, "provider")?;

        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::UpdateProvider {
                input: managed_input("provider", "https://example.com/v1"),
            },
        )?;
        assert!(
            !state.db.is_in_failover_queue(PI_APP, "provider")?,
            "a stale direct-only membership must not resurrect after becoming proxyable"
        );

        state.db.add_to_failover_queue(PI_APP, "provider")?;
        let mut next_direct = managed_input("provider", "https://example.com/v1");
        next_direct.settings_config["api"] = json!("future-wire-v10");
        let error = PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::UpdateProvider { input: next_direct },
        )
        .expect_err("a queued provider cannot become direct-only");
        assert!(error.to_string().contains("remove this Pi provider"));
        assert_eq!(
            PiCatalogCoordinator::gateway_status(&state, "provider")?,
            PiGatewayStatus::Proxyable
        );
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn repairing_a_legacy_direct_only_primary_preserves_enabled_failover_ownership(
    ) -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        configure_pi_directory(&temp.path().join("pi-agent"))?;
        let state = AppState::new(Arc::new(Database::memory()?));
        let mut direct = managed_input("primary", "https://example.com/v1");
        direct.settings_config["api"] = json!("future-wire-v9");
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: direct,
                provider_key: "managed-primary".to_string(),
                activate_if_first: false,
            },
        )?;
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::SetDefault {
                provider_id: "primary".to_string(),
                model_id: "model-a".to_string(),
            },
        )?;
        state.db.add_to_failover_queue(PI_APP, "primary")?;
        let mut proxy = settings::get_pi_proxy_settings();
        proxy.auto_failover_enabled = true;
        settings::update_pi_proxy_settings(proxy)?;

        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::UpdateProvider {
                input: managed_input("primary", "https://example.com/v1"),
            },
        )?;

        assert!(
            state.db.is_in_failover_queue(PI_APP, "primary")?,
            "repairing queue P1 must not silently release its ownership"
        );
        assert_eq!(
            state.db.get_current_provider(PI_APP)?.as_deref(),
            Some("primary")
        );
        assert_eq!(
            PiCatalogCoordinator::gateway_ready_failover_queue(&state)?
                .first()
                .map(|item| item.provider_id.as_str()),
            Some("primary")
        );
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    #[cfg(unix)]
    fn create_durability_failure_compensates_database_and_native_projection() -> Result<(), AppError>
    {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        let pi_dir = temp.path().join("pi-agent");
        configure_pi_directory(&pi_dir)?;
        let state = AppState::new(Arc::new(Database::memory()?));
        let models_path = pi_dir.join("models.json");
        crate::pi_config::shared_file::fail_next_parent_sync_for_test(&models_path);

        let error = PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: managed_input("ambiguous-create", "https://create.example/v1"),
                provider_key: "ambiguous-native".to_string(),
                activate_if_first: false,
            },
        )
        .expect_err("directory durability failure must fail the service operation");
        assert!(error.to_string().contains("injected"));
        assert!(state
            .db
            .get_provider_aggregate(PI_APP, "ambiguous-create")?
            .is_none());
        assert!(state.db.get_pi_projection("ambiguous-create")?.is_none());
        assert!(
            !models_path.exists(),
            "the failed create must not leave a provider or empty shadow file"
        );
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn pi_endpoint_writes_reuse_gateway_validation_without_partial_state() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        let pi_dir = temp.path().join("pi-agent");
        configure_pi_directory(&pi_dir)?;
        let state = AppState::new(Arc::new(Database::memory()?));
        let bad_url = "https://user:secret@mirror.example/v1";

        let mut invalid_initial = managed_input("invalid-initial", "https://primary.example/v1");
        invalid_initial.meta = Some(crate::provider::ProviderMeta {
            custom_endpoints: std::collections::HashMap::from([(
                bad_url.to_string(),
                crate::settings::CustomEndpoint {
                    url: bad_url.to_string(),
                    added_at: Some(1),
                    last_used: None,
                },
            )]),
            ..Default::default()
        });
        let error = PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: invalid_initial,
                provider_key: "invalid-initial".to_string(),
                activate_if_first: false,
            },
        )
        .expect_err("initial gateway endpoints must be validated before create");
        assert!(matches!(error, AppError::InvalidInput(_)));
        assert!(!error.to_string().contains("secret"));
        assert!(state
            .db
            .get_provider_aggregate(PI_APP, "invalid-initial")?
            .is_none());

        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: managed_input("endpoint-owner", "https://primary.example/v1"),
                provider_key: "endpoint-owner".to_string(),
                activate_if_first: false,
            },
        )?;
        let error = PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::AddEndpoint {
                provider_id: "endpoint-owner".to_string(),
                url: bad_url.to_string(),
            },
        )
        .expect_err("an unusable endpoint must not be persisted");
        assert!(matches!(error, AppError::InvalidInput(_)));
        assert!(!error.to_string().contains("secret"));
        assert!(state
            .db
            .get_provider_aggregate(PI_APP, "endpoint-owner")?
            .expect("provider remains")
            .endpoints
            .is_empty());

        // A database restored from an older build may already contain this
        // value. Runtime quarantines it, while deletion remains total over the
        // persisted endpoint domain so the user can repair the row.
        let key = ProviderKey::new(PI_APP, "endpoint-owner")?;
        state
            .db
            .add_provider_endpoint(&key, NewEndpoint::new(bad_url, Some(2), None)?)?;
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::RemoveEndpoint {
                provider_id: "endpoint-owner".to_string(),
                url: bad_url.to_string(),
            },
        )?;
        assert!(state
            .db
            .get_provider_aggregate(PI_APP, "endpoint-owner")?
            .expect("provider remains")
            .endpoints
            .is_empty());
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn native_import_revalidates_exact_values_before_claiming_success() -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        let pi_dir = temp.path().join("pi-agent");
        std::fs::create_dir_all(&pi_dir).expect("Pi directory");
        configure_pi_directory(&pi_dir)?;
        let state = AppState::new(Arc::new(Database::memory()?));
        let models_path = pi_dir.join("models.json");
        let original = json!({
            "name": "Native import",
            "api": "openai-responses",
            "baseUrl": "https://original.example/v1",
            "apiKey": "literal-key",
            "models": [{"id": "model-a", "name": "Model A"}]
        });
        std::fs::write(
            &models_path,
            serde_json::to_vec_pretty(&json!({"providers": {"native": original}}))
                .expect("serialize original"),
        )
        .expect("write original");
        let fingerprint = inspect_pi_native_entry(&models_path, "native", &BTreeMap::new())?
            .expect("native entry")
            .diagnostic
            .fingerprint;
        let external = serde_json::to_vec_pretty(&json!({
            "providers": {
                "native": {
                    "name": "External replacement",
                    "api": "openai-responses",
                    "baseUrl": "https://external.example/v1",
                    "apiKey": "external-key",
                    "models": [{"id": "external-model"}]
                }
            }
        }))
        .expect("serialize external");
        crate::pi_config::document::replace_before_next_pi_provider_verify(&models_path, &external);

        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::ImportNative {
                provider_key: "native".to_string(),
                expected_fingerprint: fingerprint,
            },
        )
        .expect_err("an external edit before the final barrier must reject import");
        assert!(state.db.get_provider_aggregate(PI_APP, "native")?.is_none());
        assert!(state.db.get_pi_projection("native")?.is_none());
        assert_eq!(
            std::fs::read(&models_path).expect("external native file"),
            external,
            "the external writer remains authoritative"
        );
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn native_import_revalidates_raw_fingerprint_after_semantically_equivalent_edit(
    ) -> Result<(), AppError> {
        let temp = tempfile::tempdir().expect("tempdir");
        let _home = TestHome::install(temp.path())?;
        let pi_dir = temp.path().join("pi-agent");
        std::fs::create_dir_all(&pi_dir).expect("Pi directory");
        configure_pi_directory(&pi_dir)?;
        let state = AppState::new(Arc::new(Database::memory()?));
        let models_path = pi_dir.join("models.json");
        let original = br#"{
  "providers": {
    "native": {
      "name": "Native import",
      "api": "openai-responses",
      "baseUrl": "https://original.example/v1",
      "apiKey": "literal-key",
      "models": [{"id": "model-a", "name": "Model A"}]
    }
  }
}"#;
        std::fs::write(&models_path, original).expect("write original");
        let fingerprint = inspect_pi_native_entry(&models_path, "native", &BTreeMap::new())?
            .expect("native entry")
            .diagnostic
            .fingerprint;
        let external = br#"{
  "providers": {
    "native": {
      // raw ownership changed while parsed values stayed identical
      "name": "Native import",
      "api": "openai-responses",
      "baseUrl": "https://original.example/v1",
      "apiKey": "literal-key",
      "models": [
        {"id": "model-a", "name": "Model A"},
      ],
    },
  },
}"#;
        crate::pi_config::document::replace_before_next_pi_provider_verify(&models_path, external);

        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::ImportNative {
                provider_key: "native".to_string(),
                expected_fingerprint: fingerprint,
            },
        )
        .expect_err("a raw-only external edit must reject the ownership claim");
        assert!(state.db.get_provider_aggregate(PI_APP, "native")?.is_none());
        assert!(state.db.get_pi_projection("native")?.is_none());
        assert_eq!(
            std::fs::read(&models_path).expect("external native file"),
            external,
            "raw external ownership must remain intact"
        );
        Ok(())
    }

    #[test]
    fn portable_sql_import_claims_only_an_absent_identity_key_and_is_idempotent(
    ) -> Result<(), AppError> {
        let source = Database::memory()?;
        let expected = insert_portable_pi_provider(&source, "portable-pi")?;
        let exported = source.export_sql_string()?;
        assert!(!exported.contains("INSERT INTO \"pi_provider_projections\""));

        let target = Arc::new(Database::memory()?);
        target.import_sql_string(&exported)?;
        assert!(target.get_pi_projection("portable-pi")?.is_none());
        let state = AppState::new(target.clone());
        let temp = tempfile::tempdir().expect("tempdir");
        let models_path = temp.path().join("models.json");

        let project_direct =
            |_: &str, _: &str, config: &PiManagedProviderConfig| -> Result<Value, AppError> {
                serde_json::to_value(config).map_err(|source| AppError::JsonSerialize { source })
            };
        PiCatalogCoordinator::reconcile_portable_catalog_at(&state, &models_path, project_direct)?;
        PiCatalogCoordinator::reconcile_portable_catalog_at(&state, &models_path, project_direct)?;

        let projection = target
            .get_pi_projection("portable-pi")?
            .expect("projection");
        assert_eq!(projection.provider_key, "portable-pi");
        let document: Value =
            serde_json::from_slice(&std::fs::read(&models_path).expect("read models"))
                .expect("parse models");
        assert_eq!(
            document.pointer("/providers/portable-pi"),
            Some(&expected),
            "all portable provider fields must survive SQL import and native publication"
        );
        Ok(())
    }

    #[test]
    fn portable_import_cannot_claim_a_pi_builtin_provider_key() -> Result<(), AppError> {
        let db = Arc::new(Database::memory()?);
        insert_portable_pi_provider(&db, "anthropic")?;
        let state = AppState::new(db.clone());
        let temp = tempfile::tempdir().expect("tempdir");
        let models_path = temp.path().join("models.json");

        let error = PiCatalogCoordinator::reconcile_portable_catalog_at(
            &state,
            &models_path,
            |_, _, config| {
                serde_json::to_value(config).map_err(|source| AppError::JsonSerialize { source })
            },
        )
        .expect_err("portable data cannot claim a Pi-owned key");

        assert!(error.to_string().contains("owned by Pi"));
        assert!(db.get_pi_projection("anthropic")?.is_none());
        assert!(!models_path.exists());
        Ok(())
    }

    #[test]
    fn portable_import_never_claims_or_overwrites_an_unowned_native_key() -> Result<(), AppError> {
        let db = Arc::new(Database::memory()?);
        insert_portable_pi_provider(&db, "occupied")?;
        let state = AppState::new(db.clone());
        let temp = tempfile::tempdir().expect("tempdir");
        let models_path = temp.path().join("models.json");
        let original = json!({
            "providers": {
                "occupied": {
                    "api": "anthropic-messages",
                    "baseUrl": "https://native.example",
                    "apiKey": "native-secret",
                    "models": [{"id": "native-model"}]
                }
            }
        });
        std::fs::write(
            &models_path,
            serde_json::to_vec_pretty(&original).expect("serialize"),
        )
        .expect("write models");

        let error = PiCatalogCoordinator::reconcile_portable_catalog_at(
            &state,
            &models_path,
            |_, _, config| {
                serde_json::to_value(config).map_err(|source| AppError::JsonSerialize { source })
            },
        )
        .expect_err("unowned native value must block import publication");
        assert!(error.to_string().contains("unclaimed native key"));
        assert!(db.get_pi_projection("occupied")?.is_none());
        let after: Value =
            serde_json::from_slice(&std::fs::read(&models_path).expect("read models"))
                .expect("parse models");
        assert_eq!(after, original);
        Ok(())
    }

    #[test]
    fn portable_import_releases_orphan_claim_but_preserves_its_native_value() -> Result<(), AppError>
    {
        let source = Database::memory()?;
        let remote = insert_portable_pi_provider(&source, "remote")?;
        let exported = source.export_sql_string()?;

        let target = Arc::new(Database::memory()?);
        insert_portable_pi_provider(&target, "local")?;
        target.claim_pi_projection_key("local", "local-native")?;
        target.import_sql_string(&exported)?;
        assert!(
            target.get_provider_aggregate(PI_APP, "local")?.is_none(),
            "portable provider rows must be replaced"
        );
        // Canonical restore may preserve this device-local table, while the
        // legacy importer rebuilds it empty. Seed the post-import state
        // explicitly so this Pi-layer test continues to certify the invariant
        // without making Canonical Restore a prerequisite of Pi support.
        target.claim_pi_projection_key("local", "local-native")?;
        assert!(
            target.get_pi_projection("local")?.is_some(),
            "the reconciliation input must contain an orphan device-local claim"
        );

        let state = AppState::new(target.clone());
        let temp = tempfile::tempdir().expect("tempdir");
        let models_path = temp.path().join("models.json");
        let local_native = json!({
            "api": "anthropic-messages",
            "baseUrl": "https://local.example",
            "apiKey": "local-secret",
            "models": [{"id": "local-model"}]
        });
        std::fs::write(
            &models_path,
            serde_json::to_vec_pretty(&json!({
                "providers": {"local-native": local_native.clone()}
            }))
            .expect("serialize"),
        )
        .expect("write models");

        PiCatalogCoordinator::reconcile_portable_catalog_at(
            &state,
            &models_path,
            |_, _, config| {
                serde_json::to_value(config).map_err(|source| AppError::JsonSerialize { source })
            },
        )?;

        let manifest = target.get_pi_projection_manifest()?;
        assert_eq!(manifest.len(), 1);
        assert_eq!(manifest["remote"].provider_key, "remote");
        let document: Value =
            serde_json::from_slice(&std::fs::read(&models_path).expect("read models"))
                .expect("parse models");
        assert_eq!(
            document.pointer("/providers/local-native"),
            Some(&local_native),
            "an orphaned exact-key value has no safe deletion proof and must become user-owned"
        );
        assert_eq!(document.pointer("/providers/remote"), Some(&remote));
        Ok(())
    }

    #[test]
    fn portable_import_of_empty_pi_catalog_releases_all_device_claims() -> Result<(), AppError> {
        let empty_source = Database::memory()?;
        {
            let conn = crate::database::lock_conn!(empty_source.conn);
            conn.execute(
                "INSERT INTO providers
                    (id, app_type, name, settings_config, meta, is_current)
                 VALUES ('non-pi-sentinel', 'claude', 'Non-Pi sentinel', '{}', '{}', 0)",
                [],
            )
            .map_err(|error| AppError::Database(error.to_string()))?;
        }
        let exported = empty_source.export_sql_string()?;
        let target = Arc::new(Database::memory()?);
        insert_portable_pi_provider(&target, "local")?;
        target.claim_pi_projection_key("local", "local-native")?;
        target.import_sql_string(&exported)?;

        let state = AppState::new(target.clone());
        let temp = tempfile::tempdir().expect("tempdir");
        let models_path = temp.path().join("models.json");
        let source = json!({"providers": {"local-native": {"models": [{"id": "m"}]}}});
        std::fs::write(
            &models_path,
            serde_json::to_vec_pretty(&source).expect("serialize"),
        )
        .expect("models");

        PiCatalogCoordinator::reconcile_portable_catalog_at(
            &state,
            &models_path,
            |_, _, config| {
                serde_json::to_value(config).map_err(|source| AppError::JsonSerialize { source })
            },
        )?;

        assert!(target.get_pi_projection_manifest()?.is_empty());
        assert_eq!(
            serde_json::from_slice::<Value>(&std::fs::read(&models_path).expect("read models"))
                .expect("parse"),
            source,
            "empty portable catalogs must not create, rewrite, or delete native values"
        );
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn catalog_projection_runtime_failures_restore_new_normalized_keys_and_file_absence(
    ) -> Result<(), AppError> {
        struct HomeGuard(Option<std::ffi::OsString>);
        impl Drop for HomeGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
                    None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
                }
                let _ = crate::settings::reload_settings();
            }
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard(std::env::var_os("CC_SWITCH_TEST_HOME"));
        std::env::set_var("CC_SWITCH_TEST_HOME", temp.path());
        crate::settings::reload_settings()?;
        let pi_dir = temp.path().join("pi-agent");
        let mut app_settings = crate::settings::get_settings();
        app_settings.pi_config_dir = Some(pi_dir.to_string_lossy().into_owned());
        app_settings.pi_takeover_enabled = false;
        crate::settings::update_settings(app_settings)?;

        let db = Arc::new(Database::memory()?);
        insert_portable_pi_provider(&db, "portable-pi")?;
        let state = AppState::new(db.clone());
        state.proxy_service.fail_next_pi_reconcile_for_test();
        let error = PiCatalogCoordinator::reconcile_portable_import(&state)
            .expect_err("injected runtime publication failure");
        assert!(error.to_string().contains("previous catalog was restored"));
        assert!(
            db.get_pi_projection("portable-pi")?.is_none(),
            "a failed portable projection must release its new exact-key claim"
        );
        assert!(
            !pi_dir.join("models.json").exists(),
            "a failed portable projection must restore a previously absent native file"
        );
        assert!(
            db.get_provider_aggregate(PI_APP, "portable-pi")?.is_some(),
            "portable provider rows predate reconciliation and remain authoritative"
        );

        state.proxy_service.fail_next_pi_reconcile_for_test();
        let error = PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: managed_input("new-provider", "https://new.example/v1"),
                provider_key: "  new-native  ".to_string(),
                activate_if_first: false,
            },
        )
        .expect_err("injected create publication failure");
        assert!(error.to_string().contains("previous catalog was restored"));
        assert!(db.get_provider_aggregate(PI_APP, "new-provider")?.is_none());
        assert!(db.get_pi_projection("new-provider")?.is_none());
        assert!(
            !pi_dir.join("models.json").exists(),
            "the normalized additional create key must be part of the rollback snapshot"
        );
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn external_native_switch_repairs_indexes_before_deleting_the_inactive_provider(
    ) -> Result<(), AppError> {
        struct HomeGuard(Option<std::ffi::OsString>);
        impl Drop for HomeGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
                    None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
                }
                let _ = crate::settings::reload_settings();
            }
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard(std::env::var_os("CC_SWITCH_TEST_HOME"));
        std::env::set_var("CC_SWITCH_TEST_HOME", temp.path());
        crate::settings::reload_settings()?;
        let pi_dir = temp.path().join("pi-agent");
        let mut app_settings = crate::settings::get_settings();
        app_settings.pi_config_dir = Some(pi_dir.to_string_lossy().into_owned());
        app_settings.pi_takeover_enabled = false;
        crate::settings::update_settings(app_settings)?;

        let db = Arc::new(Database::memory()?);
        let state = AppState::new(db.clone());
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: managed_input("provider-a", "https://a.example/v1"),
                provider_key: "native-a".to_string(),
                activate_if_first: true,
            },
        )?;
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: managed_input("provider-b", "https://b.example/v1"),
                provider_key: "native-b".to_string(),
                activate_if_first: true,
            },
        )?;
        assert_eq!(
            settings::get_current_provider(&AppType::Pi).as_deref(),
            Some("provider-a")
        );
        assert_eq!(
            db.get_current_provider(PI_APP)?.as_deref(),
            Some("provider-a")
        );

        crate::pi_config::native_settings::set_pi_native_default_with_receipt(
            "native-b", "model-a",
        )?;
        assert_eq!(
            PiCatalogCoordinator::current_native_provider(&state)?.as_deref(),
            Some("provider-b"),
            "native settings must immediately drive displayed current state"
        );

        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::DeleteProvider {
                provider_id: "provider-a".to_string(),
            },
        )?;
        assert!(db.get_provider_aggregate(PI_APP, "provider-a")?.is_none());
        assert!(db.get_provider_aggregate(PI_APP, "provider-b")?.is_some());
        assert_eq!(
            settings::get_current_provider(&AppType::Pi).as_deref(),
            Some("provider-b")
        );
        assert_eq!(
            db.get_current_provider(PI_APP)?.as_deref(),
            Some("provider-b")
        );
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn runtime_publication_failure_restores_endpoint_database_and_native_snapshot(
    ) -> Result<(), AppError> {
        struct HomeGuard(Option<std::ffi::OsString>);
        impl Drop for HomeGuard {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
                    None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
                }
                let _ = crate::settings::reload_settings();
            }
        }

        let temp = tempfile::tempdir().expect("tempdir");
        let _home = HomeGuard(std::env::var_os("CC_SWITCH_TEST_HOME"));
        std::env::set_var("CC_SWITCH_TEST_HOME", temp.path());
        crate::settings::reload_settings()?;
        let pi_dir = temp.path().join("pi-agent");
        let mut app_settings = crate::settings::get_settings();
        app_settings.pi_config_dir = Some(pi_dir.to_string_lossy().into_owned());
        app_settings.pi_takeover_enabled = false;
        crate::settings::update_settings(app_settings)?;

        let db = Arc::new(Database::memory()?);
        let state = AppState::new(db.clone());
        PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::CreateProvider {
                input: managed_input("provider-a", "https://a.example/v1"),
                provider_key: "native-a".to_string(),
                activate_if_first: true,
            },
        )?;
        let before_aggregate = db
            .get_provider_aggregate(PI_APP, "provider-a")?
            .expect("provider");
        futures::executor::block_on(db.update_provider_health(
            "provider-a",
            PI_APP,
            false,
            Some("captured health".to_string()),
        ))?;
        let before_health =
            futures::executor::block_on(db.get_provider_health("provider-a", PI_APP))?;
        let models_path = get_pi_models_path()?;
        let before_models = std::fs::read(&models_path).expect("models");

        state.proxy_service.fail_next_pi_reconcile_for_test();
        let error = PiCatalogCoordinator::apply(
            &state,
            PiCatalogMutation::AddEndpoint {
                provider_id: "provider-a".to_string(),
                url: "https://endpoint.example/v1".to_string(),
            },
        )
        .expect_err("injected publication failure");
        assert!(error.to_string().contains("previous catalog was restored"));

        let after_aggregate = db
            .get_provider_aggregate(PI_APP, "provider-a")?
            .expect("provider remains");
        assert_eq!(
            serde_json::to_value(after_aggregate).expect("after aggregate"),
            serde_json::to_value(before_aggregate).expect("before aggregate")
        );
        assert_eq!(
            std::fs::read(models_path).expect("models after"),
            before_models
        );
        let after_health =
            futures::executor::block_on(db.get_provider_health("provider-a", PI_APP))?;
        assert_eq!(
            serde_json::to_value(after_health).expect("after health"),
            serde_json::to_value(before_health).expect("before health"),
            "catalog compensation must not cascade-delete provider health history"
        );
        Ok(())
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn route_sort_publishes_an_even_runtime_without_rewriting_native_models() {
        let result: Result<(), AppError> = async {
            struct HomeGuard(Option<std::ffi::OsString>);
            impl Drop for HomeGuard {
                fn drop(&mut self) {
                    match self.0.take() {
                        Some(value) => std::env::set_var("CC_SWITCH_TEST_HOME", value),
                        None => std::env::remove_var("CC_SWITCH_TEST_HOME"),
                    }
                    let _ = crate::settings::reload_settings();
                }
            }

            let temp = tempfile::tempdir().expect("tempdir");
            let _home = HomeGuard(std::env::var_os("CC_SWITCH_TEST_HOME"));
            std::env::set_var("CC_SWITCH_TEST_HOME", temp.path());
            crate::settings::reload_settings()?;
            let mut app_settings = crate::settings::get_settings();
            app_settings.pi_config_dir =
                Some(temp.path().join("pi-agent").to_string_lossy().into_owned());
            app_settings.pi_takeover_enabled = false;
            crate::settings::update_settings(app_settings)?;

            let db = Arc::new(Database::memory()?);
            let state = AppState::new(db.clone());
            for (id, key) in [("provider-a", "native-a"), ("provider-b", "native-b")] {
                PiCatalogCoordinator::apply(
                    &state,
                    PiCatalogMutation::CreateProvider {
                        input: managed_input(id, &format!("https://{id}.example/v1")),
                        provider_key: key.to_string(),
                        activate_if_first: true,
                    },
                )?;
            }
            state
                .proxy_service
                .set_takeover_for_app(PI_APP, true)
                .await
                .map_err(AppError::Message)?;
            let models_path = get_pi_models_path()?;
            let before_models = std::fs::read(&models_path).expect("models");

            PiCatalogCoordinator::update_route_order(
                &state,
                vec![
                    (ProviderKey::new(PI_APP, "provider-b")?, 0),
                    (ProviderKey::new(PI_APP, "provider-a")?, 1),
                ],
            )?;

            assert_eq!(
                std::fs::read(&models_path).expect("models after sort"),
                before_models,
                "sorting is DB/runtime-only and must not rewrite Pi's shared file"
            );
            let aggregates = db.get_all_provider_aggregates(PI_APP)?;
            assert_eq!(aggregates["provider-b"].provider.sort_index, Some(0));
            assert_eq!(aggregates["provider-a"].provider.sort_index, Some(1));
            assert_eq!(
                state
                    .proxy_service
                    .get_takeover_status()
                    .await
                    .map_err(AppError::Message)?
                    .pi_operational_state,
                crate::proxy::types::PiTakeoverOperationalState::Active,
                "sort publication must never leave the runtime at an odd admission epoch"
            );

            let mut corrupted_settings = crate::settings::get_settings();
            corrupted_settings.pi_gateway_token = None;
            crate::settings::update_settings(corrupted_settings)?;
            let missing_token = PiCatalogCoordinator::update_route_order(
                &state,
                vec![
                    (ProviderKey::new(PI_APP, "provider-a")?, 0),
                    (ProviderKey::new(PI_APP, "provider-b")?, 1),
                ],
            )
            .expect_err("pure sorting must not silently rotate the gateway credential");
            assert!(missing_token
                .to_string()
                .contains("credential is unavailable"));
            let aggregates = db.get_all_provider_aggregates(PI_APP)?;
            assert_eq!(aggregates["provider-b"].provider.sort_index, Some(0));
            assert_eq!(aggregates["provider-a"].provider.sort_index, Some(1));
            assert_eq!(
                std::fs::read(&models_path).expect("models after rejected sort"),
                before_models
            );
            state
                .proxy_service
                .set_takeover_for_app(PI_APP, false)
                .await
                .map_err(AppError::Message)?;
            Ok(())
        }
        .await;
        result.expect("route sort");
    }
}

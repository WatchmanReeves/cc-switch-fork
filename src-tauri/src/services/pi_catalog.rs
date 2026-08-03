//! Ordered mutations for Pi's managed provider catalog.
//!
//! SQLite is the managed aggregate authority, `pi_provider_projections` owns
//! exact keys in Pi's shared `models.json`, and Pi's `settings.json` owns the
//! native default. Every public mutation acquires the same Pi switch boundary;
//! callers must not compose the database and native-file primitives directly.

use crate::app_config::AppType;
use crate::database::{
    NewEndpoint, NewProviderAggregate, PiProviderProjection, ProviderKey, ProviderRowUpdate,
};
use crate::error::AppError;
use crate::pi_config::document::{
    apply_pi_provider_patch_with_receipt, snapshot_pi_provider_values, PiProviderPatchReceipt,
    PiProviderValuesSnapshot,
};
use crate::pi_config::gateway::parse_pi_gateway_endpoint;
use crate::pi_config::model::{
    effective_pi_model, validate_pi_managed_provider, PiManagedProviderConfig, PiManagementStatus,
};
use crate::pi_config::native::{
    get_pi_models_path, inspect_pi_native_entry, PiNativeInspectionService,
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
        let additional_native_key = match &mutation {
            PiCatalogMutation::CreateProvider { provider_key, .. }
            | PiCatalogMutation::ImportNative { provider_key, .. } => Some(provider_key.clone()),
            _ => None,
        };
        Self::run_with_runtime_reconcile(state, additional_native_key.as_deref(), || match mutation
        {
            PiCatalogMutation::CreateProvider {
                input,
                provider_key,
                activate_if_first,
            } => Self::create(state, input, provider_key, activate_if_first),
            PiCatalogMutation::UpdateProvider { input } => Self::update(state, input),
            PiCatalogMutation::DeleteProvider { provider_id } => Self::delete(state, &provider_id),
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
        })
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
        let _switch_guard = futures::executor::block_on(
            state
                .proxy_service
                .lock_switch_for_app(AppType::Pi.as_str()),
        );
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
        let reconcile = futures::executor::block_on(
            state
                .proxy_service
                .reconcile_pi_runtime_at_epoch_with_native_claim_precondition(
                    catalog_epoch,
                    Some(&expected_native),
                    (!native_fingerprint_preconditions.is_empty())
                        .then_some(&native_fingerprint_preconditions),
                ),
        );
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

        // Portable restore replaces provider rows but deliberately preserves
        // the device-local exact-key ledger. Claims whose providers disappeared
        // must be released before publishing a runtime, otherwise the manifest
        // and provider aggregate sets can never converge. Their native values
        // remain untouched and become user-owned; without a retained provider
        // aggregate there is no safe expected value with which to delete them.
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
            match apply_pi_provider_patch_with_receipt(models_path, &before_file, &patch) {
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
        let catalog_was_empty = state.db.get_all_providers(PI_APP)?.is_empty();
        let provider_key = non_empty_native_key(&provider_key)?;
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
        let native_patch_receipt =
            match apply_pi_provider_patch_with_receipt(&models_path, &before_file, &projection) {
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
        let previous_defaults = read_pi_native_defaults()?;
        let previous_db = state.db.get_current_provider(PI_APP)?;
        let was_current = previous_db.as_deref() == Some(&provider_id);
        let models_path = get_pi_models_path()?;
        let before_file =
            snapshot_pi_provider_values(&models_path, [projection.provider_key.clone()])?;
        let projected = state.proxy_service.project_pi_provider_value(
            &provider_id,
            &projection.provider_key,
            &config,
        )?;

        let key = ProviderKey::new(PI_APP, provider_id.clone())?;
        state
            .db
            .update_pi_catalog_provider(&key, &ProviderRowUpdate::from_input(&input)?)?;
        let patch = IndexMap::from([(projection.provider_key.clone(), Some(projected))]);
        let native_patch_receipt =
            match apply_pi_provider_patch_with_receipt(&models_path, &before_file, &patch) {
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
        let projection = state
            .db
            .get_pi_projection(provider_id)?
            .ok_or_else(|| AppError::NotFound(format!("Pi provider '{provider_id}'")))?;
        let previous = state
            .db
            .get_provider_aggregate(PI_APP, provider_id)?
            .ok_or_else(|| AppError::NotFound(format!("Pi provider '{provider_id}'")))?;
        let db_current = state.db.get_current_provider(PI_APP)?;
        let native_defaults = read_pi_native_defaults()?;
        if native_defaults.default_provider.as_deref() == Some(&projection.provider_key) {
            return Err(AppError::Conflict(
                "the active Pi provider cannot be deleted".to_string(),
            ));
        }

        let models_path = get_pi_models_path()?;
        let before_file =
            snapshot_pi_provider_values(&models_path, [projection.provider_key.clone()])?;
        let was_current = db_current.as_deref() == Some(provider_id);
        state.db.delete_pi_catalog_provider(provider_id)?;
        let patch = IndexMap::from([(projection.provider_key.clone(), None)]);
        let native_patch_receipt =
            match apply_pi_provider_patch_with_receipt(&models_path, &before_file, &patch) {
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
    let Some(projection) = state.db.get_pi_projection_for_key(provider_key)? else {
        return Ok(None);
    };
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
        assert!(
            target.get_pi_projection("local")?.is_some(),
            "the device-local claim is intentionally preserved by restore"
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

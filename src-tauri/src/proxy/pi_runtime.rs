//! Immutable Pi gateway catalog and native projection planning.
//!
//! The database remains the managed provider authority. A snapshot is built
//! from complete provider aggregates, exact-key ownership claims, and a stable
//! device token; only after the matching `models.json` patch succeeds is that
//! snapshot published for request admission.

use crate::database::Database;
use crate::error::AppError;
use crate::pi_config::composer::PiComposedNativeModel;
use crate::pi_config::gateway::{
    assess_composition_for_runtime, CandidateHeaderPlan, MaterializedCandidate, PiGatewayApiFamily,
    PiGatewayCapability, PiGatewayReason,
};
use crate::pi_config::model::PiManagedProviderConfig;
use crate::pi_config::native::compose_managed_pi_provider;
use crate::provider::ProviderAggregate;
use crate::proxy::types::AppProxyConfig;
use crate::settings::GatewayToken;
use indexmap::IndexMap;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedRwLockReadGuard, RwLock as AsyncRwLock};
use url::Url;

const PI_APP: &str = "pi";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const COMMAND_OUTPUT_LIMIT: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
struct PiRuntimeModel {
    provider_id: String,
    provider_name: String,
    family: PiGatewayApiFamily,
    wire_profile: Vec<u8>,
    plan: CandidateHeaderPlan,
    custom_endpoint_plans: Vec<CandidateHeaderPlan>,
}

#[derive(Debug, Clone)]
struct PiRuntimeProvider {
    models: HashMap<String, PiRuntimeModel>,
}

#[derive(Debug, Clone)]
struct PiRouteBinding {
    provider_id: String,
}

#[derive(Clone)]
struct PiNativeProjectionWitness(IndexMap<String, Option<Value>>);

impl fmt::Debug for PiNativeProjectionWitness {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let keys = self
            .0
            .iter()
            .map(|(key, value)| (key, if value.is_some() { "present" } else { "absent" }))
            .collect::<Vec<_>>();
        formatter
            .debug_tuple("PiNativeProjectionWitness")
            .field(&keys)
            .finish()
    }
}

/// One immutable catalog matching a successfully published native projection.
#[derive(Clone)]
pub(crate) struct PiRuntimeSnapshot {
    pub(crate) server_generation: u64,
    pub(crate) catalog_epoch: u64,
    gateway_token: GatewayToken,
    /// Exact native values which made this immutable runtime reachable.
    ///
    /// A fenced runtime may only be re-published while these values still
    /// match `models.json`; retaining the projection alongside the routes
    /// avoids reconstructing ownership from mutable database/settings state.
    native_projection: PiNativeProjectionWitness,
    app_config: AppProxyConfig,
    providers: HashMap<String, PiRuntimeProvider>,
    failover_ids: Vec<String>,
    routes: HashMap<String, PiRouteBinding>,
}

impl fmt::Debug for PiRuntimeSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PiRuntimeSnapshot")
            .field("server_generation", &self.server_generation)
            .field("catalog_epoch", &self.catalog_epoch)
            .field("gateway_token", &self.gateway_token)
            .field("native_projection", &self.native_projection)
            .field("provider_count", &self.providers.len())
            .field("failover_ids", &self.failover_ids)
            .field("route_count", &self.routes.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PiRequestCandidate {
    pub(crate) provider_id: String,
    pub(crate) provider_name: String,
    pub(crate) family: PiGatewayApiFamily,
    pub(crate) plan: CandidateHeaderPlan,
    pub(crate) is_failover: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct PiRequestRoute {
    pub(crate) catalog_epoch: u64,
    pub(crate) app_config: AppProxyConfig,
    pub(crate) candidates: Vec<PiRequestCandidate>,
}

#[derive(Debug)]
pub(crate) struct PiMaterializedAttempt {
    pub(crate) provider_id: String,
    pub(crate) provider_name: String,
    pub(crate) is_failover: bool,
    pub(crate) transport: MaterializedCandidate,
    pub(crate) url: Url,
}

pub(crate) struct PiRuntimeBuild {
    pub(crate) snapshot: Arc<PiRuntimeSnapshot>,
    /// Exact keys only. A direct-only provider deliberately keeps its original
    /// database projection while proxyable siblings point at the gateway.
    pub(crate) projection_patch: IndexMap<String, Option<Value>>,
    pub(crate) direct_only_provider_ids: Vec<String>,
}

impl PiRuntimeSnapshot {
    pub(crate) fn token_matches(&self, candidate: &str) -> bool {
        self.gateway_token.constant_time_eq(candidate)
    }

    pub(crate) fn route(
        &self,
        route_token: &str,
        family: PiGatewayApiFamily,
        model_id: &str,
    ) -> Result<PiRequestRoute, AppError> {
        let binding = self
            .routes
            .get(route_token)
            .ok_or_else(|| AppError::NotFound("unknown Pi gateway provider route".to_string()))?;
        let primary = self
            .providers
            .get(&binding.provider_id)
            .and_then(|provider| provider.models.get(model_id))
            .filter(|model| model.family == family)
            .ok_or_else(|| {
                AppError::InvalidInput(format!(
                    "Pi provider '{}' does not expose model '{model_id}' for {}",
                    binding.provider_id,
                    family.as_str()
                ))
            })?;

        let mut candidates = expand_model_attempts(primary, false);
        if self.app_config.auto_failover_enabled {
            for provider_id in &self.failover_ids {
                if provider_id == &binding.provider_id {
                    continue;
                }
                let Some(candidate) = self
                    .providers
                    .get(provider_id)
                    .and_then(|provider| provider.models.get(model_id))
                else {
                    continue;
                };
                if candidate.family != family
                    || candidate.wire_profile != primary.wire_profile
                    || !candidate.plan.protocol_identity_is_predictable()
                {
                    continue;
                }
                candidates.extend(expand_model_attempts(candidate, true));
            }
        }

        Ok(PiRequestRoute {
            catalog_epoch: self.catalog_epoch,
            app_config: self.app_config.clone(),
            candidates,
        })
    }
}

fn expand_model_attempts(model: &PiRuntimeModel, is_failover: bool) -> Vec<PiRequestCandidate> {
    let mut plans = Vec::with_capacity(model.custom_endpoint_plans.len().saturating_add(1));
    plans.push(model.plan.clone());
    plans.extend(model.custom_endpoint_plans.iter().cloned());
    plans
        .into_iter()
        .map(|plan| PiRequestCandidate {
            provider_id: model.provider_id.clone(),
            provider_name: model.provider_name.clone(),
            family: model.family,
            plan,
            is_failover,
        })
        .collect()
}

impl PiRequestCandidate {
    pub(crate) fn materialize(
        self,
        forwarded_path_and_query: &str,
    ) -> Result<PiMaterializedAttempt, AppError> {
        let resolver_failure = std::cell::Cell::new(false);
        let transport = self
            .plan
            .materialize_for_runtime(&|expression: &str| {
                let resolved = resolve_pi_config_value(expression);
                resolver_failure.set(resolver_failure.get() || resolved.is_err());
                resolved.ok()
            })
            .map_err(|reason| {
                if resolver_failure.get() {
                    AppError::Config(
                        "failed to resolve a deferred Pi gateway credential or header".to_string(),
                    )
                } else {
                    gateway_reason(reason)
                }
            })?;
        let url = build_family_url(self.family, &transport.endpoint, forwarded_path_and_query)?;
        Ok(PiMaterializedAttempt {
            provider_id: self.provider_id,
            provider_name: self.provider_name,
            is_failover: self.is_failover,
            transport,
            url,
        })
    }

    pub(crate) fn protocol_identity_is_predictable(&self) -> bool {
        self.plan.protocol_identity_is_predictable()
    }

    pub(crate) fn planned_protocol_identity(
        &self,
    ) -> Result<Option<(String, http::HeaderMap)>, AppError> {
        let resolver_failure = std::cell::Cell::new(false);
        let identity = self
            .plan
            .materialize_protocol_identity(&|expression: &str| {
                // Protocol !commands are marked unpredictable before this
                // method is called. An auth command must not be run merely to
                // decide whether a circuit-skipped primary permits failover.
                if expression.starts_with('!') {
                    resolver_failure.set(true);
                    return None;
                }
                let resolved = resolve_pi_config_value(expression);
                resolver_failure.set(resolver_failure.get() || resolved.is_err());
                resolved.ok()
            })
            .map_err(|reason| {
                if resolver_failure.get() {
                    AppError::Config(
                        "failed to pre-resolve Pi primary protocol identity".to_string(),
                    )
                } else {
                    gateway_reason(reason)
                }
            })?;
        Ok(identity.map(|(family, headers)| (family.as_str().to_string(), headers)))
    }
}

fn gateway_reason(reason: PiGatewayReason) -> AppError {
    AppError::Config(format!(
        "Pi gateway candidate rejected at {}: {:?}",
        reason.json_pointer, reason.code
    ))
}

/// Build the immutable runtime and its exact native projection in one pass.
pub(crate) fn build_pi_runtime(
    db: &Database,
    server_generation: u64,
    catalog_epoch: u64,
    gateway_origin: &Url,
    gateway_token: GatewayToken,
    app_config: AppProxyConfig,
) -> Result<PiRuntimeBuild, AppError> {
    if catalog_epoch % 2 != 0 {
        return Err(AppError::Config(
            "Pi runtime publication requires an even catalog epoch".to_string(),
        ));
    }
    let aggregates = db.get_all_provider_aggregates(PI_APP)?;
    let manifest = db.get_pi_projection_manifest()?;
    if aggregates.len() != manifest.len()
        || aggregates
            .keys()
            .any(|provider_id| !manifest.contains_key(provider_id))
    {
        return Err(AppError::Conflict(
            "Pi provider aggregates and exact-key ownership claims diverged".to_string(),
        ));
    }

    let mut providers = HashMap::new();
    let mut routes = HashMap::new();
    let mut projection_patch = IndexMap::new();
    let mut direct_only_provider_ids = Vec::new();
    for (provider_id, aggregate) in aggregates {
        let projection = manifest.get(&provider_id).ok_or_else(|| {
            AppError::Conflict(format!(
                "Pi provider '{provider_id}' has no exact-key claim"
            ))
        })?;
        let config = decode_managed_config(&aggregate)?;
        let composition = compose_managed_pi_provider(&projection.provider_key, &config)?;
        let assessment = assess_composition_for_runtime(&composition);
        if assessment.capability != PiGatewayCapability::Proxyable
            || assessment.plans.len() != composition.models.len()
        {
            projection_patch.insert(
                projection.provider_key.clone(),
                Some(serde_json::to_value(&config).map_err(|source| {
                    AppError::Config(format!(
                        "failed to serialize direct-only Pi provider: {source}"
                    ))
                })?),
            );
            direct_only_provider_ids.push(provider_id);
            continue;
        }

        let token = pi_route_token(&provider_id, &projection.provider_key);
        let local_base = gateway_origin
            .join(&format!("pi/{token}"))
            .map_err(|error| AppError::Config(format!("invalid Pi gateway origin: {error}")))?;
        let endpoints = aggregate.endpoints.keys().cloned().collect::<Vec<_>>();
        let mut models = HashMap::new();
        for ((model, plan), expected) in composition
            .models
            .iter()
            .zip(assessment.plans)
            .zip(config.models.iter())
        {
            if model.id != expected.id {
                return Err(AppError::Config(format!(
                    "Pi composer changed managed model order for '{provider_id}'"
                )));
            }
            let family = plan.family();
            let runtime_model = runtime_model(&aggregate, model, family, plan, endpoints.clone())?;
            if models.insert(model.id.clone(), runtime_model).is_some() {
                return Err(AppError::Conflict(format!(
                    "duplicate Pi model '{}' in provider '{provider_id}'",
                    model.id
                )));
            }
        }
        if routes
            .insert(
                token,
                PiRouteBinding {
                    provider_id: provider_id.clone(),
                },
            )
            .is_some()
        {
            return Err(AppError::Conflict(
                "Pi gateway route digest collision".to_string(),
            ));
        }
        let projected = project_config_for_gateway(&config, &local_base, &gateway_token)?;
        projection_patch.insert(projection.provider_key.clone(), Some(projected));
        providers.insert(provider_id, PiRuntimeProvider { models });
    }

    let failover_ids = db
        .get_failover_queue(PI_APP)?
        .into_iter()
        .map(|item| item.provider_id)
        .collect();
    Ok(PiRuntimeBuild {
        snapshot: Arc::new(PiRuntimeSnapshot {
            server_generation,
            catalog_epoch,
            gateway_token,
            native_projection: PiNativeProjectionWitness(projection_patch.clone()),
            app_config,
            providers,
            failover_ids,
            routes,
        }),
        projection_patch,
        direct_only_provider_ids,
    })
}

pub(crate) fn direct_pi_projection_patch(
    db: &Database,
) -> Result<IndexMap<String, Option<Value>>, AppError> {
    let aggregates = db.get_all_provider_aggregates(PI_APP)?;
    let manifest = db.get_pi_projection_manifest()?;
    if aggregates.len() != manifest.len() {
        return Err(AppError::Conflict(
            "Pi provider aggregates and exact-key claims diverged".to_string(),
        ));
    }
    let mut patch = IndexMap::new();
    for (provider_id, aggregate) in aggregates {
        let projection = manifest.get(&provider_id).ok_or_else(|| {
            AppError::Conflict(format!(
                "Pi provider '{provider_id}' has no exact-key claim"
            ))
        })?;
        let config = decode_managed_config(&aggregate)?;
        patch.insert(
            projection.provider_key.clone(),
            Some(serde_json::to_value(config).map_err(|source| {
                AppError::Config(format!("failed to serialize Pi provider: {source}"))
            })?),
        );
    }
    Ok(patch)
}

/// Render one managed provider for an in-progress catalog mutation. This is
/// the same planning boundary used by the full runtime build, so the
/// coordinator never carries a second notion of "proxyable".
pub(crate) fn project_managed_pi_config(
    provider_id: &str,
    provider_key: &str,
    config: &PiManagedProviderConfig,
    gateway_origin: &Url,
    gateway_token: &GatewayToken,
) -> Result<Value, AppError> {
    let composition = compose_managed_pi_provider(provider_key, config)?;
    let assessment = assess_composition_for_runtime(&composition);
    if assessment.capability != PiGatewayCapability::Proxyable
        || assessment.plans.len() != composition.models.len()
    {
        return serde_json::to_value(config).map_err(|source| AppError::JsonSerialize { source });
    }
    let token = pi_route_token(provider_id, provider_key);
    let local_base = gateway_origin
        .join(&format!("pi/{token}"))
        .map_err(|error| AppError::Config(format!("invalid Pi gateway origin: {error}")))?;
    project_config_for_gateway(config, &local_base, gateway_token)
}

fn decode_managed_config(
    aggregate: &ProviderAggregate,
) -> Result<PiManagedProviderConfig, AppError> {
    serde_json::from_value(aggregate.provider.settings_config.clone()).map_err(|error| {
        AppError::Config(format!(
            "managed Pi provider '{}' is invalid: {error}",
            aggregate.provider.id
        ))
    })
}

fn runtime_model(
    aggregate: &ProviderAggregate,
    model: &PiComposedNativeModel,
    family: PiGatewayApiFamily,
    plan: CandidateHeaderPlan,
    endpoints: Vec<String>,
) -> Result<PiRuntimeModel, AppError> {
    let custom_endpoint_plans =
        build_custom_endpoint_plans(&plan, endpoints, &aggregate.provider.id);
    Ok(PiRuntimeModel {
        provider_id: aggregate.provider.id.clone(),
        provider_name: aggregate.provider.name.clone(),
        family,
        wire_profile: canonical_wire_profile(model)?,
        plan,
        custom_endpoint_plans,
    })
}

fn build_custom_endpoint_plans(
    primary: &CandidateHeaderPlan,
    endpoints: Vec<String>,
    provider_id: &str,
) -> Vec<CandidateHeaderPlan> {
    let mut plans = Vec::<CandidateHeaderPlan>::with_capacity(endpoints.len());
    for endpoint in endpoints {
        match primary.with_endpoint(&endpoint) {
            Ok(candidate)
                if candidate.endpoint() != primary.endpoint()
                    && !plans
                        .iter()
                        .any(|existing| existing.endpoint() == candidate.endpoint()) =>
            {
                plans.push(candidate);
            }
            Ok(_) => {}
            Err(reason) => {
                // The write boundary rejects these values. Keeping this
                // defensive compatibility path prevents an old/corrupt
                // auxiliary endpoint from disabling the valid primary route.
                // Never log the URL: it may contain the very userinfo which
                // caused rejection.
                log::warn!(
                    "ignoring invalid persisted Pi custom endpoint for provider \
                     '{provider_id}': {:?}",
                    reason.code
                );
            }
        }
    }
    plans
}

fn canonical_wire_profile(model: &PiComposedNativeModel) -> Result<Vec<u8>, AppError> {
    let mut profile = Map::new();
    profile.insert("reasoning".to_string(), Value::Bool(model.reasoning));
    profile.insert(
        "thinkingLevelMap".to_string(),
        model.thinking_level_map.clone().unwrap_or(Value::Null),
    );
    profile.insert("input".to_string(), model.input.clone());
    profile.insert("contextWindow".to_string(), model.context_window.clone());
    profile.insert("maxTokens".to_string(), model.max_tokens.clone());
    profile.insert(
        "compat".to_string(),
        model.compat.clone().unwrap_or(Value::Null),
    );
    profile.insert(
        "providerExtra".to_string(),
        serde_json::to_value(&model.provider_extra)
            .map_err(|source| AppError::JsonSerialize { source })?,
    );
    profile.insert(
        "modelExtra".to_string(),
        serde_json::to_value(&model.model_extra)
            .map_err(|source| AppError::JsonSerialize { source })?,
    );
    profile.insert(
        "overrideExtra".to_string(),
        serde_json::to_value(&model.override_extra)
            .map_err(|source| AppError::JsonSerialize { source })?,
    );
    serde_json::to_vec(&canonical_json(&Value::Object(profile)))
        .map_err(|source| AppError::JsonSerialize { source })
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        Value::Object(values) => {
            let sorted = values
                .iter()
                .map(|(key, value)| (key.clone(), canonical_json(value)))
                .collect::<BTreeMap<_, _>>();
            Value::Object(sorted.into_iter().collect())
        }
        scalar => scalar.clone(),
    }
}

fn project_config_for_gateway(
    config: &PiManagedProviderConfig,
    local_base: &Url,
    gateway_token: &GatewayToken,
) -> Result<Value, AppError> {
    let mut value =
        serde_json::to_value(config).map_err(|source| AppError::JsonSerialize { source })?;
    let root = value
        .as_object_mut()
        .ok_or_else(|| AppError::Config("Pi provider projection is not an object".to_string()))?;
    root.insert(
        "apiKey".to_string(),
        Value::String(gateway_token.expose().to_string()),
    );
    root.remove("headers");
    root.remove("authHeader");
    root.remove("oauth");
    let provider_has_base = root.contains_key("baseUrl");
    if provider_has_base {
        root.insert(
            "baseUrl".to_string(),
            Value::String(local_base.as_str().trim_end_matches('/').to_string()),
        );
    }
    let models = root
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| AppError::Config("Pi provider projection has no models".to_string()))?;
    for model in models {
        let object = model
            .as_object_mut()
            .ok_or_else(|| AppError::Config("Pi model projection is not an object".to_string()))?;
        object.remove("headers");
        if object.contains_key("baseUrl") || !provider_has_base {
            object.insert(
                "baseUrl".to_string(),
                Value::String(local_base.as_str().trim_end_matches('/').to_string()),
            );
        }
    }
    if let Some(overrides) = root
        .get_mut("modelOverrides")
        .and_then(Value::as_object_mut)
    {
        for model_override in overrides.values_mut() {
            if let Some(object) = model_override.as_object_mut() {
                object.remove("headers");
            }
        }
    }
    Ok(value)
}

fn pi_route_token(provider_id: &str, provider_key: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"cc-switch:pi-route:v2\0");
    digest.update(provider_id.as_bytes());
    digest.update([0]);
    digest.update(provider_key.as_bytes());
    digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Process-local publication point. Odd epochs close admission; an even
/// snapshot is leased by `Arc`, so requests already admitted keep a coherent
/// catalog while a replacement is prepared.
#[derive(Debug, Default)]
pub(crate) struct PiRuntimeStore {
    publication: RwLock<PiRuntimePublication>,
    epoch_gate: Arc<AsyncRwLock<()>>,
}

#[derive(Debug, Default)]
struct PiRuntimePublication {
    current: Option<Arc<PiRuntimeSnapshot>>,
    catalog_epoch: u64,
}

impl PiRuntimeStore {
    pub(crate) async fn begin_mutation(&self) -> u64 {
        let _guard = self.epoch_gate.write().await;
        let mut publication = self
            .publication
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = publication.catalog_epoch;
        let odd = if current % 2 == 0 {
            current.saturating_add(1)
        } else {
            current
        };
        publication.catalog_epoch = odd;
        odd.saturating_add(1)
    }

    pub(crate) fn next_even_epoch(&self) -> Result<u64, AppError> {
        let current = self
            .publication
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .catalog_epoch;
        if current % 2 != 0 {
            return Err(AppError::Conflict(
                "cannot publish a sorted Pi runtime while catalog admission is fenced".to_string(),
            ));
        }
        let next = current.saturating_add(2);
        if next % 2 != 0 {
            return Err(AppError::Config(
                "Pi catalog epoch overflowed its even publication sequence".to_string(),
            ));
        }
        Ok(next)
    }

    pub(crate) async fn publish(&self, snapshot: Arc<PiRuntimeSnapshot>) -> Result<(), AppError> {
        if snapshot.catalog_epoch % 2 != 0 {
            return Err(AppError::Config(
                "cannot publish an odd Pi catalog epoch".to_string(),
            ));
        }
        let _guard = self.epoch_gate.write().await;
        let mut publication = self
            .publication
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        publication.catalog_epoch = snapshot.catalog_epoch;
        publication.current = Some(snapshot);
        Ok(())
    }

    pub(crate) async fn close(&self, even_epoch: u64) -> Result<(), AppError> {
        if even_epoch % 2 != 0 {
            return Err(AppError::Config(
                "Pi admission close requires an even terminal epoch".to_string(),
            ));
        }
        let _guard = self.epoch_gate.write().await;
        let mut publication = self
            .publication
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        publication.current = None;
        publication.catalog_epoch = even_epoch;
        Ok(())
    }

    pub(crate) async fn republish_current(&self, even_epoch: u64) -> Result<bool, AppError> {
        if even_epoch % 2 != 0 {
            return Err(AppError::Config(
                "Pi runtime re-publication requires an even epoch".to_string(),
            ));
        }
        let _guard = self.epoch_gate.write().await;
        let mut publication = self
            .publication
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = publication.current.as_ref().cloned();
        let Some(current) = current else {
            publication.catalog_epoch = even_epoch;
            return Ok(false);
        };
        let mut next = (*current).clone();
        next.catalog_epoch = even_epoch;
        publication.current = Some(Arc::new(next));
        publication.catalog_epoch = even_epoch;
        Ok(true)
    }

    pub(crate) fn lease(&self, server_generation: u64) -> Option<Arc<PiRuntimeSnapshot>> {
        let publication = self
            .publication
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if publication.catalog_epoch % 2 != 0 {
            return None;
        }
        publication
            .current
            .as_ref()
            .filter(|snapshot| {
                snapshot.server_generation == server_generation
                    && snapshot.catalog_epoch == publication.catalog_epoch
            })
            .cloned()
    }

    pub(crate) fn is_admitting(&self, server_generation: u64) -> bool {
        self.lease(server_generation).is_some()
    }

    /// Retain the credential of the last published generation for exact
    /// native-projection compensation even while an odd epoch fences new
    /// admission. This does not mint or rotate credentials and never crosses
    /// IPC; it is only an ownership witness for restoring `models.json`.
    pub(crate) fn retained_gateway_token(
        &self,
        server_generation: u64,
    ) -> Option<crate::settings::GatewayToken> {
        self.publication
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current
            .as_ref()
            .filter(|snapshot| snapshot.server_generation == server_generation)
            .map(|snapshot| snapshot.gateway_token.clone())
    }

    /// Return the exact native projection paired with the fenced runtime.
    /// This remains available while admission is at an odd epoch so recovery
    /// can prove that re-publication would still describe Pi's live file.
    pub(crate) fn retained_native_projection(&self) -> Option<IndexMap<String, Option<Value>>> {
        self.publication
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current
            .as_ref()
            .map(|snapshot| snapshot.native_projection.0.clone())
    }

    pub(crate) async fn admission_guard(
        self: &Arc<Self>,
        server_generation: u64,
        snapshot: &Arc<PiRuntimeSnapshot>,
    ) -> Option<OwnedRwLockReadGuard<()>> {
        let guard = self.epoch_gate.clone().read_owned().await;
        let publication = self
            .publication
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = publication.current.as_ref().is_some_and(|current| {
            snapshot.catalog_epoch % 2 == 0
                && publication.catalog_epoch == snapshot.catalog_epoch
                && current.server_generation == server_generation
                && Arc::ptr_eq(current, snapshot)
        });
        current.then_some(guard)
    }

    pub(crate) async fn writeback_guard(
        self: &Arc<Self>,
        expected_epoch: u64,
    ) -> Option<OwnedRwLockReadGuard<()>> {
        let guard = self.epoch_gate.clone().read_owned().await;
        let current = self
            .publication
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .catalog_epoch;
        (expected_epoch % 2 == 0 && current == expected_epoch).then_some(guard)
    }
}

pub(crate) fn infer_family(path: &str) -> Option<PiGatewayApiFamily> {
    if path == "/v1/messages" {
        Some(PiGatewayApiFamily::AnthropicMessages)
    } else if path == "/chat/completions" {
        Some(PiGatewayApiFamily::OpenAiCompletions)
    } else if matches!(path, "/responses" | "/responses/compact") {
        Some(PiGatewayApiFamily::OpenAiResponses)
    } else if path.starts_with("/models/") || path == "/models" {
        Some(PiGatewayApiFamily::GoogleGenerativeAi)
    } else {
        None
    }
}

fn build_family_url(
    family: PiGatewayApiFamily,
    base: &Url,
    path_and_query: &str,
) -> Result<Url, AppError> {
    let (path, query) = path_and_query
        .split_once('?')
        .map_or((path_and_query, None), |(path, query)| (path, Some(query)));
    if infer_family(path) != Some(family) {
        return Err(AppError::InvalidInput(format!(
            "Pi gateway path '{path}' does not match {}",
            family.as_str()
        )));
    }
    let mut url = base.clone();
    let base_path = base.path().trim_end_matches('/');
    let suffix = path.trim_start_matches('/');
    let combined = if base_path.is_empty() || base_path == "/" {
        format!("/{suffix}")
    } else {
        format!("{base_path}/{suffix}")
    };
    url.set_path(&combined);
    url.set_query(query);
    url.set_fragment(None);
    Ok(url)
}

fn resolve_pi_config_value(expression: &str) -> Result<String, String> {
    if let Some(command) = expression.strip_prefix('!') {
        return execute_config_command(command);
    }
    expand_environment(expression)
}

fn expand_environment(input: &str) -> Result<String, String> {
    const ESCAPED_DOLLAR: char = '\u{e000}';
    const ESCAPED_BANG: char = '\u{e001}';
    let chars = input.chars().collect::<Vec<_>>();
    let mut output = String::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != '$' {
            output.push(chars[index]);
            index += 1;
            continue;
        }
        if chars.get(index + 1) == Some(&'$') {
            output.push(ESCAPED_DOLLAR);
            index += 2;
            continue;
        }
        if chars.get(index + 1) == Some(&'!') {
            output.push(ESCAPED_BANG);
            index += 2;
            continue;
        }
        let (name, next) = if chars.get(index + 1) == Some(&'{') {
            let Some(end) = chars[index + 2..].iter().position(|value| *value == '}') else {
                return Err("unterminated Pi environment expression".to_string());
            };
            let end = index + 2 + end;
            (chars[index + 2..end].iter().collect::<String>(), end + 1)
        } else {
            let mut end = index + 1;
            while end < chars.len() && (chars[end] == '_' || chars[end].is_ascii_alphanumeric()) {
                end += 1;
            }
            if end == index + 1 {
                output.push('$');
                index += 1;
                continue;
            }
            (chars[index + 1..end].iter().collect::<String>(), end)
        };
        if name.is_empty()
            || !name
                .chars()
                .next()
                .is_some_and(|value| value == '_' || value.is_ascii_alphabetic())
        {
            return Err("invalid Pi environment variable name".to_string());
        }
        let value = std::env::var(&name)
            .map_err(|_| format!("Pi environment variable '{name}' is unavailable"))?;
        output.push_str(&value);
        index = next;
    }
    Ok(output
        .replace(ESCAPED_DOLLAR, "$")
        .replace(ESCAPED_BANG, "!"))
}

fn execute_config_command(script: &str) -> Result<String, String> {
    if script.trim().is_empty() {
        return Err("empty Pi config command".to_string());
    }
    let mut command = config_command(script);
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("failed to start Pi config command: {error}"))?;
    let command_tree = CommandTree::attach(&mut child)?;
    #[cfg(windows)]
    if let Err(error) = resume_suspended_process(child.id()) {
        command_tree.terminate(&mut child);
        let _ = child.wait();
        return Err(error);
    }
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "failed to capture Pi config command stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "failed to capture Pi config command stderr".to_string())?;
    let (stdout_sender, stdout_reader) = mpsc::sync_channel(1);
    let (stderr_sender, stderr_reader) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = stdout_sender.send(read_bounded(stdout));
    });
    std::thread::spawn(move || {
        let _ = stderr_sender.send(read_bounded(stderr));
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < COMMAND_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                command_tree.terminate(&mut child);
                let _ = child.wait();
                return Err("Pi config command timed out".to_string());
            }
            Err(error) => {
                command_tree.terminate(&mut child);
                let _ = child.wait();
                return Err(format!("failed to wait for Pi config command: {error}"));
            }
        }
    };
    // A successful shell may leave descendants holding inherited pipe handles.
    // Terminate the whole tree before draining, and keep the original deadline
    // over both process wait and output collection.
    command_tree.terminate(&mut child);
    let deadline = started + COMMAND_TIMEOUT;
    let stdout = receive_command_output(
        &stdout_reader,
        deadline,
        "Pi config command stdout did not close before timeout",
    )??;
    let stderr = receive_command_output(
        &stderr_reader,
        deadline,
        "Pi config command stderr did not close before timeout",
    )??;
    if !status.success() {
        return Err(format!(
            "Pi config command exited unsuccessfully: {}",
            String::from_utf8_lossy(&stderr).trim()
        ));
    }
    String::from_utf8(stdout)
        .map(|value| value.trim().to_string())
        .map_err(|_| "Pi config command output is not UTF-8".to_string())
}

#[cfg(windows)]
fn config_command(script: &str) -> Command {
    use std::os::windows::process::CommandExt;
    use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

    let mut command = Command::new("cmd");
    command.args(["/D", "/S", "/C"]);
    // `cmd.exe` does not follow the standard Windows argv quoting rules.
    // The value is intentionally shell source, so preserve it verbatim
    // instead of letting `Command::arg` add quotes that change `&` and nested
    // command semantics.
    command.raw_arg(script);
    // The shell must not execute user code before it belongs to the
    // kill-on-close Job Object. Its primary thread is resumed only after
    // CommandTree::attach succeeds.
    command.creation_flags(CREATE_SUSPENDED);
    command
}

#[cfg(not(windows))]
fn config_command(script: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", script]);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
}

fn read_bounded(reader: impl Read) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    reader
        .take(COMMAND_OUTPUT_LIMIT + 1)
        .read_to_end(&mut output)
        .map_err(|error| format!("failed to read Pi config command output: {error}"))?;
    if output.len() as u64 > COMMAND_OUTPUT_LIMIT {
        return Err("Pi config command output exceeded 1 MiB".to_string());
    }
    Ok(output)
}

fn receive_command_output(
    receiver: &mpsc::Receiver<Result<Vec<u8>, String>>,
    deadline: Instant,
    timeout_message: &str,
) -> Result<Result<Vec<u8>, String>, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    receiver
        .recv_timeout(remaining)
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => timeout_message.to_string(),
            mpsc::RecvTimeoutError::Disconnected => {
                "Pi config command output reader stopped unexpectedly".to_string()
            }
        })
}

struct CommandTree {
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

impl CommandTree {
    fn attach(child: &mut std::process::Child) -> Result<Self, String> {
        #[cfg(windows)]
        {
            use std::mem::size_of;
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::Foundation::CloseHandle;
            use windows_sys::Win32::System::JobObjects::{
                AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            };

            // SAFETY: all pointers are either null or point to initialized
            // values for the duration of their synchronous Win32 calls.
            unsafe {
                let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if job.is_null() {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "failed to create Pi config command job: {}",
                        std::io::Error::last_os_error()
                    ));
                }
                let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                if SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    (&raw const limits).cast(),
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                ) == 0
                {
                    let error = std::io::Error::last_os_error();
                    CloseHandle(job);
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "failed to configure Pi config command job: {error}"
                    ));
                }
                if AssignProcessToJobObject(job, child.as_raw_handle() as _) == 0 {
                    let error = std::io::Error::last_os_error();
                    CloseHandle(job);
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "failed to assign Pi config command to its job: {error}"
                    ));
                }
                Ok(Self { job })
            }
        }
        #[cfg(not(windows))]
        {
            let _ = child;
            Ok(Self {})
        }
    }

    fn terminate(&self, child: &mut std::process::Child) {
        #[cfg(unix)]
        unsafe {
            let _ = libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        #[cfg(windows)]
        unsafe {
            let _ = child;
            let _ = windows_sys::Win32::System::JobObjects::TerminateJobObject(self.job, 1);
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child.kill();
        }
    }
}

#[cfg(windows)]
fn resume_suspended_process(process_id: u32) -> Result<(), String> {
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    // SAFETY: the snapshot and thread handles are checked before use and
    // closed on every path. CREATE_SUSPENDED prevents the target from adding
    // threads while this enumeration runs.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(format!(
                "failed to enumerate the suspended Pi config command: {}",
                std::io::Error::last_os_error()
            ));
        }
        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..THREADENTRY32::default()
        };
        let mut has_entry = Thread32First(snapshot, &mut entry);
        while has_entry != 0 {
            if entry.th32OwnerProcessID == process_id {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if thread.is_null() {
                    let error = std::io::Error::last_os_error();
                    CloseHandle(snapshot);
                    return Err(format!(
                        "failed to open the suspended Pi config command thread: {error}"
                    ));
                }
                let result = ResumeThread(thread);
                let error = (result == u32::MAX).then(std::io::Error::last_os_error);
                CloseHandle(thread);
                CloseHandle(snapshot);
                return match error {
                    Some(error) => Err(format!(
                        "failed to resume the suspended Pi config command: {error}"
                    )),
                    None => Ok(()),
                };
            }
            has_entry = Thread32Next(snapshot, &mut entry);
        }
        CloseHandle(snapshot);
    }
    Err("the suspended Pi config command had no primary thread".to_string())
}

#[cfg(windows)]
impl Drop for CommandTree {
    fn drop(&mut self) {
        // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE is the final safety net if an
        // early return occurs before explicit termination.
        unsafe {
            let _ = windows_sys::Win32::Foundation::CloseHandle(self.job);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_projection_debug_exposes_only_key_presence() {
        let secret = "runtime-native-secret-never-log";
        let witness = PiNativeProjectionWitness(IndexMap::from([
            (
                "managed".to_string(),
                Some(json!({"apiKey": secret, "headers": {"x-private": secret}})),
            ),
            ("removed".to_string(), None),
        ]));
        let debug = format!("{witness:?}");
        assert!(!debug.contains(secret));
        assert!(debug.contains("managed"));
        assert!(debug.contains("present"));
        assert!(debug.contains("removed"));
        assert!(debug.contains("absent"));
    }

    #[test]
    fn invalid_persisted_custom_endpoint_is_quarantined_without_losing_primary_route() {
        let config: PiManagedProviderConfig = serde_json::from_value(json!({
            "name": "Provider",
            "api": "openai-responses",
            "baseUrl": "https://primary.example/v1",
            "apiKey": "literal",
            "models": [{"id": "model-a"}]
        }))
        .expect("managed config");
        let composition =
            compose_managed_pi_provider("provider", &config).expect("compose provider");
        let primary = assess_composition_for_runtime(&composition)
            .plans
            .into_iter()
            .next()
            .expect("primary plan");
        let custom_endpoint_plans = build_custom_endpoint_plans(
            &primary,
            vec![
                "https://user:secret@invalid.example/v1".to_string(),
                "https://mirror.example/v1".to_string(),
                "https://mirror.example/v1".to_string(),
                "https://primary.example/v1".to_string(),
            ],
            "provider",
        );
        assert_eq!(
            custom_endpoint_plans
                .iter()
                .map(|plan| plan.endpoint().as_str())
                .collect::<Vec<_>>(),
            ["https://mirror.example/v1"]
        );

        let model = PiRuntimeModel {
            provider_id: "provider".to_string(),
            provider_name: "Provider".to_string(),
            family: PiGatewayApiFamily::OpenAiResponses,
            wire_profile: Vec::new(),
            plan: primary,
            custom_endpoint_plans,
        };
        let attempts = expand_model_attempts(&model, false);
        assert_eq!(attempts.len(), 2);
        assert_eq!(
            attempts[0].plan.endpoint().as_str(),
            "https://primary.example/v1"
        );
        assert_eq!(
            attempts[1].plan.endpoint().as_str(),
            "https://mirror.example/v1"
        );
    }

    #[test]
    fn environment_resolution_matches_vendored_transport_oracle() {
        std::env::set_var("PI_RUNTIME_TEST_VALUE", "environment-secret");
        assert_eq!(
            resolve_pi_config_value("prefix-${PI_RUNTIME_TEST_VALUE}-suffix").unwrap(),
            "prefix-environment-secret-suffix"
        );
        assert_eq!(
            resolve_pi_config_value("$$literal-$!bang").unwrap(),
            "$literal-!bang"
        );
        std::env::remove_var("PI_RUNTIME_TEST_VALUE");
    }

    #[cfg(unix)]
    #[test]
    fn command_deadline_covers_descendants_holding_output_pipes() {
        let started = Instant::now();
        let output =
            execute_config_command("sleep 30 & printf inherited-pipe").expect("command output");
        assert_eq!(output, "inherited-pipe");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "background descendants must be terminated before output drain"
        );
    }

    #[test]
    fn command_resolution_matches_vendored_transport_oracle() {
        #[cfg(unix)]
        assert_eq!(
            resolve_pi_config_value("!printf pi-command-value").unwrap(),
            "pi-command-value"
        );
    }

    #[cfg(windows)]
    #[test]
    fn command_job_assignment_precedes_descendant_execution() {
        let started = Instant::now();
        for _ in 0..8 {
            let output = execute_config_command(
                "start \"\" /b cmd /D /S /C \"ping 127.0.0.1 -n 30 >nul\" & <nul set /p =ready",
            )
            .expect("command output");
            assert_eq!(output, "ready");
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "descendants must remain in the job and release inherited pipes"
        );
    }

    #[test]
    fn family_url_builders_preserve_candidate_origin_and_base_path() {
        let base = Url::parse("https://candidate.example:8443/root/v1").unwrap();
        let url = build_family_url(
            PiGatewayApiFamily::OpenAiResponses,
            &base,
            "/responses?stream=true",
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://candidate.example:8443/root/v1/responses?stream=true"
        );
        assert!(
            build_family_url(PiGatewayApiFamily::AnthropicMessages, &base, "/responses").is_err()
        );
    }

    #[test]
    fn wire_profile_is_key_order_insensitive_but_array_and_unknown_sensitive() {
        fn model(extra: Value, input: Value) -> PiComposedNativeModel {
            PiComposedNativeModel {
                id: "m".to_string(),
                name: "M".to_string(),
                api: crate::pi_config::raw_schema::PiRawApiId::new("openai-responses".to_string())
                    .unwrap(),
                provider: "p".to_string(),
                base_url: "https://example.test/v1".to_string(),
                reasoning: false,
                thinking_level_map: None,
                input,
                cost: json!({"input": 1}),
                context_window: json!(1000),
                max_tokens: json!(100),
                headers: BTreeMap::new(),
                provider_headers: Vec::new(),
                model_headers: Vec::new(),
                compat: None,
                api_key: Some("secret".to_string()),
                oauth: None,
                auth_header: false,
                provider_extra: serde_json::from_value(extra).unwrap(),
                model_extra: BTreeMap::new(),
                override_extra: BTreeMap::new(),
            }
        }
        let first = model(json!({"z": 1, "a": {"b": 2, "a": 1}}), json!(["text"]));
        let reordered = model(json!({"a": {"a": 1, "b": 2}, "z": 1}), json!(["text"]));
        assert_eq!(
            canonical_wire_profile(&first).unwrap(),
            canonical_wire_profile(&reordered).unwrap()
        );
        let changed = model(
            json!({"z": 1, "a": {"b": 2, "a": 1}}),
            json!(["image", "text"]),
        );
        assert_ne!(
            canonical_wire_profile(&first).unwrap(),
            canonical_wire_profile(&changed).unwrap()
        );
    }
}

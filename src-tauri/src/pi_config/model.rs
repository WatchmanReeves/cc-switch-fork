//! Managed Pi control-plane types.
//!
//! API identifiers are intentionally opaque here. The closed set of API
//! families that the gateway can proxy belongs to `gateway.rs`; admitting a
//! new Pi API identifier into the managed control plane must not require a
//! gateway enum update.

#![allow(dead_code)]

use super::merge_pi_compat;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use thiserror::Error;
use url::Url;

pub(crate) type PiHeaderMap = IndexMap<String, String>;
pub(crate) type PiThinkingLevelMap = BTreeMap<String, Value>;
const PI_OWNED_AUTH_FIELD: &str = "oauth";

pub(crate) fn value_uses_pi_owned_auth(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|object| object.contains_key(PI_OWNED_AUTH_FIELD))
}

/// Pi uses JavaScript/TypeBox `Number`, not `Integer`, for model limits.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct PiNumber(f64);

impl PiNumber {
    pub(crate) const DEFAULT_CONTEXT_WINDOW: Self = Self(128_000.0);
    pub(crate) const DEFAULT_MAX_TOKENS: Self = Self(16_384.0);

    pub(crate) fn new(value: f64) -> Result<Self, PiNumberError> {
        if value.is_finite() {
            Ok(Self(value))
        } else {
            Err(PiNumberError)
        }
    }

    pub(crate) const fn get(self) -> f64 {
        self.0
    }
}

impl<'de> Deserialize<'de> for PiNumber {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = f64::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl TryFrom<f64> for PiNumber {
    type Error = PiNumberError;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<PiNumber> for f64 {
    fn from(value: PiNumber) -> Self {
        value.get()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("Pi Number must be finite")]
pub(crate) struct PiNumberError;

/// Opaque managed API identifier.
///
/// This is not the gateway support enum. Any non-empty Pi-valid identifier
/// round-trips through this type, including identifiers introduced after the
/// pinned gateway implementation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub(crate) struct PiManagedApiId(String);

impl PiManagedApiId {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, PiConfigError> {
        let value = value.into();
        if value.is_empty() {
            return Err(PiConfigError::EmptyApiId);
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for PiManagedApiId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PiModelInput {
    Text,
    Image,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiModelCostRates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

impl Default for PiModelCostRates {
    fn default() -> Self {
        Self {
            input: 0.0,
            output: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiModelCostTier {
    pub input_tokens_above: f64,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiModelCost {
    #[serde(flatten)]
    pub rates: PiModelCostRates,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<PiModelCostTier>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiModelCostOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<PiModelCostTier>>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiManagedModelOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<PiThinkingLevelMap>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<PiModelInput>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<PiModelCostOverride>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<PiNumber>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<PiNumber>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub headers: PiHeaderMap,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiManagedModel {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<PiManagedApiId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<PiThinkingLevelMap>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Vec<PiModelInput>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<PiModelCost>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_window: Option<PiNumber>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<PiNumber>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub headers: PiHeaderMap,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiManagedProviderConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api: Option<PiManagedApiId>,
    /// Literal or deferred transport material. The managed layer never
    /// executes env/command/file/network resolution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub headers: PiHeaderMap,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_header: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<PiManagedModel>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_overrides: BTreeMap<String, PiManagedModelOverride>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<Value>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiEffectiveModel {
    pub id: String,
    pub name: String,
    pub api: PiManagedApiId,
    pub base_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    pub auth_header: bool,
    pub reasoning: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_level_map: Option<PiThinkingLevelMap>,
    pub input: Vec<PiModelInput>,
    pub cost: PiModelCost,
    pub context_window: PiNumber,
    pub max_tokens: PiNumber,
    pub headers: PiHeaderMap,
    /// Provider auth headers and the later model overlay are kept distinct so
    /// a gateway can reproduce pinned Pi's case-insensitive runtime merge
    /// without changing the serialized effective projection.
    #[serde(skip)]
    pub header_layers: PiEffectiveHeaderLayers,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compat: Option<Value>,
    pub provider_extra: BTreeMap<String, Value>,
    pub model_extra: BTreeMap<String, Value>,
    pub override_extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct PiEffectiveHeaderLayers {
    pub provider: PiHeaderMap,
    pub model: PiHeaderMap,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub(crate) enum PiConfigError {
    #[error("Pi provider must declare at least one managed model")]
    ProviderHasNoModels,
    #[error("Pi API id cannot be empty")]
    EmptyApiId,
    #[error("Pi model id cannot be empty")]
    EmptyModelId,
    #[error("Pi provider contains duplicate model id '{0}'")]
    DuplicateModelId(String),
    #[error("Pi model '{0}' does not exist in this provider")]
    ModelNotFound(String),
    #[error("Pi model '{model_id}' has no effective API id")]
    MissingEffectiveApi { model_id: String },
    #[error("Pi model '{model_id}' has no effective endpoint")]
    MissingEffectiveEndpoint { model_id: String },
    #[error("Pi endpoint at '{json_pointer}' must be an absolute HTTP(S) URL: {reason}")]
    InvalidEndpoint {
        json_pointer: String,
        reason: String,
    },
    #[error("Pi model override '{0}' does not match an explicit model id")]
    UnknownModelOverride(String),
    #[error("Pi compat at '{json_pointer}' must be an object")]
    InvalidCompat { json_pointer: String },
    #[error(
        "Pi compat at '{json_pointer}' requires JavaScript UTF-16 values that cannot be represented"
    )]
    UnrepresentableCompat { json_pointer: String },
    #[error("Pi {field} cannot be empty when present")]
    EmptyOptionalField { field: &'static str },
    #[error("Pi model '{model_id}' {field} must be greater than zero")]
    NonPositiveModelLimit {
        model_id: String,
        field: &'static str,
    },
    #[error("Pi model '{model_id}' contains an invalid value for thinking level '{level}'")]
    InvalidThinkingLevelValue { model_id: String, level: String },
    #[error(
        "Pi-owned authentication field '{json_pointer}' cannot be stored in a managed provider"
    )]
    PiOwnedAuthField { json_pointer: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PiNativeEntryKind {
    BuiltInOverlay,
    CustomCatalog,
    ExtensionOverlay,
    UnknownShape,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PiRawNativeValidity {
    Valid,
    Invalid,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PiManagedAssessment {
    Manageable,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PiCompositionStatus {
    Composed,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum PiManagementStatus {
    Importable,
    Managed {
        #[serde(rename = "providerId")]
        provider_id: String,
    },
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PiGatewayStatus {
    Proxyable,
    DirectOnly,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PiDiagnosticLayer {
    RawSchema,
    Managed,
    Composition,
    Gateway,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PiReasonCode {
    RawSchemaMismatch,
    RawSchemaUnsupportedOperator,
    RawSchemaPinDrift,
    RawSchemaAmbiguous,
    CatalogRequired,
    ModelOverridesOnly,
    MissingExplicitModels,
    ManagedTypeConversionFailed,
    EmptyOptionalField,
    EmptyModelId,
    DuplicateModelId,
    UnknownModelOverride,
    MissingEffectiveApi,
    MissingEffectiveEndpoint,
    InvalidEndpoint,
    InvalidCompat,
    UnrepresentableCompat,
    NonPositiveModelLimit,
    InvalidThinkingLevel,
    PiOwnedAuthField,
    CompositionFailed,
    GatewayCredentialUnavailable,
    UnsupportedCredentialKind,
    UnsupportedGatewayFamily,
    InvalidHeaderName,
    InvalidHeaderValue,
    ProtectedHeader,
    DeferredValueUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiDiagnosticReason {
    pub layer: PiDiagnosticLayer,
    pub code: PiReasonCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json_pointer: Option<String>,
}

impl PiDiagnosticReason {
    pub(crate) fn new(
        layer: PiDiagnosticLayer,
        code: PiReasonCode,
        json_pointer: Option<String>,
    ) -> Self {
        Self {
            layer,
            code,
            json_pointer,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PiNativeDiagnostic {
    pub provider_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    pub fingerprint: String,
    pub kind: PiNativeEntryKind,
    pub raw_validity: PiRawNativeValidity,
    pub managed_assessment: PiManagedAssessment,
    pub composition_status: PiCompositionStatus,
    pub management_status: PiManagementStatus,
    pub gateway_status: PiGatewayStatus,
    pub reasons: Vec<PiDiagnosticReason>,
}

const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

pub(crate) fn validate_pi_managed_provider(
    provider: &PiManagedProviderConfig,
) -> Result<(), PiConfigError> {
    if provider.extra.contains_key(PI_OWNED_AUTH_FIELD) {
        return Err(PiConfigError::PiOwnedAuthField {
            json_pointer: format!("/{PI_OWNED_AUTH_FIELD}"),
        });
    }
    if provider.models.is_empty() {
        return Err(PiConfigError::ProviderHasNoModels);
    }
    validate_optional_text(provider.name.as_deref(), "provider name")?;
    validate_optional_text(provider.api_key.as_deref(), "provider apiKey")?;
    validate_present_endpoint(provider.base_url.as_deref(), "/baseUrl")?;
    validate_compat(provider.compat.as_ref(), "/compat")?;

    let mut model_ids = HashSet::with_capacity(provider.models.len());
    for (index, model) in provider.models.iter().enumerate() {
        if model.id.is_empty() {
            return Err(PiConfigError::EmptyModelId);
        }
        if !model_ids.insert(model.id.as_str()) {
            return Err(PiConfigError::DuplicateModelId(model.id.clone()));
        }
        validate_optional_text(model.name.as_deref(), "model name")?;
        validate_present_endpoint(
            model.base_url.as_deref(),
            &format!("/models/{index}/baseUrl"),
        )?;
        validate_compat(model.compat.as_ref(), &format!("/models/{index}/compat"))?;
        validate_model_limit(model, model.context_window, "contextWindow")?;
        validate_model_limit(model, model.max_tokens, "maxTokens")?;
        validate_thinking_levels(&model.id, model.thinking_level_map.as_ref())?;
        let _ = effective_pi_model_unchecked(provider, model)?;
    }

    for (model_id, model_override) in &provider.model_overrides {
        if !model_ids.contains(model_id.as_str()) {
            return Err(PiConfigError::UnknownModelOverride(model_id.clone()));
        }
        validate_optional_text(model_override.name.as_deref(), "model override name")?;
        validate_optional_limit(model_id, model_override.context_window, "contextWindow")?;
        validate_optional_limit(model_id, model_override.max_tokens, "maxTokens")?;
        validate_thinking_levels(model_id, model_override.thinking_level_map.as_ref())?;
        validate_compat(
            model_override.compat.as_ref(),
            &format!("/modelOverrides/{}/compat", escape_json_pointer(model_id)),
        )?;
    }
    Ok(())
}

pub(crate) fn effective_pi_model(
    provider: &PiManagedProviderConfig,
    model_id: &str,
) -> Result<PiEffectiveModel, PiConfigError> {
    let model = provider
        .models
        .iter()
        .find(|model| model.id == model_id)
        .ok_or_else(|| PiConfigError::ModelNotFound(model_id.to_string()))?;
    validate_pi_managed_provider(provider)?;
    effective_pi_model_unchecked(provider, model)
}

fn effective_pi_model_unchecked(
    provider: &PiManagedProviderConfig,
    model: &PiManagedModel,
) -> Result<PiEffectiveModel, PiConfigError> {
    let api = model
        .api
        .clone()
        .or_else(|| provider.api.clone())
        .ok_or_else(|| PiConfigError::MissingEffectiveApi {
            model_id: model.id.clone(),
        })?;
    let base_url = model
        .base_url
        .as_ref()
        .or(provider.base_url.as_ref())
        .ok_or_else(|| PiConfigError::MissingEffectiveEndpoint {
            model_id: model.id.clone(),
        })?;
    let model_override = provider.model_overrides.get(&model.id);
    let mut model_headers = PiHeaderMap::new();
    if let Some(model_override) = model_override {
        model_headers.extend(model_override.headers.clone());
    }
    model_headers.extend(model.headers.clone());
    let mut headers = provider.headers.clone();
    headers.extend(model_headers.clone());

    let thinking_level_map = merge_thinking_level_maps(
        model.thinking_level_map.as_ref(),
        model_override.and_then(|entry| entry.thinking_level_map.as_ref()),
    );

    let base_cost = model.cost.clone().unwrap_or_default();
    let cost = model_override
        .and_then(|entry| entry.cost.as_ref())
        .map(|entry| apply_cost_override(base_cost.clone(), entry))
        .unwrap_or(base_cost);

    let compat = merge_pi_compat(provider.compat.clone(), model.compat.clone()).map_err(|_| {
        PiConfigError::UnrepresentableCompat {
            json_pointer: "/compat".to_string(),
        }
    })?;
    let compat = merge_pi_compat(
        compat,
        model_override.and_then(|entry| entry.compat.clone()),
    )
    .map_err(|_| PiConfigError::UnrepresentableCompat {
        json_pointer: format!("/modelOverrides/{}/compat", escape_json_pointer(&model.id)),
    })?;

    Ok(PiEffectiveModel {
        id: model.id.clone(),
        name: model_override
            .and_then(|entry| entry.name.clone())
            .or_else(|| model.name.clone())
            .unwrap_or_else(|| model.id.clone()),
        api,
        base_url: base_url.clone(),
        api_key: provider.api_key.clone(),
        auth_header: provider.auth_header.unwrap_or(false),
        reasoning: model_override
            .and_then(|entry| entry.reasoning)
            .or(model.reasoning)
            .unwrap_or(false),
        thinking_level_map,
        input: model_override
            .and_then(|entry| entry.input.clone())
            .or_else(|| model.input.clone())
            .unwrap_or_else(|| vec![PiModelInput::Text]),
        cost,
        context_window: model_override
            .and_then(|entry| entry.context_window)
            .or(model.context_window)
            .unwrap_or(PiNumber::DEFAULT_CONTEXT_WINDOW),
        max_tokens: model_override
            .and_then(|entry| entry.max_tokens)
            .or(model.max_tokens)
            .unwrap_or(PiNumber::DEFAULT_MAX_TOKENS),
        headers,
        header_layers: PiEffectiveHeaderLayers {
            provider: provider.headers.clone(),
            model: model_headers,
        },
        compat,
        provider_extra: provider.extra.clone(),
        model_extra: model.extra.clone(),
        override_extra: model_override
            .map(|entry| entry.extra.clone())
            .unwrap_or_default(),
    })
}

fn merge_thinking_level_maps(
    base: Option<&PiThinkingLevelMap>,
    overlay: Option<&PiThinkingLevelMap>,
) -> Option<PiThinkingLevelMap> {
    match (base, overlay) {
        (None, None) => None,
        (Some(value), None) | (None, Some(value)) => Some(value.clone()),
        (Some(base), Some(overlay)) => {
            let mut merged = base.clone();
            merged.extend(overlay.clone());
            Some(merged)
        }
    }
}

fn apply_cost_override(mut base: PiModelCost, model_override: &PiModelCostOverride) -> PiModelCost {
    base.rates = PiModelCostRates {
        input: model_override.input.unwrap_or(base.rates.input),
        output: model_override.output.unwrap_or(base.rates.output),
        cache_read: model_override.cache_read.unwrap_or(base.rates.cache_read),
        cache_write: model_override.cache_write.unwrap_or(base.rates.cache_write),
    };
    if let Some(tiers) = &model_override.tiers {
        base.tiers = Some(tiers.clone());
    }
    base.extra.extend(model_override.extra.clone());
    base
}

fn validate_optional_text(value: Option<&str>, field: &'static str) -> Result<(), PiConfigError> {
    if value.is_some_and(str::is_empty) {
        return Err(PiConfigError::EmptyOptionalField { field });
    }
    Ok(())
}

fn validate_model_limit(
    model: &PiManagedModel,
    value: Option<PiNumber>,
    field: &'static str,
) -> Result<(), PiConfigError> {
    validate_optional_limit(&model.id, value, field)
}

fn validate_optional_limit(
    model_id: &str,
    value: Option<PiNumber>,
    field: &'static str,
) -> Result<(), PiConfigError> {
    if value.is_some_and(|value| value.get() <= 0.0) {
        return Err(PiConfigError::NonPositiveModelLimit {
            model_id: model_id.to_string(),
            field,
        });
    }
    Ok(())
}

fn validate_present_endpoint(
    endpoint: Option<&str>,
    json_pointer: &str,
) -> Result<(), PiConfigError> {
    let Some(endpoint) = endpoint else {
        return Ok(());
    };
    if endpoint.trim().is_empty() {
        return Err(PiConfigError::InvalidEndpoint {
            json_pointer: json_pointer.to_string(),
            reason: "endpoint cannot be empty".to_string(),
        });
    }
    let parsed = Url::parse(endpoint).map_err(|error| PiConfigError::InvalidEndpoint {
        json_pointer: json_pointer.to_string(),
        reason: error.to_string(),
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
        return Err(PiConfigError::InvalidEndpoint {
            json_pointer: json_pointer.to_string(),
            reason: "endpoint must be an absolute HTTP(S) URL".to_string(),
        });
    }
    Ok(())
}

fn validate_compat(compat: Option<&Value>, json_pointer: &str) -> Result<(), PiConfigError> {
    if compat.is_some_and(|value| !value.is_object()) {
        return Err(PiConfigError::InvalidCompat {
            json_pointer: json_pointer.to_string(),
        });
    }
    Ok(())
}

fn escape_json_pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn validate_thinking_levels(
    model_id: &str,
    levels: Option<&PiThinkingLevelMap>,
) -> Result<(), PiConfigError> {
    let Some(levels) = levels else {
        return Ok(());
    };
    for (level, value) in levels {
        if THINKING_LEVELS.contains(&level.as_str()) && !(value.is_string() || value.is_null()) {
            return Err(PiConfigError::InvalidThinkingLevelValue {
                model_id: model_id.to_string(),
                level: level.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn api(value: &str) -> PiManagedApiId {
        PiManagedApiId::new(value).expect("non-empty API id")
    }

    fn number(value: f64) -> PiNumber {
        PiNumber::new(value).expect("finite Pi Number")
    }

    fn model(id: &str) -> PiManagedModel {
        PiManagedModel {
            id: id.to_string(),
            name: None,
            base_url: None,
            api: None,
            reasoning: None,
            thinking_level_map: None,
            input: None,
            cost: None,
            context_window: None,
            max_tokens: None,
            headers: PiHeaderMap::new(),
            compat: None,
            extra: BTreeMap::new(),
        }
    }

    fn provider(models: Vec<PiManagedModel>) -> PiManagedProviderConfig {
        PiManagedProviderConfig {
            name: Some("Example".into()),
            base_url: Some("https://example.com/api".into()),
            api: Some(api("anthropic-messages")),
            api_key: Some("$PI_KEY".into()),
            headers: PiHeaderMap::new(),
            auth_header: None,
            models,
            model_overrides: BTreeMap::new(),
            compat: None,
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn opaque_api_ids_round_trip_without_gateway_narrowing() {
        let config: PiManagedProviderConfig = serde_json::from_value(json!({
            "baseUrl": "https://future.example/v9",
            "api": "future-wire-v9",
            "models": [{"id": "future"}]
        }))
        .expect("opaque API id");
        validate_pi_managed_provider(&config).expect("future API is manageable");
        let effective = effective_pi_model(&config, "future").expect("effective future model");
        assert_eq!(effective.api.as_str(), "future-wire-v9");
        assert_eq!(
            serde_json::to_value(&config).expect("serialize")["api"],
            "future-wire-v9"
        );
    }

    #[test]
    fn managed_provider_rejects_pi_owned_oauth_state() {
        let config: PiManagedProviderConfig = serde_json::from_value(json!({
            "baseUrl": "https://example.com/v1",
            "api": "anthropic-messages",
            "oauth": "radius",
            "models": [{"id": "model"}]
        }))
        .expect("the raw Pi field remains parseable for inspection");

        assert_eq!(
            validate_pi_managed_provider(&config),
            Err(PiConfigError::PiOwnedAuthField {
                json_pointer: "/oauth".to_string()
            })
        );
    }

    #[test]
    fn provider_and_model_level_inheritance_is_exactly_two_levels() {
        let inherited = effective_pi_model(&provider(vec![model("claude")]), "claude")
            .expect("provider defaults");
        assert_eq!(inherited.api.as_str(), "anthropic-messages");
        assert_eq!(inherited.base_url, "https://example.com/api");

        let mut self_contained = model("future");
        self_contained.api = Some(api("future-api"));
        self_contained.base_url = Some("https://model.example/v2".into());
        let mut config = provider(vec![self_contained]);
        config.api = None;
        config.base_url = None;
        let effective = effective_pi_model(&config, "future").expect("model defaults");
        assert_eq!(effective.api.as_str(), "future-api");
        assert_eq!(effective.base_url, "https://model.example/v2");
    }

    #[test]
    fn schema_valid_whitespace_strings_remain_manageable() {
        let config: PiManagedProviderConfig = serde_json::from_value(json!({
            "name": " ",
            "api": "anthropic-messages",
            "baseUrl": "https://example.com",
            "apiKey": " ",
            "models": [{"id": " ", "name": " "}]
        }))
        .expect("pinned schema-valid whitespace fields");
        validate_pi_managed_provider(&config)
            .expect("managed validation must use pinned minLength semantics without trimming");
        assert_eq!(
            effective_pi_model(&config, " ")
                .expect("whitespace model id")
                .id,
            " "
        );
    }

    #[test]
    fn override_precedence_and_nested_compat_are_stable() {
        let mut base_model = model("m");
        base_model.reasoning = Some(false);
        base_model.headers = PiHeaderMap::from([
            ("layer".into(), "model".into()),
            ("model".into(), "yes".into()),
        ]);
        base_model.compat = Some(json!({
            "supportsStore": true,
            "openRouterRouting": {"only": ["model"], "zdr": true}
        }));
        let mut config = provider(vec![base_model]);
        config.headers = PiHeaderMap::from([
            ("layer".into(), "provider".into()),
            ("provider".into(), "yes".into()),
        ]);
        config.compat = Some(json!({"supportsDeveloperRole": true}));
        config.model_overrides.insert(
            "m".into(),
            PiManagedModelOverride {
                reasoning: Some(true),
                headers: PiHeaderMap::from([
                    ("layer".into(), "override".into()),
                    ("override".into(), "yes".into()),
                ]),
                compat: Some(json!({
                    "supportsStore": false,
                    "openRouterRouting": {"order": ["override"]}
                })),
                ..Default::default()
            },
        );

        let effective = effective_pi_model(&config, "m").expect("effective");
        assert!(effective.reasoning);
        assert_eq!(effective.headers["layer"], "model");
        assert_eq!(effective.header_layers.provider["layer"], "provider");
        assert_eq!(effective.header_layers.model["layer"], "model");
        assert_eq!(
            effective.compat,
            Some(json!({
                "supportsDeveloperRole": true,
                "supportsStore": false,
                "openRouterRouting": {
                    "only": ["model"],
                    "zdr": true,
                    "order": ["override"]
                }
            }))
        );
    }

    #[test]
    fn effective_compat_uses_the_pinned_composer_spread_result() {
        let config: PiManagedProviderConfig = serde_json::from_value(json!({
            "api": "openai-responses",
            "baseUrl": "https://compat.example/v1",
            "compat": {
                "openRouterRouting": ["first", "second"],
                "chatTemplateKwargs": "ab",
                "baseOnly": true
            },
            "models": [{
                "id": "m",
                "compat": {"supportsStore": true}
            }],
            "modelOverrides": {
                "m": {
                    "compat": {
                        "openRouterRouting": null,
                        "chatTemplateKwargs": {"named": true},
                        "overlayOnly": true
                    }
                }
            }
        }))
        .expect("deserialize pinned compat vector");

        assert_eq!(
            effective_pi_model(&config, "m")
                .expect("effective compat")
                .compat,
            Some(json!({
                "openRouterRouting": {"0": "first", "1": "second"},
                "chatTemplateKwargs": {"0": "a", "1": "b", "named": true},
                "baseOnly": true,
                "supportsStore": true,
                "overlayOnly": true
            }))
        );
    }

    #[test]
    fn effective_compat_rejects_unrepresentable_javascript_surrogate_spread() {
        let config: PiManagedProviderConfig = serde_json::from_value(json!({
            "api": "openai-responses",
            "baseUrl": "https://compat.example/v1",
            "compat": {"chatTemplateKwargs": "😀"},
            "models": [{"id": "m"}],
            "modelOverrides": {
                "m": {"compat": {"chatTemplateKwargs": {"named": true}}}
            }
        }))
        .expect("deserialize pinned compat vector");

        assert_eq!(
            effective_pi_model(&config, "m"),
            Err(PiConfigError::UnrepresentableCompat {
                json_pointer: "/modelOverrides/m/compat".to_string(),
            })
        );
    }

    #[test]
    fn fractional_pi_numbers_survive_managed_round_trip() {
        let mut fractional = model("fractional");
        fractional.context_window = Some(number(128000.5));
        fractional.max_tokens = Some(number(16384.25));
        let config = provider(vec![fractional]);
        let encoded = serde_json::to_value(&config).expect("serialize");
        let decoded: PiManagedProviderConfig =
            serde_json::from_value(encoded).expect("deserialize");
        let effective = effective_pi_model(&decoded, "fractional").expect("effective");
        assert_eq!(effective.context_window.get(), 128000.5);
        assert_eq!(effective.max_tokens.get(), 16384.25);
    }

    #[test]
    fn managed_validation_rejects_duplicates_but_preserves_schema_valid_thinking_maps() {
        let duplicate = provider(vec![model("same"), model("same")]);
        assert_eq!(
            validate_pi_managed_provider(&duplicate),
            Err(PiConfigError::DuplicateModelId("same".into()))
        );

        let mut future_thinking = model("thinking");
        future_thinking.thinking_level_map = Some(BTreeMap::from([
            ("high".into(), json!("native-high")),
            ("future".into(), json!({"opaque": true})),
        ]));
        let future_config = provider(vec![future_thinking]);
        validate_pi_managed_provider(&future_config)
            .expect("pinned-schema additional keys remain manageable");
        assert_eq!(
            serde_json::to_value(&future_config)
                .expect("serialize")
                .pointer("/models/0/thinkingLevelMap/future"),
            Some(&json!({"opaque": true}))
        );

        let mut invalid_known = model("invalid-known");
        invalid_known.thinking_level_map = Some(BTreeMap::from([("low".into(), json!(2))]));
        assert_eq!(
            validate_pi_managed_provider(&provider(vec![invalid_known])),
            Err(PiConfigError::InvalidThinkingLevelValue {
                model_id: "invalid-known".into(),
                level: "low".into()
            })
        );
    }

    #[test]
    fn future_cost_members_survive_model_override_and_effective_projection() {
        let config: PiManagedProviderConfig = serde_json::from_value(json!({
            "api": "anthropic-messages",
            "baseUrl": "https://cost.example",
            "models": [{
                "id": "m",
                "cost": {
                    "input": 1.0,
                    "output": 2.0,
                    "cacheRead": 0.5,
                    "cacheWrite": 0.25,
                    "futureRate": {"opaque": true},
                    "tiers": [{
                        "inputTokensAbove": 100.0,
                        "input": 1.0,
                        "output": 2.0,
                        "cacheRead": 0.5,
                        "cacheWrite": 0.25,
                        "futureTierField": ["preserved"]
                    }]
                }
            }],
            "modelOverrides": {
                "m": {
                    "cost": {
                        "output": 3.0,
                        "futureOverrideRate": "preserved"
                    }
                }
            }
        }))
        .expect("deserialize future cost members");
        validate_pi_managed_provider(&config).expect("future cost members are manageable");

        let round_trip = serde_json::to_value(&config).expect("serialize managed config");
        assert_eq!(
            round_trip.pointer("/models/0/cost/futureRate"),
            Some(&json!({"opaque": true}))
        );
        assert_eq!(
            round_trip.pointer("/models/0/cost/tiers/0/futureTierField"),
            Some(&json!(["preserved"]))
        );
        let effective =
            serde_json::to_value(effective_pi_model(&config, "m").expect("effective model"))
                .expect("serialize effective model");
        assert_eq!(effective.pointer("/cost/output"), Some(&json!(3.0)));
        assert_eq!(
            effective.pointer("/cost/futureRate"),
            Some(&json!({"opaque": true}))
        );
        assert_eq!(
            effective.pointer("/cost/futureOverrideRate"),
            Some(&json!("preserved"))
        );
        assert_eq!(
            effective.pointer("/cost/tiers/0/futureTierField"),
            Some(&json!(["preserved"]))
        );
    }

    #[test]
    fn empty_cost_tiers_remain_distinct_from_absent_at_managed_and_effective_boundaries() {
        let config: PiManagedProviderConfig = serde_json::from_value(json!({
            "api": "anthropic-messages",
            "baseUrl": "https://cost.example",
            "models": [
                {
                    "id": "empty",
                    "cost": {
                        "input": 1.0,
                        "output": 2.0,
                        "cacheRead": 0.5,
                        "cacheWrite": 0.25,
                        "tiers": []
                    }
                },
                {
                    "id": "absent",
                    "cost": {
                        "input": 1.0,
                        "output": 2.0,
                        "cacheRead": 0.5,
                        "cacheWrite": 0.25
                    }
                }
            ]
        }))
        .expect("deserialize explicit and absent tiers");

        let managed = serde_json::to_value(&config).expect("serialize managed config");
        assert_eq!(managed.pointer("/models/0/cost/tiers"), Some(&json!([])));
        assert_eq!(managed.pointer("/models/1/cost/tiers"), None);

        let empty_effective = serde_json::to_value(
            effective_pi_model(&config, "empty").expect("effective empty tiers"),
        )
        .expect("serialize effective empty tiers");
        let absent_effective = serde_json::to_value(
            effective_pi_model(&config, "absent").expect("effective absent tiers"),
        )
        .expect("serialize effective absent tiers");
        assert_eq!(empty_effective.pointer("/cost/tiers"), Some(&json!([])));
        assert_eq!(absent_effective.pointer("/cost/tiers"), None);
    }

    #[test]
    fn diagnostic_reason_serialization_is_structured() {
        let reason = PiDiagnosticReason::new(
            PiDiagnosticLayer::Gateway,
            PiReasonCode::UnsupportedGatewayFamily,
            Some("/models/0/api".into()),
        );
        assert_eq!(
            serde_json::to_value(reason).expect("serialize"),
            json!({
                "layer": "gateway",
                "code": "unsupported_gateway_family",
                "jsonPointer": "/models/0/api"
            })
        );

        let credential_reason = PiDiagnosticReason::new(
            PiDiagnosticLayer::Gateway,
            PiReasonCode::UnsupportedCredentialKind,
            Some("/apiKey".into()),
        );
        assert_eq!(
            serde_json::to_value(credential_reason).expect("serialize"),
            json!({
                "layer": "gateway",
                "code": "unsupported_credential_kind",
                "jsonPointer": "/apiKey"
            })
        );

        let compat_reason = PiDiagnosticReason::new(
            PiDiagnosticLayer::Composition,
            PiReasonCode::UnrepresentableCompat,
            Some("/modelOverrides/m/compat".into()),
        );
        assert_eq!(
            serde_json::to_value(compat_reason).expect("serialize"),
            json!({
                "layer": "composition",
                "code": "unrepresentable_compat",
                "jsonPointer": "/modelOverrides/m/compat"
            })
        );
    }
}

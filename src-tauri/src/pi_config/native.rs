//! Public, side-effect-free inspection service for Pi's native catalog.
//!
//! This module orchestrates independent raw, managed, composer and gateway
//! assessments. No assessment is allowed to gate execution of a sibling layer.

#![allow(dead_code)]

use super::composer::{
    compose_explicit_custom_catalog, PiComposerReasonCode, PiComposerStatus, PiNativeComposition,
};
use super::document::{pi_raw_provider_fingerprint, read_pi_models_document, PiRawProviderEntry};
use super::gateway::{
    assess_composition, PiGatewayAssessment, PiGatewayCapability, PiGatewayReasonCode,
};
use super::model::{
    validate_pi_managed_provider, PiCompositionStatus, PiConfigError, PiDiagnosticLayer,
    PiDiagnosticReason, PiGatewayStatus, PiManagedAssessment, PiManagedProviderConfig,
    PiManagementStatus, PiNativeDiagnostic, PiNativeEntryKind, PiRawNativeValidity, PiReasonCode,
};
use super::raw_schema::{
    evaluate_provider_value, PiRawReasonCode, PiRawSchemaEvaluation, PiRawValidity,
};
use crate::config::get_home_dir;
use crate::error::AppError;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use url::Url;

/// Built-in provider IDs at Pi commit
/// `ab366ebe94cacd419d986be454f12b1b9913aaca`.
const PI_BUILTIN_PROVIDER_KEYS: &[&str] = &[
    "amazon-bedrock",
    "ant-ling",
    "anthropic",
    "azure-openai-responses",
    "cerebras",
    "cloudflare-ai-gateway",
    "cloudflare-workers-ai",
    "deepseek",
    "fireworks",
    "github-copilot",
    "google",
    "google-vertex",
    "groq",
    "huggingface",
    "kimi-coding",
    "minimax",
    "minimax-cn",
    "mistral",
    "moonshotai",
    "moonshotai-cn",
    "nvidia",
    "openai",
    "openai-codex",
    "opencode",
    "opencode-go",
    "openrouter",
    "qwen-token-plan",
    "qwen-token-plan-cn",
    "radius",
    "together",
    "vercel-ai-gateway",
    "xai",
    "xiaomi",
    "xiaomi-token-plan-ams",
    "xiaomi-token-plan-cn",
    "xiaomi-token-plan-sgp",
    "zai",
    "zai-coding-cn",
];

const RECOGNIZED_PROVIDER_FIELDS: &[&str] = &[
    "name",
    "baseUrl",
    "apiKey",
    "api",
    "oauth",
    "headers",
    "compat",
    "authHeader",
    "models",
    "modelOverrides",
];

#[derive(Debug, Clone)]
pub(crate) struct PiNativeEntryInspection {
    pub diagnostic: PiNativeDiagnostic,
    pub managed_config: Option<PiManagedProviderConfig>,
    pub composition: PiNativeComposition,
}

#[derive(Debug)]
struct ManagedResult {
    assessment: PiManagedAssessment,
    config: Option<PiManagedProviderConfig>,
    reasons: Vec<PiDiagnosticReason>,
}

/// The public read-only service entry used by commands and certification tests.
pub(crate) struct PiNativeInspectionService;

impl PiNativeInspectionService {
    pub(crate) fn inspect_current(
        managed_claims: &BTreeMap<String, String>,
    ) -> Result<Vec<PiNativeDiagnostic>, AppError> {
        Self::inspect_catalog(&get_pi_models_path()?, managed_claims)
    }

    pub(crate) fn inspect_catalog(
        path: &Path,
        managed_claims: &BTreeMap<String, String>,
    ) -> Result<Vec<PiNativeDiagnostic>, AppError> {
        let document = read_pi_models_document(path)?;
        Ok(document
            .providers()
            .iter()
            .map(|(provider_key, entry)| {
                analyze_native_entry(provider_key, entry, managed_claims).diagnostic
            })
            .collect())
    }

    pub(crate) fn inspect_entry(
        path: &Path,
        provider_key: &str,
        managed_claims: &BTreeMap<String, String>,
    ) -> Result<Option<PiNativeEntryInspection>, AppError> {
        let document = read_pi_models_document(path)?;
        Ok(document
            .providers()
            .get(provider_key)
            .map(|entry| analyze_native_entry(provider_key, entry, managed_claims)))
    }
}

pub(crate) fn inspect_current_pi_native_catalog(
    managed_claims: &BTreeMap<String, String>,
) -> Result<Vec<PiNativeDiagnostic>, AppError> {
    PiNativeInspectionService::inspect_current(managed_claims)
}

pub(crate) fn inspect_pi_native_catalog(
    path: &Path,
    managed_claims: &BTreeMap<String, String>,
) -> Result<Vec<PiNativeDiagnostic>, AppError> {
    PiNativeInspectionService::inspect_catalog(path, managed_claims)
}

pub(crate) fn inspect_pi_native_entry(
    path: &Path,
    provider_key: &str,
    managed_claims: &BTreeMap<String, String>,
) -> Result<Option<PiNativeEntryInspection>, AppError> {
    PiNativeInspectionService::inspect_entry(path, provider_key, managed_claims)
}

/// Compose a database-authoritative managed provider through the same raw and
/// composer layers used by native inspection. Runtime construction must not
/// reimplement inheritance or field semantics.
pub(crate) fn compose_managed_pi_provider(
    provider_key: &str,
    config: &PiManagedProviderConfig,
) -> Result<PiNativeComposition, AppError> {
    validate_pi_managed_provider(config)
        .map_err(|error| AppError::InvalidInput(error.to_string()))?;
    let value =
        serde_json::to_value(config).map_err(|source| AppError::JsonSerialize { source })?;
    let raw = evaluate_provider_value(&value);
    let provider = raw.valid_provider.as_ref().ok_or_else(|| {
        AppError::Config(format!(
            "managed Pi provider '{provider_key}' did not pass the pinned raw schema"
        ))
    })?;
    Ok(compose_explicit_custom_catalog(provider_key, provider))
}

fn normalize_pi_agent_dir(value: &str, home: &Path) -> Result<PathBuf, AppError> {
    if value == "~" {
        return Ok(home.to_path_buf());
    }
    if let Some(suffix) = value.strip_prefix("~/") {
        return Ok(home.join(suffix));
    }
    #[cfg(windows)]
    if let Some(suffix) = value.strip_prefix("~\\") {
        return Ok(home.join(suffix));
    }
    if value.starts_with("file://") {
        let url = Url::parse(value).map_err(|error| {
            AppError::Config(format!("invalid Pi agent directory URL: {error}"))
        })?;
        return url.to_file_path().map_err(|_| {
            AppError::Config(format!(
                "Pi agent directory URL is not a local file path: {value}"
            ))
        });
    }
    Ok(PathBuf::from(value))
}

pub(crate) fn get_pi_agent_dir_for_override(
    override_dir: Option<&str>,
) -> Result<PathBuf, AppError> {
    if let Some(override_dir) = override_dir
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(crate::settings::resolve_override_path(override_dir));
    }
    let Some(raw) = std::env::var_os("PI_CODING_AGENT_DIR") else {
        return Ok(get_home_dir().join(".pi").join("agent"));
    };
    if raw.is_empty() {
        return Ok(get_home_dir().join(".pi").join("agent"));
    }
    normalize_pi_agent_dir(&raw.to_string_lossy(), &get_home_dir())
}

pub(crate) fn get_pi_agent_dir() -> Result<PathBuf, AppError> {
    if let Some(override_dir) = crate::settings::get_pi_override_dir() {
        return Ok(override_dir);
    }
    get_pi_agent_dir_for_override(None)
}

pub(crate) fn get_pi_models_path_for_override(
    override_dir: Option<&str>,
) -> Result<PathBuf, AppError> {
    Ok(get_pi_agent_dir_for_override(override_dir)?.join("models.json"))
}

pub(crate) fn get_pi_models_path() -> Result<PathBuf, AppError> {
    Ok(get_pi_agent_dir()?.join("models.json"))
}

fn analyze_native_entry(
    provider_key: &str,
    entry: &PiRawProviderEntry,
    managed_claims: &BTreeMap<String, String>,
) -> PiNativeEntryInspection {
    // Raw, composition and managed conversion are deliberately invoked from
    // the same immutable JSON value. Neither result controls whether a sibling
    // assessment is attempted.
    let raw = evaluate_provider_value(&entry.value);
    let raw_validity = map_raw_validity(raw.validity);
    let kind = classify_kind(provider_key, &entry.value, raw.validity);
    let composition = match (raw.valid_provider.as_ref(), kind) {
        (Some(provider), PiNativeEntryKind::CustomCatalog) => {
            compose_explicit_custom_catalog(provider_key, provider)
        }
        (Some(_), _) => PiNativeComposition::catalog_required("/models"),
        (None, _) => PiNativeComposition::unavailable_without_valid_raw(),
    };
    let managed = assess_managed(raw.validity, kind, &entry.value);
    let gateway = if raw.validity == PiRawValidity::Valid {
        assess_composition(&composition)
    } else {
        PiGatewayAssessment {
            capability: PiGatewayCapability::Unknown,
            reasons: Vec::new(),
            plans: Vec::new(),
        }
    };

    let management_status = managed_claims
        .get(provider_key)
        .map(|provider_id| PiManagementStatus::Managed {
            provider_id: provider_id.clone(),
        })
        .unwrap_or_else(|| {
            if raw.validity == PiRawValidity::Valid
                && managed.assessment == PiManagedAssessment::Manageable
            {
                PiManagementStatus::Importable
            } else {
                PiManagementStatus::Unsupported
            }
        });

    let mut reasons = map_raw_reasons(&raw);
    extend_reasons(&mut reasons, managed.reasons.clone());
    extend_reasons(&mut reasons, map_composer_reasons(&composition));
    extend_reasons(&mut reasons, map_gateway_reasons(&gateway));

    PiNativeEntryInspection {
        diagnostic: PiNativeDiagnostic {
            provider_key: provider_key.to_string(),
            display_name: entry
                .value
                .get("name")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned),
            fingerprint: pi_raw_provider_fingerprint(&entry.raw_source),
            kind,
            raw_validity,
            managed_assessment: managed.assessment,
            composition_status: map_composition_status(composition.status),
            management_status,
            gateway_status: map_gateway_status(gateway.capability),
            reasons,
        },
        managed_config: managed.config,
        composition,
    }
}

fn assess_managed(
    raw_validity: PiRawValidity,
    kind: PiNativeEntryKind,
    value: &Value,
) -> ManagedResult {
    if raw_validity != PiRawValidity::Valid {
        return ManagedResult {
            assessment: PiManagedAssessment::Unsupported,
            config: None,
            reasons: Vec::new(),
        };
    }
    if kind != PiNativeEntryKind::CustomCatalog {
        let mut reasons = vec![diagnostic_reason(
            PiDiagnosticLayer::Managed,
            PiReasonCode::CatalogRequired,
            "/models",
        )];
        if value
            .get("modelOverrides")
            .and_then(Value::as_object)
            .is_some_and(|overrides| !overrides.is_empty())
        {
            reasons.push(diagnostic_reason(
                PiDiagnosticLayer::Managed,
                PiReasonCode::ModelOverridesOnly,
                "/modelOverrides",
            ));
        }
        return ManagedResult {
            assessment: PiManagedAssessment::Unsupported,
            config: None,
            reasons,
        };
    }

    let config = match serde_json::from_value::<PiManagedProviderConfig>(value.clone()) {
        Ok(config) => config,
        Err(_) => {
            return ManagedResult {
                assessment: PiManagedAssessment::Unsupported,
                config: None,
                reasons: vec![diagnostic_reason(
                    PiDiagnosticLayer::Managed,
                    PiReasonCode::ManagedTypeConversionFailed,
                    "",
                )],
            };
        }
    };
    let mut reasons = collect_managed_reasons(&config);
    if reasons.is_empty() {
        if let Err(error) = validate_pi_managed_provider(&config) {
            reasons.push(managed_validation_reason(error));
        }
    }
    if !reasons.is_empty() {
        return ManagedResult {
            assessment: PiManagedAssessment::Unsupported,
            config: None,
            reasons,
        };
    }
    ManagedResult {
        assessment: PiManagedAssessment::Manageable,
        config: Some(config),
        reasons,
    }
}

fn collect_managed_reasons(config: &PiManagedProviderConfig) -> Vec<PiDiagnosticReason> {
    let mut reasons = Vec::new();
    let mut ids = HashSet::with_capacity(config.models.len());
    if config.models.is_empty() {
        add_reason(
            &mut reasons,
            diagnostic_reason(
                PiDiagnosticLayer::Managed,
                PiReasonCode::MissingExplicitModels,
                "/models",
            ),
        );
    }
    if config
        .base_url
        .as_deref()
        .is_some_and(|value| !valid_http_endpoint(value))
    {
        add_reason(
            &mut reasons,
            diagnostic_reason(
                PiDiagnosticLayer::Managed,
                PiReasonCode::InvalidEndpoint,
                "/baseUrl",
            ),
        );
    }
    for (index, model) in config.models.iter().enumerate() {
        let pointer = format!("/models/{index}");
        if model.id.is_empty() {
            add_reason(
                &mut reasons,
                diagnostic_reason(
                    PiDiagnosticLayer::Managed,
                    PiReasonCode::EmptyModelId,
                    &format!("{pointer}/id"),
                ),
            );
        } else if !ids.insert(model.id.as_str()) {
            add_reason(
                &mut reasons,
                diagnostic_reason(
                    PiDiagnosticLayer::Managed,
                    PiReasonCode::DuplicateModelId,
                    &format!("{pointer}/id"),
                ),
            );
        }
        if model
            .base_url
            .as_deref()
            .is_some_and(|value| !valid_http_endpoint(value))
        {
            add_reason(
                &mut reasons,
                diagnostic_reason(
                    PiDiagnosticLayer::Managed,
                    PiReasonCode::InvalidEndpoint,
                    &format!("{pointer}/baseUrl"),
                ),
            );
        }
        if model.api.is_none() && config.api.is_none() {
            add_reason(
                &mut reasons,
                diagnostic_reason(
                    PiDiagnosticLayer::Managed,
                    PiReasonCode::MissingEffectiveApi,
                    &format!("{pointer}/api"),
                ),
            );
        }
        if model.base_url.is_none() && config.base_url.is_none() {
            add_reason(
                &mut reasons,
                diagnostic_reason(
                    PiDiagnosticLayer::Managed,
                    PiReasonCode::MissingEffectiveEndpoint,
                    &format!("{pointer}/baseUrl"),
                ),
            );
        }
        for (field, value) in [
            ("contextWindow", model.context_window),
            ("maxTokens", model.max_tokens),
        ] {
            if value.is_some_and(|value| value.get() <= 0.0) {
                add_reason(
                    &mut reasons,
                    diagnostic_reason(
                        PiDiagnosticLayer::Managed,
                        PiReasonCode::NonPositiveModelLimit,
                        &format!("{pointer}/{field}"),
                    ),
                );
            }
        }
    }

    for (model_id, model_override) in &config.model_overrides {
        let pointer = format!("/modelOverrides/{}", escape_json_pointer(model_id));
        if !ids.contains(model_id.as_str()) {
            add_reason(
                &mut reasons,
                diagnostic_reason(
                    PiDiagnosticLayer::Managed,
                    PiReasonCode::UnknownModelOverride,
                    &pointer,
                ),
            );
        }
        for (field, value) in [
            ("contextWindow", model_override.context_window),
            ("maxTokens", model_override.max_tokens),
        ] {
            if value.is_some_and(|value| value.get() <= 0.0) {
                add_reason(
                    &mut reasons,
                    diagnostic_reason(
                        PiDiagnosticLayer::Managed,
                        PiReasonCode::NonPositiveModelLimit,
                        &format!("{pointer}/{field}"),
                    ),
                );
            }
        }
    }
    reasons
}

fn managed_validation_reason(error: PiConfigError) -> PiDiagnosticReason {
    if let PiConfigError::UnrepresentableCompat { json_pointer } = &error {
        return diagnostic_reason(
            PiDiagnosticLayer::Managed,
            PiReasonCode::UnrepresentableCompat,
            json_pointer,
        );
    }
    let (code, pointer) = match error {
        PiConfigError::ProviderHasNoModels => (PiReasonCode::MissingExplicitModels, "/models"),
        PiConfigError::EmptyApiId => (PiReasonCode::ManagedTypeConversionFailed, "/api"),
        PiConfigError::EmptyModelId => (PiReasonCode::EmptyModelId, "/models"),
        PiConfigError::DuplicateModelId(_) => (PiReasonCode::DuplicateModelId, "/models"),
        PiConfigError::ModelNotFound(_) => (PiReasonCode::ManagedTypeConversionFailed, "/models"),
        PiConfigError::MissingEffectiveApi { .. } => (PiReasonCode::MissingEffectiveApi, "/models"),
        PiConfigError::MissingEffectiveEndpoint { .. } => {
            (PiReasonCode::MissingEffectiveEndpoint, "/models")
        }
        PiConfigError::InvalidEndpoint { .. } => (PiReasonCode::InvalidEndpoint, "/baseUrl"),
        PiConfigError::UnknownModelOverride(_) => {
            (PiReasonCode::UnknownModelOverride, "/modelOverrides")
        }
        PiConfigError::InvalidCompat { .. } => (PiReasonCode::InvalidCompat, "/compat"),
        PiConfigError::UnrepresentableCompat { .. } => {
            unreachable!("handled before the exhaustive mapping")
        }
        PiConfigError::EmptyOptionalField { .. } => (PiReasonCode::EmptyOptionalField, ""),
        PiConfigError::NonPositiveModelLimit { .. } => {
            (PiReasonCode::NonPositiveModelLimit, "/models")
        }
        PiConfigError::InvalidThinkingLevelValue { .. } => {
            (PiReasonCode::InvalidThinkingLevel, "/models")
        }
    };
    diagnostic_reason(PiDiagnosticLayer::Managed, code, pointer)
}

fn classify_kind(
    provider_key: &str,
    value: &Value,
    raw_validity: PiRawValidity,
) -> PiNativeEntryKind {
    if PI_BUILTIN_PROVIDER_KEYS.contains(&provider_key) {
        return PiNativeEntryKind::BuiltInOverlay;
    }
    if raw_validity != PiRawValidity::Valid {
        return PiNativeEntryKind::UnknownShape;
    }
    let Some(object) = value.as_object() else {
        return PiNativeEntryKind::UnknownShape;
    };
    if object
        .get("models")
        .and_then(Value::as_array)
        .is_some_and(|models| !models.is_empty())
    {
        PiNativeEntryKind::CustomCatalog
    } else if object
        .keys()
        .any(|key| RECOGNIZED_PROVIDER_FIELDS.contains(&key.as_str()))
    {
        PiNativeEntryKind::ExtensionOverlay
    } else {
        PiNativeEntryKind::UnknownShape
    }
}

fn map_raw_validity(validity: PiRawValidity) -> PiRawNativeValidity {
    match validity {
        PiRawValidity::Valid => PiRawNativeValidity::Valid,
        PiRawValidity::Invalid => PiRawNativeValidity::Invalid,
        PiRawValidity::Unknown => PiRawNativeValidity::Unknown,
    }
}

fn map_composition_status(status: PiComposerStatus) -> PiCompositionStatus {
    match status {
        PiComposerStatus::Composed => PiCompositionStatus::Composed,
        PiComposerStatus::Failed => PiCompositionStatus::Failed,
        PiComposerStatus::Unknown => PiCompositionStatus::Unknown,
    }
}

fn map_gateway_status(capability: PiGatewayCapability) -> PiGatewayStatus {
    match capability {
        PiGatewayCapability::Proxyable => PiGatewayStatus::Proxyable,
        PiGatewayCapability::DirectOnly => PiGatewayStatus::DirectOnly,
        PiGatewayCapability::Unknown => PiGatewayStatus::Unknown,
    }
}

fn map_raw_reasons(raw: &PiRawSchemaEvaluation) -> Vec<PiDiagnosticReason> {
    raw.reasons
        .iter()
        .map(|reason| {
            diagnostic_reason(
                PiDiagnosticLayer::RawSchema,
                match reason.code {
                    PiRawReasonCode::SchemaMismatch => PiReasonCode::RawSchemaMismatch,
                    PiRawReasonCode::UnsupportedOperator => {
                        PiReasonCode::RawSchemaUnsupportedOperator
                    }
                    PiRawReasonCode::PinDrift => PiReasonCode::RawSchemaPinDrift,
                    PiRawReasonCode::AmbiguousSchema => PiReasonCode::RawSchemaAmbiguous,
                },
                &reason.json_pointer,
            )
        })
        .collect()
}

fn map_composer_reasons(composition: &PiNativeComposition) -> Vec<PiDiagnosticReason> {
    composition
        .reasons
        .iter()
        .map(|reason| {
            diagnostic_reason(
                PiDiagnosticLayer::Composition,
                match reason.code {
                    PiComposerReasonCode::CatalogRequired => PiReasonCode::CatalogRequired,
                    PiComposerReasonCode::MissingExplicitModels => {
                        PiReasonCode::MissingExplicitModels
                    }
                    PiComposerReasonCode::MissingEffectiveApi => PiReasonCode::MissingEffectiveApi,
                    PiComposerReasonCode::MissingEffectiveEndpoint => {
                        PiReasonCode::MissingEffectiveEndpoint
                    }
                    PiComposerReasonCode::NonPositiveModelLimit => {
                        PiReasonCode::NonPositiveModelLimit
                    }
                    PiComposerReasonCode::UnrepresentableCompat => {
                        PiReasonCode::UnrepresentableCompat
                    }
                    PiComposerReasonCode::CompositionFailed => PiReasonCode::CompositionFailed,
                },
                &reason.json_pointer,
            )
        })
        .collect()
}

fn map_gateway_reasons(gateway: &PiGatewayAssessment) -> Vec<PiDiagnosticReason> {
    gateway
        .reasons
        .iter()
        .map(|reason| {
            diagnostic_reason(
                PiDiagnosticLayer::Gateway,
                match reason.code {
                    PiGatewayReasonCode::UnsupportedFamily => {
                        PiReasonCode::UnsupportedGatewayFamily
                    }
                    PiGatewayReasonCode::UnsupportedCredentialKind => {
                        PiReasonCode::UnsupportedCredentialKind
                    }
                    PiGatewayReasonCode::InvalidEndpoint => PiReasonCode::InvalidEndpoint,
                    PiGatewayReasonCode::MissingCredential => {
                        PiReasonCode::GatewayCredentialUnavailable
                    }
                    PiGatewayReasonCode::InvalidHeaderName => PiReasonCode::InvalidHeaderName,
                    PiGatewayReasonCode::InvalidHeaderValue => PiReasonCode::InvalidHeaderValue,
                    PiGatewayReasonCode::ProtectedHeader => PiReasonCode::ProtectedHeader,
                    PiGatewayReasonCode::DeferredValueUnavailable => {
                        PiReasonCode::DeferredValueUnavailable
                    }
                },
                &reason.json_pointer,
            )
        })
        .collect()
}

fn diagnostic_reason(
    layer: PiDiagnosticLayer,
    code: PiReasonCode,
    pointer: &str,
) -> PiDiagnosticReason {
    PiDiagnosticReason::new(layer, code, Some(pointer.to_string()))
}

fn add_reason(reasons: &mut Vec<PiDiagnosticReason>, reason: PiDiagnosticReason) {
    if !reasons.contains(&reason) {
        reasons.push(reason);
    }
}

fn extend_reasons(
    reasons: &mut Vec<PiDiagnosticReason>,
    candidates: impl IntoIterator<Item = PiDiagnosticReason>,
) {
    for reason in candidates {
        add_reason(reasons, reason);
    }
}

fn valid_http_endpoint(value: &str) -> bool {
    Url::parse(value)
        .ok()
        .is_some_and(|url| matches!(url.scheme(), "http" | "https") && url.host().is_some())
}

fn escape_json_pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pi_config::model::effective_pi_model;
    use serde_json::json;
    use std::fs;

    fn by_key<'a>(diagnostics: &'a [PiNativeDiagnostic], key: &str) -> &'a PiNativeDiagnostic {
        diagnostics
            .iter()
            .find(|diagnostic| diagnostic.provider_key == key)
            .expect("diagnostic")
    }

    fn has_reason(
        diagnostic: &PiNativeDiagnostic,
        layer: PiDiagnosticLayer,
        code: PiReasonCode,
        pointer: &str,
    ) -> bool {
        diagnostic.reasons.iter().any(|reason| {
            reason.layer == layer
                && reason.code == code
                && reason.json_pointer.as_deref() == Some(pointer)
        })
    }

    #[test]
    fn public_inspection_service_certifies_the_native_state_matrix() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        fs::write(
            &path,
            r#"{
  "providers": {
    "anthropic": {"baseUrl": "https://builtin.example"},
    "extension": {"api": "openai-responses"},
    "future-custom": {
      "api": "future-wire-v9",
      "baseUrl": "https://future.example/v9",
      "apiKey": "$FUTURE_KEY",
      "models": [{"id": "future"}]
    },
    "known-custom": {
      "api": "openai-responses",
      "baseUrl": "https://known.example/v1",
      "apiKey": "!read-secret",
      "headers": {"x-tenant": "${TENANT}"},
      "models": [{"id": "known"}]
    },
    "malformed": {"models": "not-an-array"}
  }
}"#,
        )
        .expect("write fixture");
        let bytes_before = fs::read(&path).expect("before");
        let claims = BTreeMap::new();
        let diagnostics =
            PiNativeInspectionService::inspect_catalog(&path, &claims).expect("inspect service");
        assert_eq!(fs::read(&path).expect("after"), bytes_before);
        assert_eq!(diagnostics.len(), 5);

        for key in ["anthropic", "extension"] {
            let diagnostic = by_key(&diagnostics, key);
            assert_eq!(diagnostic.raw_validity, PiRawNativeValidity::Valid);
            assert_eq!(diagnostic.composition_status, PiCompositionStatus::Unknown);
            assert_eq!(diagnostic.gateway_status, PiGatewayStatus::Unknown);
            assert!(has_reason(
                diagnostic,
                PiDiagnosticLayer::Composition,
                PiReasonCode::CatalogRequired,
                "/models"
            ));
        }

        let future = by_key(&diagnostics, "future-custom");
        assert_eq!(future.raw_validity, PiRawNativeValidity::Valid);
        assert_eq!(future.composition_status, PiCompositionStatus::Composed);
        assert_eq!(future.managed_assessment, PiManagedAssessment::Manageable);
        assert_eq!(future.management_status, PiManagementStatus::Importable);
        assert_eq!(future.gateway_status, PiGatewayStatus::DirectOnly);
        assert!(has_reason(
            future,
            PiDiagnosticLayer::Gateway,
            PiReasonCode::UnsupportedGatewayFamily,
            "/models/0/api"
        ));

        let known = by_key(&diagnostics, "known-custom");
        assert_eq!(known.composition_status, PiCompositionStatus::Composed);
        assert_eq!(known.management_status, PiManagementStatus::Importable);
        assert_eq!(known.gateway_status, PiGatewayStatus::Proxyable);

        let malformed = by_key(&diagnostics, "malformed");
        assert_eq!(malformed.raw_validity, PiRawNativeValidity::Invalid);
        assert_eq!(malformed.composition_status, PiCompositionStatus::Unknown);
        assert_eq!(malformed.gateway_status, PiGatewayStatus::Unknown);
    }

    #[test]
    fn public_inspection_fails_closed_for_unrepresentable_compat_spread() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        fs::write(
            &path,
            r#"{"providers":{"surrogate":{
  "api":"openai-responses",
  "baseUrl":"https://compat.example/v1",
  "apiKey":"literal",
  "compat":{"chatTemplateKwargs":"😀"},
  "models":[{"id":"m"}],
  "modelOverrides":{"m":{"compat":{"chatTemplateKwargs":{"named":true}}}}
}}}"#,
        )
        .expect("write");

        let diagnostic =
            &PiNativeInspectionService::inspect_catalog(&path, &BTreeMap::new()).unwrap()[0];
        assert_eq!(diagnostic.raw_validity, PiRawNativeValidity::Valid);
        assert_eq!(
            diagnostic.managed_assessment,
            PiManagedAssessment::Unsupported
        );
        assert_eq!(diagnostic.composition_status, PiCompositionStatus::Unknown);
        assert_eq!(diagnostic.gateway_status, PiGatewayStatus::Unknown);
        assert!(has_reason(
            diagnostic,
            PiDiagnosticLayer::Managed,
            PiReasonCode::UnrepresentableCompat,
            "/modelOverrides/m/compat"
        ));
        assert!(has_reason(
            diagnostic,
            PiDiagnosticLayer::Composition,
            PiReasonCode::UnrepresentableCompat,
            "/modelOverrides/m/compat"
        ));
    }

    #[test]
    fn public_inspection_accepts_surrogates_overridden_before_the_final_result() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        fs::write(
            &path,
            r#"{"providers":{"surrogate":{
  "api":"openai-responses",
  "baseUrl":"https://compat.example/v1",
  "apiKey":"literal",
  "compat":{"chatTemplateKwargs":"😀"},
  "models":[{"id":"m"}],
  "modelOverrides":{"m":{"compat":{"chatTemplateKwargs":{
    "0":"repaired-high",
    "1":"repaired-low",
    "named":true
  }}}}
}}}"#,
        )
        .expect("write");

        let inspection =
            PiNativeInspectionService::inspect_entry(&path, "surrogate", &BTreeMap::new())
                .unwrap()
                .expect("provider");
        assert_eq!(
            inspection.diagnostic.managed_assessment,
            PiManagedAssessment::Manageable
        );
        assert_eq!(
            inspection.diagnostic.composition_status,
            PiCompositionStatus::Composed
        );
        assert_eq!(
            inspection.diagnostic.gateway_status,
            PiGatewayStatus::Proxyable
        );
        assert!(!inspection
            .diagnostic
            .reasons
            .iter()
            .any(|reason| reason.code == PiReasonCode::UnrepresentableCompat));
        assert_eq!(
            inspection.composition.models[0].compat,
            Some(json!({
                "chatTemplateKwargs": {
                    "0": "repaired-high",
                    "1": "repaired-low",
                    "named": true
                }
            }))
        );
    }

    #[test]
    fn managed_rejection_does_not_control_raw_composition() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        fs::write(
            &path,
            r#"{"providers":{"duplicate":{
  "apiKey":"literal",
  "models":[
    {"id":"same","api":"openai-responses","baseUrl":"https://one.example"},
    {"id":"same","api":"future-wire","baseUrl":"https://two.example"}
  ],
  "modelOverrides":{"missing":{"maxTokens":7.5}}
}}}"#,
        )
        .expect("write");
        let diagnostic =
            &PiNativeInspectionService::inspect_catalog(&path, &BTreeMap::new()).unwrap()[0];
        assert_eq!(diagnostic.raw_validity, PiRawNativeValidity::Valid);
        assert_eq!(
            diagnostic.managed_assessment,
            PiManagedAssessment::Unsupported
        );
        assert_eq!(diagnostic.composition_status, PiCompositionStatus::Composed);
        assert!(has_reason(
            diagnostic,
            PiDiagnosticLayer::Managed,
            PiReasonCode::DuplicateModelId,
            "/models/1/id"
        ));
        assert!(has_reason(
            diagnostic,
            PiDiagnosticLayer::Managed,
            PiReasonCode::UnknownModelOverride,
            "/modelOverrides/missing"
        ));
    }

    #[test]
    fn unknown_thinking_shape_is_lossless_through_managed_and_effective_boundaries() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        fs::write(
            &path,
            r#"{"providers":{"thinking":{
  "api":"anthropic-messages",
  "baseUrl":"https://thinking.example",
  "apiKey":"literal",
  "models":[{
    "id":"m",
    "thinkingLevelMap":{"high":"future-high","future":{"opaque":true}}
  }]
}}}"#,
        )
        .expect("write");
        let inspection =
            PiNativeInspectionService::inspect_entry(&path, "thinking", &BTreeMap::new())
                .expect("inspect")
                .expect("entry");
        assert_eq!(
            inspection.diagnostic.raw_validity,
            PiRawNativeValidity::Valid
        );
        assert_eq!(
            inspection.diagnostic.composition_status,
            PiCompositionStatus::Composed
        );
        assert_eq!(
            inspection.composition.models[0]
                .thinking_level_map
                .as_ref()
                .expect("opaque thinking")["future"],
            json!({"opaque": true})
        );
        assert_eq!(
            inspection.diagnostic.managed_assessment,
            PiManagedAssessment::Manageable
        );
        let managed = serde_json::to_value(
            inspection
                .managed_config
                .as_ref()
                .expect("schema-valid managed config"),
        )
        .expect("serialize managed config");
        assert_eq!(
            managed.pointer("/models/0/thinkingLevelMap/future"),
            Some(&json!({"opaque": true}))
        );
        let effective = serde_json::to_value(
            effective_pi_model(
                inspection.managed_config.as_ref().expect("managed config"),
                "m",
            )
            .expect("effective model"),
        )
        .expect("serialize effective model");
        assert_eq!(
            effective.pointer("/thinkingLevelMap/future"),
            Some(&json!({"opaque": true}))
        );
    }

    #[test]
    fn public_inspection_accepts_schema_valid_whitespace_strings() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        fs::write(
            &path,
            r#"{"providers":{"whitespace":{
  "name":" ",
  "api":"anthropic-messages",
  "baseUrl":"https://whitespace.example",
  "apiKey":" ",
  "models":[{"id":" ","name":" "}]
}}}"#,
        )
        .expect("write");
        let inspection =
            PiNativeInspectionService::inspect_entry(&path, "whitespace", &BTreeMap::new())
                .expect("inspect")
                .expect("entry");
        assert_eq!(
            inspection.diagnostic.raw_validity,
            PiRawNativeValidity::Valid
        );
        assert_eq!(
            inspection.diagnostic.managed_assessment,
            PiManagedAssessment::Manageable
        );
        assert_eq!(
            inspection.diagnostic.management_status,
            PiManagementStatus::Importable
        );
        assert_eq!(
            inspection
                .managed_config
                .as_ref()
                .expect("managed config")
                .models[0]
                .id,
            " "
        );
    }

    #[test]
    fn exact_entry_fingerprint_changes_only_when_that_raw_entry_changes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        let claims = BTreeMap::new();
        fs::write(
            &path,
            r#"{"providers":{
  "target":{"api":"openai-responses","baseUrl":"https://target","apiKey":"x","models":[{"id":"m"}]},
  "sibling":{"api":"openai-responses","baseUrl":"https://sibling","apiKey":"x","models":[{"id":"m"}]}
}}"#,
        )
        .expect("write");
        let first = PiNativeInspectionService::inspect_entry(&path, "target", &claims)
            .unwrap()
            .unwrap()
            .diagnostic
            .fingerprint;
        fs::write(
            &path,
            r#"{"providers":{
  "target":{"api":"openai-responses","baseUrl":"https://target","apiKey":"x","models":[{"id":"m"}]},
  "sibling":{"api":"openai-responses","baseUrl":"https://changed","apiKey":"x","models":[{"id":"m"}]}
}}"#,
        )
        .expect("write sibling");
        let after_sibling = PiNativeInspectionService::inspect_entry(&path, "target", &claims)
            .unwrap()
            .unwrap()
            .diagnostic
            .fingerprint;
        assert_eq!(first, after_sibling);
    }

    #[test]
    fn agent_dir_normalization_matches_pi_path_semantics() {
        let temp = tempfile::tempdir().expect("tempdir");
        let file_url = Url::from_file_path(temp.path())
            .expect("absolute temp path")
            .to_string();
        assert_eq!(
            normalize_pi_agent_dir(&file_url, Path::new("/unused")).expect("file URL"),
            temp.path()
        );
        let spaced = "  relative agent dir  ";
        assert_eq!(
            normalize_pi_agent_dir(spaced, Path::new("/unused")).expect("spaced path"),
            PathBuf::from(spaced)
        );
    }

    #[test]
    fn pi_config_error_stays_managed_only() {
        let error = PiConfigError::EmptyApiId;
        assert_eq!(error.to_string(), "Pi API id cannot be empty");
    }
}

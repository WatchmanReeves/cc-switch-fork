//! Credential-blind Pi native model composition.
//!
//! The only Pi-layer input is [`PiRawValidProvider`]. This module does not
//! import managed DTOs or gateway families, and it never resolves credentials,
//! environment variables, commands, files, or network resources.

#![allow(dead_code)]

use super::{
    merge_pi_compat,
    raw_schema::{PiRawApiId, PiRawValidProvider},
};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashSet};

const PROVIDER_FIELDS: &[&str] = &[
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
const MODEL_FIELDS: &[&str] = &[
    "id",
    "name",
    "baseUrl",
    "api",
    "reasoning",
    "thinkingLevelMap",
    "input",
    "cost",
    "contextWindow",
    "maxTokens",
    "headers",
    "compat",
];
const OVERRIDE_FIELDS: &[&str] = &[
    "name",
    "reasoning",
    "thinkingLevelMap",
    "input",
    "cost",
    "contextWindow",
    "maxTokens",
    "headers",
    "compat",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PiComposerStatus {
    Composed,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PiComposerReasonCode {
    CatalogRequired,
    MissingExplicitModels,
    MissingEffectiveApi,
    MissingEffectiveEndpoint,
    NonPositiveModelLimit,
    UnrepresentableCompat,
    CompositionFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PiComposerReason {
    pub code: PiComposerReasonCode,
    pub json_pointer: String,
}

/// One configured header together with the source pointer that Pi resolves.
///
/// `headers` remains the pinned composer's flattened observable result, while
/// these entries retain the provider-vs-model boundary needed to reproduce
/// the later `ModelRuntime` merge on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PiComposedHeader {
    pub name: String,
    pub value: String,
    pub json_pointer: String,
}

/// The lossless native result of pinned Pi composition.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PiComposedNativeModel {
    pub id: String,
    pub name: String,
    pub api: PiRawApiId,
    pub provider: String,
    pub base_url: String,
    pub reasoning: bool,
    pub thinking_level_map: Option<Value>,
    pub input: Value,
    pub cost: Value,
    pub context_window: Value,
    pub max_tokens: Value,
    pub headers: BTreeMap<String, String>,
    pub provider_headers: Vec<PiComposedHeader>,
    pub model_headers: Vec<PiComposedHeader>,
    pub compat: Option<Value>,
    pub api_key: Option<String>,
    pub oauth: Option<Value>,
    pub auth_header: bool,
    pub provider_extra: BTreeMap<String, Value>,
    pub model_extra: BTreeMap<String, Value>,
    pub override_extra: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PiNativeComposition {
    pub status: PiComposerStatus,
    pub provider_id: Option<String>,
    pub provider_name: Option<String>,
    pub provider_base_url: Option<String>,
    pub models: Vec<PiComposedNativeModel>,
    pub ignored_override_keys: Vec<String>,
    pub reasons: Vec<PiComposerReason>,
}

impl PiNativeComposition {
    pub(super) fn unavailable_without_valid_raw() -> Self {
        Self {
            status: PiComposerStatus::Unknown,
            provider_id: None,
            provider_name: None,
            provider_base_url: None,
            models: Vec::new(),
            ignored_override_keys: Vec::new(),
            reasons: Vec::new(),
        }
    }

    pub(super) fn catalog_required(pointer: &str) -> Self {
        Self {
            status: PiComposerStatus::Unknown,
            provider_id: None,
            provider_name: None,
            provider_base_url: None,
            models: Vec::new(),
            ignored_override_keys: Vec::new(),
            reasons: vec![PiComposerReason {
                code: PiComposerReasonCode::CatalogRequired,
                json_pointer: pointer.to_string(),
            }],
        }
    }

    fn failed(code: PiComposerReasonCode, pointer: impl Into<String>) -> Self {
        Self {
            status: PiComposerStatus::Failed,
            provider_id: None,
            provider_name: None,
            provider_base_url: None,
            models: Vec::new(),
            ignored_override_keys: Vec::new(),
            reasons: vec![PiComposerReason {
                code,
                json_pointer: pointer.into(),
            }],
        }
    }

    fn unknown(code: PiComposerReasonCode, pointer: impl Into<String>) -> Self {
        Self {
            status: PiComposerStatus::Unknown,
            provider_id: None,
            provider_name: None,
            provider_base_url: None,
            models: Vec::new(),
            ignored_override_keys: Vec::new(),
            reasons: vec![PiComposerReason {
                code,
                json_pointer: pointer.into(),
            }],
        }
    }
}

pub(super) fn compose_explicit_custom_catalog(
    provider_id: &str,
    provider: &PiRawValidProvider,
) -> PiNativeComposition {
    let Some(provider_object) = provider.raw().as_object() else {
        return PiNativeComposition::failed(PiComposerReasonCode::CompositionFailed, "");
    };
    let Some(definitions) = provider_object
        .get("models")
        .and_then(Value::as_array)
        .filter(|models| !models.is_empty())
    else {
        return PiNativeComposition::failed(PiComposerReasonCode::MissingExplicitModels, "/models");
    };

    let provider_api = provider_object.get("api").and_then(Value::as_str);
    let provider_base_url = provider_object.get("baseUrl").and_then(Value::as_str);
    if provider_object.get("oauth").and_then(Value::as_str) == Some("radius")
        && provider_base_url.is_none()
    {
        return PiNativeComposition::failed(
            PiComposerReasonCode::MissingEffectiveEndpoint,
            "/baseUrl",
        );
    }
    let provider_compat = provider_object.get("compat").cloned();
    let provider_header_entries = header_entries(provider_object.get("headers"), "/headers");
    let provider_headers = provider_header_entries
        .iter()
        .map(|entry| (entry.name.clone(), entry.value.clone()))
        .collect::<BTreeMap<_, _>>();
    let provider_extra = unknown_fields(provider_object, PROVIDER_FIELDS);
    let api_key = provider_object
        .get("apiKey")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let oauth = provider_object.get("oauth").cloned();
    let auth_header = provider_object
        .get("authHeader")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let overrides = provider_object
        .get("modelOverrides")
        .and_then(Value::as_object);

    let mut models: Vec<PiComposedNativeModel> = Vec::with_capacity(definitions.len());
    for (index, definition_value) in definitions.iter().enumerate() {
        let Some(definition) = definition_value.as_object() else {
            return PiNativeComposition::failed(
                PiComposerReasonCode::CompositionFailed,
                format!("/models/{index}"),
            );
        };
        let Some(id) = definition.get("id").and_then(Value::as_str) else {
            return PiNativeComposition::failed(
                PiComposerReasonCode::CompositionFailed,
                format!("/models/{index}/id"),
            );
        };
        let existing_index = models.iter().position(|model| model.id == id);
        let defaults = existing_index
            .and_then(|position| models.get(position))
            .or_else(|| models.first());

        let api_value = definition
            .get("api")
            .and_then(Value::as_str)
            .or(provider_api)
            .or_else(|| defaults.map(|model| model.api.as_str()));
        let Some(api_value) = api_value else {
            return PiNativeComposition::failed(
                PiComposerReasonCode::MissingEffectiveApi,
                format!("/models/{index}/api"),
            );
        };
        let Some(api) = PiRawApiId::new(api_value) else {
            return PiNativeComposition::failed(
                PiComposerReasonCode::MissingEffectiveApi,
                format!("/models/{index}/api"),
            );
        };

        let base_url = definition
            .get("baseUrl")
            .and_then(Value::as_str)
            .or(provider_base_url)
            .or_else(|| defaults.map(|model| model.base_url.as_str()));
        let Some(base_url) = base_url.filter(|value| !value.is_empty()) else {
            return PiNativeComposition::failed(
                PiComposerReasonCode::MissingEffectiveEndpoint,
                format!("/models/{index}/baseUrl"),
            );
        };

        for (field, code) in [
            ("contextWindow", PiComposerReasonCode::NonPositiveModelLimit),
            ("maxTokens", PiComposerReasonCode::NonPositiveModelLimit),
        ] {
            if definition
                .get(field)
                .and_then(Value::as_f64)
                .is_some_and(|value| value <= 0.0)
            {
                return PiNativeComposition::failed(code, format!("/models/{index}/{field}"));
            }
        }

        let compat =
            match merge_pi_compat(provider_compat.clone(), definition.get("compat").cloned()) {
                Ok(compat) => compat,
                Err(_) => {
                    return PiNativeComposition::unknown(
                        PiComposerReasonCode::UnrepresentableCompat,
                        format!("/models/{index}/compat"),
                    )
                }
            };
        let model = PiComposedNativeModel {
            id: id.to_string(),
            name: definition
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(id)
                .to_string(),
            api,
            provider: provider_id.to_string(),
            base_url: base_url.to_string(),
            reasoning: definition
                .get("reasoning")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            thinking_level_map: definition.get("thinkingLevelMap").cloned(),
            input: definition
                .get("input")
                .cloned()
                .unwrap_or_else(|| json!(["text"])),
            cost: definition.get("cost").cloned().unwrap_or_else(default_cost),
            context_window: definition
                .get("contextWindow")
                .cloned()
                .unwrap_or_else(|| json!(128000)),
            max_tokens: definition
                .get("maxTokens")
                .cloned()
                .unwrap_or_else(|| json!(16384)),
            headers: BTreeMap::new(),
            provider_headers: provider_header_entries.clone(),
            model_headers: Vec::new(),
            compat,
            api_key: api_key.clone(),
            oauth: oauth.clone(),
            auth_header,
            provider_extra: provider_extra.clone(),
            model_extra: unknown_fields(definition, MODEL_FIELDS),
            override_extra: BTreeMap::new(),
        };
        if let Some(existing_index) = existing_index {
            models[existing_index] = model;
        } else {
            models.push(model);
        }
    }

    for model in &mut models {
        // Pinned Pi's rawModelHeaders uses Array.find, so duplicate model
        // definitions obtain request headers from the first definition even
        // though the later definition replaces the composed model slot.
        let (definition_index, definition) = definitions
            .iter()
            .enumerate()
            .find_map(|(index, definition)| {
                definition
                    .as_object()
                    .filter(|definition| {
                        definition.get("id").and_then(Value::as_str) == Some(model.id.as_str())
                    })
                    .map(|definition| (index, definition))
            })
            .expect("raw-valid composed model has a source definition");
        let model_override =
            overrides.and_then(|overrides| overrides.get(&model.id).and_then(Value::as_object));

        // rawModelHeaders constructs one case-sensitive JavaScript object from
        // override headers followed by the first matching model definition.
        // Exact-name replacement keeps its insertion slot; differently-cased
        // names remain distinct until ModelRuntime performs its later
        // case-insensitive HTTP merge.
        let mut model_headers = Vec::new();
        if let Some(model_override) = model_override {
            overlay_header_entries(
                &mut model_headers,
                header_entries(
                    model_override.get("headers"),
                    &format!("/modelOverrides/{}/headers", escape_json_pointer(&model.id)),
                ),
            );
        }
        overlay_header_entries(
            &mut model_headers,
            header_entries(
                definition.get("headers"),
                &format!("/models/{definition_index}/headers"),
            ),
        );

        let mut headers = provider_headers.clone();
        for entry in &model_headers {
            headers.insert(entry.name.clone(), entry.value.clone());
        }
        model.headers = headers;
        model.model_headers = model_headers;

        if let Some(model_override) = model_override {
            if let Some(name) = model_override.get("name").and_then(Value::as_str) {
                model.name = name.to_string();
            }
            if let Some(reasoning) = model_override.get("reasoning").and_then(Value::as_bool) {
                model.reasoning = reasoning;
            }
            if let Some(override_map) = model_override
                .get("thinkingLevelMap")
                .and_then(Value::as_object)
            {
                let mut merged = model
                    .thinking_level_map
                    .take()
                    .and_then(|value| value.as_object().cloned())
                    .unwrap_or_default();
                merged.extend(override_map.clone());
                model.thinking_level_map = Some(Value::Object(merged));
            }
            if let Some(input) = model_override.get("input") {
                model.input = input.clone();
            }
            if let Some(cost) = model_override.get("cost").and_then(Value::as_object) {
                model.cost = merge_cost(&model.cost, cost);
            }
            if let Some(context_window) = model_override.get("contextWindow") {
                model.context_window = context_window.clone();
            }
            if let Some(max_tokens) = model_override.get("maxTokens") {
                model.max_tokens = max_tokens.clone();
            }
            model.compat = match merge_pi_compat(
                model.compat.clone(),
                model_override.get("compat").cloned(),
            ) {
                Ok(compat) => compat,
                Err(_) => {
                    return PiNativeComposition::unknown(
                        PiComposerReasonCode::UnrepresentableCompat,
                        format!("/modelOverrides/{}/compat", escape_json_pointer(&model.id)),
                    )
                }
            };
            model.override_extra = unknown_fields(model_override, OVERRIDE_FIELDS);
        }
    }

    let model_ids = models
        .iter()
        .map(|model| model.id.as_str())
        .collect::<HashSet<_>>();
    let ignored_override_keys = overrides
        .into_iter()
        .flat_map(|overrides| overrides.keys())
        .filter(|model_id| !model_ids.contains(model_id.as_str()))
        .cloned()
        .collect();

    PiNativeComposition {
        status: PiComposerStatus::Composed,
        provider_id: Some(provider_id.to_string()),
        provider_name: Some(
            provider_object
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or(provider_id)
                .to_string(),
        ),
        provider_base_url: provider_base_url.map(ToOwned::to_owned),
        models,
        ignored_override_keys,
        reasons: Vec::new(),
    }
}

fn default_cost() -> Value {
    json!({
        "input": 0,
        "output": 0,
        "cacheRead": 0,
        "cacheWrite": 0
    })
}

fn merge_cost(base: &Value, overlay: &Map<String, Value>) -> Value {
    let base = base.as_object();
    let mut merged = Map::new();
    for key in ["input", "output", "cacheRead", "cacheWrite", "tiers"] {
        if let Some(value) = overlay
            .get(key)
            .or_else(|| base.and_then(|base| base.get(key)))
        {
            merged.insert(key.to_string(), value.clone());
        }
    }
    Value::Object(merged)
}

fn header_entries(value: Option<&Value>, base_pointer: &str) -> Vec<PiComposedHeader> {
    value
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|object| object.iter())
        .filter_map(|(name, value)| {
            value.as_str().map(|value| PiComposedHeader {
                name: name.clone(),
                value: value.to_string(),
                json_pointer: format!("{base_pointer}/{}", escape_json_pointer(name)),
            })
        })
        .collect()
}

fn overlay_header_entries(
    base: &mut Vec<PiComposedHeader>,
    overlay: impl IntoIterator<Item = PiComposedHeader>,
) {
    for entry in overlay {
        if let Some(existing) = base.iter_mut().find(|existing| existing.name == entry.name) {
            *existing = entry;
        } else {
            base.push(entry);
        }
    }
}

fn escape_json_pointer(segment: &str) -> String {
    segment.replace('~', "~0").replace('/', "~1")
}

fn unknown_fields(object: &Map<String, Value>, recognized: &[&str]) -> BTreeMap<String, Value> {
    object
        .iter()
        .filter(|(key, _)| !recognized.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pi_config::raw_schema::{evaluate_provider_value, PiRawValidity};
    use serde::Deserialize;

    const COMPOSER_ORACLE_SOURCE: &str =
        include_str!("../../../tests/fixtures/pi/native-oracle/composer-oracle-v1.json");

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ComposerOracle {
        cases: Vec<ComposerOracleCase>,
        fail_closed_cases: Vec<FailClosedCase>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ComposerOracleCase {
        id: String,
        provider_id: String,
        input: Value,
        execution: Execution,
        #[serde(default)]
        auth_execution: Option<Value>,
        #[serde(default)]
        expected: Option<Value>,
        #[serde(default)]
        expected_error: Option<String>,
    }

    #[derive(Deserialize)]
    struct Execution {
        status: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct FailClosedCase {
        id: String,
        rust_expected_status: String,
        reason_code: String,
    }

    fn model_as_oracle_value(model: &PiComposedNativeModel) -> Value {
        let mut object = Map::new();
        object.insert("id".into(), json!(model.id));
        object.insert("name".into(), json!(model.name));
        object.insert("api".into(), json!(model.api.as_str()));
        object.insert("provider".into(), json!(model.provider));
        object.insert("baseUrl".into(), json!(model.base_url));
        object.insert("reasoning".into(), json!(model.reasoning));
        if let Some(thinking) = &model.thinking_level_map {
            object.insert("thinkingLevelMap".into(), thinking.clone());
        }
        object.insert("input".into(), model.input.clone());
        object.insert("cost".into(), model.cost.clone());
        object.insert("contextWindow".into(), model.context_window.clone());
        object.insert("maxTokens".into(), model.max_tokens.clone());
        object.insert("authHeader".into(), json!(model.auth_header));
        if let Some(compat) = &model.compat {
            object.insert("compat".into(), compat.clone());
        }
        if !model.headers.is_empty() {
            object.insert("headers".into(), json!(model.headers));
        }
        Value::Object(object)
    }

    fn provider_as_oracle_value(composition: &PiNativeComposition) -> Value {
        let mut object = Map::new();
        object.insert(
            "id".into(),
            json!(composition
                .provider_id
                .as_ref()
                .expect("composed provider id")),
        );
        object.insert(
            "name".into(),
            json!(composition
                .provider_name
                .as_ref()
                .expect("composed provider name")),
        );
        if let Some(base_url) = &composition.provider_base_url {
            object.insert("baseUrl".into(), json!(base_url));
        }
        Value::Object(object)
    }

    fn json_numbers_equal(left: &Value, right: &Value) -> bool {
        match (left, right) {
            (Value::Number(left), Value::Number(right)) => left.as_f64() == right.as_f64(),
            (Value::Array(left), Value::Array(right)) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right)
                        .all(|(left, right)| json_numbers_equal(left, right))
            }
            (Value::Object(left), Value::Object(right)) => {
                left.len() == right.len()
                    && left.iter().all(|(key, left)| {
                        right
                            .get(key)
                            .is_some_and(|right| json_numbers_equal(left, right))
                    })
            }
            _ => left == right,
        }
    }

    #[test]
    fn rust_composer_matches_actual_pinned_upstream_execution() {
        let oracle: ComposerOracle =
            serde_json::from_str(COMPOSER_ORACLE_SOURCE).expect("parse composer oracle");
        for case in oracle.cases {
            let raw = evaluate_provider_value(&case.input);
            if case.execution.status == "error" {
                assert!(
                    case.expected_error.is_some(),
                    "upstream error vector '{}' records its actual error",
                    case.id
                );
                match raw.validity {
                    PiRawValidity::Invalid => {}
                    PiRawValidity::Valid => {
                        let result = compose_explicit_custom_catalog(
                            &case.provider_id,
                            raw.valid_provider.as_ref().expect("raw-valid provider"),
                        );
                        assert_eq!(
                            result.status,
                            PiComposerStatus::Failed,
                            "raw-valid upstream error case '{}'",
                            case.id
                        );
                    }
                    PiRawValidity::Unknown => {
                        panic!("oracle case '{}' unexpectedly became Unknown", case.id)
                    }
                }
                continue;
            }
            assert_eq!(raw.validity, PiRawValidity::Valid, "case '{}'", case.id);
            let result = compose_explicit_custom_catalog(
                &case.provider_id,
                raw.valid_provider.as_ref().expect("raw-valid provider"),
            );
            assert_eq!(
                result.status,
                PiComposerStatus::Composed,
                "case '{}'",
                case.id
            );
            let auth_execution = case
                .auth_execution
                .as_ref()
                .expect("successful composer case records actual auth execution");
            assert_eq!(
                auth_execution.pointer("/status").and_then(Value::as_str),
                Some("success"),
                "case '{}'",
                case.id
            );
            let actual_resolved_key = auth_execution
                .pointer("/result/auth/apiKey")
                .and_then(Value::as_str)
                .expect("successful literal composer vector resolves an API key");
            assert!(
                result
                    .models
                    .iter()
                    .all(|model| { model.api_key.as_deref() == Some(actual_resolved_key) }),
                "case '{}' preserves the same literal key that pinned Pi resolved",
                case.id
            );
            if case
                .input
                .get("authHeader")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                let expected_bearer = format!("Bearer {actual_resolved_key}");
                assert_eq!(
                    auth_execution
                        .pointer("/result/auth/headers/Authorization")
                        .and_then(Value::as_str),
                    Some(expected_bearer.as_str()),
                    "case '{}' uses pinned Pi authHeader behavior",
                    case.id
                );
            }
            let actual = json!({
                "provider": provider_as_oracle_value(&result),
                "models": result
                    .models
                    .iter()
                    .map(model_as_oracle_value)
                    .collect::<Vec<_>>(),
                "ignoredOverrideKeys": result.ignored_override_keys,
            });
            let expected = case.expected.expect("successful upstream expected output");
            assert!(
                json_numbers_equal(&actual, &expected),
                "oracle case '{}'\nactual: {actual:#}\nexpected: {expected:#}",
                case.id
            );
        }
    }

    #[test]
    fn unavailable_upstream_catalog_semantics_are_explicitly_unknown() {
        let oracle: ComposerOracle =
            serde_json::from_str(COMPOSER_ORACLE_SOURCE).expect("parse composer oracle");
        assert_eq!(oracle.fail_closed_cases.len(), 2);
        for case in oracle.fail_closed_cases {
            assert_eq!(case.rust_expected_status, "unknown", "case '{}'", case.id);
            assert_eq!(case.reason_code, "catalog_required", "case '{}'", case.id);
            let result = PiNativeComposition::catalog_required("/models");
            assert_eq!(result.status, PiComposerStatus::Unknown);
            assert_eq!(
                result.reasons[0].code,
                PiComposerReasonCode::CatalogRequired
            );
        }
    }

    #[test]
    fn credential_expressions_are_preserved_without_execution() {
        let value = json!({
            "api": "openai-responses",
            "baseUrl": "https://example.test/v1",
            "apiKey": "!read-secret",
            "oauth": "radius",
            "authHeader": true,
            "headers": {"x-tenant": "${TENANT}"},
            "models": [{"id": "m"}]
        });
        let raw = evaluate_provider_value(&value);
        let composed = compose_explicit_custom_catalog(
            "deferred",
            raw.valid_provider.as_ref().expect("raw-valid"),
        );
        assert_eq!(composed.status, PiComposerStatus::Composed);
        assert_eq!(composed.models[0].api_key.as_deref(), Some("!read-secret"));
        assert_eq!(composed.models[0].oauth, Some(json!("radius")));
        assert!(composed.models[0].auth_header);
        assert_eq!(composed.models[0].headers["x-tenant"], "${TENANT}");
    }

    #[test]
    fn pinned_cost_override_reconstructs_only_known_cost_members() {
        let value = json!({
            "api": "anthropic-messages",
            "baseUrl": "https://cost.example",
            "apiKey": "literal",
            "models": [{
                "id": "m",
                "cost": {
                    "input": 1,
                    "output": 2,
                    "cacheRead": 0.1,
                    "cacheWrite": 0.2,
                    "futureRate": 9
                }
            }],
            "modelOverrides": {
                "m": {"cost": {"output": 3}}
            }
        });
        let raw = evaluate_provider_value(&value);
        let composed = compose_explicit_custom_catalog(
            "cost-shape",
            raw.valid_provider.as_ref().expect("raw-valid"),
        );
        assert_eq!(
            composed.models[0].cost,
            json!({
                "input": 1,
                "output": 3,
                "cacheRead": 0.1,
                "cacheWrite": 0.2
            }),
            "pinned applyModelOverride drops unknown base cost keys when an override exists"
        );
    }

    #[test]
    fn compat_spread_matches_pinned_composer_request_capture() {
        let value = json!({
            "api": "openai-responses",
            "baseUrl": "https://compat.example/v1",
            "apiKey": "literal",
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
        });
        let raw = evaluate_provider_value(&value);
        let composed = compose_explicit_custom_catalog(
            "compat-spread",
            raw.valid_provider.as_ref().expect("raw-valid"),
        );

        assert_eq!(
            composed.models[0].compat,
            Some(json!({
                "openRouterRouting": {"0": "first", "1": "second"},
                "chatTemplateKwargs": {"0": "a", "1": "b", "named": true},
                "baseOnly": true,
                "supportsStore": true,
                "overlayOnly": true
            })),
            "captured by scripts/pi-transport-capture.mjs at the pinned Pi commit"
        );
    }

    #[test]
    fn compat_spread_fails_closed_when_pinned_output_requires_lone_surrogates() {
        let value = json!({
            "api": "openai-responses",
            "baseUrl": "https://compat.example/v1",
            "apiKey": "literal",
            "compat": {"chatTemplateKwargs": "😀"},
            "models": [{"id": "m"}],
            "modelOverrides": {
                "m": {"compat": {"chatTemplateKwargs": {"named": true}}}
            }
        });
        let raw = evaluate_provider_value(&value);
        let composition = compose_explicit_custom_catalog(
            "compat-surrogate",
            raw.valid_provider.as_ref().expect("raw-valid"),
        );

        assert_eq!(composition.status, PiComposerStatus::Unknown);
        assert_eq!(
            composition.reasons,
            vec![PiComposerReason {
                code: PiComposerReasonCode::UnrepresentableCompat,
                json_pointer: "/modelOverrides/m/compat".to_string(),
            }],
            "capture records UTF-16 d83d/de00 as two lone-surrogate values, which \
             serde_json::Value cannot represent"
        );
    }

    #[test]
    fn header_layers_retain_runtime_precedence_and_source_pointers() {
        let value = json!({
            "api": "anthropic-messages",
            "baseUrl": "https://headers.example",
            "apiKey": "literal",
            "headers": {"authorization": "Bearer provider"},
            "models": [{
                "id": "m",
                "headers": {"Authorization": "Bearer model"}
            }],
            "modelOverrides": {
                "m": {"headers": {"x-layer": "override"}}
            }
        });
        let raw = evaluate_provider_value(&value);
        let composed = compose_explicit_custom_catalog(
            "header-layers",
            raw.valid_provider.as_ref().expect("raw-valid"),
        );
        let model = &composed.models[0];
        assert_eq!(
            model.provider_headers[0].json_pointer,
            "/headers/authorization"
        );
        assert_eq!(
            model
                .model_headers
                .iter()
                .map(|entry| (entry.name.as_str(), entry.value.as_str()))
                .collect::<Vec<_>>(),
            vec![("x-layer", "override"), ("Authorization", "Bearer model")]
        );
    }

    #[test]
    fn unknown_provider_model_and_override_fields_are_retained_losslessly() {
        let value = json!({
            "api": "future-wire-v9",
            "baseUrl": "https://example.test/v9",
            "apiKey": "literal",
            "futureProviderShape": {
                "nested": [1, {"flag": true}]
            },
            "models": [{
                "id": "m",
                "futureModelShape": {
                    "mode": "novel",
                    "threshold": 0.125
                }
            }],
            "modelOverrides": {
                "m": {
                    "futureOverrideShape": [
                        null,
                        {"preserve": "exactly"}
                    ]
                }
            }
        });
        let raw = evaluate_provider_value(&value);
        let composed = compose_explicit_custom_catalog(
            "lossless",
            raw.valid_provider.as_ref().expect("raw-valid"),
        );
        assert_eq!(composed.status, PiComposerStatus::Composed);
        let model = &composed.models[0];
        assert_eq!(
            model.provider_extra["futureProviderShape"],
            json!({"nested": [1, {"flag": true}]})
        );
        assert_eq!(
            model.model_extra["futureModelShape"],
            json!({"mode": "novel", "threshold": 0.125})
        );
        assert_eq!(
            model.override_extra["futureOverrideShape"],
            json!([null, {"preserve": "exactly"}])
        );
    }
}

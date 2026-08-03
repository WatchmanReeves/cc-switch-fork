//! Lossless raw Pi catalog types and pinned TypeBox schema evaluation.
//!
//! This module deliberately imports neither managed control-plane types nor
//! gateway types. A raw-valid provider remains its original JSON value until
//! the independent managed assessor or composer explicitly consumes it.

#![allow(dead_code)]

use regex::Regex;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::sync::LazyLock;

const PI_REPOSITORY: &str = "https://github.com/earendil-works/pi.git";
const PI_COMMIT: &str = "ab366ebe94cacd419d986be454f12b1b9913aaca";
const TYPEBOX_VERSION: &str = "1.3.7";
const MODEL_CONFIG_PATH: &str = "packages/coding-agent/src/core/model-config.ts";
const MODEL_CONFIG_SHA256: &str =
    "62141770d675ad6357a72e07354355f0eda29281c0e5be1b48d2360f341c7360";
const PROVIDER_COMPOSER_PATH: &str = "packages/coding-agent/src/core/provider-composer.ts";
const PROVIDER_COMPOSER_SHA256: &str =
    "17308a4179b330526eabf6c917fa13e9dbd9ece90d1555b870e87d39b5b60d9d";
const RESOLVE_CONFIG_VALUE_PATH: &str = "packages/coding-agent/src/core/resolve-config-value.ts";
const RESOLVE_CONFIG_VALUE_SHA256: &str =
    "0f53dad47fe5d5d8837c022b7951ccd3bd5a9b577bd662f0986272110e83bcc7";
const SCHEMA_SHA256: &str = "e498c9f1b344eee1bd3c3ba74d1b648dcb835378cfad92800ec80078b825745c";
const RAW_ORACLE_SHA256: &str = "5aaa37160f96a0fe50867d900ca38c73f13aba769e156a883324368d9dbeeb9a";
const COMPOSER_ORACLE_SHA256: &str =
    "f7e54bb84e5fd6d50e5762dc304834410fa73ef608c2f9c42475c5983f8e0cf5";
const TRANSPORT_ORACLE_SHA256: &str =
    "b2c816e53b60da5cd6352d2c23934939e9f6dd0077971488fe9dd36fa723e855";
const FIELD_COVERAGE_SHA256: &str =
    "b8b85e611cf1dbef86c611df185ba8ac2d64160087d0c6e47747f838a0fafe42";
const HARNESS_PATH: &str = "scripts/generate-pi-native-oracle.mjs";
const HARNESS_SHA256: &str = "f7a138831284b48ef655ef500a63313f0fc89cf08d319094895027ba4777cc20";
const EVALUATOR_OPERATOR_ALLOWLIST: &[&str] = &[
    "additionalProperties",
    "anyOf",
    "const",
    "items",
    "minLength",
    "patternProperties",
    "properties",
    "required",
    "type",
];

const SCHEMA_SOURCE: &str =
    include_str!("../../../tests/fixtures/pi/native-oracle/provider-schema.snapshot.json");
const RAW_ORACLE_SOURCE: &str =
    include_str!("../../../tests/fixtures/pi/native-oracle/raw-oracle-v1.json");
const COMPOSER_ORACLE_SOURCE: &str =
    include_str!("../../../tests/fixtures/pi/native-oracle/composer-oracle-v1.json");
const TRANSPORT_ORACLE_SOURCE: &str =
    include_str!("../../../tests/fixtures/pi/native-oracle/transport-oracle-v1.json");
const FIELD_COVERAGE_SOURCE: &str =
    include_str!("../../../tests/fixtures/pi/native-oracle/field-coverage-v1.json");
const PROVENANCE_SOURCE: &str =
    include_str!("../../../tests/fixtures/pi/native-oracle/provenance-v1.json");
const GENERATOR_SOURCE: &str = include_str!("../../../scripts/generate-pi-native-oracle.mjs");

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PiRawApiId(String);

impl PiRawApiId {
    pub(crate) fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (!value.is_empty()).then_some(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct PiRawValidProvider {
    raw: Value,
}

impl PiRawValidProvider {
    fn new(raw: Value) -> Self {
        Self { raw }
    }

    pub(super) fn raw(&self) -> &Value {
        &self.raw
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PiRawValidity {
    Valid,
    Invalid,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PiRawReasonCode {
    SchemaMismatch,
    UnsupportedOperator,
    PinDrift,
    AmbiguousSchema,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PiRawReason {
    pub code: PiRawReasonCode,
    pub json_pointer: String,
}

#[derive(Debug, Clone)]
pub(super) struct PiRawSchemaEvaluation {
    pub validity: PiRawValidity,
    pub valid_provider: Option<PiRawValidProvider>,
    pub reasons: Vec<PiRawReason>,
}

#[derive(Debug)]
struct OracleBundle {
    provider_schema: Value,
}

#[derive(Debug, Clone, Copy)]
struct OracleSources<'a> {
    schema: &'a str,
    raw_oracle: &'a str,
    composer_oracle: &'a str,
    transport_oracle: &'a str,
    field_coverage: &'a str,
    provenance: &'a str,
    generator: &'a str,
}

static ORACLE_BUNDLE: LazyLock<Result<OracleBundle, String>> =
    LazyLock::new(load_and_verify_oracle_bundle);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnknownKind {
    UnsupportedOperator,
    AmbiguousSchema,
}

#[derive(Debug, PartialEq, Eq)]
enum SchemaOutcome {
    Valid,
    Invalid(String),
    Unknown {
        kind: UnknownKind,
        instance_pointer: String,
    },
}

pub(super) fn evaluate_provider_value(value: &Value) -> PiRawSchemaEvaluation {
    evaluate_provider_value_against(value, &ORACLE_BUNDLE)
}

fn evaluate_provider_value_against(
    value: &Value,
    oracle_bundle: &Result<OracleBundle, String>,
) -> PiRawSchemaEvaluation {
    let bundle = match oracle_bundle {
        Ok(bundle) => bundle,
        Err(_) => {
            return PiRawSchemaEvaluation {
                validity: PiRawValidity::Unknown,
                valid_provider: None,
                reasons: vec![PiRawReason {
                    code: PiRawReasonCode::PinDrift,
                    json_pointer: String::new(),
                }],
            };
        }
    };
    outcome_to_evaluation(evaluate_schema(&bundle.provider_schema, value, ""), value)
}

fn outcome_to_evaluation(outcome: SchemaOutcome, value: &Value) -> PiRawSchemaEvaluation {
    match outcome {
        SchemaOutcome::Valid => PiRawSchemaEvaluation {
            validity: PiRawValidity::Valid,
            valid_provider: Some(PiRawValidProvider::new(value.clone())),
            reasons: Vec::new(),
        },
        SchemaOutcome::Invalid(pointer) => PiRawSchemaEvaluation {
            validity: PiRawValidity::Invalid,
            valid_provider: None,
            reasons: vec![PiRawReason {
                code: PiRawReasonCode::SchemaMismatch,
                json_pointer: pointer,
            }],
        },
        SchemaOutcome::Unknown {
            kind,
            instance_pointer,
        } => PiRawSchemaEvaluation {
            validity: PiRawValidity::Unknown,
            valid_provider: None,
            reasons: vec![PiRawReason {
                code: match kind {
                    UnknownKind::UnsupportedOperator => PiRawReasonCode::UnsupportedOperator,
                    UnknownKind::AmbiguousSchema => PiRawReasonCode::AmbiguousSchema,
                },
                json_pointer: instance_pointer,
            }],
        },
    }
}

fn load_and_verify_oracle_bundle() -> Result<OracleBundle, String> {
    load_and_verify_oracle_bundle_from(OracleSources {
        schema: SCHEMA_SOURCE,
        raw_oracle: RAW_ORACLE_SOURCE,
        composer_oracle: COMPOSER_ORACLE_SOURCE,
        transport_oracle: TRANSPORT_ORACLE_SOURCE,
        field_coverage: FIELD_COVERAGE_SOURCE,
        provenance: PROVENANCE_SOURCE,
        generator: GENERATOR_SOURCE,
    })
}

fn load_and_verify_oracle_bundle_from(sources: OracleSources<'_>) -> Result<OracleBundle, String> {
    let provenance: Value =
        serde_json::from_str(sources.provenance).map_err(|error| error.to_string())?;
    if provenance.pointer("/version").and_then(Value::as_u64) != Some(1) {
        return Err("provenance version does not match the pinned value".to_string());
    }
    verify_string(
        "/pi/repository",
        provenance.pointer("/pi/repository"),
        PI_REPOSITORY,
    )?;
    verify_string("/pi/commit", provenance.pointer("/pi/commit"), PI_COMMIT)?;
    verify_string(
        "/typeboxVersion",
        provenance.pointer("/typeboxVersion"),
        TYPEBOX_VERSION,
    )?;
    verify_string(
        "/sources/modelConfig/path",
        provenance.pointer("/sources/modelConfig/path"),
        MODEL_CONFIG_PATH,
    )?;
    verify_string(
        "/sources/modelConfig/sha256",
        provenance.pointer("/sources/modelConfig/sha256"),
        MODEL_CONFIG_SHA256,
    )?;
    verify_string(
        "/sources/providerComposer/path",
        provenance.pointer("/sources/providerComposer/path"),
        PROVIDER_COMPOSER_PATH,
    )?;
    verify_string(
        "/sources/providerComposer/sha256",
        provenance.pointer("/sources/providerComposer/sha256"),
        PROVIDER_COMPOSER_SHA256,
    )?;
    verify_string(
        "/sources/resolveConfigValue/path",
        provenance.pointer("/sources/resolveConfigValue/path"),
        RESOLVE_CONFIG_VALUE_PATH,
    )?;
    verify_string(
        "/sources/resolveConfigValue/sha256",
        provenance.pointer("/sources/resolveConfigValue/sha256"),
        RESOLVE_CONFIG_VALUE_SHA256,
    )?;
    for (filename, source, expected_hash) in [
        (
            "provider-schema.snapshot.json",
            sources.schema,
            SCHEMA_SHA256,
        ),
        ("raw-oracle-v1.json", sources.raw_oracle, RAW_ORACLE_SHA256),
        (
            "composer-oracle-v1.json",
            sources.composer_oracle,
            COMPOSER_ORACLE_SHA256,
        ),
        (
            "transport-oracle-v1.json",
            sources.transport_oracle,
            TRANSPORT_ORACLE_SHA256,
        ),
        (
            "field-coverage-v1.json",
            sources.field_coverage,
            FIELD_COVERAGE_SHA256,
        ),
    ] {
        if sha256_hex(source.as_bytes()) != expected_hash {
            return Err(format!("{filename} does not match the code pin"));
        }
        let pointer = format!("/artifacts/{}", escape_json_pointer(filename));
        verify_string(&pointer, provenance.pointer(&pointer), expected_hash)?;
    }
    verify_string(
        "/harness/path",
        provenance.pointer("/harness/path"),
        HARNESS_PATH,
    )?;
    if sha256_hex(sources.generator.as_bytes()) != HARNESS_SHA256 {
        return Err("oracle execution harness does not match the code pin".to_string());
    }
    verify_string(
        "/harness/sha256",
        provenance.pointer("/harness/sha256"),
        HARNESS_SHA256,
    )?;
    verify_string(
        "/harness/upstreamEntry",
        provenance.pointer("/harness/upstreamEntry"),
        PROVIDER_COMPOSER_PATH,
    )?;
    let entry_functions = provenance
        .pointer("/harness/entryFunctions")
        .and_then(Value::as_array)
        .ok_or_else(|| "composer harness entry functions are missing".to_string())?;
    for expected in [
        "composeModelProvider",
        "Provider.getModels",
        "resolveCompatibilityRequestConfig",
        "Provider.auth.apiKey.resolve",
    ] {
        if !entry_functions
            .iter()
            .any(|value| value.as_str() == Some(expected))
        {
            return Err(format!(
                "composer harness does not record upstream entry function '{expected}'"
            ));
        }
    }
    let transport_functions = provenance
        .pointer("/harness/transportResolver/entryFunctions")
        .and_then(Value::as_array)
        .ok_or_else(|| "transport resolver entry functions are missing".to_string())?;
    for expected in ["resolveConfigValueOrThrow", "resolveHeadersOrThrow"] {
        if !transport_functions
            .iter()
            .any(|value| value.as_str() == Some(expected))
        {
            return Err(format!(
                "transport harness does not record upstream entry function '{expected}'"
            ));
        }
    }

    let allowlist = provenance
        .pointer("/evaluatorOperatorAllowlist")
        .and_then(Value::as_array)
        .ok_or_else(|| "provenance evaluator allowlist is missing".to_string())?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| "provenance evaluator allowlist is malformed".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if allowlist
        != EVALUATOR_OPERATOR_ALLOWLIST
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>()
    {
        return Err("provenance evaluator allowlist drifted".to_string());
    }

    let schema: Value = serde_json::from_str(sources.schema).map_err(|error| error.to_string())?;
    // The vendored snapshot is the provider schema itself. The harness records
    // the extraction target used in the pinned ModelsConfigSchema.
    let provider_schema = schema.clone();
    let mut inventory = BTreeSet::new();
    collect_schema_operators(&schema, &mut inventory)?;
    let expected_inventory = provenance
        .pointer("/schemaOperatorInventory")
        .and_then(Value::as_array)
        .ok_or_else(|| "provenance schema operator inventory is missing".to_string())?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(ToOwned::to_owned)
                .ok_or_else(|| "provenance schema operator inventory is malformed".to_string())
        })
        .collect::<Result<BTreeSet<_>, _>>()?;
    if inventory != expected_inventory {
        return Err("schema operator inventory drifted".to_string());
    }
    verify_field_coverage(
        &schema,
        sources.field_coverage,
        sources.raw_oracle,
        sources.composer_oracle,
    )?;
    Ok(OracleBundle { provider_schema })
}

fn verify_field_coverage(
    schema: &Value,
    source: &str,
    raw_oracle_source: &str,
    composer_oracle_source: &str,
) -> Result<(), String> {
    let coverage: Value = serde_json::from_str(source).map_err(|error| error.to_string())?;
    let raw_oracle: Value =
        serde_json::from_str(raw_oracle_source).map_err(|error| error.to_string())?;
    let composer_oracle: Value =
        serde_json::from_str(composer_oracle_source).map_err(|error| error.to_string())?;
    verify_string("/piCommit", coverage.pointer("/piCommit"), PI_COMMIT)?;
    let entries = coverage
        .pointer("/fields")
        .and_then(Value::as_array)
        .ok_or_else(|| "field coverage entries are missing".to_string())?;
    let mut covered = BTreeSet::new();
    for entry in entries {
        let field_path = entry
            .get("fieldPath")
            .and_then(Value::as_str)
            .ok_or_else(|| "field coverage path is malformed".to_string())?;
        if !covered.insert(field_path.to_string()) {
            return Err(format!("field coverage duplicates '{field_path}'"));
        }
        let raw_cases = entry
            .get("rawOracleCases")
            .and_then(Value::as_array)
            .filter(|cases| !cases.is_empty())
            .ok_or_else(|| {
                format!("field coverage '{field_path}' has no rawOracleCases evidence")
            })?;
        let mut has_successful_raw_execution = false;
        for case_id in raw_cases {
            let case_id = case_id.as_str().ok_or_else(|| {
                format!("field coverage '{field_path}' has a malformed raw case id")
            })?;
            let case = oracle_case(&raw_oracle, case_id).ok_or_else(|| {
                format!("field coverage '{field_path}' cites unknown raw case '{case_id}'")
            })?;
            if !input_has_field_path(&case["input"], field_path) {
                return Err(format!(
                    "raw case '{case_id}' does not contain covered field '{field_path}'"
                ));
            }
            has_successful_raw_execution |=
                case.get("expectedValid").and_then(Value::as_bool) == Some(true);
        }
        if !has_successful_raw_execution {
            return Err(format!(
                "field coverage '{field_path}' has no successful pinned TypeBox execution"
            ));
        }

        let composer_cases = entry
            .get("composerOracleCases")
            .and_then(Value::as_array)
            .filter(|cases| !cases.is_empty())
            .ok_or_else(|| {
                format!("field coverage '{field_path}' has no composerOracleCases evidence")
            })?;
        let expected_behavior_case = if field_path == "/models"
            || field_path.starts_with("/models/")
        {
            "model-fields-executed"
        } else if field_path == "/modelOverrides" || field_path.starts_with("/modelOverrides/") {
            "override-fields-executed"
        } else {
            "provider-fields-inherited"
        };
        verify_string(
            &format!("{field_path}/composerBehaviorCase"),
            entry.get("composerBehaviorCase"),
            expected_behavior_case,
        )?;
        let mut has_own_layer_execution = false;
        for case_id in composer_cases {
            let case_id = case_id.as_str().ok_or_else(|| {
                format!("field coverage '{field_path}' has a malformed composer case id")
            })?;
            let case = oracle_case(&composer_oracle, case_id).ok_or_else(|| {
                format!("field coverage '{field_path}' cites unknown composer case '{case_id}'")
            })?;
            if !input_has_field_path(&case["input"], field_path) {
                return Err(format!(
                    "composer case '{case_id}' does not contain covered field '{field_path}'"
                ));
            }
            if case_id == expected_behavior_case
                && case.pointer("/execution/status").and_then(Value::as_str) == Some("success")
                && case.get("expected").is_some_and(|value| !value.is_null())
            {
                has_own_layer_execution = true;
            }
        }
        if !has_own_layer_execution {
            return Err(format!(
                "field coverage '{field_path}' has no successful pinned Pi execution at its own layer"
            ));
        }
    }
    let mut expected = BTreeSet::new();
    collect_schema_field_paths(schema, "", &mut expected)?;
    if covered != expected {
        let missing = expected.difference(&covered).cloned().collect::<Vec<_>>();
        let stale = covered.difference(&expected).cloned().collect::<Vec<_>>();
        return Err(format!(
            "field coverage differs from pinned schema; missing={missing:?}, stale={stale:?}"
        ));
    }
    Ok(())
}

fn oracle_case<'a>(oracle: &'a Value, id: &str) -> Option<&'a Value> {
    oracle
        .get("cases")
        .and_then(Value::as_array)?
        .iter()
        .find(|case| case.get("id").and_then(Value::as_str) == Some(id))
}

fn input_has_field_path(value: &Value, field_path: &str) -> bool {
    fn descend(value: &Value, segments: &[&str]) -> bool {
        let Some((head, tail)) = segments.split_first() else {
            return true;
        };
        if *head == "*" {
            return match value {
                Value::Array(values) => values.iter().any(|value| descend(value, tail)),
                Value::Object(values) => values.values().any(|value| descend(value, tail)),
                _ => false,
            };
        }
        let decoded = head.replace("~1", "/").replace("~0", "~");
        value
            .get(decoded.as_str())
            .is_some_and(|value| descend(value, tail))
    }

    let segments = field_path
        .strip_prefix('/')
        .unwrap_or(field_path)
        .split('/')
        .collect::<Vec<_>>();
    descend(value, &segments)
}

fn collect_schema_field_paths(
    schema: &Value,
    pointer: &str,
    output: &mut BTreeSet<String>,
) -> Result<(), String> {
    let object = schema
        .as_object()
        .ok_or_else(|| format!("schema field node at '{pointer}' is not an object"))?;
    if let Some(branches) = object.get("anyOf") {
        for branch in branches
            .as_array()
            .ok_or_else(|| format!("schema anyOf at '{pointer}' is not an array"))?
        {
            collect_schema_field_paths(branch, pointer, output)?;
        }
    }
    if let Some(properties) = object.get("properties") {
        for (name, child) in properties
            .as_object()
            .ok_or_else(|| format!("schema properties at '{pointer}' are malformed"))?
        {
            let child_pointer = join_json_pointer(pointer, name);
            output.insert(child_pointer.clone());
            collect_schema_field_paths(child, &child_pointer, output)?;
        }
    }
    if let Some(patterns) = object.get("patternProperties") {
        for child in patterns
            .as_object()
            .ok_or_else(|| format!("schema patterns at '{pointer}' are malformed"))?
            .values()
        {
            let child_pointer = join_json_pointer(pointer, "*");
            output.insert(child_pointer.clone());
            collect_schema_field_paths(child, &child_pointer, output)?;
        }
    }
    if let Some(items) = object.get("items") {
        collect_schema_field_paths(items, &join_json_pointer(pointer, "*"), output)?;
    }
    Ok(())
}

fn verify_string(label: &str, actual: Option<&Value>, expected: &str) -> Result<(), String> {
    if actual.and_then(Value::as_str) == Some(expected) {
        Ok(())
    } else {
        Err(format!("{label} does not match the pinned value"))
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn collect_schema_operators(
    schema: &Value,
    inventory: &mut BTreeSet<String>,
) -> Result<(), String> {
    let object = schema
        .as_object()
        .ok_or_else(|| "schema node is not an object".to_string())?;
    for key in object.keys() {
        inventory.insert(key.clone());
    }
    for map_name in ["properties", "patternProperties"] {
        if let Some(children) = object.get(map_name) {
            let children = children
                .as_object()
                .ok_or_else(|| format!("{map_name} is not an object"))?;
            for child in children.values() {
                collect_schema_operators(child, inventory)?;
            }
        }
    }
    if let Some(branches) = object.get("anyOf") {
        for branch in branches
            .as_array()
            .ok_or_else(|| "anyOf is not an array".to_string())?
        {
            collect_schema_operators(branch, inventory)?;
        }
    }
    if let Some(items) = object.get("items") {
        collect_schema_operators(items, inventory)?;
    }
    if let Some(Value::Object(_)) = object.get("additionalProperties") {
        collect_schema_operators(
            object
                .get("additionalProperties")
                .expect("checked additionalProperties"),
            inventory,
        )?;
    }
    Ok(())
}

fn evaluate_schema(schema: &Value, instance: &Value, pointer: &str) -> SchemaOutcome {
    let Some(schema) = schema.as_object() else {
        return ambiguous(pointer);
    };
    if schema
        .keys()
        .any(|key| !EVALUATOR_OPERATOR_ALLOWLIST.contains(&key.as_str()))
    {
        return unsupported(pointer);
    }

    if let Some(expected_type) = schema.get("type") {
        let Some(expected_type) = expected_type.as_str() else {
            return ambiguous(pointer);
        };
        let matches = match expected_type {
            "object" => instance.is_object(),
            "array" => instance.is_array(),
            "string" => instance.is_string(),
            // TypeBox Number follows JavaScript Number semantics and rejects
            // NaN/Infinity. With serde_json arbitrary precision, an overflow
            // token remains a Number but has no finite f64 representation.
            "number" => instance
                .as_number()
                .and_then(serde_json::Number::as_f64)
                .is_some(),
            "boolean" => instance.is_boolean(),
            "null" => instance.is_null(),
            _ => return unsupported(pointer),
        };
        if !matches {
            return SchemaOutcome::Invalid(pointer.to_string());
        }
    }

    if let Some(constant) = schema.get("const") {
        if !json_value_equal(instance, constant) {
            return SchemaOutcome::Invalid(pointer.to_string());
        }
    }

    if let Some(min_length) = schema.get("minLength") {
        let Some(min_length) = min_length.as_u64() else {
            return ambiguous(pointer);
        };
        let Some(value) = instance.as_str() else {
            return SchemaOutcome::Invalid(pointer.to_string());
        };
        if value.encode_utf16().count() < min_length as usize {
            return SchemaOutcome::Invalid(pointer.to_string());
        }
    }

    if let Some(branches) = schema.get("anyOf") {
        let Some(branches) = branches.as_array() else {
            return ambiguous(pointer);
        };
        if branches.is_empty() {
            return SchemaOutcome::Invalid(pointer.to_string());
        }
        let mut first_unknown = None;
        for branch in branches {
            match evaluate_schema(branch, instance, pointer) {
                SchemaOutcome::Valid => return SchemaOutcome::Valid,
                SchemaOutcome::Invalid(_) => {}
                unknown @ SchemaOutcome::Unknown { .. } => {
                    first_unknown.get_or_insert(unknown);
                }
            }
        }
        return first_unknown.unwrap_or_else(|| SchemaOutcome::Invalid(pointer.to_string()));
    }

    if let Some(items) = schema.get("items") {
        let Some(values) = instance.as_array() else {
            return SchemaOutcome::Invalid(pointer.to_string());
        };
        for (index, value) in values.iter().enumerate() {
            let child_pointer = join_json_pointer(pointer, &index.to_string());
            match evaluate_schema(items, value, &child_pointer) {
                SchemaOutcome::Valid => {}
                outcome => return outcome,
            }
        }
    }

    if schema.contains_key("required")
        || schema.contains_key("properties")
        || schema.contains_key("patternProperties")
        || schema.contains_key("additionalProperties")
    {
        let Some(object) = instance.as_object() else {
            return SchemaOutcome::Invalid(pointer.to_string());
        };
        match evaluate_object(schema, object, pointer) {
            SchemaOutcome::Valid => {}
            outcome => return outcome,
        }
    }

    SchemaOutcome::Valid
}

fn evaluate_object(
    schema: &Map<String, Value>,
    instance: &Map<String, Value>,
    pointer: &str,
) -> SchemaOutcome {
    if let Some(required) = schema.get("required") {
        let Some(required) = required.as_array() else {
            return ambiguous(pointer);
        };
        for name in required {
            let Some(name) = name.as_str() else {
                return ambiguous(pointer);
            };
            if !instance.contains_key(name) {
                return SchemaOutcome::Invalid(join_json_pointer(pointer, name));
            }
        }
    }

    let properties = match schema.get("properties") {
        None => None,
        Some(Value::Object(properties)) => Some(properties),
        Some(_) => return ambiguous(pointer),
    };
    let pattern_properties = match schema.get("patternProperties") {
        None => Vec::new(),
        Some(Value::Object(patterns)) => {
            let mut compiled = Vec::with_capacity(patterns.len());
            for (pattern, child_schema) in patterns {
                let Ok(regex) = Regex::new(pattern) else {
                    return ambiguous(pointer);
                };
                compiled.push((regex, child_schema));
            }
            compiled
        }
        Some(_) => return ambiguous(pointer),
    };

    for (name, value) in instance {
        let child_pointer = join_json_pointer(pointer, name);
        let mut covered = false;
        if let Some(child_schema) = properties.and_then(|properties| properties.get(name)) {
            covered = true;
            match evaluate_schema(child_schema, value, &child_pointer) {
                SchemaOutcome::Valid => {}
                outcome => return outcome,
            }
        }
        for (pattern, child_schema) in &pattern_properties {
            if pattern.is_match(name) {
                covered = true;
                match evaluate_schema(child_schema, value, &child_pointer) {
                    SchemaOutcome::Valid => {}
                    outcome => return outcome,
                }
            }
        }
        if !covered {
            match schema.get("additionalProperties") {
                None | Some(Value::Bool(true)) => {}
                Some(Value::Bool(false)) => {
                    return SchemaOutcome::Invalid(child_pointer);
                }
                Some(child_schema @ Value::Object(_)) => {
                    match evaluate_schema(child_schema, value, &child_pointer) {
                        SchemaOutcome::Valid => {}
                        outcome => return outcome,
                    }
                }
                Some(_) => return ambiguous(pointer),
            }
        }
    }
    SchemaOutcome::Valid
}

fn json_value_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(left), Value::Number(right)) => left.as_f64() == right.as_f64(),
        _ => left == right,
    }
}

fn join_json_pointer(parent: &str, token: &str) -> String {
    format!("{parent}/{}", escape_json_pointer(token))
}

fn escape_json_pointer(value: &str) -> String {
    value.replace('~', "~0").replace('/', "~1")
}

fn unsupported(pointer: &str) -> SchemaOutcome {
    SchemaOutcome::Unknown {
        kind: UnknownKind::UnsupportedOperator,
        instance_pointer: pointer.to_string(),
    }
}

fn ambiguous(pointer: &str) -> SchemaOutcome {
    SchemaOutcome::Unknown {
        kind: UnknownKind::AmbiguousSchema,
        instance_pointer: pointer.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RawOracle {
        cases: Vec<RawOracleCase>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct RawOracleCase {
        id: String,
        input: Value,
        expected_valid: bool,
    }

    #[test]
    fn rust_evaluator_matches_typebox_1_3_7_oracle() {
        assert!(
            ORACLE_BUNDLE.is_ok(),
            "vendored oracle bundle must verify: {:?}",
            ORACLE_BUNDLE.as_ref().err()
        );
        let oracle: RawOracle = serde_json::from_str(RAW_ORACLE_SOURCE).expect("parse raw oracle");
        for case in oracle.cases {
            let actual = evaluate_provider_value(&case.input).validity;
            let expected = if case.expected_valid {
                PiRawValidity::Valid
            } else {
                PiRawValidity::Invalid
            };
            assert_eq!(actual, expected, "oracle case '{}'", case.id);
            let evaluated = evaluate_provider_value(&case.input);
            assert_eq!(
                evaluated
                    .valid_provider
                    .as_ref()
                    .map(PiRawValidProvider::raw),
                case.expected_valid.then_some(&case.input),
                "raw-valid type barrier case '{}'",
                case.id
            );
        }
    }

    #[test]
    fn every_schema_field_is_bound_to_real_successful_pi_executions() {
        let schema: Value = serde_json::from_str(SCHEMA_SOURCE).expect("parse schema");
        verify_field_coverage(
            &schema,
            FIELD_COVERAGE_SOURCE,
            RAW_ORACLE_SOURCE,
            COMPOSER_ORACLE_SOURCE,
        )
        .expect("all fields cite actual pinned Pi executions");

        let invented_raw_case =
            FIELD_COVERAGE_SOURCE.replacen("all-schema-fields-valid", "invented-raw-case", 1);
        assert!(
            verify_field_coverage(
                &schema,
                &invented_raw_case,
                RAW_ORACLE_SOURCE,
                COMPOSER_ORACLE_SOURCE,
            )
            .is_err(),
            "a field cannot be certified by a made-up execution id"
        );

        let invented_composer_case = FIELD_COVERAGE_SOURCE.replacen(
            "combined-all-fields-precedence",
            "invented-composer-case",
            1,
        );
        assert!(
            verify_field_coverage(
                &schema,
                &invented_composer_case,
                RAW_ORACLE_SOURCE,
                COMPOSER_ORACLE_SOURCE,
            )
            .is_err(),
            "a field cannot cite a composer execution that did not occur"
        );
    }

    #[test]
    fn unsupported_operator_is_unknown_only_when_input_traverses_it() {
        let schema = json!({
            "type": "object",
            "properties": {
                "optional": {
                    "type": "string",
                    "unevaluatedProperties": false
                }
            }
        });
        assert_eq!(
            evaluate_schema(&schema, &json!({}), ""),
            SchemaOutcome::Valid
        );
        assert!(matches!(
            evaluate_schema(&schema, &json!({"optional": "value"}), ""),
            SchemaOutcome::Unknown {
                kind: UnknownKind::UnsupportedOperator,
                instance_pointer
            } if instance_pointer == "/optional"
        ));
    }

    #[test]
    fn additional_properties_operator_is_explicitly_supported() {
        let schema = json!({
            "type": "object",
            "properties": {"known": {"type": "string"}},
            "additionalProperties": false
        });
        assert_eq!(
            evaluate_schema(&schema, &json!({"known": "yes"}), ""),
            SchemaOutcome::Valid
        );
        assert_eq!(
            evaluate_schema(&schema, &json!({"unknown": true}), ""),
            SchemaOutcome::Invalid("/unknown".into())
        );
    }

    #[test]
    fn artifact_or_provenance_drift_fails_closed_as_raw_unknown() {
        let tampered_schema = SCHEMA_SOURCE.replacen("\"baseUrl\"", "\"baseUrl-tampered\"", 1);
        let schema_bundle = load_and_verify_oracle_bundle_from(OracleSources {
            schema: &tampered_schema,
            raw_oracle: RAW_ORACLE_SOURCE,
            composer_oracle: COMPOSER_ORACLE_SOURCE,
            transport_oracle: TRANSPORT_ORACLE_SOURCE,
            field_coverage: FIELD_COVERAGE_SOURCE,
            provenance: PROVENANCE_SOURCE,
            generator: GENERATOR_SOURCE,
        });
        assert!(schema_bundle.is_err());
        let schema_result = evaluate_provider_value_against(&json!({}), &schema_bundle);
        assert_eq!(schema_result.validity, PiRawValidity::Unknown);
        assert_eq!(
            schema_result.reasons,
            vec![PiRawReason {
                code: PiRawReasonCode::PinDrift,
                json_pointer: String::new(),
            }]
        );

        let tampered_provenance =
            PROVENANCE_SOURCE.replacen(PI_COMMIT, "0000000000000000000000000000000000000000", 1);
        let provenance_bundle = load_and_verify_oracle_bundle_from(OracleSources {
            schema: SCHEMA_SOURCE,
            raw_oracle: RAW_ORACLE_SOURCE,
            composer_oracle: COMPOSER_ORACLE_SOURCE,
            transport_oracle: TRANSPORT_ORACLE_SOURCE,
            field_coverage: FIELD_COVERAGE_SOURCE,
            provenance: &tampered_provenance,
            generator: GENERATOR_SOURCE,
        });
        assert!(provenance_bundle.is_err());
        assert_eq!(
            evaluate_provider_value_against(&json!({}), &provenance_bundle).validity,
            PiRawValidity::Unknown
        );

        let tampered_coverage = FIELD_COVERAGE_SOURCE.replacen("\"/api\"", "\"/api-tampered\"", 1);
        let coverage_bundle = load_and_verify_oracle_bundle_from(OracleSources {
            schema: SCHEMA_SOURCE,
            raw_oracle: RAW_ORACLE_SOURCE,
            composer_oracle: COMPOSER_ORACLE_SOURCE,
            transport_oracle: TRANSPORT_ORACLE_SOURCE,
            field_coverage: &tampered_coverage,
            provenance: PROVENANCE_SOURCE,
            generator: GENERATOR_SOURCE,
        });
        assert!(coverage_bundle.is_err());
        assert_eq!(
            evaluate_provider_value_against(&json!({}), &coverage_bundle).validity,
            PiRawValidity::Unknown
        );
    }
}

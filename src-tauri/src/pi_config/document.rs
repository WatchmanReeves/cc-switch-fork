//! Read-only access to Pi's shared `models.json` document.
//!
//! The semantic parse mirrors Pi's pinned `stripJsonComments()` behavior.
//! A CST parse is additionally required so callers can fingerprint one exact
//! provider value without making unrelated entries part of the revision.

use crate::error::AppError;
use indexmap::IndexMap;
use jsonc_parser::cst::{
    CstArray, CstContainerNode, CstInputValue, CstLeafNode, CstNode, CstObject, CstObjectProp,
    CstRootNode,
};
use jsonc_parser::ParseOptions;
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};

use super::shared_file::{
    compare_exchange_shared_file_bytes, delete_shared_file, read_shared_file,
    sync_shared_file_parent,
};

const MAX_PI_MODELS_BYTES: u64 = 8 * 1024 * 1024;
const EMPTY_MODELS_DOCUMENT: &str = "{\"providers\":{}}";
const MAX_MUTATION_ATTEMPTS: usize = 3;

static PI_JSON_LINE_COMMENTS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""(?:\\.|[^"\\])*"|//[^\n]*"#).expect("Pi JSON line-comment regex must compile")
});
static PI_JSON_TRAILING_COMMAS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""(?:\\.|[^"\\])*"|,(\s*[}\]])"#)
        .expect("Pi JSON trailing-comma regex must compile")
});
static PATH_LOCKS: LazyLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
#[cfg(test)]
static BEFORE_PROVIDER_VERIFY: LazyLock<Mutex<HashMap<PathBuf, Vec<u8>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn path_lock(path: &Path) -> Result<Arc<Mutex<()>>, AppError> {
    let mut locks = PATH_LOCKS
        .lock()
        .map_err(|error| AppError::Config(format!("Pi path-lock registry is poisoned: {error}")))?;
    Ok(locks
        .entry(path.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone())
}

fn lock_path(lock: &Mutex<()>) -> Result<MutexGuard<'_, ()>, AppError> {
    lock.lock()
        .map_err(|error| AppError::Config(format!("Pi config path lock is poisoned: {error}")))
}

#[derive(Debug, Clone)]
pub(super) struct PiRawProviderEntry {
    pub value: Value,
    pub raw_source: String,
}

#[derive(Debug, Clone)]
pub(super) struct PiModelsDocument {
    providers: IndexMap<String, PiRawProviderEntry>,
}

impl PiModelsDocument {
    pub fn providers(&self) -> &IndexMap<String, PiRawProviderEntry> {
        &self.providers
    }
}

fn pi_models_parse_options() -> ParseOptions {
    // Pinned Pi accepts standard double-quoted JSON with `//` comments and
    // trailing commas. jsonc-parser is broader by default, so all other
    // extensions stay disabled.
    ParseOptions {
        allow_comments: true,
        allow_loose_object_property_names: false,
        allow_trailing_commas: true,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    }
}

fn jsonc_error(path: &Path, message: impl std::fmt::Display) -> AppError {
    AppError::Config(format!(
        "JSON parse error in Pi models file {}: {message}",
        path.display()
    ))
}

/// Mirrors Pi commit `ab366ebe94cacd419d986be454f12b1b9913aaca`
/// (`packages/coding-agent/src/utils/json.ts`).
fn strip_pi_json_comments(input: &str) -> String {
    let without_comments =
        PI_JSON_LINE_COMMENTS.replace_all(input, |captures: &regex::Captures<'_>| {
            let matched = captures
                .get(0)
                .expect("the full regex match is always present")
                .as_str();
            if matched.starts_with('"') {
                matched.to_string()
            } else {
                String::new()
            }
        });
    PI_JSON_TRAILING_COMMAS
        .replace_all(&without_comments, |captures: &regex::Captures<'_>| {
            captures
                .get(1)
                .or_else(|| captures.get(0))
                .expect("the full regex match is always present")
                .as_str()
                .to_string()
        })
        .into_owned()
}

fn cst_property_name(property: &CstObjectProp) -> Option<String> {
    property.name()?.decoded_value().ok()
}

fn last_cst_property(object: &CstObject, name: &str) -> Option<CstObjectProp> {
    object
        .properties()
        .into_iter()
        .rev()
        .find(|property| cst_property_name(property).as_deref() == Some(name))
}

fn cst_object(node: CstNode, path: &Path, label: &str) -> Result<CstObject, AppError> {
    match node {
        CstNode::Container(CstContainerNode::Object(object)) => Ok(object),
        _ => Err(jsonc_error(path, format!("{label} must be an object"))),
    }
}

fn cst_input(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(value) => CstInputValue::Bool(*value),
        Value::Number(value) => CstInputValue::Number(value.to_string()),
        Value::String(value) => CstInputValue::String(value.clone()),
        Value::Array(values) => CstInputValue::Array(values.iter().map(cst_input).collect()),
        Value::Object(values) => CstInputValue::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), cst_input(value)))
                .collect(),
        ),
    }
}

fn replace_cst_node(node: CstNode, replacement: &Value) -> Result<(), AppError> {
    let replacement = cst_input(replacement);
    let replaced = match node {
        CstNode::Container(CstContainerNode::Array(node)) => node.replace_with(replacement),
        CstNode::Container(CstContainerNode::Object(node)) => node.replace_with(replacement),
        CstNode::Leaf(CstLeafNode::BooleanLit(node)) => node.replace_with(replacement),
        CstNode::Leaf(CstLeafNode::NullKeyword(node)) => node.replace_with(replacement),
        CstNode::Leaf(CstLeafNode::NumberLit(node)) => node.replace_with(replacement),
        CstNode::Leaf(CstLeafNode::StringLit(node)) => node.replace_with(replacement),
        CstNode::Leaf(CstLeafNode::WordLit(node)) => node.replace_with(replacement),
        CstNode::Container(CstContainerNode::Root(_))
        | CstNode::Container(CstContainerNode::ObjectProp(_))
        | CstNode::Leaf(CstLeafNode::Token(_))
        | CstNode::Leaf(CstLeafNode::Whitespace(_))
        | CstNode::Leaf(CstLeafNode::Newline(_))
        | CstNode::Leaf(CstLeafNode::Comment(_)) => None,
    };
    replaced.map(|_| ()).ok_or_else(|| {
        AppError::Config("Pi models.json CST became disconnected during update".to_string())
    })
}

fn patch_cst_object(
    object: &CstObject,
    before: &serde_json::Map<String, Value>,
    after: &serde_json::Map<String, Value>,
) -> Result<(), AppError> {
    for key in before.keys().filter(|key| !after.contains_key(*key)) {
        let matching = object
            .properties()
            .into_iter()
            .filter(|property| cst_property_name(property).as_deref() == Some(key.as_str()))
            .collect::<Vec<_>>();
        for property in matching.into_iter().rev() {
            property.remove();
        }
    }

    for (key, after_value) in after {
        if let Some(before_value) = before.get(key) {
            let property = last_cst_property(object, key).ok_or_else(|| {
                AppError::Config(format!(
                    "Pi models.json CST is missing existing property '{key}'"
                ))
            })?;
            let value = property.value().ok_or_else(|| {
                AppError::Config(format!("Pi models.json CST property '{key}' has no value"))
            })?;
            patch_cst_node(value, before_value, after_value)?;
        } else {
            object.append(key, cst_input(after_value));
        }
    }
    Ok(())
}

fn patch_cst_array(array: &CstArray, before: &[Value], after: &[Value]) -> Result<(), AppError> {
    let elements = array.elements();
    if elements.len() != before.len() {
        return Err(AppError::Config(
            "Pi models.json CST array does not match its parsed value".to_string(),
        ));
    }

    for (index, (before_value, after_value)) in before.iter().zip(after).enumerate() {
        patch_cst_node(elements[index].clone(), before_value, after_value)?;
    }
    for element in elements.into_iter().skip(after.len()).rev() {
        element.remove();
    }
    for value in after.iter().skip(before.len()) {
        array.append(cst_input(value));
    }
    Ok(())
}

fn patch_cst_node(node: CstNode, before: &Value, after: &Value) -> Result<(), AppError> {
    if before == after {
        return Ok(());
    }
    match (&node, before, after) {
        (
            CstNode::Container(CstContainerNode::Object(object)),
            Value::Object(before),
            Value::Object(after),
        ) => patch_cst_object(object, before, after),
        (
            CstNode::Container(CstContainerNode::Array(array)),
            Value::Array(before),
            Value::Array(after),
        ) => patch_cst_array(array, before, after),
        _ => replace_cst_node(node, after),
    }
}

fn parse_models_source(path: &Path, source: &str) -> Result<PiModelsDocument, AppError> {
    let document: Value = serde_json::from_str(&strip_pi_json_comments(source))
        .map_err(|error| AppError::json(path, error))?;
    let root = CstRootNode::parse(source, &pi_models_parse_options())
        .map_err(|error| jsonc_error(path, error))?;
    if root.to_serde_value().as_ref() != Some(&document) {
        return Err(jsonc_error(path, "CST does not match Pi's parsed document"));
    }

    let semantic_providers = document
        .as_object()
        .and_then(|root| root.get("providers"))
        .and_then(Value::as_object)
        .ok_or_else(|| jsonc_error(path, "root must contain a providers object"))?;

    let root_object = cst_object(
        root.value()
            .ok_or_else(|| jsonc_error(path, "document must contain a JSON value"))?,
        path,
        "root",
    )?;
    let providers_node = last_cst_property(&root_object, "providers")
        .and_then(|property| property.value())
        .ok_or_else(|| jsonc_error(path, "CST is missing the providers value"))?;
    let providers_object = cst_object(providers_node, path, "providers")?;

    let mut providers = IndexMap::with_capacity(semantic_providers.len());
    for (provider_key, value) in semantic_providers {
        let raw_source = last_cst_property(&providers_object, provider_key)
            .and_then(|property| property.value())
            .ok_or_else(|| {
                jsonc_error(
                    path,
                    format!("CST is missing provider entry '{provider_key}'"),
                )
            })?
            .to_string();
        providers.insert(
            provider_key.clone(),
            PiRawProviderEntry {
                value: value.clone(),
                raw_source,
            },
        );
    }
    Ok(PiModelsDocument { providers })
}

fn read_models_bytes(path: &Path) -> Result<Option<Vec<u8>>, AppError> {
    Ok(read_shared_file(path, MAX_PI_MODELS_BYTES, "Pi models file")?.bytes)
}

fn parse_pi_models_document(
    path: &Path,
    bytes: Option<&[u8]>,
) -> Result<PiModelsDocument, AppError> {
    let bytes = bytes.unwrap_or_else(|| EMPTY_MODELS_DOCUMENT.as_bytes());
    let source = std::str::from_utf8(bytes)
        .map_err(|error| jsonc_error(path, format!("file is not UTF-8: {error}")))?;
    parse_models_source(path, source)
}

pub(super) fn read_pi_models_document(path: &Path) -> Result<PiModelsDocument, AppError> {
    let bytes = read_models_bytes(path)?;
    parse_pi_models_document(path, bytes.as_deref())
}

pub(super) fn pi_raw_provider_fingerprint(raw_source: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(raw_source.as_bytes()))
}

fn serialize_models_mutation(
    path: &Path,
    before: Option<&[u8]>,
    mutator: &impl Fn(&mut Value) -> Result<(), AppError>,
) -> Result<Vec<u8>, AppError> {
    if let Some(bytes) = before {
        let source = std::str::from_utf8(bytes)
            .map_err(|error| jsonc_error(path, format!("file is not UTF-8: {error}")))?;
        let mut document: Value = serde_json::from_str(&strip_pi_json_comments(source))
            .map_err(|error| AppError::json(path, error))?;
        let root = CstRootNode::parse(source, &pi_models_parse_options())
            .map_err(|error| jsonc_error(path, error))?;
        if root.to_serde_value().as_ref() != Some(&document) {
            return Err(jsonc_error(path, "CST does not match Pi's parsed document"));
        }
        let original = document.clone();
        mutator(&mut document)?;
        let root_value = root
            .value()
            .ok_or_else(|| jsonc_error(path, "document must contain a JSON value"))?;
        patch_cst_node(root_value, &original, &document)?;
        if root.to_serde_value().as_ref() != Some(&document) {
            return Err(AppError::Config(
                "Pi models.json CST update did not produce the requested document".to_string(),
            ));
        }
        return Ok(root.to_string().into_bytes());
    }

    let mut document: Value = serde_json::from_str(EMPTY_MODELS_DOCUMENT)
        .expect("empty Pi models document is valid JSON");
    mutator(&mut document)?;
    let mut serialized = serde_json::to_vec_pretty(&document)
        .map_err(|source| AppError::JsonSerialize { source })?;
    serialized.push(b'\n');
    Ok(serialized)
}

/// Patch only the explicitly named provider keys in Pi's shared models.json.
///
/// Unknown root fields, unowned provider entries, comments, and formatting are
/// preserved by the CST patch. An optimistic fingerprint check prevents a
/// Pi/user write observed before replacement from being silently overwritten.
#[cfg(test)]
pub(crate) fn apply_pi_provider_patch(
    path: &Path,
    patch: &IndexMap<String, Option<Value>>,
) -> Result<(), AppError> {
    apply_pi_provider_patch_checked(path, None, None, patch).map(|_| ())
}

fn apply_pi_provider_patch_checked(
    path: &Path,
    expected: Option<&IndexMap<String, Option<Value>>>,
    expected_fingerprints: Option<&IndexMap<String, String>>,
    patch: &IndexMap<String, Option<Value>>,
) -> Result<PiDocumentCommit, AppError> {
    let lock = path_lock(path)?;
    let _guard = lock_path(&lock)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| AppError::io(parent, error))?;
    }

    for _ in 0..MAX_MUTATION_ATTEMPTS {
        let before = read_models_bytes(path)?;
        if let Some(expected) = expected {
            ensure_provider_values_match(path, before.as_deref(), expected)?;
        }
        if let Some(expected_fingerprints) = expected_fingerprints {
            ensure_provider_fingerprints_match(path, before.as_deref(), expected_fingerprints)?;
        }
        if before.is_none() && patch.values().all(Option::is_none) {
            return Ok(PiDocumentCommit { bytes: None });
        }
        let serialized = serialize_models_mutation(path, before.as_deref(), &|document| {
            let providers = document
                .as_object_mut()
                .and_then(|root| root.get_mut("providers"))
                .and_then(Value::as_object_mut)
                .ok_or_else(|| jsonc_error(path, "root must contain a providers object"))?;
            for (provider_key, replacement) in patch {
                match replacement {
                    Some(value) => {
                        providers.insert(provider_key.clone(), value.clone());
                    }
                    None => {
                        providers.remove(provider_key);
                    }
                }
            }
            Ok(())
        })?;
        if before.as_deref() == Some(serialized.as_slice()) {
            return Ok(PiDocumentCommit { bytes: before });
        }

        match compare_exchange_shared_file_bytes(
            path,
            before.as_deref(),
            &serialized,
            MAX_PI_MODELS_BYTES,
            None,
            "Pi models file",
        ) {
            Ok(_) => {
                return Ok(PiDocumentCommit {
                    bytes: Some(serialized),
                })
            }
            Err(AppError::Conflict(_)) => continue,
            Err(error) => return Err(error),
        }
    }

    Err(AppError::Conflict(format!(
        "Pi models file changed concurrently too many times: {}",
        path.display()
    )))
}

struct PiDocumentCommit {
    bytes: Option<Vec<u8>>,
}

fn provider_values_from_bytes<'a>(
    path: &Path,
    bytes: Option<&[u8]>,
    provider_keys: impl IntoIterator<Item = &'a String>,
) -> Result<IndexMap<String, Option<Value>>, AppError> {
    let document = parse_pi_models_document(path, bytes)?;
    Ok(provider_keys
        .into_iter()
        .map(|key| {
            let value = document
                .providers()
                .get(key)
                .map(|entry| entry.value.clone());
            (key.clone(), value)
        })
        .collect())
}

fn ensure_provider_values_match(
    path: &Path,
    bytes: Option<&[u8]>,
    expected: &IndexMap<String, Option<Value>>,
) -> Result<(), AppError> {
    let observed = provider_values_from_bytes(path, bytes, expected.keys())?;
    if let Some((provider_key, expected_value)) = expected
        .iter()
        .find(|(provider_key, expected_value)| observed.get(*provider_key) != Some(*expected_value))
    {
        return Err(AppError::Conflict(format!(
            "Pi provider key '{provider_key}' changed since directory/catalog preflight \
             (expected {}, observed {})",
            provider_value_label(expected_value),
            provider_value_label(
                observed
                    .get(provider_key)
                    .expect("every requested provider key is observed")
            )
        )));
    }
    Ok(())
}

fn ensure_provider_fingerprints_match(
    path: &Path,
    bytes: Option<&[u8]>,
    expected: &IndexMap<String, String>,
) -> Result<(), AppError> {
    let document = parse_pi_models_document(path, bytes)?;
    for (provider_key, expected_fingerprint) in expected {
        let observed = document
            .providers()
            .get(provider_key)
            .map(|entry| pi_raw_provider_fingerprint(&entry.raw_source));
        if observed.as_deref() != Some(expected_fingerprint) {
            return Err(AppError::Conflict(format!(
                "Pi native provider '{provider_key}' changed since inspection \
                 (expected raw fingerprint {expected_fingerprint}, observed {})",
                observed.as_deref().unwrap_or("missing")
            )));
        }
    }
    Ok(())
}

fn provider_fingerprints_from_bytes<'a>(
    path: &Path,
    bytes: Option<&[u8]>,
    provider_keys: impl IntoIterator<Item = &'a String>,
) -> Result<IndexMap<String, String>, AppError> {
    let document = parse_pi_models_document(path, bytes)?;
    Ok(provider_keys
        .into_iter()
        .filter_map(|provider_key| {
            document.providers().get(provider_key).map(|entry| {
                (
                    provider_key.clone(),
                    pi_raw_provider_fingerprint(&entry.raw_source),
                )
            })
        })
        .collect())
}

fn provider_value_label(value: &Option<Value>) -> &'static str {
    if value.is_some() {
        "present"
    } else {
        "absent"
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PiProviderValuesSnapshot {
    pub file_existed: bool,
    pub values: IndexMap<String, Option<Value>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PiProviderPatchReceipt {
    path: PathBuf,
    before: PiProviderValuesSnapshot,
    attempted: IndexMap<String, Option<Value>>,
    attempted_fingerprints: IndexMap<String, String>,
    attempted_file_existed: bool,
}

impl PiProviderPatchReceipt {
    pub(crate) fn attempted_values(&self) -> &IndexMap<String, Option<Value>> {
        &self.attempted
    }

    pub(crate) fn attempted_snapshot(&self) -> PiProviderValuesSnapshot {
        PiProviderValuesSnapshot {
            file_existed: self.attempted_file_existed,
            values: self.attempted.clone(),
        }
    }

    /// Restore only provider keys which still contain this operation's exact
    /// attempted values. Unrelated root/provider edits are preserved, while a
    /// concurrent edit of an owned key turns compensation into an explicit
    /// conflict instead of being overwritten.
    pub(crate) fn rollback(&self) -> Result<(), AppError> {
        if let Err(error) = apply_pi_provider_patch_checked(
            &self.path,
            Some(&self.attempted),
            Some(&self.attempted_fingerprints),
            &self.before.values,
        ) {
            let observed =
                snapshot_pi_provider_values(&self.path, self.before.values.keys().cloned())?;
            if observed.values != self.before.values {
                return Err(error);
            }
            // A namespace mutation may have committed before its durability
            // barrier reported failure. Re-observe the exact semantic state
            // and retry the parent sync before declaring compensation done.
            sync_shared_file_parent(&self.path).map_err(|sync_error| {
                AppError::Config(format!(
                    "Pi provider rollback reached the previous exact-key state but could not \
                     confirm directory durability ({error}; retry={sync_error})"
                ))
            })?;
        }
        remove_new_empty_document(&self.path, &self.before)
    }
}

pub(crate) fn snapshot_pi_provider_values(
    path: &Path,
    provider_keys: impl IntoIterator<Item = String>,
) -> Result<PiProviderValuesSnapshot, AppError> {
    let bytes = read_models_bytes(path)?;
    let file_existed = bytes.is_some();
    Ok(PiProviderValuesSnapshot {
        file_existed,
        values: provider_values_from_bytes(
            path,
            bytes.as_deref(),
            provider_keys.into_iter().collect::<Vec<_>>().iter(),
        )?,
    })
}

/// Revalidate a previously captured exact-key set without mutating the
/// document. This is the ownership-claim barrier used when runtime takeover is
/// disabled and therefore has no gateway projection write of its own.
pub(crate) fn verify_pi_provider_values(
    path: &Path,
    expected: &IndexMap<String, Option<Value>>,
) -> Result<(), AppError> {
    let lock = path_lock(path)?;
    let _guard = lock_path(&lock)?;
    #[cfg(test)]
    run_before_provider_verify_hook(path)?;
    let bytes = read_models_bytes(path)?;
    ensure_provider_values_match(path, bytes.as_deref(), expected)
}

/// Revalidate semantic exact-key values and raw entry fingerprints under one
/// path lock. Native import uses the stronger raw barrier because its ownership
/// token is the fingerprint returned by public inspection.
pub(crate) fn verify_pi_provider_preconditions(
    path: &Path,
    expected_values: &IndexMap<String, Option<Value>>,
    expected_fingerprints: &IndexMap<String, String>,
) -> Result<(), AppError> {
    let lock = path_lock(path)?;
    let _guard = lock_path(&lock)?;
    #[cfg(test)]
    run_before_provider_verify_hook(path)?;
    let bytes = read_models_bytes(path)?;
    ensure_provider_values_match(path, bytes.as_deref(), expected_values)?;
    ensure_provider_fingerprints_match(path, bytes.as_deref(), expected_fingerprints)
}

#[cfg(test)]
fn run_before_provider_verify_hook(path: &Path) -> Result<(), AppError> {
    if let Some(replacement) = BEFORE_PROVIDER_VERIFY
        .lock()
        .map_err(|error| AppError::Lock(error.to_string()))?
        .remove(path)
    {
        fs::write(path, replacement).map_err(|error| AppError::io(path, error))?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn replace_before_next_pi_provider_verify(path: &Path, bytes: &[u8]) {
    BEFORE_PROVIDER_VERIFY
        .lock()
        .expect("Pi provider verify hook lock")
        .insert(path.to_path_buf(), bytes.to_vec());
}

fn attempted_provider_values(
    before: &PiProviderValuesSnapshot,
    patch: &IndexMap<String, Option<Value>>,
) -> IndexMap<String, Option<Value>> {
    before
        .values
        .iter()
        .map(|(provider_key, previous)| {
            (
                provider_key.clone(),
                patch
                    .get(provider_key)
                    .cloned()
                    .unwrap_or_else(|| previous.clone()),
            )
        })
        .collect()
}

/// Publish an exact-key patch only if every preflighted provider value still
/// matches. Whole-file CAS retries preserve unrelated edits, but re-check the
/// provider precondition before every retry.
pub(crate) fn apply_pi_provider_patch_with_receipt(
    path: &Path,
    before: &PiProviderValuesSnapshot,
    patch: &IndexMap<String, Option<Value>>,
) -> Result<PiProviderPatchReceipt, AppError> {
    apply_pi_provider_patch_with_receipt_and_fingerprints(path, before, None, patch)
}

/// Publish an exact-key patch while atomically binding selected entries to the
/// raw inspection fingerprints which authorized an ownership claim.
pub(crate) fn apply_pi_provider_patch_with_receipt_and_fingerprints(
    path: &Path,
    before: &PiProviderValuesSnapshot,
    expected_fingerprints: Option<&IndexMap<String, String>>,
    patch: &IndexMap<String, Option<Value>>,
) -> Result<PiProviderPatchReceipt, AppError> {
    if patch
        .keys()
        .any(|provider_key| !before.values.contains_key(provider_key))
    {
        return Err(AppError::InvalidInput(
            "Pi provider patch contains a key which was not preflighted".to_string(),
        ));
    }
    let attempted = attempted_provider_values(before, patch);
    let commit =
        apply_pi_provider_patch_checked(path, Some(&before.values), expected_fingerprints, patch)?;
    let attempted_fingerprints =
        provider_fingerprints_from_bytes(path, commit.bytes.as_deref(), attempted.keys())?;
    Ok(PiProviderPatchReceipt {
        path: path.to_path_buf(),
        before: before.clone(),
        attempted,
        attempted_fingerprints,
        attempted_file_existed: commit.bytes.is_some(),
    })
}

/// Remove a file which this operation created only when its bytes are still
/// the canonical empty document. If Pi or the user added any other content,
/// the file is retained.
fn remove_new_empty_document(
    path: &Path,
    before: &PiProviderValuesSnapshot,
) -> Result<(), AppError> {
    if before.file_existed {
        return Ok(());
    }

    let canonical_empty = serialize_models_mutation(path, None, &|_| Ok(()))?;
    let mut last_error = None;
    for _ in 0..MAX_MUTATION_ATTEMPTS {
        let current = read_shared_file(path, MAX_PI_MODELS_BYTES, "Pi models file")?;
        match current.bytes.as_deref() {
            None => {
                if last_error.is_some() {
                    sync_shared_file_parent(path)?;
                }
                return Ok(());
            }
            Some(bytes) if bytes != canonical_empty.as_slice() => {
                // A concurrent writer added unrelated content after our key
                // rollback. File ownership therefore belongs to that writer.
                return Ok(());
            }
            Some(_) => {}
        }
        match delete_shared_file(
            path,
            &current.revision,
            MAX_PI_MODELS_BYTES,
            "Pi models file",
        ) {
            Ok(_) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        AppError::Config("Pi empty models document cleanup did not make progress".to_string())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    #[test]
    fn reads_exact_pi_jsonc_dialect_and_retains_entry_cst() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        fs::write(
            &path,
            r#"{
  // root
  "providers": {
    "custom": {
      // nested comment
      "baseUrl": "https://example.test/v1",
      "models": [{"id": "model//literal",},],
    },
  },
}
"#,
        )
        .expect("write");

        let document = read_pi_models_document(&path).expect("read");
        let entry = &document.providers()["custom"];
        assert_eq!(entry.value["models"][0]["id"], "model//literal");
        assert!(entry.raw_source.contains("// nested comment"));
        assert!(entry.raw_source.contains("\"model//literal\""));
    }

    #[test]
    fn exact_key_patch_preserves_unowned_entries_comments_and_root_fields() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        fs::write(
            &path,
            r#"{
  "theme": "native",
  "providers": {
    // user-owned
    "native": {"models": [{"id": "native"}]},
    "managed": {"models": [{"id": "old"}]}
  }
}
"#,
        )
        .expect("write");
        let patch = IndexMap::from([(
            "managed".to_string(),
            Some(serde_json::json!({"models": [{"id": "new"}]})),
        )]);

        apply_pi_provider_patch(&path, &patch).expect("patch");

        let saved = fs::read_to_string(&path).expect("read");
        assert!(saved.contains("// user-owned"));
        assert!(saved.contains("\"theme\": \"native\""));
        let document = read_pi_models_document(&path).expect("parse");
        assert_eq!(
            document.providers()["native"].value["models"][0]["id"],
            "native"
        );
        assert_eq!(
            document.providers()["managed"].value["models"][0]["id"],
            "new"
        );
    }

    #[test]
    fn missing_models_snapshot_restores_absence_without_deleting_external_content() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        let before =
            snapshot_pi_provider_values(&path, ["managed".to_string()]).expect("snapshot absence");
        let receipt = apply_pi_provider_patch_with_receipt(
            &path,
            &before,
            &IndexMap::from([("managed".to_string(), Some(serde_json::json!({"api": "x"})))]),
        )
        .expect("publish");
        receipt.rollback().expect("restore absence");
        assert!(
            !path.exists(),
            "rollback must not leave an empty shadow file"
        );

        let before =
            snapshot_pi_provider_values(&path, ["managed".to_string()]).expect("snapshot absence");
        let receipt = apply_pi_provider_patch_with_receipt(
            &path,
            &before,
            &IndexMap::from([("managed".to_string(), Some(serde_json::json!({"api": "x"})))]),
        )
        .expect("publish");
        let mut external = fs::read_to_string(&path).expect("published document");
        let root_end = external.rfind("\n}").expect("root closing brace");
        external.insert_str(root_end, ",\n  \"external\": true");
        fs::write(&path, external).expect("external root-only update");
        receipt.rollback().expect("restore managed key");
        let restored: Value =
            serde_json::from_slice(&fs::read(&path).expect("external file retained"))
                .expect("parse restored");
        assert_eq!(restored.get("external"), Some(&Value::Bool(true)));
        assert!(restored
            .get("providers")
            .and_then(Value::as_object)
            .is_some_and(serde_json::Map::is_empty));
    }

    #[test]
    fn checked_provider_publish_rejects_a_key_changed_after_preflight() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        fs::write(&path, r#"{"providers":{}}"#).expect("seed");
        let before =
            snapshot_pi_provider_values(&path, ["managed".to_string()]).expect("preflight");
        crate::pi_config::shared_file::replace_before_next_compare_exchange(
            &path,
            br#"{"providers":{"managed":{"api":"external"}}}"#,
        );
        let error = apply_pi_provider_patch_with_receipt(
            &path,
            &before,
            &IndexMap::from([(
                "managed".to_string(),
                Some(serde_json::json!({"api": "attempted"})),
            )]),
        )
        .expect_err("external key must win");
        assert!(matches!(error, AppError::Conflict(_)));
        let document = read_pi_models_document(&path).expect("external document");
        assert_eq!(document.providers()["managed"].value["api"], "external");
    }

    #[test]
    fn provider_receipt_rollback_rejects_a_newer_exact_key_but_preserves_it() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        fs::write(
            &path,
            r#"{"root":"preserved","providers":{"managed":{"api":"before"}}}"#,
        )
        .expect("seed");
        let before =
            snapshot_pi_provider_values(&path, ["managed".to_string()]).expect("preflight");
        let receipt = apply_pi_provider_patch_with_receipt(
            &path,
            &before,
            &IndexMap::from([(
                "managed".to_string(),
                Some(serde_json::json!({"api": "attempted"})),
            )]),
        )
        .expect("publish");
        crate::pi_config::shared_file::replace_before_next_compare_exchange(
            &path,
            br#"{"root":"external","providers":{"managed":{"api":"external"}}}"#,
        );

        let error = receipt
            .rollback()
            .expect_err("rollback must not overwrite the newer key");
        assert!(matches!(error, AppError::Conflict(_)));
        let document: Value =
            serde_json::from_slice(&fs::read(&path).expect("read external")).expect("parse");
        assert_eq!(document["root"], "external");
        assert_eq!(document["providers"]["managed"]["api"], "external");
    }

    #[cfg(unix)]
    #[test]
    fn create_parent_sync_failure_conditionally_removes_the_committed_provider() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        let before =
            snapshot_pi_provider_values(&path, ["managed".to_string()]).expect("preflight");
        crate::pi_config::shared_file::fail_next_parent_sync_for_test(&path);

        let error = apply_pi_provider_patch_with_receipt(
            &path,
            &before,
            &IndexMap::from([(
                "managed".to_string(),
                Some(serde_json::json!({"api": "attempted"})),
            )]),
        )
        .expect_err("durability failure must remain visible");
        assert!(error.to_string().contains("injected"));
        assert!(
            !path.exists(),
            "a failed create must not leave a committed provider or empty shadow file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn replace_parent_sync_failure_conditionally_restores_the_previous_provider() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        fs::write(
            &path,
            r#"{"root":"preserved","providers":{"managed":{"api":"before"}}}"#,
        )
        .expect("seed");
        let before =
            snapshot_pi_provider_values(&path, ["managed".to_string()]).expect("preflight");
        crate::pi_config::shared_file::fail_next_parent_sync_for_test(&path);

        apply_pi_provider_patch_with_receipt(
            &path,
            &before,
            &IndexMap::from([(
                "managed".to_string(),
                Some(serde_json::json!({"api": "attempted"})),
            )]),
        )
        .expect_err("durability failure must remain visible");
        let restored: Value =
            serde_json::from_slice(&fs::read(&path).expect("restored document")).expect("parse");
        assert_eq!(restored["root"], "preserved");
        assert_eq!(restored["providers"]["managed"]["api"], "before");
    }

    #[cfg(unix)]
    #[test]
    fn remove_key_parent_sync_failure_conditionally_restores_the_deleted_provider() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        fs::write(
            &path,
            r#"{"providers":{"managed":{"api":"before"},"external":{"api":"keep"}}}"#,
        )
        .expect("seed");
        let before =
            snapshot_pi_provider_values(&path, ["managed".to_string()]).expect("preflight");
        crate::pi_config::shared_file::fail_next_parent_sync_for_test(&path);

        apply_pi_provider_patch_with_receipt(
            &path,
            &before,
            &IndexMap::from([("managed".to_string(), None)]),
        )
        .expect_err("durability failure must remain visible");
        let restored: Value =
            serde_json::from_slice(&fs::read(&path).expect("restored document")).expect("parse");
        assert_eq!(restored["providers"]["managed"]["api"], "before");
        assert_eq!(restored["providers"]["external"]["api"], "keep");
    }

    #[test]
    fn transient_conflict_recovery_failure_still_restores_external_provider_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        fs::write(&path, br#"{"providers":{"managed":{"api":"before"}}}"#).expect("seed");
        let before =
            snapshot_pi_provider_values(&path, ["managed".to_string()]).expect("preflight");
        let external = br#"{"providers":{"managed":{"api":"external"}}}"#;
        crate::pi_config::shared_file::replace_before_next_compare_exchange(&path, external);
        crate::pi_config::shared_file::fail_next_rollback_restore_for_test(&path);

        let error = apply_pi_provider_patch_with_receipt(
            &path,
            &before,
            &IndexMap::from([(
                "managed".to_string(),
                Some(serde_json::json!({"api": "attempted"})),
            )]),
        )
        .expect_err("an uncertain conflict must remain an error");
        assert!(matches!(error, AppError::Conflict(_)));
        let restored = read_pi_models_document(&path).expect("restored document");
        assert_eq!(restored.providers()["managed"].value["api"], "external");
    }

    #[test]
    fn transient_delete_recovery_failure_still_restores_external_provider_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        fs::write(&path, br#"{"providers":{"managed":{"api":"before"}}}"#).expect("seed");
        let before =
            snapshot_pi_provider_values(&path, ["managed".to_string()]).expect("preflight");
        let external = br#"{"providers":{"managed":{"api":"external"}}}"#;
        crate::pi_config::shared_file::replace_before_next_compare_exchange(&path, external);
        crate::pi_config::shared_file::fail_next_rollback_restore_for_test(&path);

        let error = apply_pi_provider_patch_with_receipt(
            &path,
            &before,
            &IndexMap::from([("managed".to_string(), None)]),
        )
        .expect_err("an uncertain delete conflict must remain an error");
        assert!(matches!(error, AppError::Conflict(_)));
        let restored = read_pi_models_document(&path).expect("restored document");
        assert_eq!(restored.providers()["managed"].value["api"], "external");
    }

    #[test]
    fn precondition_conflict_never_compensates_an_external_same_value_writer() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        let before =
            snapshot_pi_provider_values(&path, ["managed".to_string()]).expect("preflight");
        let external = br#"{
  "providers": {
    // external ownership and formatting must survive
    "managed": {"api": "attempted"}
  }
}"#;
        crate::pi_config::shared_file::replace_before_next_compare_exchange(&path, external);

        let error = apply_pi_provider_patch_with_receipt(
            &path,
            &before,
            &IndexMap::from([(
                "managed".to_string(),
                Some(serde_json::json!({"api": "attempted"})),
            )]),
        )
        .expect_err("the external create must own the conflict");
        assert!(matches!(error, AppError::Conflict(_)));
        assert_eq!(
            fs::read(&path).expect("external document preserved"),
            external,
            "semantic equality must never be used as writer identity"
        );
    }

    #[test]
    fn receipt_rollback_rejects_same_value_entry_with_new_raw_ownership() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        fs::write(&path, br#"{"providers":{"managed":{"api":"before"}}}"#).expect("seed");
        let before =
            snapshot_pi_provider_values(&path, ["managed".to_string()]).expect("preflight");
        let receipt = apply_pi_provider_patch_with_receipt(
            &path,
            &before,
            &IndexMap::from([(
                "managed".to_string(),
                Some(serde_json::json!({"api": "attempted"})),
            )]),
        )
        .expect("publish");
        let external = br#"{
  "providers": {
    // same value, independently published raw entry
    "managed": { "api": "attempted" }
  }
}"#;
        fs::write(&path, external).expect("external same-value rewrite");

        let error = receipt
            .rollback()
            .expect_err("raw ownership change must stop compensation");
        assert!(matches!(error, AppError::Conflict(_)));
        assert_eq!(fs::read(&path).expect("external retained"), external);
    }

    #[test]
    fn external_rename_during_patch_is_reparsed_before_owned_fields_change() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        fs::write(
            &path,
            r#"{"providers":{"managed":{"models":[{"id":"old"}]}}}"#,
        )
        .expect("seed");
        crate::pi_config::shared_file::replace_before_next_compare_exchange(
            &path,
            br#"{
  "externalRevision": 7,
  "providers": {
    "native": {"models": [{"id": "external"}]},
    "managed": {"models": [{"id": "old"}]}
  }
}"#,
        );
        let patch = IndexMap::from([(
            "managed".to_string(),
            Some(serde_json::json!({"models": [{"id": "new"}]})),
        )]);

        apply_pi_provider_patch(&path, &patch).expect("retry patch");

        let saved: Value = serde_json::from_slice(&fs::read(&path).expect("read")).expect("parse");
        assert_eq!(saved["externalRevision"], 7);
        assert_eq!(saved["providers"]["native"]["models"][0]["id"], "external");
        assert_eq!(saved["providers"]["managed"]["models"][0]["id"], "new");
    }

    #[test]
    fn exact_key_delete_does_not_delete_same_content_sibling() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("models.json");
        fs::write(
            &path,
            r#"{"providers":{
  "managed":{"models":[{"id":"same"}]},
  "native":{"models":[{"id":"same"}]}
}}"#,
        )
        .expect("write");
        let patch = IndexMap::from([("managed".to_string(), None)]);

        apply_pi_provider_patch(&path, &patch).expect("delete");

        let document = read_pi_models_document(&path).expect("parse");
        assert!(!document.providers().contains_key("managed"));
        assert!(document.providers().contains_key("native"));
    }

    #[test]
    fn parses_javascript_overflow_without_hiding_sibling_entries() {
        let document = parse_models_source(
            Path::new("models.json"),
            r#"{
  "providers": {
    "healthy": {"models": [{"id": "healthy"}]},
    "overflow": {"models": [{"id": "m", "contextWindow": 1e400}]}
  }
}"#,
        )
        .expect("JSON.parse accepts the numeric token before TypeBox validation");

        assert!(document.providers().contains_key("healthy"));
        let overflow = &document.providers()["overflow"].value["models"][0]["contextWindow"];
        assert!(
            overflow
                .as_number()
                .is_some_and(|number| number.as_f64().is_none()),
            "the raw evaluator must still be able to distinguish non-finite JavaScript Number"
        );
    }

    #[test]
    fn rejects_json_extensions_that_pinned_pi_rejects() {
        let temp = tempfile::tempdir().expect("tempdir");
        for (case, source) in [
            ("single-quoted", "{'providers': {}}"),
            ("bare-key", "{providers: {}}"),
            ("block-comment", "{\"providers\": {/* nope */}}"),
            ("missing-comma", "{\"providers\": {} \"other\": 1}"),
        ] {
            let path = temp.path().join(format!("{case}.json"));
            fs::write(&path, source).expect("write");
            assert!(read_pi_models_document(&path).is_err(), "{case} must fail");
        }
    }

    #[test]
    fn missing_file_is_an_empty_catalog_and_oversized_file_fails() {
        let temp = tempfile::tempdir().expect("tempdir");
        let missing = temp.path().join("missing-models.json");
        assert!(read_pi_models_document(&missing)
            .expect("missing")
            .providers()
            .is_empty());

        let oversized = temp.path().join("oversized-models.json");
        let mut file = File::create(&oversized).expect("create");
        file.write_all(b"{").expect("seed");
        file.set_len(MAX_PI_MODELS_BYTES + 1).expect("extend");
        assert!(read_pi_models_document(&oversized).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_is_never_followed() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("target.json");
        let link = temp.path().join("models.json");
        fs::write(&target, EMPTY_MODELS_DOCUMENT).expect("target");
        symlink(&target, &link).expect("symlink");
        assert!(read_pi_models_document(&link).is_err());
    }
}

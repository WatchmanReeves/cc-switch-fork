//! Pi Coding Agent integration boundaries.
//!
//! This module deliberately separates the managed control-plane model from
//! Pi's shared files and from the proxy data plane.  Callers must use the
//! typed model resolver rather than reimplementing provider/model inheritance.

use indexmap::IndexMap;
use serde_json::{Map, Value};

pub(crate) mod composer;
pub(crate) mod document;
pub(crate) mod gateway;
pub(crate) mod model;
pub(crate) mod native;
#[cfg(test)]
mod native_inspection_certification;
pub(crate) mod native_settings;
pub(crate) mod raw_schema;
pub(crate) mod shared_file;

const PI_COMPAT_NESTED_SPREAD_KEYS: [&str; 3] = [
    "openRouterRouting",
    "vercelGatewayRouting",
    "chatTemplateKwargs",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PiCompatMergeError;

#[derive(Debug, Clone, PartialEq)]
enum JavaScriptSpreadValue {
    Json(Value),
    LoneSurrogate,
}

type JavaScriptSpreadMap = IndexMap<String, JavaScriptSpreadValue>;

/// Mirror pinned Pi's `mergeCompat` JavaScript object-spread semantics.
///
/// Arrays expose numeric enumerable properties, strings expose character
/// properties, objects expose their own fields, and the remaining JSON
/// primitives expose none. Existing key positions are retained when an
/// overlay replaces their values, matching object spread.
///
/// A JavaScript string is indexed by UTF-16 code unit. Spreading an astral
/// character therefore creates lone-surrogate string values, which cannot be
/// represented by Rust `String` or `serde_json::Value`. That shape is rejected
/// explicitly so callers can fail closed instead of emitting a different
/// composed model.
fn merge_pi_compat(
    base: Option<Value>,
    overlay: Option<Value>,
) -> Result<Option<Value>, PiCompatMergeError> {
    let Some(overlay) = overlay else {
        return Ok(base);
    };
    if !javascript_truthy(&overlay) {
        return Ok(base);
    }

    let mut merged = javascript_object_spread(base.as_ref());
    merged.extend(javascript_object_spread(Some(&overlay)));

    for key in PI_COMPAT_NESTED_SPREAD_KEYS {
        let base_value = javascript_property(base.as_ref(), key);
        let overlay_value = javascript_property(Some(&overlay), key);
        if base_value.is_some_and(javascript_is_object)
            || overlay_value.is_some_and(javascript_is_object)
        {
            let mut nested = javascript_object_spread(base_value);
            nested.extend(javascript_object_spread(overlay_value));
            merged.insert(
                key.to_string(),
                JavaScriptSpreadValue::Json(Value::Object(finish_javascript_object_spread(
                    nested,
                )?)),
            );
        }
    }
    Ok(Some(Value::Object(finish_javascript_object_spread(
        merged,
    )?)))
}

fn javascript_truthy(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => false,
        Value::Number(value) => value.as_f64().is_none_or(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Bool(true) | Value::Array(_) | Value::Object(_) => true,
    }
}

fn javascript_is_object(value: &Value) -> bool {
    matches!(value, Value::Array(_) | Value::Object(_))
}

fn javascript_property<'a>(value: Option<&'a Value>, key: &str) -> Option<&'a Value> {
    value.and_then(Value::as_object)?.get(key)
}

fn javascript_object_spread(value: Option<&Value>) -> JavaScriptSpreadMap {
    match value {
        Some(Value::Object(object)) => object
            .iter()
            .map(|(key, value)| (key.clone(), JavaScriptSpreadValue::Json(value.clone())))
            .collect(),
        Some(Value::Array(values)) => values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                (
                    index.to_string(),
                    JavaScriptSpreadValue::Json(value.clone()),
                )
            })
            .collect(),
        Some(Value::String(value)) => value
            .encode_utf16()
            .enumerate()
            .map(|(index, unit)| {
                let value = char::from_u32(u32::from(unit))
                    .map(|character| {
                        JavaScriptSpreadValue::Json(Value::String(character.to_string()))
                    })
                    .unwrap_or(JavaScriptSpreadValue::LoneSurrogate);
                (index.to_string(), value)
            })
            .collect(),
        Some(Value::Null | Value::Bool(_) | Value::Number(_)) | None => IndexMap::new(),
    }
}

fn finish_javascript_object_spread(
    spread: JavaScriptSpreadMap,
) -> Result<Map<String, Value>, PiCompatMergeError> {
    spread
        .into_iter()
        .map(|(key, value)| match value {
            JavaScriptSpreadValue::Json(value) => Ok((key, value)),
            JavaScriptSpreadValue::LoneSurrogate => Err(PiCompatMergeError),
        })
        .collect()
}

#[cfg(test)]
mod compat_spread_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compat_nested_values_follow_javascript_object_spread() {
        let merged = merge_pi_compat(
            Some(json!({
                "openRouterRouting": ["first", "second"],
                "chatTemplateKwargs": "ab",
                "baseOnly": true
            })),
            Some(json!({
                "openRouterRouting": null,
                "chatTemplateKwargs": {"named": true},
                "overlayOnly": true
            })),
        )
        .expect("representable compat spread")
        .expect("truthy overlay produces an object");

        assert_eq!(
            merged,
            json!({
                "openRouterRouting": {"0": "first", "1": "second"},
                "chatTemplateKwargs": {"0": "a", "1": "b", "named": true},
                "baseOnly": true,
                "overlayOnly": true
            })
        );
    }

    #[test]
    fn compat_falsy_overlay_returns_base_without_spreading() {
        let base = Some(json!({"openRouterRouting": ["kept"]}));
        assert_eq!(merge_pi_compat(base.clone(), Some(Value::Null)), Ok(base));
    }

    #[test]
    fn compat_spread_rejects_unrepresentable_javascript_surrogates() {
        assert_eq!(
            merge_pi_compat(
                Some(json!({"chatTemplateKwargs": "😀"})),
                Some(json!({"chatTemplateKwargs": {"named": true}})),
            ),
            Err(PiCompatMergeError)
        );
    }

    #[test]
    fn compat_spread_checks_surrogates_after_later_properties_override_them() {
        assert_eq!(
            merge_pi_compat(
                Some(json!({"chatTemplateKwargs": "😀"})),
                Some(json!({
                    "chatTemplateKwargs": {
                        "0": "repaired-high",
                        "1": "repaired-low",
                        "named": true
                    }
                })),
            ),
            Ok(Some(json!({
                "chatTemplateKwargs": {
                    "0": "repaired-high",
                    "1": "repaired-low",
                    "named": true
                }
            })))
        );
    }
}

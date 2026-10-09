//! MCP 2026-07-28 HTTP request headers that mirror the JSON-RPC body:
//! `Mcp-Name` and the `Mcp-Param-{name}` headers a tool asks for with
//! `x-mcp-header` in its `inputSchema`.
//!
//! The spec makes these mandatory for a client on Streamable HTTP: a tool
//! whose `x-mcp-header` annotations break the rules is left out of
//! `tools/list`, and every call to a valid one carries the annotated
//! argument values as headers. Values that are not plain header-safe ASCII
//! go as `=?base64?…?=`.

use anyhow::{Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use std::collections::HashSet;

const ANNOTATION: &str = "x-mcp-header";
/// `Mcp-Name`: `params.name` of a `tools/call`.
const NAME_HEADER: HeaderName = HeaderName::from_static("mcp-name");
const SENTINEL_PREFIX: &str = "=?base64?";
const SENTINEL_SUFFIX: &str = "?=";
/// JavaScript's safe-integer bound, which the spec applies to integer
/// header parameters.
const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

/// One annotated parameter: where its value sits in the arguments (a chain of
/// `properties` keys) and the `{name}` of its `Mcp-Param-{name}` header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ParamHeader {
    pub path: Vec<String>,
    pub name: String,
}

/// The `x-mcp-header` annotations of one tool's `inputSchema`.
///
/// # Errors
///
/// The reason the tool must be left out: an annotation that is not a
/// non-empty token, a duplicate name (case-insensitive), an annotation on a
/// non-primitive or `number` property, or one that is not reachable through
/// `properties` keys alone (under `items`, `oneOf`, `$ref`, ...).
pub(super) fn param_headers(input_schema: &Value) -> Result<Vec<ParamHeader>> {
    let mut found = Vec::new();
    collect_reachable(input_schema, &mut Vec::new(), &mut found)?;
    let total = count_annotations(input_schema, false);
    if total != found.len() {
        return Err(anyhow!(
            "an {ANNOTATION} annotation is not reachable through 'properties' alone"
        ));
    }
    let mut seen = HashSet::new();
    for header in &found {
        if !seen.insert(header.name.to_ascii_lowercase()) {
            return Err(anyhow!("{ANNOTATION} '{}' is used twice", header.name));
        }
    }
    Ok(found)
}

/// Walk `properties` chains from the root and validate each annotation found.
fn collect_reachable(
    schema: &Value,
    path: &mut Vec<String>,
    found: &mut Vec<ParamHeader>,
) -> Result<()> {
    let Some(object) = schema.as_object() else {
        return Ok(());
    };
    if let Some(annotation) = object.get(ANNOTATION) {
        let name = annotation
            .as_str()
            .filter(|n| !n.is_empty() && n.bytes().all(is_tchar))
            .ok_or_else(|| anyhow!("{ANNOTATION} {annotation} is not a header-name token"))?;
        match object.get("type").and_then(Value::as_str) {
            Some("string" | "integer" | "boolean") => {},
            _ => {
                return Err(anyhow!(
                    "{ANNOTATION} '{name}' is on a property that is not a string, integer or boolean"
                ));
            },
        }
        if path.is_empty() {
            return Err(anyhow!("{ANNOTATION} '{name}' is on the schema root"));
        }
        found.push(ParamHeader {
            path: path.clone(),
            name: name.to_string(),
        });
    }
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        for (key, child) in properties {
            path.push(key.clone());
            collect_reachable(child, path, found)?;
            path.pop();
        }
    }
    Ok(())
}

/// Every `x-mcp-header` keyword anywhere in the schema. `names` is true while
/// walking a map whose keys are names, not keywords (`properties`, `$defs`):
/// a property called "x-mcp-header" is not an annotation.
fn count_annotations(schema: &Value, names: bool) -> usize {
    match schema {
        Value::Object(object) => object
            .iter()
            .map(|(key, value)| {
                if names {
                    return count_annotations(value, false);
                }
                match key.as_str() {
                    ANNOTATION => 1 + count_annotations(value, false),
                    // Instance data, not schema.
                    "const" | "enum" | "default" | "examples" => 0,
                    "properties" | "patternProperties" | "$defs" | "definitions"
                    | "dependentSchemas" => count_annotations(value, true),
                    _ => count_annotations(value, false),
                }
            })
            .sum(),
        Value::Array(items) => items.iter().map(|v| count_annotations(v, false)).sum(),
        _ => 0,
    }
}

/// RFC 9110 `tchar`.
fn is_tchar(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

/// The headers for one `tools/call`: `Mcp-Name`, plus `Mcp-Param-{name}` for
/// each annotated argument that has a value. A missing or `null` value sends
/// no header.
///
/// # Errors
///
/// None in practice: every value is encoded to a valid header first. The
/// `Result` covers a header name the validation above already vetted.
pub(super) fn call_headers(
    tool: &str,
    params: &[ParamHeader],
    arguments: &Value,
) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(NAME_HEADER, encode_value(tool)?);
    for param in params {
        let mut value = arguments;
        for key in &param.path {
            match value.get(key) {
                Some(next) => value = next,
                None => {
                    value = &Value::Null;
                    break;
                },
            }
        }
        let text = match value {
            Value::String(s) => s.clone(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => match integer(n) {
                Some(i) => i.to_string(),
                // Not an integer: the arguments do not match the schema, so
                // send no header and let the server judge the call.
                None => continue,
            },
            _ => continue,
        };
        let name = HeaderName::from_bytes(format!("mcp-param-{}", param.name).as_bytes())
            .map_err(|_| anyhow!("invalid {ANNOTATION} '{}'", param.name))?;
        headers.insert(name, encode_value(&text)?);
    }
    Ok(headers)
}

/// A JSON number that is an integer in the safe range (`1.0` counts).
fn integer(n: &serde_json::Number) -> Option<i64> {
    let i = n.as_i64().or_else(|| {
        n.as_f64()
            .filter(|f| f.fract() == 0.0 && f.abs() <= MAX_SAFE_INTEGER as f64)
            .map(|f| f as i64)
    })?;
    (i.abs() <= MAX_SAFE_INTEGER).then_some(i)
}

/// A header value: as-is when it is visible ASCII with inner spaces or tabs,
/// else base64 inside the sentinel. A plain value that looks like the
/// sentinel is encoded too, so it cannot be misread.
fn encode_value(value: &str) -> Result<HeaderValue> {
    let plain = value
        .bytes()
        .all(|b| b == b'\t' || (0x20..=0x7e).contains(&b))
        && !value.starts_with([' ', '\t'])
        && !value.ends_with([' ', '\t'])
        && !(value.starts_with(SENTINEL_PREFIX) && value.ends_with(SENTINEL_SUFFIX));
    let text = if plain {
        value.to_string()
    } else {
        format!(
            "{SENTINEL_PREFIX}{}{SENTINEL_SUFFIX}",
            STANDARD.encode(value.as_bytes())
        )
    };
    HeaderValue::from_str(&text).map_err(|e| anyhow!("MCP header value: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn header(map: &HeaderMap, name: &str) -> Option<String> {
        map.get(name).map(|v| v.to_str().unwrap().to_string())
    }

    #[test]
    fn spec_example_tool_gets_its_region_header() {
        let schema = json!({
            "type": "object",
            "properties": {
                "region": {"type": "string", "x-mcp-header": "Region"},
                "query": {"type": "string"}
            }
        });
        let params = param_headers(&schema).unwrap();
        assert_eq!(
            params,
            vec![ParamHeader {
                path: vec!["region".into()],
                name: "Region".into()
            }]
        );
        let headers = call_headers(
            "execute_sql",
            &params,
            &json!({"region": "us-west1", "query": "SELECT 1"}),
        )
        .unwrap();
        assert_eq!(header(&headers, "mcp-name").as_deref(), Some("execute_sql"));
        assert_eq!(
            header(&headers, "mcp-param-region").as_deref(),
            Some("us-west1")
        );
    }

    #[test]
    fn nested_properties_are_reachable_and_types_convert() {
        let schema = json!({
            "type": "object",
            "properties": {
                "opts": {"type": "object", "properties": {
                    "count": {"type": "integer", "x-mcp-header": "Count"},
                    "dry": {"type": "boolean", "x-mcp-header": "Dry"}
                }},
                "missing": {"type": "string", "x-mcp-header": "Missing"},
                "nothing": {"type": "string", "x-mcp-header": "Nothing"}
            }
        });
        let params = param_headers(&schema).unwrap();
        let headers = call_headers(
            "t",
            &params,
            &json!({"opts": {"count": -7, "dry": false}, "nothing": null}),
        )
        .unwrap();
        assert_eq!(header(&headers, "mcp-param-count").as_deref(), Some("-7"));
        assert_eq!(header(&headers, "mcp-param-dry").as_deref(), Some("false"));
        assert!(headers.get("mcp-param-missing").is_none(), "absent value");
        assert!(headers.get("mcp-param-nothing").is_none(), "null value");
    }

    #[test]
    fn values_are_encoded_like_the_spec_table() {
        let cases = [
            ("us-west1", "us-west1"),
            ("Hello, 世界", "=?base64?SGVsbG8sIOS4lueVjA==?="),
            (" padded ", "=?base64?IHBhZGRlZCA=?="),
            ("line1\nline2", "=?base64?bGluZTEKbGluZTI=?="),
            ("=?base64?literal?=", "=?base64?PT9iYXNlNjQ/bGl0ZXJhbD89?="),
        ];
        for (raw, want) in cases {
            assert_eq!(
                encode_value(raw).unwrap().to_str().unwrap(),
                want,
                "{raw:?}"
            );
        }
    }

    #[test]
    fn invalid_annotations_reject_the_tool() {
        let bad = [
            // Not a token.
            json!({"properties": {"a": {"type": "string", "x-mcp-header": "Bad Name"}}}),
            json!({"properties": {"a": {"type": "string", "x-mcp-header": ""}}}),
            json!({"properties": {"a": {"type": "string", "x-mcp-header": 5}}}),
            // Not a primitive, or a float.
            json!({"properties": {"a": {"type": "number", "x-mcp-header": "A"}}}),
            json!({"properties": {"a": {"type": "object", "x-mcp-header": "A"}}}),
            json!({"properties": {"a": {"x-mcp-header": "A"}}}),
            // Duplicate, case-insensitive.
            json!({"properties": {
                "a": {"type": "string", "x-mcp-header": "Id"},
                "b": {"type": "string", "x-mcp-header": "ID"}
            }}),
            // Not statically reachable.
            json!({"properties": {"a": {"type": "array", "items": {"type": "string", "x-mcp-header": "A"}}}}),
            json!({"oneOf": [{"properties": {"a": {"type": "string", "x-mcp-header": "A"}}}]}),
            json!({"$defs": {"a": {"type": "string", "x-mcp-header": "A"}}}),
        ];
        for schema in bad {
            assert!(param_headers(&schema).is_err(), "{schema}");
        }
    }

    #[test]
    fn a_property_named_like_the_keyword_is_not_an_annotation() {
        let schema = json!({"properties": {"x-mcp-header": {"type": "string"}},
                            "default": {"x-mcp-header": "data"}});
        assert_eq!(param_headers(&schema).unwrap(), vec![]);
    }

    #[test]
    fn a_tool_name_outside_the_safe_set_is_base64() {
        let headers = call_headers("météo", &[], &json!({})).unwrap();
        assert_eq!(
            header(&headers, "mcp-name").as_deref(),
            Some("=?base64?bcOpdMOpbw==?=")
        );
    }
}

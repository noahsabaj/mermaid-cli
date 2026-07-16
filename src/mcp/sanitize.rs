//! Provider-safe sanitization of MCP tool names and schemas.
//!
//! MCP servers advertise arbitrary tool names and full JSON-Schema input
//! schemas. Providers are stricter: OpenAI caps function names at 64 chars
//! of `[A-Za-z0-9_-]`, Gemini requires a letter start and rejects most
//! ref/composition keywords, and interleaved `__` breaks our own
//! `mcp__<server>__<tool>` routing split. Everything here is pure string /
//! value processing so it can run at ingestion time (once per server
//! startup) and be tested exhaustively.
//!
//! Raw names are preserved by the caller (`McpServerManager`) in lookup
//! maps; sanitization is one-way and deterministic: the same inputs always
//! produce the same advertised names, independent of startup completion
//! order.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::Value;

use super::client::McpToolDef;
use crate::domain::McpToolSpec;

/// Maximum length of the full advertised tool name (`mcp__srv__tool`).
/// 64 is the common safe bound: OpenAI caps function names at 64 chars;
/// Gemini allows 63 plus a letter start (the `mcp` prefix guarantees the
/// letter start); Anthropic allows 128.
pub const MAX_TOOL_NAME_LEN: usize = 64;

/// Maximum length of the sanitized server segment inside the full name.
/// Keeps room for the tool segment: 64 - "mcp__".len() - "__".len() - 24
/// leaves 33 chars for the tool.
pub const SERVER_SEGMENT_MAX: usize = 24;

/// Number of hex chars in a collision/overflow suffix (preceded by `_`).
const HASH_SUFFIX_LEN: usize = 6;

/// Map every char outside `[A-Za-z0-9_-]` to `_`, collapse runs of `_`
/// to a single `_`, and trim leading/trailing `_`. Collapsing is what
/// makes the `mcp__<server>__<tool>` split round-trip: a sanitized
/// segment can never contain `__`. Empty results fall back to `"x"`.
pub fn sanitize_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut last_underscore = false;
    for ch in raw.chars() {
        let mapped = if ch.is_ascii_alphanumeric() || ch == '-' {
            ch
        } else {
            '_'
        };
        if mapped == '_' {
            if last_underscore {
                continue;
            }
            last_underscore = true;
        } else {
            last_underscore = false;
        }
        out.push(mapped);
    }
    let trimmed = out.trim_matches('_');
    if trimmed.is_empty() {
        "x".to_string()
    } else {
        trimmed.to_string()
    }
}

/// FNV-1a over the raw bytes, rendered as lowercase hex. Collision
/// avoidance only — not cryptographic. Deterministic across runs.
fn fnv1a_hex(input: &str, len: usize) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    let hex = format!("{hash:016x}");
    hex[hex.len() - len..].to_string()
}

/// Truncate `segment` to `max_len`, replacing the tail with `_` + a
/// stable hash of `raw` when truncation or a collision (`taken`) forces
/// disambiguation. Also disambiguates non-truncated collisions.
fn fit_segment(segment: &str, raw: &str, max_len: usize, taken: &HashSet<String>) -> String {
    let needs_hash = segment.len() > max_len || taken.contains(segment);
    if !needs_hash {
        return segment.to_string();
    }
    let suffix = format!("_{}", fnv1a_hex(raw, HASH_SUFFIX_LEN));
    let keep = max_len.saturating_sub(suffix.len()).max(1);
    let head: String = segment.chars().take(keep).collect();
    let head = head.trim_end_matches('_');
    let mut candidate = format!("{head}{suffix}");
    // A hash collision on top of a name collision is vanishingly rare but
    // cheap to break deterministically: extend the hash source.
    let mut salt = 0u32;
    while taken.contains(&candidate) {
        salt += 1;
        let suffix = format!("_{}", fnv1a_hex(&format!("{raw}#{salt}"), HASH_SUFFIX_LEN));
        let keep = max_len.saturating_sub(suffix.len()).max(1);
        let head: String = segment.chars().take(keep).collect();
        candidate = format!("{}{suffix}", head.trim_end_matches('_'));
    }
    candidate
}

/// Assign sanitized server-name aliases for a SORTED list of raw config
/// names. Sorted input makes suffix assignment independent of startup
/// completion order. Returns sanitized-alias -> raw-name.
pub fn assign_server_aliases<'a, I>(sorted_raw_names: I) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = &'a str>,
{
    let mut aliases = BTreeMap::new();
    let mut taken: HashSet<String> = HashSet::new();
    for raw in sorted_raw_names {
        let base = sanitize_segment(raw);
        let alias = fit_segment(&base, raw, SERVER_SEGMENT_MAX, &taken);
        taken.insert(alias.clone());
        aliases.insert(alias, raw.to_string());
    }
    aliases
}

/// Sanitize one server's tool list into advertised specs plus the
/// sanitized-full-name -> raw-tool-name dispatch map. Collisions are
/// resolved in `tools` order (the server's own `list_tools` order).
pub fn sanitize_server_tools(
    sanitized_server: &str,
    tools: &[McpToolDef],
) -> (Vec<McpToolSpec>, HashMap<String, String>) {
    let prefix = format!("mcp__{sanitized_server}__");
    let tool_budget = MAX_TOOL_NAME_LEN.saturating_sub(prefix.len()).max(1);

    let mut specs = Vec::with_capacity(tools.len());
    let mut raw_names = HashMap::with_capacity(tools.len());
    let mut taken: HashSet<String> = HashSet::new();

    for def in tools {
        let base = sanitize_segment(&def.name);
        let segment = fit_segment(&base, &def.name, tool_budget, &taken);
        taken.insert(segment.clone());
        let full = format!("{prefix}{segment}");
        raw_names.insert(full.clone(), def.name.clone());
        specs.push(McpToolSpec {
            name: full,
            raw_name: def.name.clone(),
            description: def.description.clone(),
            input_schema: sanitize_schema(&def.input_schema),
            read_only_hint: def.read_only_hint,
        });
    }
    (specs, raw_names)
}

/// Depth cap for `$ref` inlining; a deeper (or cyclic) reference degrades
/// to a permissive object schema that keeps the original description.
const REF_INLINE_DEPTH: usize = 8;

/// Strip/normalize JSON-Schema constructs that commonly break provider
/// function-calling validators, leaving everything else untouched:
///
/// - local `$ref` (`#/$defs/...`, `#/definitions/...`) inlined (depth-capped,
///   cycle-safe; unresolvable refs become `{"type":"object"}` keeping any
///   sibling `description`)
/// - `$schema`, `$id`, `$defs`, `definitions`, `$comment` dropped
/// - `const: v` rewritten to `enum: [v]` (Gemini)
/// - `anyOf`/`oneOf` where removing `{"type":"null"}` branches leaves exactly
///   one branch flatten to that branch (the pervasive "nullable" pattern)
/// - single-element `allOf` flattened
/// - draft-4 boolean `exclusiveMinimum`/`exclusiveMaximum` dropped
pub fn sanitize_schema(schema: &Value) -> Value {
    let defs = collect_defs(schema);
    sanitize_node(schema, &defs, 0)
}

/// Gather the definition tables reachable from the root so `$ref` targets
/// can be inlined. Keyed by full pointer (`#/$defs/Name`).
fn collect_defs(root: &Value) -> HashMap<String, Value> {
    let mut defs = HashMap::new();
    for table_key in ["$defs", "definitions"] {
        if let Some(Value::Object(table)) = root.get(table_key) {
            for (name, sub) in table {
                defs.insert(format!("#/{table_key}/{name}"), sub.clone());
            }
        }
    }
    defs
}

fn resolve_ref<'a>(target: &str, defs: &'a HashMap<String, Value>) -> Option<&'a Value> {
    defs.get(target)
}

fn sanitize_node(node: &Value, defs: &HashMap<String, Value>, depth: usize) -> Value {
    let Value::Object(obj) = node else {
        return node.clone();
    };

    // $ref: inline the target (merging is not attempted; JSON Schema
    // semantics say siblings of $ref are ignored pre-2020 anyway, except
    // description which we deliberately preserve for the model).
    if let Some(Value::String(target)) = obj.get("$ref") {
        let description = obj.get("description").cloned();
        let mut inlined = if depth >= REF_INLINE_DEPTH {
            serde_json::json!({"type": "object"})
        } else {
            match resolve_ref(target, defs) {
                Some(resolved) => sanitize_node(resolved, defs, depth + 1),
                None => serde_json::json!({"type": "object"}),
            }
        };
        if let (Value::Object(map), Some(desc)) = (&mut inlined, description)
            && !map.contains_key("description")
        {
            map.insert("description".to_string(), desc);
        }
        return inlined;
    }

    let mut out = serde_json::Map::with_capacity(obj.len());
    for (key, value) in obj {
        match key.as_str() {
            "$schema" | "$id" | "$defs" | "definitions" | "$comment" => {},
            "exclusiveMinimum" | "exclusiveMaximum" if value.is_boolean() => {},
            "const" => {
                out.insert("enum".to_string(), Value::Array(vec![value.clone()]));
            },
            "properties" | "patternProperties" => {
                let Value::Object(props) = value else {
                    out.insert(key.clone(), value.clone());
                    continue;
                };
                let mapped: serde_json::Map<String, Value> = props
                    .iter()
                    .map(|(name, sub)| (name.clone(), sanitize_node(sub, defs, depth + 1)))
                    .collect();
                out.insert(key.clone(), Value::Object(mapped));
            },
            "items" | "additionalProperties" if value.is_object() => {
                out.insert(key.clone(), sanitize_node(value, defs, depth + 1));
            },
            "prefixItems" => {
                if let Value::Array(items) = value {
                    let mapped: Vec<Value> = items
                        .iter()
                        .map(|sub| sanitize_node(sub, defs, depth + 1))
                        .collect();
                    out.insert(key.clone(), Value::Array(mapped));
                } else {
                    out.insert(key.clone(), value.clone());
                }
            },
            "anyOf" | "oneOf" | "allOf" => {
                let Value::Array(branches) = value else {
                    out.insert(key.clone(), value.clone());
                    continue;
                };
                let mapped: Vec<Value> = branches
                    .iter()
                    .map(|sub| sanitize_node(sub, defs, depth + 1))
                    .collect();
                let flattened = flatten_combinator(key, mapped);
                match flattened {
                    Flattened::Single(inner) => {
                        // Merge the single branch into the parent node,
                        // parent keys (description etc.) win.
                        if let Value::Object(inner_map) = inner {
                            for (k, v) in inner_map {
                                out.entry(k).or_insert(v);
                            }
                        }
                    },
                    Flattened::Keep(branches) => {
                        out.insert(key.clone(), Value::Array(branches));
                    },
                }
            },
            _ => {
                out.insert(key.clone(), value.clone());
            },
        }
    }
    Value::Object(out)
}

enum Flattened {
    /// Combinator collapsed to exactly one branch — inline it.
    Single(Value),
    /// Keep the (sanitized) branch list under the original keyword.
    Keep(Vec<Value>),
}

/// Collapse "nullable" anyOf/oneOf (drop `{"type":"null"}` branches when
/// exactly one non-null branch remains) and single-element allOf.
fn flatten_combinator(keyword: &str, branches: Vec<Value>) -> Flattened {
    if keyword == "allOf" {
        if branches.len() == 1 {
            return Flattened::Single(branches.into_iter().next().expect("len checked"));
        }
        return Flattened::Keep(branches);
    }
    let non_null: Vec<&Value> = branches.iter().filter(|b| !is_null_type(b)).collect();
    if non_null.len() == 1 && non_null.len() < branches.len() {
        return Flattened::Single((*non_null[0]).clone());
    }
    Flattened::Keep(branches)
}

fn is_null_type(v: &Value) -> bool {
    matches!(
        v.get("type"),
        Some(Value::String(t)) if t == "null"
    ) && v.as_object().is_some_and(|o| o.len() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn def(name: &str, schema: Value) -> McpToolDef {
        McpToolDef {
            name: name.to_string(),
            description: format!("{name} description"),
            input_schema: schema,
            read_only_hint: false,
        }
    }

    #[test]
    fn sanitize_carries_the_read_only_hint() {
        let mut read = def("get_thing", json!({"type": "object"}));
        read.read_only_hint = true;
        let write = def("send_thing", json!({"type": "object"}));
        let (specs, _) = sanitize_server_tools("srv", &[read, write]);
        assert!(specs[0].read_only_hint, "hint must survive sanitization");
        assert!(!specs[1].read_only_hint);
    }

    #[test]
    fn segment_coerces_charset_and_collapses_underscores() {
        assert_eq!(sanitize_segment("my.tool name"), "my_tool_name");
        assert_eq!(sanitize_segment("srv__with__doubles"), "srv_with_doubles");
        assert_eq!(sanitize_segment("__trim__"), "trim");
        assert_eq!(sanitize_segment("ok-name_1"), "ok-name_1");
        assert_eq!(sanitize_segment("日本語"), "x");
        assert_eq!(sanitize_segment(""), "x");
    }

    #[test]
    fn aliases_are_deterministic_and_collision_free() {
        let names = ["a.b", "a_b", "plain"];
        let aliases = assign_server_aliases(names.iter().copied());
        assert_eq!(aliases.len(), 3);
        // Both "a.b" and "a_b" sanitize to "a_b"; the later sorted key gets
        // the hash suffix. Sorted order: "a.b" < "a_b" (\u{2e} < \u{5f}).
        assert_eq!(aliases.get("a_b").map(String::as_str), Some("a.b"));
        let suffixed = aliases
            .iter()
            .find(|(_, raw)| raw.as_str() == "a_b")
            .expect("a_b present");
        assert!(suffixed.0.starts_with("a_b_") || suffixed.0.starts_with("a_"),);
        assert_ne!(suffixed.0, "a_b");
        // Deterministic across calls.
        assert_eq!(aliases, assign_server_aliases(names.iter().copied()));
    }

    #[test]
    fn long_server_segment_is_capped_with_hash() {
        let long = "x".repeat(60);
        let aliases = assign_server_aliases([long.as_str()]);
        let (alias, raw) = aliases.iter().next().expect("one alias");
        assert_eq!(raw, &long);
        assert!(alias.len() <= SERVER_SEGMENT_MAX);
        assert!(alias.contains('_'), "hash suffix expected: {alias}");
    }

    #[test]
    fn tool_names_fit_the_full_name_budget() {
        let long_tool = "t".repeat(100);
        let (specs, raw_names) =
            sanitize_server_tools("server", &[def(&long_tool, json!({"type":"object"}))]);
        assert_eq!(specs.len(), 1);
        assert!(
            specs[0].name.len() <= MAX_TOOL_NAME_LEN,
            "{}",
            specs[0].name
        );
        assert!(specs[0].name.starts_with("mcp__server__"));
        assert_eq!(specs[0].raw_name, long_tool);
        assert_eq!(raw_names.get(&specs[0].name), Some(&long_tool));
    }

    #[test]
    fn tool_collisions_resolve_in_list_order() {
        let (specs, raw_names) = sanitize_server_tools(
            "s",
            &[def("get.data", json!({})), def("get_data", json!({}))],
        );
        assert_eq!(specs[0].name, "mcp__s__get_data");
        assert_ne!(specs[1].name, specs[0].name);
        assert_eq!(raw_names.len(), 2);
        assert_eq!(
            raw_names.get(&specs[1].name).map(String::as_str),
            Some("get_data")
        );
    }

    #[test]
    fn sanitized_names_never_break_the_routing_split() {
        let (specs, _) = sanitize_server_tools("srv", &[def("a__b__c", json!({}))]);
        let name = &specs[0].name;
        let rest = name.strip_prefix("mcp__").expect("prefix");
        let (server, tool) = rest.split_once("__").expect("split");
        assert_eq!(server, "srv");
        assert_eq!(tool, "a_b_c");
    }

    #[test]
    fn schema_inlines_local_refs() {
        let schema = json!({
            "type": "object",
            "properties": {
                "item": { "$ref": "#/$defs/Item", "description": "the item" }
            },
            "$defs": {
                "Item": { "type": "string", "minLength": 1 }
            }
        });
        let out = sanitize_schema(&schema);
        assert_eq!(out["properties"]["item"]["type"], "string");
        assert_eq!(out["properties"]["item"]["description"], "the item");
        assert!(out.get("$defs").is_none());
    }

    #[test]
    fn schema_cyclic_ref_degrades_to_object() {
        let schema = json!({
            "type": "object",
            "properties": { "node": { "$ref": "#/$defs/Node" } },
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": { "child": { "$ref": "#/$defs/Node" } }
                }
            }
        });
        let out = sanitize_schema(&schema);
        // The cycle bottoms out at the depth cap into {"type":"object"}.
        let mut node = &out["properties"]["node"];
        for _ in 0..REF_INLINE_DEPTH {
            match node.get("properties").and_then(|p| p.get("child")) {
                Some(child) => node = child,
                None => break,
            }
        }
        assert_eq!(node["type"], "object");
    }

    #[test]
    fn schema_unresolvable_ref_degrades_to_object() {
        let schema = json!({ "$ref": "#/definitions/Missing" });
        let out = sanitize_schema(&schema);
        assert_eq!(out, json!({"type": "object"}));
    }

    #[test]
    fn schema_const_becomes_enum() {
        let out = sanitize_schema(&json!({"const": "fixed"}));
        assert_eq!(out, json!({"enum": ["fixed"]}));
    }

    #[test]
    fn schema_nullable_anyof_flattens() {
        let schema = json!({
            "description": "maybe a string",
            "anyOf": [ {"type": "string", "maxLength": 5}, {"type": "null"} ]
        });
        let out = sanitize_schema(&schema);
        assert_eq!(out["type"], "string");
        assert_eq!(out["maxLength"], 5);
        assert_eq!(out["description"], "maybe a string");
        assert!(out.get("anyOf").is_none());
    }

    #[test]
    fn schema_multi_branch_anyof_kept() {
        let schema = json!({
            "anyOf": [ {"type": "string"}, {"type": "integer"} ]
        });
        let out = sanitize_schema(&schema);
        assert_eq!(out["anyOf"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn schema_single_allof_flattens() {
        let out = sanitize_schema(&json!({"allOf": [ {"type": "integer"} ]}));
        assert_eq!(out, json!({"type": "integer"}));
    }

    #[test]
    fn schema_draft4_exclusive_bounds_dropped() {
        let out = sanitize_schema(&json!({
            "type": "number", "minimum": 0, "exclusiveMinimum": true
        }));
        assert_eq!(out, json!({"type": "number", "minimum": 0}));
    }

    #[test]
    fn schema_numeric_exclusive_bounds_kept() {
        let out = sanitize_schema(&json!({"type": "number", "exclusiveMinimum": 0}));
        assert_eq!(out, json!({"type": "number", "exclusiveMinimum": 0}));
    }

    #[test]
    fn schema_benign_keywords_pass_through() {
        let schema = json!({
            "type": "object",
            "properties": { "q": {"type": "string", "description": "query"} },
            "required": ["q"],
            "additionalProperties": false
        });
        assert_eq!(sanitize_schema(&schema), schema);
    }
}

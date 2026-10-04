//! The OpenAPI 3.1 document of the REST API, generated from the two things
//! it describes: the route table ([`super::ROUTES`]) and the protos (the
//! descriptor set, with their comments as descriptions). The server serves
//! it at `/v1/openapi.json`; `documentation/api/openapi.json` is a copy that
//! a test keeps equal to it.
//!
//! Schemas follow the proto3 JSON mapping that pbjson implements:
//! lowerCamelCase names, 64-bit integers as decimal strings, enums by name,
//! bytes as base64, floats as numbers or `"NaN"` / `"Infinity"` /
//! `"-Infinity"`, and maps as objects. `Value`, `Expr` and `Pattern` are the
//! core's JSON form (`super::json`), written out here.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use prost::Message;
use prost_types::field_descriptor_proto::{Label, Type};
use prost_types::{DescriptorProto, FileDescriptorSet, MethodDescriptorProto};
use serde_json::{Map, Value as Json, json};

use super::{CHANGES_PARAMETERS, EVENT_STREAM, Input, NDJSON, OPTION_PARAMETERS, ROUTES, Route};
use crate::proto::DESCRIPTORS;

const PACKAGE: &str = "ironweaver_db.v1";

/// The document, as pretty JSON with a final newline.
pub fn document() -> &'static str {
    static DOCUMENT: OnceLock<String> = OnceLock::new();
    DOCUMENT.get_or_init(|| {
        let mut text = serde_json::to_string_pretty(&generate()).unwrap_or_default();
        text.push('\n');
        text
    })
}

/// The protos: messages, enums and RPCs by name, with their comments.
#[derive(Default)]
struct Protos {
    messages: BTreeMap<String, (DescriptorProto, Comments)>,
    enums: BTreeMap<String, (Vec<(String, String)>, String)>,
    rpcs: BTreeMap<String, (MethodDescriptorProto, String)>,
}

/// A message's comment and its fields'.
#[derive(Default)]
struct Comments {
    message: String,
    fields: BTreeMap<i32, String>,
}

impl Protos {
    fn load() -> Protos {
        let mut protos = Protos::default();
        // The bytes are the build script's encoding: they decode
        let Ok(set) = FileDescriptorSet::decode(DESCRIPTORS) else { return protos };
        for file in set.file {
            let mut comments: BTreeMap<Vec<i32>, String> = BTreeMap::new();
            for location in file.source_code_info.iter().flat_map(|info| &info.location) {
                if let Some(text) = &location.leading_comments {
                    comments.insert(location.path.clone(), clean(text));
                }
            }
            let comment = |path: &[i32]| comments.get(path).cloned().unwrap_or_default();
            for (m, message) in file.message_type.iter().enumerate() {
                let m = m as i32;
                let fields = (0..message.field.len() as i32).map(|f| (f, comment(&[4, m, 2, f]))).collect();
                let name = message.name().to_owned();
                protos.messages.insert(name, (message.clone(), Comments { message: comment(&[4, m]), fields }));
            }
            for (e, enumeration) in file.enum_type.iter().enumerate() {
                let e = e as i32;
                let values = enumeration
                    .value
                    .iter()
                    .enumerate()
                    .map(|(v, value)| (value.name().to_owned(), comment(&[5, e, 2, v as i32])));
                protos.enums.insert(enumeration.name().to_owned(), (values.collect(), comment(&[5, e])));
            }
            for (s, service) in file.service.iter().enumerate() {
                for (r, rpc) in service.method.iter().enumerate() {
                    protos.rpcs.insert(rpc.name().to_owned(), (rpc.clone(), comment(&[6, s as i32, 2, r as i32])));
                }
            }
        }
        protos
    }
}

/// A comment's text: one leading space per line removed.
fn clean(text: &str) -> String {
    text.lines().map(|l| l.strip_prefix(' ').unwrap_or(l).trim_end()).collect::<Vec<_>>().join("\n").trim().to_owned()
}

/// `.ironweaver_db.v1.Node` → `Node`.
fn short(type_name: &str) -> &str {
    type_name.rsplit('.').next().unwrap_or(type_name)
}

fn reference(name: &str) -> Json {
    json!({ "$ref": format!("#/components/schemas/{}", name) })
}

fn with_description(mut schema: Json, description: &str) -> Json {
    if !description.is_empty()
        && let Some(object) = schema.as_object_mut()
    {
        object.insert("description".into(), description.into());
    }
    schema
}

/// The JSON names of a message's fields, as pbjson writes them.
fn json_name(field: &prost_types::FieldDescriptorProto) -> String {
    match &field.json_name {
        Some(name) if !name.is_empty() => name.clone(),
        _ => {
            let mut out = String::new();
            let mut upper = false;
            for c in field.name().chars() {
                if c == '_' {
                    upper = true;
                } else if upper {
                    out.extend(c.to_uppercase());
                    upper = false;
                } else {
                    out.push(c);
                }
            }
            out
        }
    }
}

impl Protos {
    /// The schema of a field's value (one item, for repeated fields).
    fn scalar(&self, owner: &DescriptorProto, field: &prost_types::FieldDescriptorProto) -> Json {
        match field.r#type() {
            Type::Double | Type::Float => reference("Double"),
            Type::Int64 | Type::Sint64 | Type::Sfixed64 => reference("Int64"),
            Type::Uint64 | Type::Fixed64 => reference("Uint64"),
            Type::Int32 | Type::Sint32 | Type::Sfixed32 => json!({ "type": "integer", "format": "int32" }),
            Type::Uint32 | Type::Fixed32 => {
                json!({ "type": "integer", "format": "int64", "minimum": 0, "maximum": u32::MAX })
            }
            Type::Bool => json!({ "type": "boolean" }),
            Type::String => json!({ "type": "string" }),
            Type::Bytes => json!({ "type": "string", "contentEncoding": "base64" }),
            Type::Enum => reference(short(field.type_name())),
            Type::Message | Type::Group => {
                let name = short(field.type_name());
                match owner
                    .nested_type
                    .iter()
                    .find(|n| n.name() == name && n.options.as_ref().is_some_and(|o| o.map_entry()))
                {
                    // A map: the entry's `value` field is the map's values
                    Some(entry) => {
                        let value = entry.field.iter().find(|f| f.number() == 2);
                        let values = value.map_or(json!({}), |v| self.scalar(entry, v));
                        json!({ "type": "object", "additionalProperties": values })
                    }
                    None => reference(name),
                }
            }
        }
    }

    fn message_schema(&self, message: &DescriptorProto, comments: &Comments) -> Json {
        let mut properties = Map::new();
        for (i, field) in message.field.iter().enumerate() {
            let item = self.scalar(message, field);
            let is_map = item.get("additionalProperties").is_some();
            let schema = if field.label() == Label::Repeated && !is_map {
                json!({ "type": "array", "items": item })
            } else {
                item
            };
            let description = comments.fields.get(&(i as i32)).map_or("", String::as_str);
            // A `$ref` can't carry siblings in every tool: wrap it
            let schema = if schema.get("$ref").is_some() && !description.is_empty() {
                json!({ "allOf": [schema], "description": description })
            } else {
                with_description(schema, description)
            };
            properties.insert(json_name(field), schema);
        }
        // Real oneofs (proto3 `optional` makes a synthetic one per field)
        let mut description = comments.message.clone();
        for (i, oneof) in message.oneof_decl.iter().enumerate() {
            let members: Vec<String> = message
                .field
                .iter()
                .filter(|f| f.oneof_index == Some(i as i32) && !f.proto3_optional())
                .map(|f| format!("`{}`", json_name(f)))
                .collect();
            if !members.is_empty() {
                let sentence = format!("At most one of {} (`{}`).", members.join(", "), oneof.name());
                description =
                    if description.is_empty() { sentence } else { format!("{}\n\n{}", description, sentence) };
            }
        }
        with_description(json!({ "type": "object", "properties": properties }), &description)
    }

    fn schemas(&self) -> Map<String, Json> {
        let mut schemas = Map::new();
        for (name, (message, comments)) in &self.messages {
            let schema = match name.as_str() {
                "Value" => value_schema(),
                "Expr" => expr_schema(),
                "Pattern" => pattern_schema(),
                _ => self.message_schema(message, comments),
            };
            let schema =
                if schema.get("description").is_none() { with_description(schema, &comments.message) } else { schema };
            schemas.insert(name.clone(), schema);
        }
        for (name, (values, comment)) in &self.enums {
            let mut description = comment.clone();
            for (value, text) in values.iter().filter(|(_, t)| !t.is_empty()) {
                description.push_str(&format!("\n\n- `{}`: {}", value, text.replace('\n', " ")));
            }
            let names: Vec<&str> = values.iter().map(|(v, _)| v.as_str()).collect();
            schemas
                .insert(name.clone(), with_description(json!({ "type": "string", "enum": names }), description.trim()));
        }
        schemas.insert(
            "Double".into(),
            json!({
                "description": "A floating-point number; NaN and the infinities as strings.",
                "oneOf": [{ "type": "number" }, { "type": "string", "enum": ["NaN", "Infinity", "-Infinity"] }],
            }),
        );
        schemas.insert(
            "Int64".into(),
            json!({
                "description": "A 64-bit integer as a decimal string (a JSON number is accepted too).",
                "type": "string",
                "pattern": "^-?[0-9]+$",
            }),
        );
        schemas.insert(
            "Uint64".into(),
            json!({
                "description": "An unsigned 64-bit integer as a decimal string (a JSON number is accepted too).",
                "type": "string",
                "pattern": "^[0-9]+$",
            }),
        );
        schemas
    }

    fn operation(&self, route: &Route) -> Json {
        let mut operation = Map::new();
        operation.insert("operationId".into(), route.operation.into());
        operation.insert("summary".into(), route.summary.into());
        let mut parameters = Vec::new();
        for name in ["ns", "id"].into_iter().filter(|p| route.path.contains(&format!("{{{}}}", p))) {
            let (schema, description) = match (name, route.rpc) {
                ("ns", _) => (json!({ "type": "string" }), "The namespace."),
                (_, Some("GetEdges")) => (json!({ "type": "string", "pattern": "^[0-9]+$" }), "The edge's id."),
                _ => (json!({ "type": "string" }), "The node's id (percent-encoded)."),
            };
            parameters.push(
                json!({ "name": name, "in": "path", "required": true, "schema": schema, "description": description }),
            );
        }
        let query = match route.input {
            Input::Options => OPTION_PARAMETERS.iter().collect(),
            Input::Changes { stream } => {
                CHANGES_PARAMETERS.iter().filter(|(name, ..)| !stream || *name != "wait").collect()
            }
            _ => Vec::new(),
        };
        for (name, kind, description) in query {
            parameters
                .push(json!({ "name": name, "in": "query", "schema": { "type": kind }, "description": description }));
        }
        if route.input == (Input::Changes { stream: true }) {
            parameters.push(json!({
                "name": "Last-Event-ID",
                "in": "header",
                "schema": { "type": "string", "pattern": "^[0-9]+$" },
                "description": "Resume after this seq (what `EventSource` sends when it reconnects); \
                                overrides `from_seq`.",
            }));
        }
        if !parameters.is_empty() {
            operation.insert("parameters".into(), parameters.into());
        }
        let error = |kind: &str| {
            json!({
                "description": format!("{}: the HTTP status of its code (documentation/api/errors.md).", kind),
                "content": { "application/json": { "schema": reference("Error") } },
            })
        };
        if route.health() {
            let health = json!({ "application/json": { "schema": reference("Health") } });
            let mut responses = json!({ "200": { "description": "The server's state.", "content": health } });
            let comment = self.messages.get("Health").map(|(_, c)| c.message.clone()).unwrap_or_default();
            if !comment.is_empty() {
                operation.insert("description".into(), comment.into());
            }
            if route.operation == "ready" {
                responses["503"] =
                    json!({ "description": "Not ready: recovering or shutting down.", "content": health });
            }
            operation.insert("responses".into(), responses);
            return operation.into();
        }
        let Some((rpc, comment)) = route.rpc.and_then(|r| self.rpcs.get(r)) else {
            let document = json!({ "application/json": { "schema": { "type": "object" } } });
            operation.insert(
                "responses".into(),
                json!({
                    "200": { "description": "The OpenAPI document.", "content": document },
                    "4XX": error("A request the server refuses"),
                    "5XX": error("A failure of the server"),
                }),
            );
            return operation.into();
        };
        let request = short(rpc.input_type());
        let description = if comment.is_empty() {
            self.messages.get(request).map(|(_, c)| c.message.clone()).unwrap_or_default()
        } else {
            comment.clone()
        };
        if !description.is_empty() {
            operation.insert("description".into(), description.into());
        }
        if let Input::Body { required } = route.input {
            operation.insert(
                "requestBody".into(),
                json!({ "required": required, "content": { "application/json": { "schema": reference(request) } } }),
            );
        }
        let response = reference(short(rpc.output_type()));
        let mut content = json!({ "application/json": { "schema": response } });
        let mut answer = "The answer.".to_owned();
        if route.input == (Input::Changes { stream: true }) {
            content = json!({ EVENT_STREAM: { "schema": { "type": "string" } } });
            answer = "Server-Sent Events: a `change` event per commit, its `id` the seq and its `data` a \
                      `ChangeEvent` in JSON; a comment line as heartbeat; an `error` event with an `Error` \
                      before the stream ends on an error."
                .to_owned();
        } else if rpc.server_streaming() && route.input != Input::Options {
            content[NDJSON] = json!({ "schema": response });
            answer.push_str(
                " With `Accept: application/x-ndjson`, the answer's chunks, one per line, `meta` in the last; \
                 otherwise one message.",
            );
        }
        operation.insert(
            "responses".into(),
            json!({
                "200": { "description": answer, "content": content },
                "4XX": error("A request the server refuses"),
                "5XX": error("A failure of the server"),
            }),
        );
        operation.into()
    }
}

/// `Value`: the core's serde form, externally tagged.
fn value_schema() -> Json {
    let value = reference("Value");
    let one = |tag: &str, schema: Json| json!({ "type": "object", "properties": { tag: schema }, "required": [tag], "additionalProperties": false });
    json!({
        "description": "An attribute or meta value: the core's `Value` in its serde JSON form, for example `{\"Int\": 30}`, \
            `{\"String\": \"ann\"}` or `\"None\"`. Lists and dicts nest at most 100 levels.",
        "oneOf": [
            one("String", json!({ "type": "string" })),
            one("Int", json!({ "type": "integer", "format": "int64" })),
            one("Float", reference("Double")),
            one("Half", json!({ "type": "number", "description": "Stored at half precision." })),
            one("Bool", json!({ "type": "boolean" })),
            { "const": "None" },
            one("List", json!({ "type": "array", "items": value })),
            one("Dict", json!({ "type": "object", "additionalProperties": value })),
            one("Bytes", json!({ "type": "string", "contentEncoding": "base64" })),
            one("Date", json!({ "type": "string", "format": "date" })),
            one("DateTime", json!({ "type": "string", "description": "ISO 8601, with or without a UTC offset." })),
        ],
    })
}

/// `Expr`: the core's serde form, externally tagged.
fn expr_schema() -> Json {
    let expr = reference("Expr");
    let path = json!({ "type": "array", "items": { "type": "string" }, "description": "An attribute name, then keys into nested dicts." });
    let one = |tag: &str, schema: Json| json!({ "type": "object", "properties": { tag: schema }, "required": [tag], "additionalProperties": false });
    let fields = |properties: Json, required: Json| json!({ "type": "object", "properties": properties, "required": required, "additionalProperties": false });
    json!({
        "description": "A filter over a node or an edge: the core's `Expr` in its serde JSON form, for example \
            `{\"Label\": \"Person\"}` or `{\"Compare\": {\"path\": [\"age\"], \"op\": \"Ge\", \"value\": {\"Int\": 18}}}`. \
            `And`, `Or` and `Not` nest at most 100 levels.",
        "oneOf": [
            one("Const", json!({ "type": "boolean" })),
            one("Compare", fields(
                json!({ "path": path, "op": { "type": "string", "enum": ["Eq", "Ne", "Lt", "Le", "Gt", "Ge"] }, "value": reference("Value") }),
                json!(["path", "op", "value"]),
            )),
            one("In", fields(json!({ "path": path, "values": { "type": "array", "items": reference("Value") } }), json!(["path", "values"]))),
            one("Exists", fields(json!({ "path": path }), json!(["path"]))),
            one("Label", json!({ "type": "string" })),
            one("Type", json!({ "type": "string" })),
            one("And", json!({ "type": "array", "items": expr })),
            one("Or", json!({ "type": "array", "items": expr })),
            one("Not", expr),
        ],
    })
}

/// `Pattern`: its text, or the core's serde form.
fn pattern_schema() -> Json {
    json!({
        "description": "A graph pattern: the core's Cypher-like text, for example `(a:Person {age: 30})-[:knows*1..3]->(b)`, \
            or, for patterns the text can't express (filters other than property equality, bound ids), the core's \
            `Pattern` in its serde JSON form (an object with `nodes` and `edges`).",
        "oneOf": [
            { "type": "string" },
            { "type": "object", "properties": { "nodes": { "type": "array" }, "edges": { "type": "array" } }, "required": ["nodes", "edges"] },
        ],
    })
}

/// The schema names `value` refers to.
fn references(value: &Json, out: &mut Vec<String>) {
    match value {
        Json::Object(map) => {
            for (key, v) in map {
                match (key.as_str(), v.as_str().and_then(|r| r.strip_prefix("#/components/schemas/"))) {
                    ("$ref", Some(name)) => out.push(name.to_owned()),
                    _ => references(v, out),
                }
            }
        }
        Json::Array(items) => items.iter().for_each(|v| references(v, out)),
        _ => {}
    }
}

/// The schemas the paths reach (requests of routes without a body, such
/// as `ListNamespacesRequest`, are left out).
fn reachable(paths: &Json, mut schemas: Map<String, Json>) -> Map<String, Json> {
    let mut todo = Vec::new();
    references(paths, &mut todo);
    let mut out = Map::new();
    while let Some(name) = todo.pop() {
        if let Some(schema) = schemas.remove(&name) {
            references(&schema, &mut todo);
            out.insert(name, schema);
        }
    }
    out
}

/// The document as JSON.
pub fn generate() -> Json {
    let protos = Protos::load();
    let mut paths: BTreeMap<&str, Map<String, Json>> = BTreeMap::new();
    for route in ROUTES {
        paths
            .entry(route.path)
            .or_default()
            .insert(route.method.as_str().to_ascii_lowercase(), protos.operation(route));
    }
    let paths = json!(paths);
    let schemas = reachable(&paths, protos.schemas());
    json!({
        "openapi": "3.1.0",
        // No authentication yet (step 15 adds it)
        "security": [],
        "info": {
            "title": "Ironweaver DB REST API",
            "version": "v1",
            "license": { "name": "AGPL-3.0-only (or a commercial license)", "identifier": "AGPL-3.0-only" },
            "description": format!(
                "The `Database` trait over HTTP/JSON, with the messages of the gRPC contract (`proto/{}`) in their \
                 proto3 JSON form. See documentation/api/rest.md.",
                PACKAGE.replace('.', "/")
            ),
        },
        "servers": [{ "url": "http://127.0.0.1:7600" }],
        "paths": paths,
        "components": { "schemas": schemas },
    })
}

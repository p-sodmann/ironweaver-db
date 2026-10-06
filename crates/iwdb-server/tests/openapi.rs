//! The OpenAPI document (ADR 0030): every reference resolves and every
//! schema is used, it describes exactly the routes the router serves
//! (`rest::ROUTES`, from which the router is built), every RPC has a route,
//! and `documentation/api/openapi.json` is the document the server serves.
//! CI validates that copy against OpenAPI 3.1 with Redocly (`redocly.yaml`).
//!
//! After changing the protos or the routes, regenerate the copy with
//! `IWDB_BLESS=1 cargo test -p iwdb-server --test openapi`, and validate it
//! with `npx @redocly/cli lint documentation/api/openapi.json`.

#![cfg(feature = "rest")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::Path;

use iwdb_server::rest::{Input, ROUTES, openapi};
use serde_json::Value as Json;

fn document() -> Json {
    serde_json::from_str(openapi::document()).unwrap()
}

#[test]
fn the_documentation_holds_the_served_document() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../documentation/api/openapi.json");
    if std::env::var_os("IWDB_BLESS").is_some() {
        std::fs::write(&path, openapi::document()).unwrap();
    }
    let copy = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        copy == openapi::document(),
        "{} is out of date: run IWDB_BLESS=1 cargo test -p iwdb-server --test openapi",
        path.display()
    );
}

#[test]
fn the_document_is_openapi_3_1_with_an_operation_per_route() {
    let doc = document();
    assert_eq!(doc["openapi"], "3.1.0");
    assert!(doc["info"]["title"].is_string() && doc["info"]["version"].is_string());
    let operations: usize = doc["paths"].as_object().unwrap().values().map(|p| p.as_object().unwrap().len()).sum();
    assert_eq!(operations, ROUTES.len());
}

/// Every `$ref` in `value`.
fn references(value: &Json, out: &mut Vec<String>) {
    match value {
        Json::Object(map) => {
            for (key, v) in map {
                match (key.as_str(), v) {
                    ("$ref", Json::String(r)) => out.push(r.clone()),
                    _ => references(v, out),
                }
            }
        }
        Json::Array(items) => items.iter().for_each(|v| references(v, out)),
        _ => {}
    }
}

#[test]
fn every_reference_resolves_and_every_schema_is_used() {
    let doc = document();
    let schemas = doc["components"]["schemas"].as_object().unwrap();
    let mut refs = Vec::new();
    references(&doc, &mut refs);
    let mut used = BTreeSet::new();
    for r in &refs {
        let name =
            r.strip_prefix("#/components/schemas/").unwrap_or_else(|| panic!("a reference outside the schemas: {}", r));
        assert!(schemas.contains_key(name), "{} doesn't resolve", r);
        used.insert(name.to_owned());
    }
    let unused: Vec<_> = schemas.keys().filter(|k| !used.contains(*k)).collect();
    assert!(unused.is_empty(), "unused schemas: {:?}", unused);
}

#[test]
fn the_document_describes_exactly_the_routes() {
    let doc = document();
    let paths = doc["paths"].as_object().unwrap();
    let mut described = BTreeSet::new();
    for (path, item) in paths {
        for (method, op) in item.as_object().unwrap() {
            described.insert((method.to_uppercase(), path.clone()));
            // Every template parameter is declared
            let declared: BTreeSet<&str> = op["parameters"]
                .as_array()
                .map(|p| p.iter().filter(|p| p["in"] == "path").map(|p| p["name"].as_str().unwrap()).collect())
                .unwrap_or_default();
            let template: BTreeSet<&str> =
                path.split('/').filter_map(|s| s.strip_prefix('{').and_then(|s| s.strip_suffix('}'))).collect();
            assert_eq!(declared, template, "{} {}", method, path);
            assert!(op["responses"]["200"].is_object(), "{} {}", method, path);
        }
    }
    let routes: BTreeSet<(String, String)> = ROUTES.iter().map(|r| (r.method.to_string(), r.path.to_owned())).collect();
    assert_eq!(described, routes);
    for route in ROUTES {
        let op = &paths[route.path][route.method.as_str().to_lowercase()];
        assert_eq!(op["operationId"], route.operation);
        assert_eq!(op["requestBody"].is_object(), matches!(route.input, Input::Body { .. }), "{}", route.operation);
        if route.rpc.is_some() {
            for range in ["4XX", "5XX"] {
                assert!(
                    op["responses"][range]["content"]["application/json"]["schema"]["$ref"]
                        .as_str()
                        .is_some_and(|r| r.ends_with("/Error"))
                );
            }
        }
    }
}

#[test]
fn every_rpc_has_a_route() {
    let proto = [
        include_str!("../../../proto/ironweaver_db/v1/service.proto"),
        include_str!("../../../proto/ironweaver_db/v1/auth.proto"),
        include_str!("../../../proto/ironweaver_db/v1/admin.proto"),
    ]
    .concat();
    let rpcs: BTreeSet<&str> =
        proto.lines().filter_map(|l| l.trim().strip_prefix("rpc ")).filter_map(|l| l.split('(').next()).collect();
    let routed: BTreeSet<&str> = ROUTES.iter().filter_map(|r| r.rpc).collect();
    assert_eq!(rpcs.len(), 22 + 13 + 10);
    assert_eq!(routed, rpcs);
}

/// rest.md's route table lists exactly the routes.
#[test]
fn rest_md_lists_every_route() {
    let doc = include_str!("../../../documentation/api/rest.md");
    let documented: BTreeSet<(String, String)> = doc
        .lines()
        .skip_while(|l| !l.starts_with("## Routes"))
        .take_while(|l| !l.starts_with("## Requests"))
        .filter(|l| {
            l.starts_with("| GET") || l.starts_with("| POST") || l.starts_with("| PUT") || l.starts_with("| DELETE")
        })
        .map(|l| {
            let cells: Vec<&str> = l.trim_matches('|').split('|').map(str::trim).collect();
            (cells[0].to_owned(), cells[1].trim_matches('`').to_owned())
        })
        .collect();
    let routes: BTreeSet<(String, String)> = ROUTES.iter().map(|r| (r.method.to_string(), r.path.to_owned())).collect();
    assert_eq!(documented, routes);
}

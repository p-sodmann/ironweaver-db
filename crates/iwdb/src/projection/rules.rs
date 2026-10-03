//! [`Rules`]: the declarative [`Mapping`] of projection mode (ADR 0032),
//! read from the server's config (or any serde format).
//!
//! ```toml
//! [[rule]]
//! when = { kind = "customer_created" }
//! mutations = [
//!   { upsert_node = { id = "customer:${customer_id}", labels = ["Customer"], attr = { name = "${name}" } } },
//! ]
//! ```
//!
//! Every rule whose `when` matches applies, in order, and their mutations
//! form the event's transaction. `when` maps field paths (dots go into
//! dicts) to values: all must be equal (a missing field doesn't match).
//! No `when`: every event.
//!
//! In a template, a string that is exactly `${path}` becomes the field's
//! value, of any type; a string with `${path}` inside it is text with the
//! field's text in it (strings, integers, floats and booleans); `$$` is a
//! `$`. Other values (numbers, booleans, lists, tables) are themselves,
//! with their strings templated. A missing field is a [`MappingError`].
//! Rules are data, not code: no expressions, no conditions beyond
//! equality (design rule 5).

use std::collections::BTreeMap;
use std::fmt;

use ironweaver_core::{Attrs, Value};
use iwdb_engine::{EdgeKey, Mutation, Target};
use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};

use super::{Mapping, MappingError, SourceEvent};

/// A value with `${field}` placeholders, filled from an event.
#[derive(Clone, Debug, PartialEq)]
pub enum Template {
    /// `"${path}"`: the field's value.
    Field(Vec<String>),
    /// A string with placeholders in it.
    Text(Vec<Piece>),
    /// A value without placeholders.
    Value(Value),
    List(Vec<Template>),
    Dict(BTreeMap<String, Template>),
}

/// A part of a [`Template::Text`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    Literal(String),
    Field(Vec<String>),
}

impl Template {
    /// Parse a string: `${path}` alone is a field, placeholders inside
    /// text make a text, `$$` is `$`.
    pub fn parse(s: &str) -> Result<Template, String> {
        let mut pieces = Vec::new();
        let mut literal = String::new();
        let mut rest = s;
        while let Some(at) = rest.find('$') {
            literal.push_str(&rest[..at]);
            let after = &rest[at + 1..];
            if let Some(more) = after.strip_prefix('$') {
                literal.push('$');
                rest = more;
            } else if let Some(more) = after.strip_prefix('{') {
                let end = more.find('}').ok_or_else(|| format!("'{}' has a '${{' without its '}}'", s))?;
                let path = parse_path(&more[..end]).ok_or_else(|| format!("'{}' has an empty field path", s))?;
                if !literal.is_empty() {
                    pieces.push(Piece::Literal(std::mem::take(&mut literal)));
                }
                pieces.push(Piece::Field(path));
                rest = &more[end + 1..];
            } else {
                return Err(format!("'{}' has a '$' that is neither '${{field}}' nor '$$'", s));
            }
        }
        literal.push_str(rest);
        if !literal.is_empty() {
            pieces.push(Piece::Literal(literal));
        }
        Ok(match pieces.as_slice() {
            [] => Template::Value(Value::String(String::new())),
            [Piece::Field(path)] => Template::Field(path.clone()),
            [Piece::Literal(text)] => Template::Value(Value::String(text.clone())),
            _ => Template::Text(pieces),
        })
    }

    /// The value for `event`.
    pub fn fill(&self, event: &SourceEvent) -> Result<Value, MappingError> {
        Ok(match self {
            Template::Field(path) => field(event, path)?.clone(),
            Template::Text(pieces) => {
                let mut out = String::new();
                for piece in pieces {
                    match piece {
                        Piece::Literal(text) => out.push_str(text),
                        Piece::Field(path) => out.push_str(&text_of(field(event, path)?, path)?),
                    }
                }
                Value::String(out)
            }
            Template::Value(value) => value.clone(),
            Template::List(items) => Value::List(items.iter().map(|t| t.fill(event)).collect::<Result<_, _>>()?),
            Template::Dict(entries) => Value::Dict(
                entries.iter().map(|(k, t)| Ok((k.clone(), t.fill(event)?))).collect::<Result<_, MappingError>>()?,
            ),
        })
    }

    /// The text for `event`: a string, or an integer (ids are often
    /// numbers in the source).
    fn text(&self, event: &SourceEvent, what: &str) -> Result<String, MappingError> {
        match self.fill(event)? {
            Value::String(s) => Ok(s),
            Value::Int(i) => Ok(i.to_string()),
            other => Err(MappingError::new(format!("{} must be a string, not {:?}", what, other))),
        }
    }
}

fn parse_path(path: &str) -> Option<Vec<String>> {
    let keys: Vec<String> = path.split('.').map(str::to_owned).collect();
    (!keys.iter().any(String::is_empty)).then_some(keys)
}

fn field<'e>(event: &'e SourceEvent, path: &[String]) -> Result<&'e Value, MappingError> {
    let missing = || MappingError::new(format!("event {} has no field '{}'", event.position, path.join(".")));
    let (first, rest) = path.split_first().ok_or_else(missing)?;
    let mut value = event.fields.get(first).ok_or_else(missing)?;
    for key in rest {
        value = match value {
            Value::Dict(entries) => entries.get(key).ok_or_else(missing)?,
            _ => return Err(missing()),
        };
    }
    Ok(value)
}

fn text_of(value: &Value, path: &[String]) -> Result<String, MappingError> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Int(i) => Ok(i.to_string()),
        Value::Float(f) => Ok(f.to_string()),
        Value::Bool(b) => Ok(b.to_string()),
        other => Err(MappingError::new(format!("field '{}' ({:?}) can't be put in text", path.join("."), other))),
    }
}

impl<'de> Deserialize<'de> for Template {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_any(TemplateVisitor)
    }
}

struct TemplateVisitor;

impl<'de> Visitor<'de> for TemplateVisitor {
    type Value = Template;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a string, number, boolean, list or table")
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Template, E> {
        Ok(Template::Value(Value::Bool(v)))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Template, E> {
        Ok(Template::Value(Value::Int(v)))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Template, E> {
        i64::try_from(v).map(|v| Template::Value(Value::Int(v))).map_err(|_| E::custom(format!("{} is too large", v)))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Template, E> {
        Ok(Template::Value(Value::Float(v)))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Template, E> {
        Template::parse(v).map_err(E::custom)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Template, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element()? {
            items.push(item);
        }
        Ok(Template::List(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Template, A::Error> {
        let mut entries = BTreeMap::new();
        while let Some((key, value)) = map.next_entry::<String, Template>()? {
            entries.insert(key, value);
        }
        Ok(Template::Dict(entries))
    }
}

/// A mutation with templates, made for each event a rule matches.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RuleMutation {
    /// Create the node, or replace its attributes and meta; add the labels.
    UpsertNode {
        id: Template,
        #[serde(default)]
        labels: Vec<Template>,
        #[serde(default)]
        attr: BTreeMap<String, Template>,
        #[serde(default)]
        meta: BTreeMap<String, Template>,
    },
    /// Delete the node and its edges (it must exist).
    DeleteNode {
        id: Template,
    },
    /// Set one attribute of a node.
    SetAttr {
        node: Template,
        key: String,
        value: Template,
    },
    /// Remove one attribute of a node (nothing to remove is fine).
    RemoveAttr {
        node: Template,
        key: String,
    },
    /// Append to a list attribute of a node.
    AppendAttr {
        node: Template,
        key: String,
        value: Template,
    },
    AddLabel {
        node: Template,
        label: Template,
    },
    RemoveLabel {
        node: Template,
        label: Template,
    },
    /// Create the edge from `from` to `to` with this type, or replace its
    /// attributes and meta (both nodes must exist).
    UpsertEdge {
        from: Template,
        to: Template,
        #[serde(rename = "type", default)]
        ty: Option<Template>,
        #[serde(default)]
        attr: BTreeMap<String, Template>,
        #[serde(default)]
        meta: BTreeMap<String, Template>,
    },
}

impl RuleMutation {
    fn make(&self, event: &SourceEvent) -> Result<Mutation, MappingError> {
        let attrs = |map: &BTreeMap<String, Template>| -> Result<Attrs, MappingError> {
            map.iter().map(|(k, t)| Ok((k.clone(), t.fill(event)?))).collect()
        };
        let node = |t: &Template| t.text(event, "a node id");
        Ok(match self {
            RuleMutation::UpsertNode { id, labels, attr, meta } => Mutation::UpsertNode {
                id: node(id)?,
                labels: labels.iter().map(|l| l.text(event, "a label")).collect::<Result<_, _>>()?,
                attr: attrs(attr)?,
                meta: attrs(meta)?,
                expected_version: None,
            },
            RuleMutation::DeleteNode { id } => Mutation::DeleteNode { id: node(id)?, expected_version: None },
            RuleMutation::SetAttr { node: id, key, value } => Mutation::SetAttr {
                target: Target::Node(node(id)?),
                key: key.clone(),
                value: value.fill(event)?,
                expected_version: None,
            },
            RuleMutation::RemoveAttr { node: id, key } => {
                Mutation::RemoveAttr { target: Target::Node(node(id)?), key: key.clone(), expected_version: None }
            }
            RuleMutation::AppendAttr { node: id, key, value } => Mutation::AppendAttr {
                target: Target::Node(node(id)?),
                key: key.clone(),
                value: value.fill(event)?,
                expected_version: None,
            },
            RuleMutation::AddLabel { node: id, label } => {
                Mutation::AddLabel { id: node(id)?, label: label.text(event, "a label")?, expected_version: None }
            }
            RuleMutation::RemoveLabel { node: id, label } => {
                Mutation::RemoveLabel { id: node(id)?, label: label.text(event, "a label")?, expected_version: None }
            }
            RuleMutation::UpsertEdge { from, to, ty, attr, meta } => Mutation::UpsertEdge {
                key: EdgeKey::Endpoints {
                    from: node(from)?,
                    to: node(to)?,
                    ty: ty.as_ref().map(|t| t.text(event, "an edge type")).transpose()?,
                },
                attr: attrs(attr)?,
                meta: attrs(meta)?,
                expected_version: None,
            },
        })
    }
}

/// A rule: the mutations made for events that match `when`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// Field path (dotted) to value: all must be equal. Empty: every
    /// event.
    #[serde(default)]
    pub when: BTreeMap<String, Template>,
    pub mutations: Vec<RuleMutation>,
}

impl Rule {
    fn matches(&self, event: &SourceEvent) -> Result<bool, MappingError> {
        for (path, expected) in &self.when {
            let path = parse_path(path).ok_or_else(|| MappingError::new(format!("'{}' is not a field path", path)))?;
            match field(event, &path) {
                Ok(value) if *value == expected.fill(event)? => {}
                _ => return Ok(false),
            }
        }
        Ok(true)
    }
}

/// The declarative mapping: a list of [`Rule`]s (see the module docs).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(transparent)]
pub struct Rules(pub Vec<Rule>);

impl Mapping for Rules {
    fn map(&self, event: &SourceEvent) -> Result<Vec<Mutation>, MappingError> {
        let mut out = Vec::new();
        for rule in &self.0 {
            if rule.matches(event)? {
                for mutation in &rule.mutations {
                    out.push(mutation.make(event)?);
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(fields: Vec<(&str, Value)>) -> SourceEvent {
        SourceEvent { position: 7, fields: fields.into_iter().map(|(k, v)| (k.to_owned(), v)).collect() }
    }

    fn path(p: &str) -> Vec<String> {
        p.split('.').map(str::to_owned).collect()
    }

    #[test]
    fn strings_parse_into_fields_text_and_literals() {
        assert_eq!(Template::parse("${a.b}"), Ok(Template::Field(path("a.b"))));
        assert_eq!(Template::parse("plain"), Ok(Template::Value(Value::String("plain".into()))));
        assert_eq!(Template::parse("cost: $$5"), Ok(Template::Value(Value::String("cost: $5".into()))));
        assert_eq!(
            Template::parse("c:${id}!"),
            Ok(Template::Text(vec![Piece::Literal("c:".into()), Piece::Field(path("id")), Piece::Literal("!".into())]))
        );
        for bad in ["${", "${}", "${a..b}", "$x", "a$"] {
            assert!(Template::parse(bad).is_err(), "{}", bad);
        }
    }

    #[test]
    fn templates_fill_from_the_event() {
        let dict: std::collections::HashMap<String, Value> = [("city".to_owned(), Value::String("Oslo".into()))].into();
        let e = event(vec![("id", Value::Int(4)), ("n", Value::Float(1.5)), ("addr", Value::Dict(dict))]);
        let fill = |s: &str| Template::parse(s).unwrap().fill(&e);
        assert_eq!(fill("${id}"), Ok(Value::Int(4)));
        assert_eq!(fill("${addr.city}"), Ok(Value::String("Oslo".into())));
        assert_eq!(fill("p-${id}-${n}"), Ok(Value::String("p-4-1.5".into())));
        assert!(fill("${nope}").unwrap_err().message.contains("no field 'nope'"));
        assert!(fill("${id.x}").is_err());
        assert!(fill("x${addr}").unwrap_err().message.contains("can't be put in text"));
    }

    #[derive(Deserialize)]
    struct Config {
        rule: Rules,
    }

    #[test]
    fn rules_from_toml_map_matching_events_in_order() {
        let config: Config = toml::from_str(
            r#"
            [[rule]]
            when = { kind = "created" }
            mutations = [
              { upsert_node = { id = "c:${id}", labels = ["Customer"], attr = { name = "${name}", tags = ["a", "${kind}"], vip = false } } },
            ]

            [[rule]]
            mutations = [{ append_attr = { node = "log", key = "seen", value = "${id}" } }]

            [[rule]]
            when = { kind = "linked" }
            mutations = [{ upsert_edge = { from = "c:${id}", to = "c:${other}", type = "KNOWS", attr = { since = 2020 } } }]
            "#,
        )
        .unwrap();
        let created = event(vec![
            ("kind", Value::String("created".into())),
            ("id", Value::Int(1)),
            ("name", Value::String("Ann".into())),
        ]);
        let mutations = config.rule.map(&created).unwrap();
        assert_eq!(mutations.len(), 2);
        let Mutation::UpsertNode { id, labels, attr, .. } = &mutations[0] else { panic!("{:?}", mutations[0]) };
        assert_eq!((id.as_str(), labels.as_slice()), ("c:1", ["Customer".to_owned()].as_slice()));
        assert_eq!(attr.get("name"), Some(&Value::String("Ann".into())));
        assert_eq!(attr.get("vip"), Some(&Value::Bool(false)));
        let tags = Value::List(vec![Value::String("a".into()), Value::String("created".into())]);
        assert_eq!(attr.get("tags"), Some(&tags));
        assert!(matches!(&mutations[1], Mutation::AppendAttr { value: Value::Int(1), .. }));

        let linked =
            event(vec![("kind", Value::String("linked".into())), ("id", Value::Int(1)), ("other", Value::Int(2))]);
        let mutations = config.rule.map(&linked).unwrap();
        assert!(matches!(
            &mutations[1],
            Mutation::UpsertEdge { key: EdgeKey::Endpoints { from, to, ty: Some(ty) }, .. }
                if from == "c:1" && to == "c:2" && ty == "KNOWS"
        ));
        // `created` needs a name
        let nameless = event(vec![("kind", Value::String("created".into())), ("id", Value::Int(1))]);
        assert!(config.rule.map(&nameless).is_err());
        // No kind: only the second rule
        assert_eq!(config.rule.map(&event(vec![("id", Value::Int(3))])).unwrap().len(), 1);
    }

    #[test]
    fn unknown_mutations_and_fields_are_refused() {
        let parse = |s: &str| toml::from_str::<Config>(s).map(drop);
        assert!(
            parse(
                r#"[[rule]]
            mutations = [{ drop_table = { id = "x" } }]"#
            )
            .is_err()
        );
        assert!(
            parse(
                r#"[[rule]]
            mutations = [{ delete_node = { id = "x", force = true } }]"#
            )
            .is_err()
        );
        assert!(
            parse(
                r#"[[rule]]
            mutations = []
            unless = {}"#
            )
            .is_err()
        );
        assert!(
            parse(
                r#"[[rule]]
            mutations = [{ delete_node = { id = "${" } }]"#
            )
            .is_err()
        );
    }
}

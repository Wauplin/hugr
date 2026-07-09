//! JSON Schema → typed Python dataclasses (ARCHITECTURE §21.2, the Python
//! surface).
//!
//! Hugr validates model output *once*, on the Rust side: the response contract
//! casts the model's JSON into the agent's Rust type (`serde` + `schemars`)
//! before it is ever placed on `Answer.response`. Every language surface should
//! therefore be a thin, generated **typed-deserialization** layer over that
//! already-validated JSON — never a second validator. Re-validating per surface
//! would mean re-implementing the same schema in Python, then Kotlin, then TS…
//! exactly the duplication the narrow-waist rule (§14) rejects.
//!
//! This module turns the schemars JSON Schema (read from the built artifact's
//! `--config`, the single source of truth) into stdlib `@dataclass` types plus a
//! generated `_from_dict` that recursively *casts* JSON into typed objects. It
//! trusts Rust for correctness (it will not reject an extra or wrong-typed leaf)
//! — that is the point. Anything the generator does not understand degrades to
//! `Any`, so generation never fails on an exotic schema.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

/// The generated Python for one response schema: the (enum + dataclass) type
/// definitions and the name of the root response class.
pub struct Generated {
    /// The root response class name (e.g. `DocsResponse`), used as the type of
    /// `Answer.response`.
    pub root_class: String,
    /// Rendered Python: zero or more enum/dataclass definitions, no imports.
    pub code: String,
}

/// Generate typed Python model definitions from a response JSON Schema.
///
/// `fallback_root` names the root class when the schema carries no `title`.
/// When `schema` is not a usable object (e.g. schema extraction failed), the
/// root becomes a permissive `dict` alias so the surface still generates.
pub fn generate(schema: &Value, fallback_root: &str) -> Generated {
    let Some(obj) = schema.as_object() else {
        return Generated {
            root_class: fallback_root.to_string(),
            code: format!("{fallback_root} = dict  # unknown response schema\n"),
        };
    };

    let defs = obj
        .get("$defs")
        .or_else(|| obj.get("definitions"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let root_class = sanitize_class(
        obj.get("title")
            .and_then(Value::as_str)
            .unwrap_or(fallback_root),
    );

    // Enum vs object is decided up front so a `$ref` resolves to the right
    // constructor (`Enum(value)` vs `Cls._from_dict(...)`).
    let mut enum_names = BTreeSet::new();
    for (name, def) in &defs {
        if is_enum(def) {
            enum_names.insert(sanitize_class(name));
        }
    }

    let mut pygen = PyGen {
        enum_names,
        blocks: Vec::new(),
    };

    // `$defs` first (deterministic, sorted by BTreeMap), then the root — so the
    // root's fields can reference the named types defined above it.
    for (name, def) in &defs {
        let class = sanitize_class(name);
        pygen.emit_type(&class, def);
    }
    pygen.emit_dataclass(&root_class, obj);

    Generated {
        root_class,
        code: pygen.blocks.join("\n\n"),
    }
}

struct PyGen {
    enum_names: BTreeSet<String>,
    blocks: Vec<String>,
}

impl PyGen {
    /// Emit either an `Enum` or a `@dataclass` for a named `$def`.
    fn emit_type(&mut self, class: &str, schema: &Value) {
        if is_enum(schema) {
            self.emit_enum(class, schema);
        } else {
            let obj = schema.as_object().cloned().unwrap_or_default();
            self.emit_dataclass(class, &obj);
        }
    }

    fn emit_enum(&mut self, class: &str, schema: &Value) {
        let mut out = String::new();
        out.push_str(&format!("class {class}(str, Enum):\n"));
        if let Some(doc) = doc_line(schema) {
            out.push_str(&format!("    {doc}\n"));
        }
        let values = schema
            .get("enum")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut members = Vec::new();
        for value in &values {
            if let Some(s) = value.as_str() {
                let member = enum_member(s);
                out.push_str(&format!("    {member} = {}\n", py_str(s)));
                members.push(member);
            }
        }
        if members.is_empty() {
            out.push_str("    pass\n");
        }
        self.blocks.push(out.trim_end().to_string());
    }

    fn emit_dataclass(&mut self, class: &str, schema: &Map<String, Value>) {
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let required: BTreeSet<String> = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        // Required fields (no default) must precede optional ones (with a
        // default) — a dataclass rule. Within each group, keep the schema's
        // (sorted) property order for deterministic output.
        let mut fields: Vec<Field> = properties
            .iter()
            .map(|(key, prop)| self.field(key, prop, required.contains(key)))
            .collect();
        fields.sort_by_key(|f| f.optional); // false (required) before true

        let mut out = String::new();
        out.push_str(&format!("@dataclass\nclass {class}:\n"));
        if let Some(doc) = doc_line(&Value::Object(schema.clone())) {
            out.push_str(&format!("    {doc}\n"));
        }
        if fields.is_empty() {
            out.push_str("    pass\n");
        } else {
            for f in &fields {
                out.push_str(&format!("    {}: {}", f.py_name, f.annotation));
                if f.optional {
                    out.push_str(&format!(" = {}", f.default));
                }
                out.push('\n');
            }
            out.push('\n');
            out.push_str(&format!(
                "    @classmethod\n    def _from_dict(cls, d: dict) -> \"{class}\":\n        return cls(\n"
            ));
            for f in &fields {
                out.push_str(&format!("            {}={},\n", f.py_name, f.convert));
            }
            out.push_str("        )\n");
        }
        self.blocks.push(out.trim_end().to_string());
    }

    /// Build one dataclass field: its Python name, annotation, default, and the
    /// `_from_dict` conversion expression.
    fn field(&self, key: &str, prop: &Value, required: bool) -> Field {
        let ty = self.resolve(prop);
        let optional = !required || ty.is_optional();
        let inner = ty.strip_optional();
        let src = if required {
            format!("d[{}]", py_str(key))
        } else {
            format!("d.get({})", py_str(key))
        };
        let (annotation, convert) = if optional {
            (
                format!("Optional[{}]", inner.annotation()),
                format!("(None if {src} is None else {})", inner.convert(&src)),
            )
        } else {
            (inner.annotation(), inner.convert(&src))
        };
        Field {
            py_name: py_ident(key),
            annotation,
            default: "None".to_string(),
            convert,
            optional,
        }
    }

    /// Map a property schema to a [`PyType`]. Unknown shapes fall back to `Any`.
    fn resolve(&self, schema: &Value) -> PyType {
        let Some(obj) = schema.as_object() else {
            return PyType::Any;
        };

        if let Some(reference) = obj.get("$ref").and_then(Value::as_str) {
            let name = sanitize_class(reference.rsplit('/').next().unwrap_or(reference));
            return PyType::Ref {
                is_enum: self.enum_names.contains(&name),
                name,
            };
        }

        // `anyOf`/`oneOf`: the common serde shape is `[T, {"type":"null"}]` for
        // `Option<T>`. Anything richer degrades to Optional[...] or Any.
        for key in ["anyOf", "oneOf"] {
            if let Some(variants) = obj.get(key).and_then(Value::as_array) {
                let non_null: Vec<&Value> =
                    variants.iter().filter(|v| !is_null_schema(v)).collect();
                let has_null = variants.iter().any(is_null_schema);
                return match non_null.as_slice() {
                    [only] => {
                        let inner = self.resolve(only);
                        if has_null {
                            PyType::Optional(Box::new(inner))
                        } else {
                            inner
                        }
                    }
                    _ => {
                        if has_null {
                            PyType::Optional(Box::new(PyType::Any))
                        } else {
                            PyType::Any
                        }
                    }
                };
            }
        }

        // Inline string enum → Literal[...]; a named enum arrives via `$ref`.
        if is_enum(schema)
            && let Some(values) = obj.get("enum").and_then(Value::as_array)
        {
            let literals: Vec<String> = values
                .iter()
                .filter_map(|v| v.as_str().map(py_str))
                .collect();
            if !literals.is_empty() {
                return PyType::Literal(literals);
            }
        }

        match type_tag(obj) {
            Some(("string", nullable)) => wrap(nullable, PyType::Str),
            Some(("integer", nullable)) => wrap(nullable, PyType::Int),
            Some(("number", nullable)) => wrap(nullable, PyType::Float),
            Some(("boolean", nullable)) => wrap(nullable, PyType::Bool),
            Some(("array", nullable)) => {
                let items = obj
                    .get("items")
                    .map(|i| self.resolve(i))
                    .unwrap_or(PyType::Any);
                wrap(nullable, PyType::List(Box::new(items)))
            }
            Some(("object", nullable)) => {
                // A map (`additionalProperties: schema`) → Dict; an object with
                // inline `properties` but no `$ref` has no name to bind, so it
                // degrades to Dict[str, Any] rather than an anonymous class.
                let value_ty = match obj.get("additionalProperties") {
                    Some(Value::Object(_)) => self.resolve(&obj["additionalProperties"]),
                    _ => PyType::Any,
                };
                wrap(nullable, PyType::Dict(Box::new(value_ty)))
            }
            _ => PyType::Any,
        }
    }
}

struct Field {
    py_name: String,
    annotation: String,
    default: String,
    convert: String,
    optional: bool,
}

/// A Python type the generator can render and cast into.
enum PyType {
    Str,
    Int,
    Float,
    Bool,
    Any,
    List(Box<PyType>),
    Dict(Box<PyType>),
    Ref { name: String, is_enum: bool },
    Optional(Box<PyType>),
    Literal(Vec<String>),
}

impl PyType {
    fn is_optional(&self) -> bool {
        matches!(self, PyType::Optional(_))
    }

    fn strip_optional(&self) -> &PyType {
        match self {
            PyType::Optional(inner) => inner,
            other => other,
        }
    }

    fn annotation(&self) -> String {
        match self {
            PyType::Str => "str".to_string(),
            PyType::Int => "int".to_string(),
            PyType::Float => "float".to_string(),
            PyType::Bool => "bool".to_string(),
            PyType::Any => "Any".to_string(),
            PyType::List(inner) => format!("List[{}]", inner.annotation()),
            PyType::Dict(inner) => format!("Dict[str, {}]", inner.annotation()),
            PyType::Ref { name, .. } => name.clone(),
            PyType::Optional(inner) => format!("Optional[{}]", inner.annotation()),
            PyType::Literal(values) => format!("Literal[{}]", values.join(", ")),
        }
    }

    /// A Python expression casting the JSON value `src` into this type.
    fn convert(&self, src: &str) -> String {
        match self {
            PyType::Str | PyType::Int | PyType::Float | PyType::Bool | PyType::Any => {
                src.to_string()
            }
            PyType::Literal(_) => src.to_string(),
            PyType::List(inner) => {
                format!("[{} for _x in {src}]", inner.convert("_x"))
            }
            PyType::Dict(inner) => {
                format!(
                    "{{_k: {} for _k, _v in ({src}).items()}}",
                    inner.convert("_v")
                )
            }
            PyType::Ref { name, is_enum } => {
                if *is_enum {
                    format!("{name}({src})")
                } else {
                    format!("{name}._from_dict({src})")
                }
            }
            PyType::Optional(inner) => {
                format!("(None if {src} is None else {})", inner.convert(src))
            }
        }
    }
}

fn wrap(nullable: bool, ty: PyType) -> PyType {
    if nullable {
        PyType::Optional(Box::new(ty))
    } else {
        ty
    }
}

/// The primary JSON Schema `type`, plus whether `null` is also allowed
/// (`"type": ["string", "null"]`).
fn type_tag(obj: &Map<String, Value>) -> Option<(&'static str, bool)> {
    let known = |s: &str| -> Option<&'static str> {
        match s {
            "string" => Some("string"),
            "integer" => Some("integer"),
            "number" => Some("number"),
            "boolean" => Some("boolean"),
            "array" => Some("array"),
            "object" => Some("object"),
            _ => None,
        }
    };
    match obj.get("type")? {
        Value::String(s) => known(s).map(|t| (t, false)),
        Value::Array(items) => {
            let nullable = items.iter().any(|v| v.as_str() == Some("null"));
            let primary = items.iter().filter_map(|v| v.as_str()).find_map(known)?;
            Some((primary, nullable))
        }
        _ => None,
    }
}

fn is_null_schema(schema: &Value) -> bool {
    schema.get("type").and_then(Value::as_str) == Some("null")
}

fn is_enum(schema: &Value) -> bool {
    schema
        .get("enum")
        .and_then(Value::as_array)
        .map(|values| values.iter().all(Value::is_string))
        .unwrap_or(false)
}

/// A one-line Python docstring from a schema `description`, or `None`.
fn doc_line(schema: &Value) -> Option<String> {
    schema.get("description").and_then(Value::as_str).map(|d| {
        format!(
            "\"\"\"{}\"\"\"",
            d.replace('\\', "\\\\").replace('"', "\\\"")
        )
    })
}

/// A Python-safe, PascalCase-ish class name (keeps existing casing, strips
/// non-ident characters, prefixes a leading digit).
fn sanitize_class(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(true)
    {
        out.insert(0, '_');
    }
    out
}

/// A Python identifier for a field, escaping the handful of keywords that can
/// appear as serde field names.
fn py_ident(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = if cleaned
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(true)
    {
        format!("_{cleaned}")
    } else {
        cleaned
    };
    if is_py_keyword(&cleaned) {
        format!("{cleaned}_")
    } else {
        cleaned
    }
}

fn enum_member(value: &str) -> String {
    let upper: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    if upper
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(true)
    {
        format!("_{upper}")
    } else {
        upper
    }
}

fn is_py_keyword(s: &str) -> bool {
    matches!(
        s,
        "False"
            | "None"
            | "True"
            | "and"
            | "as"
            | "assert"
            | "async"
            | "await"
            | "break"
            | "class"
            | "continue"
            | "def"
            | "del"
            | "elif"
            | "else"
            | "except"
            | "finally"
            | "for"
            | "from"
            | "global"
            | "if"
            | "import"
            | "in"
            | "is"
            | "lambda"
            | "nonlocal"
            | "not"
            | "or"
            | "pass"
            | "raise"
            | "return"
            | "try"
            | "while"
            | "with"
            | "yield"
    )
}

/// A double-quoted, escaped Python string literal.
fn py_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn docs_response_schema_generates_typed_dataclass() {
        let schema = json!({
            "title": "DocsResponse",
            "type": "object",
            "description": "Final response payload produced by the docs agent.",
            "properties": {
                "response": {"type": "string"},
                "related_documents": {"type": "array", "items": {"$ref": "#/$defs/Document"}}
            },
            "required": ["response", "related_documents"],
            "$defs": {
                "Document": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "url": {"type": "string"}
                    },
                    "required": ["path", "url"]
                }
            }
        });
        let out = generate(&schema, "Response");
        assert_eq!(out.root_class, "DocsResponse");
        assert!(
            out.code.contains("@dataclass\nclass Document:"),
            "{}",
            out.code
        );
        assert!(out.code.contains("path: str"), "{}", out.code);
        assert!(out.code.contains("url: str"), "{}", out.code);
        assert!(
            out.code.contains("@dataclass\nclass DocsResponse:"),
            "{}",
            out.code
        );
        assert!(out.code.contains("response: str"), "{}", out.code);
        assert!(
            out.code.contains("related_documents: List[Document]"),
            "{}",
            out.code
        );
        assert!(
            out.code.contains(
                "related_documents=[Document._from_dict(_x) for _x in d[\"related_documents\"]]"
            ),
            "{}",
            out.code
        );
        assert!(
            out.code.contains("def _from_dict(cls, d: dict)"),
            "{}",
            out.code
        );
    }

    #[test]
    fn optional_and_nullable_fields_become_optional() {
        let schema = json!({
            "title": "Thing",
            "type": "object",
            "properties": {
                "always": {"type": "string"},
                "maybe": {"type": "string"},
                "nullable": {"type": ["integer", "null"]}
            },
            "required": ["always"]
        });
        let out = generate(&schema, "Thing");
        // Required field first, no default.
        assert!(out.code.contains("always: str\n"), "{}", out.code);
        // Not-required → Optional with default None.
        assert!(
            out.code.contains("maybe: Optional[str] = None"),
            "{}",
            out.code
        );
        assert!(
            out.code.contains("nullable: Optional[int] = None"),
            "{}",
            out.code
        );
        assert!(
            out.code
                .contains("maybe=(None if d.get(\"maybe\") is None else d.get(\"maybe\"))"),
            "{}",
            out.code
        );
    }

    #[test]
    fn nested_refs_and_enums_resolve_to_constructors() {
        let schema = json!({
            "title": "Root",
            "type": "object",
            "properties": {
                "kind": {"$ref": "#/$defs/Kind"},
                "child": {"$ref": "#/$defs/Child"},
                "children": {"type": "array", "items": {"$ref": "#/$defs/Child"}}
            },
            "required": ["kind", "child", "children"],
            "$defs": {
                "Kind": {"type": "string", "enum": ["a", "b"]},
                "Child": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                }
            }
        });
        let out = generate(&schema, "Root");
        assert!(out.code.contains("class Kind(str, Enum):"), "{}", out.code);
        assert!(out.code.contains("A = \"a\""), "{}", out.code);
        assert!(
            out.code.contains("@dataclass\nclass Child:"),
            "{}",
            out.code
        );
        // Enum ref → Enum(value); object ref → Cls._from_dict; list of refs maps.
        assert!(out.code.contains("kind=Kind(d[\"kind\"])"), "{}", out.code);
        assert!(
            out.code.contains("child=Child._from_dict(d[\"child\"])"),
            "{}",
            out.code
        );
        assert!(
            out.code
                .contains("children=[Child._from_dict(_x) for _x in d[\"children\"]]"),
            "{}",
            out.code
        );
        // Child ($def) is emitted before Root uses it.
        let child_at = out.code.find("class Child:").unwrap();
        let root_at = out.code.find("class Root:").unwrap();
        assert!(child_at < root_at, "defs must precede the root class");
    }

    #[test]
    fn map_and_inline_enum_and_unknown_degrade_sensibly() {
        let schema = json!({
            "title": "Bag",
            "type": "object",
            "properties": {
                "counts": {"type": "object", "additionalProperties": {"type": "integer"}},
                "mode": {"type": "string", "enum": ["fast", "slow"]},
                "weird": {"not": {}}
            },
            "required": ["counts", "mode"]
        });
        let out = generate(&schema, "Bag");
        assert!(out.code.contains("counts: Dict[str, int]"), "{}", out.code);
        assert!(
            out.code.contains("mode: Literal[\"fast\", \"slow\"]"),
            "{}",
            out.code
        );
        // Unknown, not required → Optional[Any].
        assert!(
            out.code.contains("weird: Optional[Any] = None"),
            "{}",
            out.code
        );
    }

    #[test]
    fn non_object_schema_degrades_to_dict_alias() {
        let out = generate(&Value::Bool(true), "Response");
        assert_eq!(out.root_class, "Response");
        assert!(out.code.contains("Response = dict"), "{}", out.code);
    }
}

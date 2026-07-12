use huggr_core::{ToolSchema, Value};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BrowserCapability {
    pub name: String,
    pub description: String,
    pub read_only: bool,
    pub schema: ToolSchema,
}

pub fn browser_tool_schemas() -> Vec<ToolSchema> {
    browser_capabilities()
        .into_iter()
        .map(|capability| capability.schema)
        .collect()
}

pub fn browser_capabilities() -> Vec<BrowserCapability> {
    vec![
        read_only(
            "tabs_list",
            "List browser tabs with id, title, URL, active status, and window id.",
            object(&[]),
        ),
        mutating(
            "tab_open_url",
            "Open a URL in a new or existing tab.",
            object(&[
                string("url", "The absolute http(s) URL to open."),
                optional(boolean("active", "Whether the tab should become active.")),
            ]),
        ),
        mutating(
            "tab_close",
            "Close a browser tab by id.",
            object(&[integer("tab_id", "The Chrome tab id to close.")]),
        ),
        mutating(
            "tab_switch",
            "Make a browser tab active by id.",
            object(&[integer("tab_id", "The Chrome tab id to activate.")]),
        ),
        mutating(
            "tab_reload",
            "Reload a browser tab.",
            object(&[integer("tab_id", "The Chrome tab id to reload.")]),
        ),
        mutating(
            "tab_back",
            "Navigate a tab backward in history.",
            object(&[integer("tab_id", "The Chrome tab id.")]),
        ),
        mutating(
            "tab_forward",
            "Navigate a tab forward in history.",
            object(&[integer("tab_id", "The Chrome tab id.")]),
        ),
        read_only(
            "wait_for_navigation",
            "Wait until a tab finishes navigation and the page is briefly settled.",
            object(&[
                integer("tab_id", "The Chrome tab id."),
                optional(integer(
                    "timeout_ms",
                    "Maximum time to wait in milliseconds.",
                )),
                optional(integer(
                    "settle_ms",
                    "How long the DOM should be quiet before returning.",
                )),
            ]),
        ),
        read_only(
            "wait_for_page_settled",
            "Wait until the current page DOM has been quiet briefly.",
            object(&[
                integer("tab_id", "The Chrome tab id."),
                optional(integer(
                    "settle_ms",
                    "How long the DOM should be quiet before returning.",
                )),
                optional(integer(
                    "timeout_ms",
                    "Maximum time to wait in milliseconds.",
                )),
            ]),
        ),
        read_only(
            "wait_for_tab_opened",
            "Wait until a new tab is opened.",
            object(&[optional(integer(
                "timeout_ms",
                "Maximum time to wait in milliseconds.",
            ))]),
        ),
        read_only(
            "wait_for_url",
            "Wait until a tab URL contains the expected text.",
            object(&[
                integer("tab_id", "The Chrome tab id."),
                string("contains", "Text that must appear in the URL."),
                optional(integer(
                    "timeout_ms",
                    "Maximum time to wait in milliseconds.",
                )),
            ]),
        ),
        read_only(
            "wait_for_selector",
            "Wait until a CSS selector exists in a tab.",
            object(&[
                integer("tab_id", "The Chrome tab id."),
                string("selector", "The CSS selector to wait for."),
                optional(integer(
                    "timeout_ms",
                    "Maximum time to wait in milliseconds.",
                )),
            ]),
        ),
        read_only(
            "wait_for_text",
            "Wait until visible page text contains the expected text.",
            object(&[
                integer("tab_id", "The Chrome tab id."),
                string("text", "Visible text to wait for."),
                optional(integer(
                    "timeout_ms",
                    "Maximum time to wait in milliseconds.",
                )),
            ]),
        ),
        read_only(
            "page_read_html",
            "Read the current page HTML from a tab.",
            object(&[integer("tab_id", "The Chrome tab id.")]),
        ),
        read_only(
            "page_read_text",
            "Read visible page text from a tab.",
            object(&[integer("tab_id", "The Chrome tab id.")]),
        ),
        read_only(
            "page_snapshot",
            "Return a compact DOM snapshot with stable node ids for visible actionable elements.",
            object(&[integer("tab_id", "The Chrome tab id.")]),
        ),
        mutating(
            "page_click",
            "Click an element by snapshot node id or CSS selector.",
            object(&[
                integer("tab_id", "The Chrome tab id."),
                optional(string(
                    "node_id",
                    "Preferred snapshot node id. Leave empty when using selector.",
                )),
                optional(string(
                    "selector",
                    "Fallback CSS selector. Leave empty when using node_id.",
                )),
            ]),
        ),
        mutating(
            "page_type",
            "Type text into an input-like element.",
            object(&[
                integer("tab_id", "The Chrome tab id."),
                optional(string(
                    "node_id",
                    "Preferred snapshot node id. Leave empty when using selector.",
                )),
                optional(string(
                    "selector",
                    "Fallback CSS selector. Leave empty when using node_id.",
                )),
                string("text", "Text to type."),
                optional(boolean("clear", "Whether to clear existing text first.")),
            ]),
        ),
        mutating(
            "page_select",
            "Select an option in a select element.",
            object(&[
                integer("tab_id", "The Chrome tab id."),
                optional(string(
                    "node_id",
                    "Preferred snapshot node id. Leave empty when using selector.",
                )),
                optional(string(
                    "selector",
                    "Fallback CSS selector. Leave empty when using node_id.",
                )),
                string("value", "Option value to select."),
            ]),
        ),
        mutating(
            "page_scroll",
            "Scroll a page by a pixel delta.",
            object(&[
                integer("tab_id", "The Chrome tab id."),
                optional(integer("delta_x", "Horizontal scroll delta in pixels.")),
                optional(integer("delta_y", "Vertical scroll delta in pixels.")),
            ]),
        ),
        mutating(
            "page_submit",
            "Submit a form element.",
            object(&[
                integer("tab_id", "The Chrome tab id."),
                optional(string(
                    "node_id",
                    "Preferred snapshot node id. Leave empty when using selector.",
                )),
                optional(string(
                    "selector",
                    "Fallback CSS selector. Leave empty when using node_id.",
                )),
            ]),
        ),
        mutating(
            "page_focus",
            "Focus an element.",
            object(&[
                integer("tab_id", "The Chrome tab id."),
                optional(string(
                    "node_id",
                    "Preferred snapshot node id. Leave empty when using selector.",
                )),
                optional(string(
                    "selector",
                    "Fallback CSS selector. Leave empty when using node_id.",
                )),
            ]),
        ),
        mutating(
            "file_download_url",
            "Download a URL into Huggr's extension-local file store.",
            object(&[
                string("url", "The absolute http(s) URL to download."),
                optional(string("filename", "Optional preferred filename.")),
            ]),
        ),
        read_only(
            "file_list_downloads",
            "List files downloaded into Huggr's extension-local file store.",
            object(&[]),
        ),
        read_only(
            "file_read_metadata",
            "Read metadata for a file in Huggr's extension-local file store.",
            object(&[string("file_id", "The local Huggr file id.")]),
        ),
        mutating(
            "file_upload_to_input",
            "Upload a Huggr-local file to a page file input.",
            object(&[
                integer("tab_id", "The Chrome tab id."),
                string("file_id", "The local Huggr file id to upload."),
                optional(string(
                    "node_id",
                    "Preferred snapshot node id. Leave empty when using selector.",
                )),
                optional(string(
                    "selector",
                    "Fallback CSS selector. Leave empty when using node_id.",
                )),
            ]),
        ),
        mutating(
            "file_delete",
            "Delete a file from Huggr's extension-local file store.",
            object(&[string("file_id", "The local Huggr file id.")]),
        ),
    ]
}

fn read_only(name: &str, description: &str, parameters: Value) -> BrowserCapability {
    capability(name, description, parameters, true)
}

fn mutating(name: &str, description: &str, parameters: Value) -> BrowserCapability {
    capability(name, description, parameters, false)
}

fn capability(
    name: &str,
    description: &str,
    parameters: Value,
    read_only: bool,
) -> BrowserCapability {
    BrowserCapability {
        name: name.to_string(),
        description: description.to_string(),
        read_only,
        schema: ToolSchema::new(name, description, parameters),
    }
}

fn object(fields: &[Value]) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for field in fields {
        let name = field
            .get("name")
            .and_then(Value::as_str)
            .expect("schema field has a name")
            .to_string();
        let mut schema = field.clone();
        let schema_object = schema.as_object_mut().expect("schema field is an object");
        schema_object.remove("name");
        let is_required = schema_object
            .remove("required")
            .and_then(|value| value.as_bool())
            .expect("schema field declares requiredness");
        if is_required {
            required.push(Value::String(name.clone()));
        }
        properties.insert(name, schema);
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn optional(mut field: Value) -> Value {
    field
        .as_object_mut()
        .expect("schema field is an object")
        .insert("required".to_string(), Value::Bool(false));
    field
}

fn string(name: &str, description: &str) -> Value {
    json!({ "name": name, "type": "string", "description": description, "required": true })
}

fn integer(name: &str, description: &str) -> Value {
    json!({ "name": name, "type": "integer", "description": description, "required": true })
}

fn boolean(name: &str, description: &str) -> Value {
    json!({ "name": name, "type": "boolean", "description": description, "required": true })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn required_fields(tool: &str) -> Vec<String> {
        let capabilities = browser_capabilities();
        let capability = capabilities
            .iter()
            .find(|capability| capability.name == tool)
            .unwrap_or_else(|| panic!("unknown tool: {tool}"));
        capability.schema.parameters["required"]
            .as_array()
            .expect("required is an array")
            .iter()
            .map(|name| {
                name.as_str()
                    .expect("required entry is a string")
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn required_matches_what_the_dispatcher_needs() {
        let expected: &[(&str, &[&str])] = &[
            ("tabs_list", &[]),
            ("tab_open_url", &["url"]),
            ("tab_close", &["tab_id"]),
            ("tab_switch", &["tab_id"]),
            ("tab_reload", &["tab_id"]),
            ("tab_back", &["tab_id"]),
            ("tab_forward", &["tab_id"]),
            ("wait_for_navigation", &["tab_id"]),
            ("wait_for_page_settled", &["tab_id"]),
            ("wait_for_tab_opened", &[]),
            ("wait_for_url", &["tab_id", "contains"]),
            ("wait_for_selector", &["tab_id", "selector"]),
            ("wait_for_text", &["tab_id", "text"]),
            ("page_read_html", &["tab_id"]),
            ("page_read_text", &["tab_id"]),
            ("page_snapshot", &["tab_id"]),
            ("page_click", &["tab_id"]),
            ("page_type", &["tab_id", "text"]),
            ("page_select", &["tab_id", "value"]),
            ("page_scroll", &["tab_id"]),
            ("page_submit", &["tab_id"]),
            ("page_focus", &["tab_id"]),
            ("file_download_url", &["url"]),
            ("file_list_downloads", &[]),
            ("file_read_metadata", &["file_id"]),
            ("file_upload_to_input", &["tab_id", "file_id"]),
            ("file_delete", &["file_id"]),
        ];
        assert_eq!(expected.len(), browser_capabilities().len());
        for (tool, required) in expected {
            assert_eq!(
                required_fields(tool),
                *required,
                "required mismatch for {tool}"
            );
        }
    }

    #[test]
    fn fields_with_defaults_or_alternatives_are_optional_but_still_declared() {
        let optional: &[(&str, &[&str])] = &[
            ("tab_open_url", &["active"]),
            ("wait_for_navigation", &["timeout_ms", "settle_ms"]),
            ("wait_for_page_settled", &["settle_ms", "timeout_ms"]),
            ("wait_for_tab_opened", &["timeout_ms"]),
            ("page_click", &["node_id", "selector"]),
            ("page_type", &["node_id", "selector", "clear"]),
            ("page_scroll", &["delta_x", "delta_y"]),
            ("file_download_url", &["filename"]),
            ("file_upload_to_input", &["node_id", "selector"]),
        ];
        let capabilities = browser_capabilities();
        for (tool, fields) in optional {
            let capability = capabilities
                .iter()
                .find(|capability| capability.name == *tool)
                .unwrap_or_else(|| panic!("unknown tool: {tool}"));
            let required = required_fields(tool);
            for field in *fields {
                assert!(
                    capability.schema.parameters["properties"]
                        .get(*field)
                        .is_some(),
                    "{tool} should declare property {field}"
                );
                assert!(
                    !required.contains(&field.to_string()),
                    "{tool} should not require {field}"
                );
            }
        }
    }

    #[test]
    fn field_helper_markers_never_leak_into_schemas() {
        for capability in browser_capabilities() {
            let properties = capability.schema.parameters["properties"]
                .as_object()
                .expect("properties is an object");
            for (field, schema) in properties {
                assert!(
                    schema.get("name").is_none() && schema.get("required").is_none(),
                    "{}.{field} leaks a builder marker",
                    capability.name
                );
            }
        }
    }
}

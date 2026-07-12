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
                boolean("active", "Whether the tab should become active."),
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
                integer("timeout_ms", "Maximum time to wait in milliseconds."),
                integer(
                    "settle_ms",
                    "How long the DOM should be quiet before returning.",
                ),
            ]),
        ),
        read_only(
            "wait_for_page_settled",
            "Wait until the current page DOM has been quiet briefly.",
            object(&[
                integer("tab_id", "The Chrome tab id."),
                integer(
                    "settle_ms",
                    "How long the DOM should be quiet before returning.",
                ),
                integer("timeout_ms", "Maximum time to wait in milliseconds."),
            ]),
        ),
        read_only(
            "wait_for_tab_opened",
            "Wait until a new tab is opened.",
            object(&[integer(
                "timeout_ms",
                "Maximum time to wait in milliseconds.",
            )]),
        ),
        read_only(
            "wait_for_url",
            "Wait until a tab URL contains the expected text.",
            object(&[
                integer("tab_id", "The Chrome tab id."),
                string("contains", "Text that must appear in the URL."),
                integer("timeout_ms", "Maximum time to wait in milliseconds."),
            ]),
        ),
        read_only(
            "wait_for_selector",
            "Wait until a CSS selector exists in a tab.",
            object(&[
                integer("tab_id", "The Chrome tab id."),
                string("selector", "The CSS selector to wait for."),
                integer("timeout_ms", "Maximum time to wait in milliseconds."),
            ]),
        ),
        read_only(
            "wait_for_text",
            "Wait until visible page text contains the expected text.",
            object(&[
                integer("tab_id", "The Chrome tab id."),
                string("text", "Visible text to wait for."),
                integer("timeout_ms", "Maximum time to wait in milliseconds."),
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
                string(
                    "node_id",
                    "Preferred snapshot node id. Leave empty when using selector.",
                ),
                string(
                    "selector",
                    "Fallback CSS selector. Leave empty when using node_id.",
                ),
            ]),
        ),
        mutating(
            "page_type",
            "Type text into an input-like element.",
            object(&[
                integer("tab_id", "The Chrome tab id."),
                string(
                    "node_id",
                    "Preferred snapshot node id. Leave empty when using selector.",
                ),
                string(
                    "selector",
                    "Fallback CSS selector. Leave empty when using node_id.",
                ),
                string("text", "Text to type."),
                boolean("clear", "Whether to clear existing text first."),
            ]),
        ),
        mutating(
            "page_select",
            "Select an option in a select element.",
            object(&[
                integer("tab_id", "The Chrome tab id."),
                string(
                    "node_id",
                    "Preferred snapshot node id. Leave empty when using selector.",
                ),
                string(
                    "selector",
                    "Fallback CSS selector. Leave empty when using node_id.",
                ),
                string("value", "Option value to select."),
            ]),
        ),
        mutating(
            "page_scroll",
            "Scroll a page by a pixel delta.",
            object(&[
                integer("tab_id", "The Chrome tab id."),
                integer("delta_x", "Horizontal scroll delta in pixels."),
                integer("delta_y", "Vertical scroll delta in pixels."),
            ]),
        ),
        mutating(
            "page_submit",
            "Submit a form element.",
            object(&[
                integer("tab_id", "The Chrome tab id."),
                string(
                    "node_id",
                    "Preferred snapshot node id. Leave empty when using selector.",
                ),
                string(
                    "selector",
                    "Fallback CSS selector. Leave empty when using node_id.",
                ),
            ]),
        ),
        mutating(
            "page_focus",
            "Focus an element.",
            object(&[
                integer("tab_id", "The Chrome tab id."),
                string(
                    "node_id",
                    "Preferred snapshot node id. Leave empty when using selector.",
                ),
                string(
                    "selector",
                    "Fallback CSS selector. Leave empty when using node_id.",
                ),
            ]),
        ),
        mutating(
            "file_download_url",
            "Download a URL into Huggr's extension-local file store.",
            object(&[
                string("url", "The absolute http(s) URL to download."),
                string("filename", "Optional preferred filename."),
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
                string(
                    "node_id",
                    "Preferred snapshot node id. Leave empty when using selector.",
                ),
                string(
                    "selector",
                    "Fallback CSS selector. Leave empty when using node_id.",
                ),
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
            .expect("schema field has a name");
        let mut schema = field.clone();
        schema
            .as_object_mut()
            .expect("schema field is an object")
            .remove("name");
        required.push(Value::String(name.to_string()));
        properties.insert(name.to_string(), schema);
    }
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

fn string(name: &str, description: &str) -> Value {
    json!({ "name": name, "type": "string", "description": description })
}

fn integer(name: &str, description: &str) -> Value {
    json!({ "name": name, "type": "integer", "description": description })
}

fn boolean(name: &str, description: &str) -> Value {
    json!({ "name": name, "type": "boolean", "description": description })
}

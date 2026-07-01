// The tools advertised to the model, as canonical `ToolSchema`s (name +
// description + JSON-schema parameters). This is data the brain forwards
// verbatim (ARCHITECTURE §2.4) — the model reads the descriptions, the host
// implements the behaviour in tools.js.
//
// Design choice (per the task): this build is **read + navigate only**. The
// agent can observe pages and move between/open/close tabs, but it CANNOT click
// elements or submit forms. There is deliberately no `click`/`type`/`submit`
// tool, so a page can never be mutated on the user's behalf.

const TAB_ID = {
  tab_id: {
    type: "integer",
    description: "The id of the tab (from list_tabs / get_current_page). Omit to use the active tab.",
  },
};

/** All tool schemas, in the order shown to the model. */
export const TOOL_SCHEMAS = [
  // --- Browser / tab tools ------------------------------------------------
  {
    name: "list_tabs",
    description:
      "List the user's open tabs (id, title, url, whether active). Use this first to discover tab ids.",
    parameters: { type: "object", properties: {}, additionalProperties: false },
  },
  {
    name: "get_current_page",
    description:
      "Get metadata for the currently active tab: id, title, url, and window id. A cheap 'where am I?'.",
    parameters: { type: "object", properties: {}, additionalProperties: false },
  },
  {
    name: "open_tab",
    description: "Open a NEW tab at the given URL. Requires permission.",
    parameters: {
      type: "object",
      properties: {
        url: { type: "string", description: "The URL to open (include the scheme, e.g. https://)." },
        active: {
          type: "boolean",
          description: "Whether to focus the new tab (default true).",
        },
      },
      required: ["url"],
      additionalProperties: false,
    },
  },
  {
    name: "navigate_tab",
    description: "Navigate an EXISTING tab to a new URL. Requires permission.",
    parameters: {
      type: "object",
      properties: {
        ...TAB_ID,
        url: { type: "string", description: "The URL to navigate to (include the scheme)." },
      },
      required: ["url"],
      additionalProperties: false,
    },
  },
  {
    name: "activate_tab",
    description: "Bring an existing tab to the foreground (focus it). Requires permission.",
    parameters: {
      type: "object",
      properties: { ...TAB_ID },
      required: ["tab_id"],
      additionalProperties: false,
    },
  },
  {
    name: "close_tab",
    description: "Close a tab by id. Requires permission.",
    parameters: {
      type: "object",
      properties: { ...TAB_ID },
      required: ["tab_id"],
      additionalProperties: false,
    },
  },

  // --- Page observation tools (read-only, no permission) ------------------
  {
    name: "get_page_text",
    description:
      "Extract the visible text of a page (its readable content). Use this to read/summarize a page.",
    parameters: {
      type: "object",
      properties: {
        ...TAB_ID,
        max_chars: {
          type: "integer",
          description: "Truncate to at most this many characters (default 8000).",
        },
      },
      additionalProperties: false,
    },
  },
  {
    name: "get_page_links",
    description:
      "List the hyperlinks on a page as {text, href} pairs. Use to find where to navigate next.",
    parameters: {
      type: "object",
      properties: {
        ...TAB_ID,
        max_links: { type: "integer", description: "Cap the number of links returned (default 100)." },
      },
      additionalProperties: false,
    },
  },
  {
    name: "get_page_outline",
    description:
      "Get the heading outline of a page as {level, text} entries (h1..h6), giving its structure at a glance.",
    parameters: {
      type: "object",
      properties: { ...TAB_ID },
      additionalProperties: false,
    },
  },
  {
    name: "wait_for_page",
    description:
      "Wait until a tab has finished loading before reading it. Resolves once the page's load " +
      "completes (optionally also until `selector` appears and a `settle_ms` quiet period passes). " +
      "The read tools already auto-wait briefly; use this for heavy or JS-rendered (SPA) pages, or " +
      "right after navigating. Returns {ready, timed_out, readyState, url, title}.",
    parameters: {
      type: "object",
      properties: {
        ...TAB_ID,
        timeout_ms: {
          type: "integer",
          description: "Max time to wait in milliseconds (default 15000).",
        },
        selector: {
          type: "string",
          description: "Optional CSS selector to wait for (e.g. a main content container).",
        },
        settle_ms: {
          type: "integer",
          description: "Extra quiet time to wait after ready, for late-rendering content (default 0).",
        },
      },
      additionalProperties: false,
    },
  },
  {
    name: "get_interactive_elements",
    description:
      "List the interactive elements on a page (links, buttons, inputs) as read-only descriptions — " +
      "useful to explain what a user COULD do here. This build cannot click or type; it only describes.",
    parameters: {
      type: "object",
      properties: {
        ...TAB_ID,
        max_elements: { type: "integer", description: "Cap the number returned (default 50)." },
      },
      additionalProperties: false,
    },
  },

  // --- Agent UX tools ------------------------------------------------------
  {
    name: "ask_user_confirmation",
    description:
      "Ask the user a yes/no question before doing something consequential. Returns {confirmed: bool}. " +
      "The `markdown` is shown to the user.",
    parameters: {
      type: "object",
      properties: {
        markdown: { type: "string", description: "The question / context to show the user (markdown)." },
      },
      required: ["markdown"],
      additionalProperties: false,
    },
  },
  {
    name: "show_plan",
    description:
      "Show the user a short numbered plan of what you intend to do. Purely informational; returns {shown: true}.",
    parameters: {
      type: "object",
      properties: {
        steps: {
          type: "array",
          items: { type: "string" },
          description: "The ordered plan steps.",
        },
      },
      required: ["steps"],
      additionalProperties: false,
    },
  },
];

/**
 * Capabilities that require a permission round-trip before running. These are
 * exactly the tools that CHANGE tab state (navigation, opening, closing,
 * focusing). Read-only observation tools and the UX tools run without a prompt.
 * This list becomes the brain's `permissioned` set (StaticPolicy), so the brain
 * emits `RequestPermission` for them (ARCHITECTURE §7.2).
 */
export const PERMISSIONED = ["open_tab", "navigate_tab", "activate_tab", "close_tab"];

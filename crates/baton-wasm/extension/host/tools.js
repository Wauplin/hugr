// The Chrome capabilities — the host-side implementations of the tools declared
// in schemas.js. Each is `async (args) => result`; a thrown error becomes a
// semantic CapabilityError the model can react to (ARCHITECTURE §5.4). The brain
// never interprets args or results — it only routes them.
//
// READ + NAVIGATE ONLY: there is deliberately no code path that clicks an
// element, types into a field, or submits a form.

/** Resolve the active tab of the focused window. */
async function activeTab() {
  const [tab] = await chrome.tabs.query({ active: true, lastFocusedWindow: true });
  if (!tab) throw new Error("no active tab");
  return tab;
}

/** Resolve a tab id from args.tab_id, falling back to the active tab. */
async function resolveTabId(args) {
  if (args && typeof args.tab_id === "number") return args.tab_id;
  return (await activeTab()).id;
}

/** Run a function in the page and return its single result value. */
async function execOnTab(tabId, func, funcArgs = []) {
  let results;
  try {
    results = await chrome.scripting.executeScript({
      target: { tabId },
      func,
      args: funcArgs,
    });
  } catch (e) {
    // Chrome refuses to inject into privileged pages (chrome://, the Web Store,
    // the new-tab page, PDF viewer). Turn that into a clear semantic error.
    throw new Error(
      `cannot read this page (${e.message}). It may be a chrome:// page, the Web Store, or a file the extension can't access.`,
    );
  }
  if (!results || !results[0]) throw new Error("no result from page");
  return results[0].result;
}

function summarizeTab(t) {
  return { id: t.id, title: t.title ?? "", url: t.url ?? "", active: !!t.active, window_id: t.windowId };
}

/**
 * Build the capability table. `ui` provides the two agent-UX tools with a way to
 * talk to the panel: `ui.confirm(markdown) -> Promise<bool>` and
 * `ui.showPlan(steps)`.
 */
export function createTools(ui) {
  return {
    // --- Browser / tab tools ---------------------------------------------
    async list_tabs() {
      const tabs = await chrome.tabs.query({});
      return { tabs: tabs.map(summarizeTab) };
    },

    async get_current_page() {
      return summarizeTab(await activeTab());
    },

    async open_tab(args) {
      if (!args || typeof args.url !== "string") throw new Error("open_tab requires a `url` string");
      const tab = await chrome.tabs.create({
        url: args.url,
        active: args.active !== false,
      });
      return { tab_id: tab.id, url: tab.pendingUrl ?? tab.url ?? args.url };
    },

    async navigate_tab(args) {
      if (!args || typeof args.url !== "string") throw new Error("navigate_tab requires a `url` string");
      const tabId = await resolveTabId(args);
      const tab = await chrome.tabs.update(tabId, { url: args.url });
      return { tab_id: tab.id, url: args.url };
    },

    async activate_tab(args) {
      const tabId = await resolveTabId(args);
      const tab = await chrome.tabs.update(tabId, { active: true });
      // Also focus the tab's window so it actually comes to the foreground.
      if (tab.windowId != null) {
        await chrome.windows.update(tab.windowId, { focused: true });
      }
      return { tab_id: tab.id, active: true };
    },

    async close_tab(args) {
      const tabId = await resolveTabId(args);
      await chrome.tabs.remove(tabId);
      return { closed: tabId };
    },

    // --- Page observation (read-only) ------------------------------------
    async get_page_text(args) {
      const tabId = await resolveTabId(args);
      const max = (args && args.max_chars) || 8000;
      const text = await execOnTab(tabId, () => {
        // innerText approximates the visible, readable text.
        return (document.body && document.body.innerText) || "";
      });
      const clean = (text || "").replace(/\n{3,}/g, "\n\n").trim();
      const truncated = clean.length > max;
      return {
        text: truncated ? clean.slice(0, max) : clean,
        chars: clean.length,
        truncated,
      };
    },

    async get_page_links(args) {
      const tabId = await resolveTabId(args);
      const max = (args && args.max_links) || 100;
      const links = await execOnTab(
        tabId,
        (cap) => {
          const out = [];
          for (const a of document.querySelectorAll("a[href]")) {
            const text = (a.innerText || a.textContent || "").trim();
            const href = a.href;
            if (!href || href.startsWith("javascript:")) continue;
            out.push({ text: text.slice(0, 200), href });
            if (out.length >= cap) break;
          }
          return out;
        },
        [max],
      );
      return { links, count: links.length };
    },

    async get_page_outline(args) {
      const tabId = await resolveTabId(args);
      const headings = await execOnTab(tabId, () => {
        const out = [];
        for (const h of document.querySelectorAll("h1,h2,h3,h4,h5,h6")) {
          const text = (h.innerText || h.textContent || "").trim();
          if (text) out.push({ level: Number(h.tagName[1]), text: text.slice(0, 200) });
        }
        return out;
      });
      return { headings, count: headings.length };
    },

    async get_interactive_elements(args) {
      const tabId = await resolveTabId(args);
      const max = (args && args.max_elements) || 50;
      const elements = await execOnTab(
        tabId,
        (cap) => {
          const out = [];
          const sel = "a[href], button, input, select, textarea, [role=button]";
          for (const el of document.querySelectorAll(sel)) {
            const tag = el.tagName.toLowerCase();
            const label =
              (el.innerText || el.value || el.getAttribute("aria-label") || el.getAttribute("placeholder") || el.name || "")
                .toString()
                .trim()
                .slice(0, 120);
            const type = el.getAttribute("type") || el.getAttribute("role") || tag;
            out.push({ tag, type, label });
            if (out.length >= cap) break;
          }
          return out;
        },
        [max],
      );
      return { elements, count: elements.length, note: "read-only; this build cannot click or type" };
    },

    // --- Agent UX tools ---------------------------------------------------
    async ask_user_confirmation(args) {
      const markdown = (args && args.markdown) || "Are you sure?";
      const confirmed = await ui.confirm(markdown);
      return { confirmed };
    },

    async show_plan(args) {
      const steps = (args && Array.isArray(args.steps) ? args.steps : []).map(String);
      ui.showPlan(steps);
      return { shown: true, steps: steps.length };
    },
  };
}

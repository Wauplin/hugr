# Hugr browser agent — demos

A few things to try once the extension is loaded and an API key is set (see [`README.md`](./README.md)). Each demo highlights a different part of the design. Keep the side panel open and switch between tabs freely — the agent reads whatever you point it at.

## 1. Read & summarize the current page

> **"Summarize this page in 5 bullets."**

Open any article, then ask. Watch the agent call `get_current_page` → `get_page_text`, stream a summary, and show token counts. Shows: **page observation**, live **streaming**, and the read-only tools running with **no permission prompt**.

## 2. List and triage tabs

> **"List my open tabs and tell me which ones look like documentation."**

The agent calls `list_tabs` and reasons over the titles/URLs. Shows: the **tab tools** and that the brain routes opaque tool results back into the turn loop.

## 3. Navigate with auto-approve

> **"Open Hacker News and read me the top 5 headlines."**

The agent calls `open_tab` (or `navigate_tab`) — the browser host runs the configured `small` tier permission judge and feeds back `Allow` or `Deny { reason }` as a recorded `PermissionDecision`. If denied, the model sees the reason and can adapt; if allowed, it reads the page and reports back. Toggle **yolo navigation** to skip the judge (the browser equivalent of the CLI's `--yolo` / `-y`).

## 4. Multi-step research across tabs

> **"Open the Rust book, find the chapter on ownership, and summarize its key ideas."**

The agent will `open_tab`, `get_page_outline` to find the chapter, `navigate_tab` (or `get_page_links` + open) into it, then `get_page_text` and summarize — several tool round-trips in one turn. Shows: the **turn loop** driving `model → tools → model → …` until done, exactly like the CLI.

## 5. Plan first, confirm before acting

> **"Plan how you'd compare the pricing pages of two cloud providers, then do it."**

The agent uses `show_plan` to lay out the steps, may use `ask_user_confirmation` before opening tabs, then executes. Shows: the **agent-UX tools** (`show_plan`, `ask_user_confirmation`) rendered as interactive cards.

## 6. Describe what's on a page (read-only)

> **"What could I do on this page? List the buttons and inputs."**

The agent calls `get_interactive_elements` and describes them — but note it will tell you it **cannot** click or type. Shows: the deliberate **read + navigate only** boundary.

## 7. Interrupt a long turn

Start a multi-step task (e.g. demo 4), then hit **Stop** mid-stream. Shows: **first-class cancellation** — the host aborts the in-flight fetch and the brain records the turn as cancelled (`UserAbort` → `Cancel` → `Done { Cancelled }`), the same machinery as the CLI's Ctrl-C.

## 8. Same brain, prove it

Open the DevTools console for the side panel — the subtitle shows `core vX · <model>`, and you can watch the JSON `Command`/`Event` stream. It is byte-for-byte the same contract the native CLI uses; the brain is the identical `hugr-core`, just compiled to WebAssembly.

---

**Tips**

- Point at a normal web page — `chrome://` pages, the Web Store, and PDFs can't be read (Chrome blocks injection there).
- If nothing happens, check Settings: the API key/base URL/model must be valid for a **tool-calling** model.
- Shift+Enter for a newline in the composer; Enter to send.

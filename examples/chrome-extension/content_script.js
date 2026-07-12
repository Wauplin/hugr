chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  Promise.resolve()
    .then(() => handleContentMessage(message))
    .then((result) => sendResponse({ ok: true, result }))
    .catch((error) => sendResponse({ ok: false, error: String(error?.message || error) }));
  return true;
});

function handleContentMessage(message) {
  const args = message?.args || {};
  switch (message?.type) {
    case "page_read_html":
      return cappedText("html", document.documentElement.outerHTML);
    case "page_read_text":
      return cappedText("text", document.body?.innerText || "");
    case "page_snapshot":
      return { url: location.href, title: document.title, nodes: snapshotNodes() };
    case "wait_for_page_settled":
      return waitForPageSettled(args.settle_ms, args.timeout_ms);
    case "wait_for_selector":
      return waitFor(() => document.querySelector(args.selector), args.timeout_ms, "selector wait timed out");
    case "wait_for_text":
      return waitFor(() => (document.body?.innerText || "").includes(args.text), args.timeout_ms, "text wait timed out");
    case "page_click":
      return withTarget(args, (target) => {
        target.click();
        return { clicked: true };
      });
    case "page_type":
      return withTarget(args, (target) => {
        if (!("value" in target)) throw new Error("target is not text-input-like");
        if (args.clear !== false) target.value = "";
        target.focus();
        target.value += args.text || "";
        target.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: args.text || "" }));
        target.dispatchEvent(new Event("change", { bubbles: true }));
        return { typed: true };
      });
    case "page_select":
      return withTarget(args, (target) => {
        if (!(target instanceof HTMLSelectElement)) throw new Error("target is not a select element");
        target.value = args.value || "";
        target.dispatchEvent(new Event("input", { bubbles: true }));
        target.dispatchEvent(new Event("change", { bubbles: true }));
        return { selected: true };
      });
    case "page_scroll":
      window.scrollBy(args.delta_x || 0, args.delta_y || 0);
      return { scrolled: true, x: window.scrollX, y: window.scrollY };
    case "page_submit":
      return withTarget(args, (target) => {
        const form = target instanceof HTMLFormElement ? target : target.closest("form");
        if (!form) throw new Error("no form found for target");
        form.requestSubmit();
        return { submitted: true };
      });
    case "page_focus":
      return withTarget(args, (target) => {
        target.focus();
        return { focused: true };
      });
    case "file_upload_to_input":
      return withTarget(args, (target) => {
        if (!(target instanceof HTMLInputElement) || target.type !== "file") {
          throw new Error("target is not a file input");
        }
        const file = fileFromDataUrl(args.file);
        const transfer = new DataTransfer();
        transfer.items.add(file);
        target.files = transfer.files;
        target.dispatchEvent(new Event("input", { bubbles: true }));
        target.dispatchEvent(new Event("change", { bubbles: true }));
        return { uploaded: true, filename: file.name, byte_length: file.size };
      });
    default:
      throw new Error(`unknown content script message: ${message?.type}`);
  }
}

function cappedText(key, value, maxChars = 1_000_000) {
  return { [key]: value.slice(0, maxChars), truncated: value.length > maxChars };
}

function snapshotNodes() {
  const selector = "a,button,input,select,textarea,[role='button'],[onclick]";
  return [...document.querySelectorAll(selector)].slice(0, 200).map((node, index) => {
    const rect = node.getBoundingClientRect();
    const nodeId = `n${index + 1}`;
    node.dataset.hugrNodeId = nodeId;
    return {
      node_id: nodeId,
      tag: node.tagName.toLowerCase(),
      role: node.getAttribute("role") || "",
      text: visibleLabel(node),
      type: node.getAttribute("type") || "",
      disabled: Boolean(node.disabled),
      href: node.href || "",
      rect: {
        x: Math.round(rect.x),
        y: Math.round(rect.y),
        width: Math.round(rect.width),
        height: Math.round(rect.height)
      }
    };
  });
}

function visibleLabel(node) {
  if (node instanceof HTMLInputElement || node instanceof HTMLTextAreaElement) {
    return node.getAttribute("aria-label") || node.placeholder || node.value || "";
  }
  return (node.innerText || node.textContent || node.getAttribute("aria-label") || "").trim().replace(/\s+/g, " ").slice(0, 200);
}

function withTarget(args, fn) {
  const target = findTarget(args);
  if (!target) throw new Error("target element not found");
  return fn(target);
}

function findTarget(args) {
  if (args.node_id) {
    const byNodeId = document.querySelector(`[data-hugr-node-id="${cssEscape(args.node_id)}"]`);
    if (byNodeId) return byNodeId;
  }
  if (args.selector) return document.querySelector(args.selector);
  return null;
}

function waitFor(predicate, timeoutMs = 30000, timeoutMessage = "wait timed out") {
  const started = Date.now();
  return new Promise((resolve, reject) => {
    const tick = () => {
      if (predicate()) {
        resolve({ matched: true });
      } else if (Date.now() - started >= timeoutMs) {
        reject(new Error(timeoutMessage));
      } else {
        setTimeout(tick, 250);
      }
    };
    tick();
  });
}

function waitForPageSettled(settleMs = 650, timeoutMs = 2500) {
  const started = Date.now();
  let lastMutation = Date.now();
  const observer = new MutationObserver(() => {
    lastMutation = Date.now();
  });
  observer.observe(document.documentElement, {
    childList: true,
    subtree: true,
    attributes: true,
    characterData: true
  });
  return new Promise((resolve) => {
    const tick = () => {
      const now = Date.now();
      const readyEnough = document.readyState === "interactive" || document.readyState === "complete";
      if (readyEnough && now - lastMutation >= settleMs) {
        observer.disconnect();
        resolve({
          settled: true,
          ready_state: document.readyState,
          waited_ms: now - started,
          quiet_ms: now - lastMutation,
          url: location.href,
          title: document.title
        });
      } else if (now - started >= timeoutMs) {
        observer.disconnect();
        resolve({
          settled: false,
          ready_state: document.readyState,
          waited_ms: now - started,
          quiet_ms: now - lastMutation,
          url: location.href,
          title: document.title
        });
      } else {
        setTimeout(tick, 100);
      }
    };
    tick();
  });
}

function cssEscape(value) {
  if (globalThis.CSS?.escape) return CSS.escape(value);
  return String(value).replace(/["\\]/g, "\\$&");
}

function fileFromDataUrl(file) {
  if (!file?.dataUrl) throw new Error("missing local file payload");
  const [header, payload] = file.dataUrl.split(",", 2);
  const mediaType = /data:([^;]+)/.exec(header)?.[1] || file.mediaType || "application/octet-stream";
  const raw = atob(payload || "");
  const bytes = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i += 1) bytes[i] = raw.charCodeAt(i);
  return new File([bytes], file.filename || "upload", { type: mediaType });
}

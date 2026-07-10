export async function chromeCall(message) {
  const response = await chrome.runtime.sendMessage(message);
  if (!response?.ok) {
    throw new Error(response?.error || "Chrome call failed");
  }
  return response;
}

export async function invokeBrowserCapability(name, args) {
  switch (name) {
    case "tabs_list":
      return (await chromeCall({ type: "hugr.tabs.list" })).tabs.map(normalizeTab);
    case "tab_open_url":
      return normalizeTab((await chromeCall({ type: "hugr.tab.open", url: args.url, active: args.active })).tab);
    case "tab_close":
      await chromeCall({ type: "hugr.tab.close", tabId: args.tab_id });
      return { closed: true };
    case "tab_switch":
      await chromeCall({ type: "hugr.tab.switch", tabId: args.tab_id });
      return { active: true };
    case "tab_reload":
      await chrome.tabs.reload(args.tab_id);
      return { reloaded: true };
    case "tab_back":
      await chrome.tabs.goBack(args.tab_id);
      return { navigated: true };
    case "tab_forward":
      await chrome.tabs.goForward(args.tab_id);
      return { navigated: true };
    case "wait_for_navigation":
      return await waitForNavigation(args.tab_id, args.timeout_ms, args.settle_ms);
    case "wait_for_tab_opened":
      return await waitForTabOpened(args.timeout_ms);
    case "wait_for_url":
      return await waitForUrl(args.tab_id, args.contains, args.timeout_ms);
    case "page_read_html":
    case "page_read_text":
    case "page_snapshot":
      return await contentCall(args.tab_id, { type: name });
    case "wait_for_page_settled":
      return await waitForPageSettled(args.tab_id, args.settle_ms, args.timeout_ms);
    case "wait_for_selector":
    case "wait_for_text":
    case "page_click":
    case "page_type":
    case "page_select":
    case "page_scroll":
    case "page_submit":
    case "page_focus":
      return await contentCall(args.tab_id, { type: name, args });
    case "file_upload_to_input":
      return await uploadLocalFile(args);
    case "file_download_url":
      return await downloadToLocalStore(args.url, args.filename || "");
    case "file_list_downloads":
      return await listLocalFiles();
    case "file_read_metadata":
      return await readLocalFileMetadata(args.file_id);
    case "file_delete":
      return await deleteLocalFile(args.file_id);
    default:
      throw new Error(`capability not implemented yet: ${name}`);
  }
}

export async function contentCall(tabId, message) {
  const response = await chrome.tabs.sendMessage(tabId, message);
  if (!response?.ok) {
    throw new Error(response?.error || "content script call failed");
  }
  return response.result;
}

function normalizeTab(tab) {
  return {
    id: tab.id,
    window_id: tab.windowId,
    active: tab.active,
    title: tab.title || "",
    url: tab.url || "",
    status: tab.status || "",
  };
}

async function waitForNavigation(tabId, timeoutMs = 12000, settleMs = 650) {
  const started = Date.now();
  const initial = await chrome.tabs.get(tabId).catch(() => null);
  const completed = await new Promise((resolve, reject) => {
    const timeout = setTimeout(async () => {
      const current = await chrome.tabs.get(tabId).catch(() => null);
      if (current && (current.status === "complete" || current.url !== initial?.url)) {
        finish(resolve, { tab_id: tabId, url: current.url || "", timed_out_waiting_for_completed: true });
      } else {
        finish(reject, new Error("navigation timed out"));
      }
    }, timeoutMs);
    const listener = (details) => {
      if (details.tabId === tabId && details.frameId === 0) {
        finish(resolve, { tab_id: tabId, url: details.url });
      }
    };
    const finish = (done, value) => {
      clearTimeout(timeout);
      chrome.webNavigation.onCompleted.removeListener(listener);
      done(value);
    };
    chrome.webNavigation.onCompleted.addListener(listener);
  });
  const remaining = Math.max(500, timeoutMs - (Date.now() - started));
  const settled = await waitForPageSettled(tabId, settleMs, Math.min(remaining, 2500)).catch((error) => ({
    settled: false,
    error: String(error?.message || error)
  }));
  return { ...completed, ...settled };
}

function waitForTabOpened(timeoutMs = 30000) {
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => finish(reject, new Error("tab-open wait timed out")), timeoutMs);
    const listener = (tab) => {
      finish(resolve, { tab: normalizeTab(tab) });
    };
    const finish = (done, value) => {
      clearTimeout(timeout);
      chrome.tabs.onCreated.removeListener(listener);
      done(value);
    };
    chrome.tabs.onCreated.addListener(listener);
  });
}

async function waitForUrl(tabId, contains, timeoutMs = 30000) {
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    const tab = await chrome.tabs.get(tabId);
    if ((tab.url || "").includes(contains)) {
      return { matched: true, url: tab.url || "" };
    }
    await sleep(250);
  }
  throw new Error("url wait timed out");
}

async function waitForPageSettled(tabId, settleMs = 650, timeoutMs = 2500) {
  return await contentCall(tabId, {
    type: "wait_for_page_settled",
    args: { settle_ms: settleMs || 650, timeout_ms: timeoutMs || 2500 }
  });
}

async function downloadToLocalStore(url, filename) {
  const { saveLocalFile } = await import("./vendor/indexed_db.js");
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`download failed with ${response.status}`);
  }
  const blob = await response.blob();
  const fileId = crypto.randomUUID();
  const stored = {
    fileId,
    filename: filename || nameFromUrl(url),
    mediaType: blob.type || response.headers.get("content-type") || "application/octet-stream",
    sourceUrl: url,
    byteLength: blob.size,
    createdAt: new Date().toISOString(),
    blob
  };
  await saveLocalFile(stored);
  return withoutBlob(stored);
}

async function listLocalFiles() {
  const { listFiles } = await import("./vendor/indexed_db.js");
  return (await listFiles()).map(withoutBlob);
}

async function readLocalFileMetadata(fileId) {
  const { getLocalFile } = await import("./vendor/indexed_db.js");
  const file = await getLocalFile(fileId);
  if (!file) throw new Error(`unknown file_id: ${fileId}`);
  return withoutBlob(file);
}

async function deleteLocalFile(fileId) {
  const { deleteLocalFile: remove } = await import("./vendor/indexed_db.js");
  await remove(fileId);
  return { deleted: true };
}

async function uploadLocalFile(args) {
  const { getLocalFile } = await import("./vendor/indexed_db.js");
  const file = await getLocalFile(args.file_id);
  if (!file) throw new Error(`unknown file_id: ${args.file_id}`);
  const dataUrl = await blobToDataUrl(file.blob);
  return await contentCall(args.tab_id, {
    type: "file_upload_to_input",
    args: {
      ...args,
      file: {
        filename: file.filename || "upload",
        mediaType: file.mediaType || "application/octet-stream",
        dataUrl
      }
    }
  });
}

function withoutBlob(file) {
  const { blob, ...metadata } = file;
  return metadata;
}

function nameFromUrl(url) {
  try {
    const pathname = new URL(url).pathname;
    return decodeURIComponent(pathname.split("/").filter(Boolean).pop() || "download");
  } catch {
    return "download";
  }
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function blobToDataUrl(blob) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result);
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(blob);
  });
}

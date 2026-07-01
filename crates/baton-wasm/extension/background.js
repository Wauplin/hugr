// Minimal service worker. Its ONLY job is to open the side panel when the
// toolbar icon is clicked — all the interesting work (the WASM brain, the driver
// loop, the model calls, the tab tools) lives in the side-panel page, which is a
// full extension page that can load WASM and call chrome.tabs / chrome.scripting
// directly. Keeping the brain in the panel (not here) means no message-passing
// plumbing and a service worker that can sleep freely.

chrome.runtime.onInstalled.addListener(() => {
  // Let clicking the toolbar icon toggle the side panel open.
  chrome.sidePanel
    .setPanelBehavior({ openPanelOnActionClick: true })
    .catch((err) => console.warn("sidePanel.setPanelBehavior:", err));
});

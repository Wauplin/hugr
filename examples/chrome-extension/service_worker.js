chrome.runtime.onInstalled.addListener(() => {
  chrome.sidePanel.setPanelBehavior({ openPanelOnActionClick: true });
});

chrome.action.onClicked.addListener(async (tab) => {
  if (tab.windowId !== undefined) {
    await chrome.sidePanel.open({ windowId: tab.windowId });
  }
});

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  handleMessage(message, sender).then(sendResponse, (error) => {
    sendResponse({ ok: false, error: String(error?.message || error) });
  });
  return true;
});

async function handleMessage(message, sender) {
  switch (message?.type) {
    case "huggr.tabs.list":
      return { ok: true, tabs: await chrome.tabs.query({}) };
    case "huggr.tab.open":
      return { ok: true, tab: await chrome.tabs.create({ url: message.url, active: message.active !== false }) };
    case "huggr.tab.close":
      await chrome.tabs.remove(message.tabId);
      return { ok: true };
    case "huggr.tab.switch":
      await chrome.tabs.update(message.tabId, { active: true });
      return { ok: true };
    default:
      return { ok: false, error: `unknown service worker message: ${message?.type}` };
  }
}


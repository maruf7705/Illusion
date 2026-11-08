// Illusion Background Service Worker (Manifest V3)
const NATIVE_HOST_NAME = "com.illusion.downloader";

// Default settings
const DEFAULT_SETTINGS = {
  enabled: true,
  interceptDownloads: true,
  snifferEnabled: true,
  minFileSizeMB: 0,
  fileExtensions: [
    "7z", "apk", "avi", "bin", "bz2", "deb", "dmg", "docx", "epub", "exe",
    "flv", "gz", "iso", "jar", "mkv", "mov", "mp3", "mp4", "mpeg", "msi",
    "pdf", "pkg", "rar", "rpm", "tar", "torrent", "ts", "wav", "webm", "wma",
    "wmv", "xz", "zip", "zst"
  ]
};

// Initialize settings and context menus on installation
chrome.runtime.onInstalled.addListener(async () => {
  const existing = await chrome.storage.local.get("settings");
  if (!existing.settings) {
    await chrome.storage.local.set({ settings: DEFAULT_SETTINGS });
  }

  // Create context menus
  chrome.contextMenus.create({
    id: "illusion-download-link",
    title: "Download with Illusion",
    contexts: ["link"]
  });

  chrome.contextMenus.create({
    id: "illusion-download-media",
    title: "Download Media with Illusion",
    contexts: ["video", "audio", "image"]
  });
});

// Handle Context Menu clicks
chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  let targetUrl = null;
  let filename = "";

  if (info.menuItemId === "illusion-download-link" && info.linkUrl) {
    targetUrl = info.linkUrl;
  } else if (info.menuItemId === "illusion-download-media" && info.srcUrl) {
    targetUrl = info.srcUrl;
  }

  if (targetUrl) {
    await sendToIllusion({
      url: targetUrl,
      referer: tab ? tab.url : "",
      page_title: tab ? tab.title : "",
      filename: filename
    });
  }
});

// Intercept browser downloads
chrome.downloads.onDeterminingFilename.addListener((downloadItem, suggest) => {
  chrome.storage.local.get("settings", async (data) => {
    const settings = data.settings || DEFAULT_SETTINGS;
    if (!settings.enabled || !settings.interceptDownloads) {
      suggest();
      return;
    }

    const url = downloadItem.url;
    // Don't intercept blob, data, or internal browser URLs
    if (url.startsWith("blob:") || url.startsWith("data:") || url.startsWith("chrome://")) {
      suggest();
      return;
    }

    const filename = downloadItem.filename || "";
    const ext = filename.split(".").pop().toLowerCase();

    // Check extension list or size threshold
    const shouldIntercept = settings.fileExtensions.includes(ext) || downloadItem.fileSize > (settings.minFileSizeMB * 1024 * 1024);

    if (shouldIntercept) {
      try {
        // Cancel browser download
        chrome.downloads.cancel(downloadItem.id, () => {
          chrome.downloads.erase({ id: downloadItem.id });
        });

        // Forward download request to Illusion
        await sendToIllusion({
          url: downloadItem.url,
          filename: downloadItem.filename,
          referer: downloadItem.referrer || "",
          file_size: downloadItem.fileSize || 0,
          mime_type: downloadItem.mime || ""
        });
      } catch (err) {
        console.error("[Illusion] Failed to intercept download:", err);
        suggest();
      }
    } else {
      suggest();
    }
  });

  return true; // Asynchronous suggestion
});

// Forward download task to Illusion Native Host
async function sendToIllusion(details) {
  try {
    // Gather cookies for the target domain
    let cookiesStr = "";
    try {
      const urlObj = new URL(details.url);
      const cookies = await chrome.cookies.getAll({ domain: urlObj.hostname });
      cookiesStr = cookies.map(c => `${c.name}=${c.value}`).join("; ");
    } catch (_) {}

    const payload = {
      action: "add_download",
      url: details.url,
      filename: details.filename || "",
      referer: details.referer || "",
      cookies: cookiesStr,
      user_agent: navigator.userAgent,
      headers: details.headers || {}
    };

    chrome.runtime.sendNativeMessage(NATIVE_HOST_NAME, payload, (response) => {
      if (chrome.runtime.lastError) {
        console.warn("[Illusion] Native host error:", chrome.runtime.lastError.message);
      } else {
        console.log("[Illusion] Native host response:", response);
      }
    });
  } catch (e) {
    console.error("[Illusion] Failed to forward to native host:", e);
  }
}

// Listen for messages from content scripts (media sniffer)
chrome.runtime.onMessage.addListener((request, sender, sendResponse) => {
  if (request.type === "MEDIA_SNIFFED_DOWNLOAD") {
    sendToIllusion({
      url: request.url,
      filename: request.filename || "",
      referer: sender.tab ? sender.tab.url : "",
      headers: request.headers || {}
    }).then(() => {
      sendResponse({ status: "success" });
    });
    return true;
  }
  if (request.type === "PING_HOST") {
    chrome.runtime.sendNativeMessage(NATIVE_HOST_NAME, { action: "ping" }, (res) => {
      if (chrome.runtime.lastError) {
        sendResponse({ connected: false, error: chrome.runtime.lastError.message });
      } else {
        sendResponse({ connected: true, data: res });
      }
    });
    return true;
  }
});

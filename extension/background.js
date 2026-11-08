// Illusion Background Service Worker (Manifest V3)
// IDM-Grade Network Sniffing & Native Messaging Bridge
const NATIVE_HOST_NAME = "com.illusion.downloader";

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

// In-memory stream cache per tab
const tabStreams = new Map();

// Helper to infer quality or stream type
function inferQuality(url, mime, sizeBytes) {
  const lowerUrl = url.toLowerCase();
  let quality = "";

  if (lowerUrl.includes("1080p") || lowerUrl.includes("1080")) quality = "1080p Full HD";
  else if (lowerUrl.includes("720p") || lowerUrl.includes("720")) quality = "720p HD";
  else if (lowerUrl.includes("480p") || lowerUrl.includes("480")) quality = "480p SD";
  else if (lowerUrl.includes("360p") || lowerUrl.includes("360")) quality = "360p Low";
  else if (lowerUrl.includes(".m3u8") || mime.includes("mpegurl")) quality = "HLS Stream (.m3u8)";
  else if (lowerUrl.includes(".mpd") || mime.includes("dash+xml")) quality = "DASH Manifest (.mpd)";
  else if (mime.startsWith("audio/")) quality = "Audio Track";
  else if (mime.startsWith("video/")) quality = "Video Stream";
  else quality = "Media File";

  let sizeStr = "";
  if (sizeBytes && sizeBytes > 0) {
    if (sizeBytes > 1024 * 1024 * 1024) {
      sizeStr = ` (${(sizeBytes / (1024 * 1024 * 1024)).toFixed(1)} GB)`;
    } else if (sizeBytes > 1024 * 1024) {
      sizeStr = ` (${(sizeBytes / (1024 * 1024)).toFixed(1)} MB)`;
    } else if (sizeBytes > 1024) {
      sizeStr = ` (${(sizeBytes / 1024).toFixed(0)} KB)`;
    }
  }

  return `${quality}${sizeStr}`;
}

// 1. Network-Level Media Request Sniffer (The Core of IDM Video Grabber!)
chrome.webRequest.onHeadersReceived.addListener(
  (details) => {
    if (details.tabId < 0) return;

    const url = details.url;
    if (url.startsWith("blob:") || url.startsWith("data:") || url.startsWith("chrome://")) {
      return;
    }

    let contentType = "";
    let contentLength = 0;

    if (details.responseHeaders) {
      for (const h of details.responseHeaders) {
        const name = h.name.toLowerCase();
        if (name === "content-type") {
          contentType = h.value ? h.value.toLowerCase() : "";
        } else if (name === "content-length") {
          contentLength = parseInt(h.value || "0", 10) || 0;
        }
      }
    }

    const cleanUrl = url.split("?")[0].toLowerCase();
    const isMediaMime =
      contentType.startsWith("video/") ||
      contentType.startsWith("audio/") ||
      contentType.includes("application/vnd.apple.mpegurl") ||
      contentType.includes("application/x-mpegurl") ||
      contentType.includes("application/dash+xml");

    const isMediaExt =
      cleanUrl.endsWith(".mp4") ||
      cleanUrl.endsWith(".m4s") ||
      cleanUrl.endsWith(".webm") ||
      cleanUrl.endsWith(".mkv") ||
      cleanUrl.endsWith(".flv") ||
      cleanUrl.endsWith(".m3u8") ||
      cleanUrl.endsWith(".mpd") ||
      cleanUrl.endsWith(".ts") ||
      cleanUrl.endsWith(".mov") ||
      cleanUrl.endsWith(".mp3") ||
      cleanUrl.endsWith(".m4a") ||
      cleanUrl.endsWith(".aac") ||
      cleanUrl.endsWith(".wav");

    // Filter out tiny audio effects/icons (< 200KB unless it is an m3u8 playlist or mpd)
    const isPlaylist = cleanUrl.endsWith(".m3u8") || cleanUrl.endsWith(".mpd") || contentType.includes("mpegurl") || contentType.includes("dash");
    if (!isPlaylist && contentLength > 0 && contentLength < 200 * 1024) {
      return;
    }

    if (isMediaMime || isMediaExt) {
      if (!tabStreams.has(details.tabId)) {
        tabStreams.set(details.tabId, []);
      }

      const streams = tabStreams.get(details.tabId);
      // Avoid duplicates
      if (!streams.some((s) => s.url === url)) {
        const quality = inferQuality(url, contentType, contentLength);
        const item = {
          url: url,
          mime: contentType,
          size: contentLength,
          quality: quality,
          timestamp: Date.now()
        };
        streams.push(item);

        // Broadcast to tab's content script to update the floating IDM bar
        chrome.tabs.sendMessage(details.tabId, {
          type: "ILLUSION_STREAM_DETECTED",
          stream: item,
          totalCount: streams.len
        }).catch(() => {});
      }
    }
  },
  {
    urls: ["<all_urls>"],
    types: ["media", "xmlhttprequest", "other"]
  },
  ["responseHeaders"]
);

// Clear stream cache when tab navigates or closes
chrome.tabs.onUpdated.addListener((tabId, changeInfo) => {
  if (changeInfo.status === "loading") {
    tabStreams.delete(tabId);
  }
});

chrome.tabs.onRemoved.addListener((tabId) => {
  tabStreams.delete(tabId);
});

// 2. Intercept regular browser downloads
chrome.downloads.onDeterminingFilename.addListener((downloadItem, suggest) => {
  chrome.storage.local.get("settings", async (data) => {
    const settings = data.settings || DEFAULT_SETTINGS;
    if (!settings.enabled || !settings.interceptDownloads) {
      suggest();
      return;
    }

    const url = downloadItem.url;
    if (url.startsWith("blob:") || url.startsWith("data:") || url.startsWith("chrome://")) {
      suggest();
      return;
    }

    const filename = downloadItem.filename || "";
    const ext = filename.split(".").pop().toLowerCase();
    const shouldIntercept =
      settings.fileExtensions.includes(ext) ||
      downloadItem.fileSize > settings.minFileSizeMB * 1024 * 1024;

    if (shouldIntercept) {
      try {
        chrome.downloads.cancel(downloadItem.id, () => {
          chrome.downloads.erase({ id: downloadItem.id });
        });

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

  return true;
});

// 3. Native Messaging communication
async function sendToIllusion(details) {
  try {
    let cookiesStr = "";
    try {
      const urlObj = new URL(details.url);
      const cookies = await chrome.cookies.getAll({ domain: urlObj.hostname });
      cookiesStr = cookies.map((c) => `${c.name}=${c.value}`).join("; ");
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

// 4. Message dispatcher
chrome.runtime.onMessage.addListener((request, sender, sendResponse) => {
  if (request.type === "MEDIA_SNIFFED_DOWNLOAD") {
    sendToIllusion({
      url: request.url,
      filename: request.filename || "",
      referer: sender.tab ? sender.tab.url : ""
    }).then(() => {
      sendResponse({ status: "success" });
    });
    return true;
  }

  if (request.type === "GET_TAB_STREAMS") {
    const tabId = sender.tab ? sender.tab.id : -1;
    const list = tabStreams.get(tabId) || [];
    sendResponse({ streams: list });
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

// Setup context menus
chrome.runtime.onInstalled.addListener(async () => {
  const existing = await chrome.storage.local.get("settings");
  if (!existing.settings) {
    await chrome.storage.local.set({ settings: DEFAULT_SETTINGS });
  }

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

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  let targetUrl = null;
  if (info.menuItemId === "illusion-download-link" && info.linkUrl) {
    targetUrl = info.linkUrl;
  } else if (info.menuItemId === "illusion-download-media" && info.srcUrl) {
    targetUrl = info.srcUrl;
  }

  if (targetUrl) {
    await sendToIllusion({
      url: targetUrl,
      referer: tab ? tab.url : "",
      filename: ""
    });
  }
});

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

// 1. Rolling 60-Second Ring Buffer for HTTP Headers & Session Correlation
const headerRingBuffer = new Map();
const responseMetadataByUrl = new Map();
const RING_BUFFER_TTL_MS = 60 * 1000;

function cleanUrlForCorrelation(url) {
  // Correlation must use the exact signed request URL.  Dropping its query
  // string can attach cookies/headers from a different playback request.
  return typeof url === "string" ? url : "";
}

function storeInRingBuffer(url, details) {
  if (!url) return;
  const now = Date.now();
  const entry = {
    headers: details.headers || {},
    referer: details.referer || "",
    userAgent: details.userAgent || navigator.userAgent,
    tabId: details.tabId,
    timestamp: now
  };

  headerRingBuffer.set(url, entry);
  const cleanKey = cleanUrlForCorrelation(url);
  if (cleanKey !== url) {
    headerRingBuffer.set(cleanKey, entry);
  }

  // Prune entries older than 60s
  if (headerRingBuffer.size > 200) {
    for (const [k, v] of headerRingBuffer.entries()) {
      if (now - v.timestamp > RING_BUFFER_TTL_MS) {
        headerRingBuffer.delete(k);
      }
    }
  }
}

function getFromRingBuffer(url) {
  if (!url) return null;
  if (headerRingBuffer.has(url)) return headerRingBuffer.get(url);
  const cleanKey = cleanUrlForCorrelation(url);
  if (headerRingBuffer.has(cleanKey)) return headerRingBuffer.get(cleanKey);
  return null;
}

function responseLength(headers) {
  const contentRange = headers["content-range"] || "";
  const total = contentRange.match(/\/([0-9]+)$/);
  if (total) return Number(total[1]) || null;
  const length = Number(headers["content-length"] || 0);
  return length > 0 ? length : null;
}

// YouTube's SABR transport can return a small UMP control payload from a URL
// that looks like /videoplayback. It is not a media container. The failures
// reported by the user have sabr=1, no itag, and a 31-byte UMP response.
function isYouTubeSabrControlUrl(url) {
  try {
    const parsed = new URL(url);
    return (parsed.hostname.includes("googlevideo.com") || parsed.pathname.includes("/videoplayback")) &&
      parsed.searchParams.has("sabr") &&
      !parsed.searchParams.has("itag");
  } catch (_) {
    return false;
  }
}

function isUnsupportedControlResponse(url, contentType) {
  return (contentType || "").toLowerCase().includes("application/vnd.yt-ump") ||
    isYouTubeSabrControlUrl(url);
}

async function removeUnsupportedControlStream(tabId, url) {
  const streams = await getTabStreams(tabId);
  const filtered = streams.filter((stream) => stream.url !== url);
  if (filtered.length === streams.length) return;
  await saveTabStreams(tabId, filtered);
  chrome.tabs.sendMessage(tabId, {
    type: "STREAM_REGISTRY_UPDATED",
    streams: filtered
  }, { frameId: 0 }).catch(() => {});
}

function candidateKind(url, mime, isProtected) {
  const lower = `${url} ${mime}`.toLowerCase();
  if (isProtected || url.startsWith("blob:")) return "protected_or_unsupported";
  if (lower.includes(".m3u8") || lower.includes("mpegurl")) return "hls_manifest";
  if (lower.includes(".mpd") || lower.includes("dash+xml")) return "dash_manifest";
  return "progressive";
}

function requestContext(url, tabId, pageUrl, metadata = {}) {
  const ring = getFromRingBuffer(url) || {};
  const observed = responseMetadataByUrl.get(url) || {};
  // `headers` are request headers captured before playback.  Response headers
  // are represented only by typed metadata below and must never be replayed.
  const headers = { ...(ring.headers || {}) };
  const referer = headers.referer || headers.Referer || ring.referer || pageUrl || "";
  const origin = headers.origin || headers.Origin || "";
  return {
    url,
    headers,
    cookies: headers.cookie || headers.Cookie || "",
    userAgent: headers["user-agent"] || headers["User-Agent"] || ring.userAgent || navigator.userAgent,
    referer,
    origin,
    contentType: metadata.contentType || observed.contentType || "",
    contentLength: metadata.contentLength || observed.contentLength || null,
    acceptRanges: Boolean(metadata.acceptRanges || observed.acceptRanges)
  };
}

function makeCandidate(tabId, stream, pageUrl, primaryMetadata = {}) {
  const primary = requestContext(stream.url, tabId, pageUrl, primaryMetadata);
  const audio = stream.audioUrl
    ? requestContext(stream.audioUrl, tabId, pageUrl, stream.audioMetadata || {})
    : null;
  const protectedReason = stream.protectedReason || null;
  return {
    id: `${tabId}:${primary.url}:${Date.now()}`,
    pairId: audio ? `${tabId}:${primary.url}:${audio.url}` : null,
    kind: protectedReason
      ? "protected_or_unsupported"
      : audio
        ? "paired_audio_video"
        : candidateKind(primary.url, primary.contentType || stream.mime || "", false),
    pageUrl: pageUrl || "",
    primary,
    audio,
    quality: stream.quality || "",
    codecs: stream.codecs || "",
    unsupportedReason: protectedReason
  };
}

// Capture request headers via webRequest (extraHeaders allows Cookie, Referer, Authorization)
chrome.webRequest.onBeforeSendHeaders.addListener(
  (details) => {
    const headers = {};
    let referer = "";
    let userAgent = "";
    if (details.requestHeaders) {
      for (const h of details.requestHeaders) {
        const name = h.name.toLowerCase();
        headers[name] = h.value;
        if (name === "referer") referer = h.value;
        if (name === "user-agent") userAgent = h.value;
      }
    }

    storeInRingBuffer(details.url, {
      headers,
      referer,
      userAgent,
      tabId: details.tabId
    });
  },
  { urls: ["<all_urls>"] },
  ["requestHeaders", "extraHeaders"]
);

// 2. In-memory stream cache per tab with chrome.storage.session persistence
const tabStreams = new Map();

async function saveTabStreams(tabId, streams) {
  tabStreams.set(tabId, streams);
  if (chrome.storage && chrome.storage.session) {
    try {
      await chrome.storage.session.set({ [`tab_${tabId}`]: streams });
    } catch (_) {}
  }
}

async function getTabStreams(tabId) {
  if (tabStreams.has(tabId)) {
    return tabStreams.get(tabId);
  }
  if (chrome.storage && chrome.storage.session) {
    try {
      const res = await chrome.storage.session.get(`tab_${tabId}`);
      if (res && res[`tab_${tabId}`]) {
        tabStreams.set(tabId, res[`tab_${tabId}`]);
        return res[`tab_${tabId}`];
      }
    } catch (_) {}
  }
  return [];
}

async function removeTabStreams(tabId) {
  tabStreams.delete(tabId);
  if (chrome.storage && chrome.storage.session) {
    try {
      await chrome.storage.session.remove(`tab_${tabId}`);
    } catch (_) {}
  }
}

// 3. YouTube & Streaming Quality Inference
function inferQuality(url, mime, sizeBytes) {
  const lowerUrl = url.toLowerCase();
  let quality = "";

  if (!sizeBytes || sizeBytes === 0) {
    try {
      const u = new URL(url);
      const clen = u.searchParams.get("clen") || u.searchParams.get("filesize") || u.searchParams.get("size");
      if (clen) sizeBytes = parseInt(clen, 10) || 0;
    } catch (_) {}
  }

  try {
    const itag = new URL(url).searchParams.get("itag");
    const ytItagMap = {
      "18": "360p",
      "22": "720p HD",
      "137": "1080p HD",
      "136": "720p HD",
      "135": "480p",
      "134": "360p",
      "140": "Audio 128k",
      "251": "Audio 160k",
      "250": "Audio 70k",
      "249": "Audio 50k",
      "313": "2160p 4K",
      "271": "1440p 2K",
      "303": "1080p60 HD",
      "298": "720p60 HD"
    };
    if (itag && ytItagMap[itag]) {
      quality = ytItagMap[itag];
    }
  } catch (_) {}

  if (!quality) {
    if (lowerUrl.includes("1080p") || lowerUrl.includes("1080")) quality = "1080p Full HD";
    else if (lowerUrl.includes("720p") || lowerUrl.includes("720")) quality = "720p HD";
    else if (lowerUrl.includes("480p") || lowerUrl.includes("480")) quality = "480p SD";
    else if (lowerUrl.includes("360p") || lowerUrl.includes("360")) quality = "360p Low";
    else if (lowerUrl.includes(".m3u8") || mime.includes("mpegurl")) quality = "HLS Stream (.m3u8)";
    else if (lowerUrl.includes(".mpd") || mime.includes("dash+xml")) quality = "DASH Manifest (.mpd)";
    else if (mime.startsWith("audio/")) quality = "Audio Track";
    else if (mime.startsWith("video/")) quality = "Video Stream";
    else quality = "Media File";
  }

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

// 4. Network-Level Media Request Sniffer (IDM Core Sniffer)
async function handleMediaResponse(tabId, details) {
  if (tabId < 0) return;

  const url = details.url;
  if (url.startsWith("blob:") || url.startsWith("data:") || url.startsWith("chrome://")) {
    return;
  }

  let contentType = "";
  let contentLength = 0;
  let acceptRanges = false;
  const responseHeaders = {};

  if (details.responseHeaders) {
    for (const h of details.responseHeaders) {
      const name = h.name.toLowerCase();
      responseHeaders[name] = h.value || "";
      if (name === "content-type") {
        contentType = h.value ? h.value.toLowerCase() : "";
      } else if (name === "content-length") {
        contentLength = parseInt(h.value || "0", 10) || 0;
      } else if (name === "content-range") {
        const total = responseLength(responseHeaders);
        if (total) contentLength = total;
      } else if (name === "accept-ranges") {
        acceptRanges = (h.value || "").toLowerCase() === "bytes";
      }
    }
  }
  // A 206 response's Content-Length is only the requested slice.  Its
  // Content-Range total is the only valid complete-file size.
  if (responseHeaders["content-range"]) {
    contentLength = responseLength(responseHeaders) || 0;
    acceptRanges = true;
  }

  const cleanUrl = url.split("?")[0].toLowerCase();
  const isYouTubePlayback = url.includes("googlevideo.com/videoplayback") || url.includes("/videoplayback");

  // IDM Architecture: Retain exact RAW URL without JS query parameter alteration
  // (Prevents HMAC signature destruction on Pinterest, YouTube, Twitter)
  const targetDownloadUrl = url;

  // This is the precise 31-byte `sabr.malformed_config` failure mode. Drop it
  // before it reaches the registry, and remove a provisional page-script item
  // in case it was emitted before response metadata arrived.
  if (isUnsupportedControlResponse(targetDownloadUrl, contentType)) {
    responseMetadataByUrl.delete(targetDownloadUrl);
    await removeUnsupportedControlStream(tabId, targetDownloadUrl);
    return;
  }

  // Filter out internal chunk segments: .m4s and .ts should NOT be treated as independent top-level videos!
  const isFragment = cleanUrl.endsWith(".m4s") || cleanUrl.endsWith(".ts");
  const isPlaylist = cleanUrl.endsWith(".m3u8") || cleanUrl.endsWith(".mpd") || contentType.includes("mpegurl") || contentType.includes("dash");

  if (isFragment) {
    // IDM Standard: Never expose internal child segments (.m4s, .ts) as separate downloads to the user
    return;
  }

  const isMediaMime =
    contentType.startsWith("video/") ||
    contentType.startsWith("audio/") ||
    contentType.includes("application/vnd.apple.mpegurl") ||
    contentType.includes("application/x-mpegurl") ||
    contentType.includes("application/dash+xml");

  const isMediaExt =
    cleanUrl.endsWith(".mp4") ||
    cleanUrl.endsWith(".webm") ||
    cleanUrl.endsWith(".mkv") ||
    cleanUrl.endsWith(".flv") ||
    cleanUrl.endsWith(".m3u8") ||
    cleanUrl.endsWith(".mpd") ||
    cleanUrl.endsWith(".mov") ||
    cleanUrl.endsWith(".mp3") ||
    cleanUrl.endsWith(".m4a") ||
    cleanUrl.endsWith(".aac") ||
    cleanUrl.endsWith(".wav");

  if (!isYouTubePlayback && !isPlaylist && contentLength > 0 && contentLength < 100 * 1024) {
    return;
  }

  if (isYouTubePlayback || isMediaMime || isMediaExt) {
    responseMetadataByUrl.set(targetDownloadUrl, {
      contentType,
      contentLength: contentLength || null,
      acceptRanges,
      timestamp: Date.now()
    });
    if (responseMetadataByUrl.size > 200) {
      const cutoff = Date.now() - RING_BUFFER_TTL_MS;
      for (const [key, value] of responseMetadataByUrl.entries()) {
        if ((value.timestamp || 0) < cutoff) responseMetadataByUrl.delete(key);
      }
    }
    let streams = await getTabStreams(tabId);
    if (isPlaylist) {
      // Prioritize master playlist and purge any child fragments
      streams = streams.filter((s) => {
        const c = s.url.split("?")[0].toLowerCase();
        return !c.endsWith(".m4s") && !c.endsWith(".ts");
      });
    }

    // Identify if this sniffed stream is audio
    let itag = null;
    try { itag = new URL(url).searchParams.get("itag"); } catch (_) {}
    const isAudioItag = ["140", "251", "250", "249", "171", "258", "256"].includes(itag);
    const isAudio = contentType.startsWith("audio/") || cleanUrl.endsWith(".m4a") || cleanUrl.endsWith(".mp3") || isAudioItag;

    // Unknown is a valid size.  Do not invent a number from URL parameters.
    let sizeBytes = contentLength;
    if (isPlaylist) {
      sizeBytes = 0; // Don't show misleading 1 KB manifest size as video size
    }

    if (!streams.some((s) => s.url === targetDownloadUrl)) {
      const quality = inferQuality(url, contentType, sizeBytes);
      const item = {
        url: targetDownloadUrl,
        mime: contentType || (isYouTubePlayback ? "video/mp4" : "application/octet-stream"),
        size: sizeBytes,
        quality: quality,
        audioUrl: null,
        isAudio: isAudio,
        candidate: makeCandidate(tabId, {
          url: targetDownloadUrl,
          mime: contentType,
          quality,
          audioUrl: null
        }, "", {
          contentType,
          contentLength: sizeBytes || null,
          acceptRanges
        }),
        timestamp: Date.now()
      };
      if (isPlaylist) {
        streams.unshift(item); // Master playlist takes top spot
      } else {
        streams.push(item);
      }
      await saveTabStreams(tabId, streams);

      // Notify Top Frame Floating Bar
      chrome.tabs.sendMessage(
        tabId,
        {
          type: "STREAM_REGISTRY_UPDATED",
          streams: streams
        },
        { frameId: 0 }
      ).catch(() => {});
    }
  }
}

chrome.webRequest.onHeadersReceived.addListener(
  (details) => {
    let tabId = details.tabId;
    if (tabId < 0) {
      chrome.tabs.query({ active: true, lastFocusedWindow: true }).then((tabs) => {
        if (tabs && tabs.length > 0) {
          handleMediaResponse(tabs[0].id, details);
        }
      });
      return;
    }
    handleMediaResponse(tabId, details);
  },
  {
    urls: ["<all_urls>"],
    types: ["media", "xmlhttprequest", "other"]
  },
  ["responseHeaders"]
);

// Tab cleanup
chrome.tabs.onUpdated.addListener((tabId, changeInfo) => {
  if (changeInfo.status === "loading") {
    removeTabStreams(tabId);
  }
});

chrome.tabs.onRemoved.addListener((tabId) => {
  removeTabStreams(tabId);
});

// 5. Intercept standard browser downloads (Cross-browser: Chrome onDeterminingFilename / Firefox onCreated)
if (chrome.downloads && chrome.downloads.onDeterminingFilename) {
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
          // IDM Pattern: Pause first, probe Illusion, only cancel if Illusion accepted!
          chrome.downloads.pause(downloadItem.id, async () => {
            const ringData = getFromRingBuffer(downloadItem.url) || {};

            const result = await sendToIllusion({
              url: downloadItem.url,
              filename: downloadItem.filename,
              referer: downloadItem.referrer || ringData.referer || "",
              file_size: downloadItem.fileSize || 0,
              mime_type: downloadItem.mime || "",
              headers: ringData.headers || {}
            });

            if (result && result.success) {
              chrome.downloads.cancel(downloadItem.id, () => {
                chrome.downloads.erase({ id: downloadItem.id });
              });
            } else {
              console.warn("[Illusion] Illusion not available, resuming browser download");
              chrome.downloads.resume(downloadItem.id);
            }
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
} else if (chrome.downloads && chrome.downloads.onCreated) {
  chrome.downloads.onCreated.addListener(async (downloadItem) => {
    chrome.storage.local.get("settings", async (data) => {
      const settings = data.settings || DEFAULT_SETTINGS;
      if (!settings.enabled || !settings.interceptDownloads) return;

      const url = downloadItem.url;
      if (url.startsWith("blob:") || url.startsWith("data:") || url.startsWith("about:")) return;

      const filename = downloadItem.filename || "";
      const ext = filename.split(".").pop().toLowerCase();
      const shouldIntercept =
        settings.fileExtensions.includes(ext) ||
        (downloadItem.fileSize > 0 && downloadItem.fileSize > settings.minFileSizeMB * 1024 * 1024);

      if (shouldIntercept) {
        chrome.downloads.pause(downloadItem.id, async () => {
          const ringData = getFromRingBuffer(downloadItem.url) || {};
          const result = await sendToIllusion({
            url: downloadItem.url,
            filename: downloadItem.filename,
            referer: downloadItem.referrer || ringData.referer || "",
            file_size: downloadItem.fileSize || 0,
            mime_type: downloadItem.mime || "",
            headers: ringData.headers || {}
          });

          if (result && result.success) {
            chrome.downloads.cancel(downloadItem.id, () => {
              chrome.downloads.erase({ id: downloadItem.id });
            });
          } else {
            chrome.downloads.resume(downloadItem.id);
          }
        });
      }
    });
  });
}

// 6. Dual-Channel IDM Communication Bridge (HTTP Loopback + Native Messaging)
const HTTP_BRIDGE_URL = "http://127.0.0.1:9095/add_download";

async function sendToIllusion(details) {
  try {
    let cookiesStr = "";
    try {
      const urlObj = new URL(details.url);
      const mediaDomain = urlObj.hostname;
      let refDomain = "";
      if (details.referer) {
        try { refDomain = new URL(details.referer).hostname; } catch (_) {}
      }

      const cookieMap = new Map();
      // 1. Get all cookies (including HttpOnly) for page/referer URL
      if (details.referer) {
        try {
          const cList = await chrome.cookies.getAll({ url: details.referer });
          if (cList) {
            for (const c of cList) cookieMap.set(c.name, c.value);
          }
        } catch (_) {}
        // Also query domain-level cookies (.youtube.com, etc.)
        try {
          const refHost = new URL(details.referer).hostname;
          const refParts = refHost.split(".");
          const refRoot = refParts.length >= 2 ? refParts.slice(-2).join(".") : refHost;
          const cList1 = await chrome.cookies.getAll({ domain: "." + refRoot });
          if (cList1) {
            for (const c of cList1) cookieMap.set(c.name, c.value);
          }
          const cList2 = await chrome.cookies.getAll({ domain: refRoot });
          if (cList2) {
            for (const c of cList2) cookieMap.set(c.name, c.value);
          }
        } catch (_) {}
      }
      // 2. Get cookies for media URL (e.g. googlevideo.com)
      if (details.url) {
        try {
          const cList = await chrome.cookies.getAll({ url: details.url });
          if (cList) {
            for (const c of cList) cookieMap.set(c.name, c.value);
          }
        } catch (_) {}
      }
      cookiesStr = Array.from(cookieMap.entries()).map(([k, v]) => `${k}=${v}`).join("; ");
    } catch (_) {}

    // Merge cached ring buffer headers if present
    const ringData = getFromRingBuffer(details.url) || {};
    const mergedHeaders = Object.assign({}, ringData.headers || {}, details.headers || {});
    if (cookiesStr && !mergedHeaders.cookie) {
      mergedHeaders.cookie = cookiesStr;
    }

    const candidate = details.candidate || makeCandidate(-1, {
      url: details.url,
      audioUrl: details.audio_url || null,
      quality: details.quality || ""
    }, details.referer || "");
    for (const context of [candidate.primary, candidate.audio].filter(Boolean)) {
      if (!context.cookies && cookiesStr) context.cookies = cookiesStr;
      if (!context.referer) context.referer = details.referer || ringData.referer || "";
      if (!context.userAgent) context.userAgent = navigator.userAgent;
      context.headers = Object.assign({}, mergedHeaders, context.headers || {});
    }

    const payload = {
      action: "add_download",
      url: details.url,
      filename: details.filename || "",
      referer: details.referer || ringData.referer || "",
      cookies: cookiesStr || ringData.headers?.cookie || "",
      user_agent: navigator.userAgent,
      audio_url: details.audio_url || null,
      quality: details.quality || "",
      headers: mergedHeaders,
      media_candidate: candidate
    };

    // 1. Primary Channel: IDM-Grade Local HTTP Loopback Bridge (127.0.0.1:9095)
    try {
      const controller = new AbortController();
      const timeoutId = setTimeout(() => controller.abort(), 1200);
      const httpResp = await fetch(HTTP_BRIDGE_URL, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(payload),
        signal: controller.signal
      });
      clearTimeout(timeoutId);

      const jsonResp = await httpResp.json().catch(() => null);
      if (jsonResp && jsonResp.status === "error") {
        return { success: false, error: jsonResp.message || "Illusion rejected the download" };
      }
      if (httpResp.ok) {
        console.log("[Illusion] HTTP Bridge download queued:", jsonResp);
        return { success: true, method: "http", data: jsonResp };
      }
    } catch (httpErr) {
      console.warn("[Illusion] HTTP Bridge unavailable, falling back to Native Messaging:", httpErr);
    }

    // 2. Secondary Channel: Native Messaging Fallback
    return new Promise((resolve) => {
      chrome.runtime.sendNativeMessage(NATIVE_HOST_NAME, payload, (response) => {
        if (chrome.runtime.lastError) {
          console.warn("[Illusion] Native host error:", chrome.runtime.lastError.message);
          resolve({ success: false, error: chrome.runtime.lastError.message });
        } else if (!response || response.status === "error") {
          const error = (response && response.message) || "Native host rejected the download";
          console.warn("[Illusion] Native host rejected download:", error);
          resolve({ success: false, error });
        } else {
          console.log("[Illusion] Native host response:", response);
          resolve({ success: true, method: "native", data: response });
        }
      });
    });
  } catch (e) {
    console.error("[Illusion] Failed to forward to Illusion:", e);
    return { success: false, error: e.toString() };
  }
}

// 7. Inter-Script Message Router
chrome.runtime.onMessage.addListener((request, sender, sendResponse) => {
  const tabId = sender.tab ? sender.tab.id : -1;

  // Video geometry detected in top frame -> Forward to Floating Bar UI in top frame
  if (request.type === "TOP_VIDEO_DETECTED") {
    if (tabId >= 0 && request.rect) {
      chrome.tabs.sendMessage(tabId, {
        type: "POSITION_BAR",
        rect: request.rect
      }, { frameId: 0 }).catch(() => {});
    }
    return;
  }

  // Video geometry detected inside child iframe -> Forward to top frame
  if (request.type === "IFRAME_VIDEO_RECT") {
    if (tabId >= 0 && request.rect) {
      chrome.tabs.sendMessage(tabId, {
        type: "POSITION_BAR",
        rect: request.rect,
        isIframe: true
      }, { frameId: 0 }).catch(() => {});
    }
    return;
  }

  // Stream discovered via Main-World MSE Hook (e.g. YouTube ytInitialPlayerResponse or fetch)
  if (request.type === "ILLUSION_STREAM_DISCOVERED") {
    if (tabId >= 0 && request.stream && request.stream.url) {
      getTabStreams(tabId).then((streams) => {
        const streamUrl = request.stream.url;
        if (isYouTubeSabrControlUrl(streamUrl)) {
          // Do not turn a control URL into a guessed MP4 just because a page
          // script supplied a fallback MIME type.
          return;
        }
        let itag = null;
        try {
          itag = new URL(streamUrl).searchParams.get("itag");
        } catch (_) {}

        const isAudioItag = ["140", "251", "250", "249", "171", "258", "256"].includes(itag);
        const isAudio = (request.stream.mime || "").startsWith("audio/") || isAudioItag;

        // Match by exact URL.  Reusing an itag from another playback can
        // cross-wire signed requests from different videos.
        const existingIdx = streams.findIndex((s) => {
          return s.url === streamUrl;
        });

        const newItem = {
          url: streamUrl,
          mime: request.stream.mime || "video/mp4",
          quality: request.stream.quality || "720p HD",
          size: request.stream.size || 0,
          audioUrl: request.stream.audioUrl || null,
          isAudio: isAudio,
          codecs: request.stream.codecs || null,
          candidate: makeCandidate(tabId, request.stream, sender.tab ? sender.tab.url : "", {
            contentType: request.stream.mime || "",
            contentLength: Number(request.stream.size) || null
          }),
          timestamp: Date.now()
        };

        if (existingIdx >= 0) {
          streams[existingIdx] = newItem;
        } else {
          streams.push(newItem);
        }

        saveTabStreams(tabId, streams);

        chrome.tabs.sendMessage(tabId, {
          type: "STREAM_REGISTRY_UPDATED",
          streams: streams
        }, { frameId: 0 }).catch(() => {});
      });
    }
    return;
  }

  // User clicked a download row in the Floating Bar UI
  if (request.type === "START_VIDEO_DOWNLOAD" || request.type === "MEDIA_SNIFFED_DOWNLOAD") {
    const referer = request.page_url || (sender.tab ? sender.tab.url : "");
    if (isYouTubeSabrControlUrl(request.url || "")) {
      sendResponse({
        status: "error",
        message: "YouTube exposed a SABR/UMP control request, not a downloadable media stream. Play the video until a direct media request is captured."
      });
      return;
    }
    const selected = request.candidate || null;
    const candidate = selected || makeCandidate(tabId, {
      url: request.url,
      audioUrl: request.audio_url || null,
      quality: request.quality || ""
    }, referer);
    sendToIllusion({
      url: request.url,
      filename: request.filename || "",
      referer: referer,
      audio_url: request.audio_url || null,
      quality: request.quality || "",
      candidate
    }).then((result) => {
      if (result && result.success) {
        sendResponse({ status: "success" });
      } else {
        sendResponse({
          status: "error",
          message: (result && result.error) || "Illusion rejected this media request"
        });
      }
    });
    return true;
  }

  if (request.type === "GET_TAB_STREAMS") {
    getTabStreams(tabId).then((list) => {
      sendResponse({ streams: list });
    });
    return true;
  }

  if (request.type === "OPEN_HELP") {
    chrome.windows.create({
      url: chrome.runtime.getURL("batch_grabber.html"),
      type: "popup",
      width: 720,
      height: 560
    });
    return true;
  }

  if (request.type === "PING_HOST") {
    (async () => {
      // 1. Check primary IDM-grade HTTP Bridge (127.0.0.1:9095)
      try {
        const controller = new AbortController();
        const tid = setTimeout(() => controller.abort(), 800);
        const resp = await fetch("http://127.0.0.1:9095/ping", { signal: controller.signal });
        clearTimeout(tid);
        if (resp.ok) {
          const json = await resp.json();
          sendResponse({ connected: true, method: "http", data: json });
          return;
        }
      } catch (_) {}

      // 2. Fallback check for Native Messaging Host
      chrome.runtime.sendNativeMessage(NATIVE_HOST_NAME, { action: "ping" }, (res) => {
        if (chrome.runtime.lastError || !res) {
          sendResponse({ connected: false, error: (chrome.runtime.lastError && chrome.runtime.lastError.message) || "No response" });
        } else {
          sendResponse({ connected: true, method: "native", data: res });
        }
      });
    })();
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

  chrome.contextMenus.create({
    id: "illusion-batch-grabber",
    title: "Grab All Media on Page with Illusion",
    contexts: ["page"]
  });
});

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === "illusion-batch-grabber") {
    chrome.windows.create({
      url: chrome.runtime.getURL("batch_grabber.html"),
      type: "popup",
      width: 720,
      height: 560
    });
    return;
  }

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

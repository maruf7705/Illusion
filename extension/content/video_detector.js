// Illusion Video & Audio Detector (Isolated World, all_frames: true)
// Observes media elements across documents, shadow roots, and nested iframes
(function () {
  "use strict";

  const isTopFrame = window === window.top;
  const detectedMediaElements = new WeakSet();

  function isValidMedia(el) {
    if (!el || !(el instanceof HTMLMediaElement)) return false;
    const tag = el.tagName.toLowerCase();
    if (tag === "audio") return true;

    // Filter out tiny ads/trackers/icons
    const width = el.clientWidth || el.videoWidth || 0;
    const height = el.clientHeight || el.videoHeight || 0;
    const duration = el.duration || 0;

    if (width > 0 && height > 0) {
      if (width < 160 && height < 90 && duration < 5) {
        return false;
      }
    }

    return true;
  }

  function safeSendMessage(payload) {
    if (typeof chrome === "undefined" || !chrome.runtime?.id) return;
    try {
      chrome.runtime.sendMessage(payload).catch(() => {});
    } catch (_) {}
  }

  function reportVideoElement(el) {
    if (!isValidMedia(el)) return;
    detectedMediaElements.add(el);

    const rect = el.getBoundingClientRect();
    const src = el.currentSrc || el.src || "";

    const payload = {
      type: isTopFrame ? "TOP_VIDEO_DETECTED" : "IFRAME_VIDEO_RECT",
      rect: {
        left: rect.left,
        top: rect.top,
        width: rect.width,
        height: rect.height,
        right: rect.right,
        bottom: rect.bottom
      },
      frameUrl: window.location.href,
      videoSrc: src,
      duration: el.duration || 0,
      isAudio: el.tagName.toLowerCase() === "audio"
    };

    safeSendMessage(payload);

    if (src && !src.startsWith("blob:") && !src.startsWith("data:") && src.startsWith("http")) {
      safeSendMessage({
        type: "ILLUSION_STREAM_DISCOVERED",
        stream: {
          url: src,
          quality: el.videoHeight ? `${el.videoHeight}p` : "Video Stream",
          mime: src.includes(".m3u8") ? "application/vnd.apple.mpegurl" : (el.tagName.toLowerCase() === "audio" ? "audio/mp4" : "video/mp4"),
          size: 0
        }
      });
    }
  }

  // 1. Scan DOM for Media Elements
  function scanForMedia() {
    const mediaList = document.querySelectorAll("video, audio");
    mediaList.forEach((el) => {
      reportVideoElement(el);
      attachListeners(el);
    });
  }

  function attachListeners(el) {
    if (el._illusion_hooked) return;
    el._illusion_hooked = true;

    ["play", "playing", "loadedmetadata", "timeupdate", "mouseenter"].forEach((evtName) => {
      el.addEventListener(
        evtName,
        () => {
          reportVideoElement(el);
        },
        { passive: true }
      );
    });
  }

  // 2. MutationObserver for dynamic players
  const observer = new MutationObserver((mutations) => {
    for (const m of mutations) {
      for (const node of m.addedNodes) {
        if (node.nodeType === Node.ELEMENT_NODE) {
          if (node.tagName === "VIDEO" || node.tagName === "AUDIO") {
            reportVideoElement(node);
            attachListeners(node);
          } else if (node.querySelectorAll) {
            const children = node.querySelectorAll("video, audio");
            children.forEach((c) => {
              reportVideoElement(c);
              attachListeners(c);
            });
          }
        }
      }
    }
  });

  observer.observe(document.documentElement || document.body, {
    childList: true,
    subtree: true
  });

  // 3. Listen to CustomEvents and cross-world postMessages from Main-World Hook
  function onDiscoveredStream(detail) {
    if (detail && detail.url) {
      safeSendMessage({
        type: "ILLUSION_STREAM_DISCOVERED",
        stream: detail
      });
    }
  }

  window.addEventListener("__ILLUSION_STREAM_DISCOVERED__", (e) => {
    onDiscoveredStream(e.detail);
  });

  window.addEventListener("message", (e) => {
    if (e.data && e.data.source === "illusion_mse_hook" && e.data.type === "__ILLUSION_STREAM_DISCOVERED__") {
      onDiscoveredStream(e.data.detail);
    }
  });

  window.addEventListener("__ILLUSION_MSE_DETECTED__", (e) => {
    if (e.detail) {
      safeSendMessage({
        type: "ILLUSION_MSE_DETECTED",
        detail: e.detail
      });
    }
  });

  // Initialize
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", scanForMedia);
  } else {
    scanForMedia();
  }

  const scanInterval = setInterval(() => {
    if (typeof chrome === "undefined" || !chrome.runtime?.id) {
      clearInterval(scanInterval);
      return;
    }
    scanForMedia();
  }, 2000);
})();

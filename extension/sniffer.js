// Illusion Media Sniffer Content Script
(function () {
  "use strict";

  const ATTACHED_CLASS = "illusion-attached";
  const MEDIA_EXTS = [".mp4", ".m3u8", ".mpd", ".webm", ".mkv", ".flv", ".mov", ".ts", ".mp3", ".wav", ".aac", ".ogg"];

  function isMediaUrl(url) {
    if (!url || typeof url !== "string") return false;
    const cleanUrl = url.split("?")[0].toLowerCase();
    return MEDIA_EXTS.some(ext => cleanUrl.endsWith(ext)) || url.includes(".m3u8") || url.includes(".mpd");
  }

  function createDownloadButton(mediaSrc, mediaElem) {
    const btn = document.createElement("div");
    btn.className = "illusion-sniffer-btn";
    btn.innerHTML = `
      <svg class="illusion-sniffer-icon" viewBox="0 0 24 24">
        <path d="M19.35 10.04C18.67 6.59 15.64 4 12 4 9.11 4 6.6 5.64 5.35 8.04 2.34 8.36 0 10.91 0 14c0 3.31 2.69 6 6 6h13c2.76 0 5-2.24 5-5 0-2.64-2.05-4.78-4.65-4.96zM17 13l-5 5-5-5h3V9h4v4h3z"/>
      </svg>
      <span>Download with Illusion</span>
    `;

    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      e.preventDefault();

      let filename = "";
      try {
        const urlObj = new URL(mediaSrc);
        filename = urlObj.pathname.split("/").pop();
      } catch (_) {}

      if (!filename || filename.length === 0) {
        filename = (document.title || "video").replace(/[^a-zA-Z0-9_-]/g, "_") + ".mp4";
      }

      chrome.runtime.sendMessage({
        type: "MEDIA_SNIFFED_DOWNLOAD",
        url: mediaSrc,
        filename: filename
      });
    });

    return btn;
  }

  function scanAndAttach() {
    const mediaElements = document.querySelectorAll("video, audio");
    mediaElements.forEach((el) => {
      if (el.classList.contains(ATTACHED_CLASS)) return;

      let src = el.currentSrc || el.src;
      if (!src) {
        const sourceChild = el.querySelector("source");
        if (sourceChild && sourceChild.src) {
          src = sourceChild.src;
        }
      }

      if (src && (isMediaUrl(src) || src.startsWith("http"))) {
        el.classList.add(ATTACHED_CLASS);
        const container = el.parentElement;
        if (container) {
          const style = window.getComputedStyle(container);
          if (style.position === "static") {
            container.style.position = "relative";
          }
          const btn = createDownloadButton(src, el);
          container.appendChild(btn);
        }
      }
    });
  }

  // Initial scan and observer for dynamically inserted video players
  scanAndAttach();
  const observer = new MutationObserver(() => scanAndAttach());
  observer.observe(document.body || document.documentElement, {
    childList: true,
    subtree: true
  });
})();

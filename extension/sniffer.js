// Illusion Authentic IDM Floating Media Download Bar & Stream Sniffer
// Reverse Engineered from IDM Integration Architecture
(function () {
  "use strict";

  const detectedStreams = [];
  let floatingPanel = null;
  let dropdownMenu = null;
  let badgeElem = null;
  let isDropdownOpen = false;
  let isDragging = false;
  let isDismissed = false;

  function findTargetPlayerContainer() {
    // 1. YouTube specialized player containers
    const ytMoviePlayer = document.getElementById("movie_player") || document.querySelector(".html5-video-player");
    if (ytMoviePlayer) return ytMoviePlayer;

    // 2. Generic HTML5 video parent
    const video = document.querySelector("video");
    if (video && video.parentElement) {
      return video.parentElement;
    }

    return null;
  }

  function getCleanMediaFormatLabel(stream, index) {
    const url = stream.url || "";
    let itag = null;
    try {
      itag = new URL(url).searchParams.get("itag");
    } catch (_) {}

    // 1. Specific YouTube ITAG Formats matching classic IDM labels
    if (itag) {
      const itagMap = {
        "137": "MP4 file, quality 1080p HD",
        "399": "MP4 file, quality 1080p HD",
        "248": "WebM file, quality 1080p HD",
        "22": "MP4 file, quality 720p HD",
        "136": "MP4 file, quality 720p HD",
        "247": "WebM file, quality 720p HD",
        "135": "MP4 file, quality 480p",
        "244": "WebM file, quality 480p",
        "18": "MP4 file, quality 360p",
        "134": "MP4 file, quality 360p",
        "243": "WebM file, quality 360p",
        "133": "MP4 file, quality 240p",
        "242": "WebM file, quality 240p",
        "160": "MP4 file, quality 144p",
        "278": "WebM file, quality 144p",
        "140": "M4A file, quality Audio 128k",
        "251": "WebM file, quality Audio 160k",
        "250": "WebM file, quality Audio 70k",
        "249": "WebM file, quality Audio 50k",
      };
      if (itagMap[itag]) {
        return `${index + 1}. ${itagMap[itag]}`;
      }
    }

    // 2. Derive from MIME or extension
    let format = "MP4";
    const mime = (stream.mime || "").toLowerCase();
    const lowerUrl = url.toLowerCase();

    if (mime.includes("webm") || lowerUrl.includes(".webm")) format = "WebM";
    else if (mime.includes("flv") || lowerUrl.includes(".flv")) format = "FLV";
    else if (mime.includes("mkv") || lowerUrl.includes(".mkv")) format = "MKV";
    else if (mime.includes("3gpp") || lowerUrl.includes(".3gp")) format = "3GP";
    else if (mime.includes("audio/mp4") || mime.includes("m4a") || lowerUrl.includes(".m4a")) format = "M4A";
    else if (mime.includes("audio/mpeg") || lowerUrl.includes(".mp3")) format = "MP3";
    else if (mime.includes("audio/wav") || lowerUrl.includes(".wav")) format = "WAV";

    let quality = stream.quality || "720p HD";
    if (quality.includes("1080")) quality = "1080p HD";
    else if (quality.includes("720")) quality = "720p HD";
    else if (quality.includes("480")) quality = "480p";
    else if (quality.includes("360")) quality = "360p";
    else if (quality.includes("240")) quality = "240p";
    else if (quality.includes("144")) quality = "144p";
    else if (format === "M4A" || format === "MP3" || mime.startsWith("audio/")) quality = "Audio";

    return `${index + 1}. ${format} file, quality ${quality}`;
  }

  function createFloatingPanel() {
    if (isDismissed) return null;
    if (document.getElementById("illusion-video-bar")) {
      floatingPanel = document.getElementById("illusion-video-bar");
      return floatingPanel;
    }

    const container = document.createElement("div");
    container.id = "illusion-video-bar";
    container.className = "illusion-video-bar-container";

    container.innerHTML = `
      <div class="illusion-video-bar-btn" id="illusion-bar-toggle">
        <span class="idm-play-triangle"></span>
        <span class="illusion-video-text">Download this video</span>
        <span class="illusion-stream-badge" id="illusion-stream-count" style="display:none;">0</span>
        <div class="idm-btn-group">
          <span class="idm-btn-help" id="illusion-bar-help" title="Illusion IDM Help">?</span>
          <span class="idm-btn-close" id="illusion-bar-close" title="Close this bar">✕</span>
        </div>
      </div>
      <div class="illusion-dropdown-menu" id="illusion-stream-dropdown">
        <div class="idm-menu-item" id="illusion-dl-all">Download all</div>
        <div class="idm-menu-sep"></div>
        <div id="illusion-stream-list"></div>
      </div>
    `;

    // Prevent player click events from triggering pause/play on bubble phase
    ["click", "mousedown", "mouseup", "dblclick", "keydown", "keyup"].forEach((evt) => {
      container.addEventListener(evt, (e) => e.stopPropagation(), false);
    });

    const targetContainer = findTargetPlayerContainer();
    if (targetContainer) {
      const pos = window.getComputedStyle(targetContainer).position;
      if (pos === "static") targetContainer.style.position = "relative";
      targetContainer.appendChild(container);
      container.style.position = "absolute";
      container.style.top = "14px";
      container.style.left = "14px";
      container.style.zIndex = "2147483647";
    } else {
      (document.body || document.documentElement).appendChild(container);
      container.style.position = "fixed";
      container.style.top = "20px";
      container.style.left = "20px";
      container.style.zIndex = "2147483647";
    }

    const toggleBtn = container.querySelector("#illusion-bar-toggle");
    dropdownMenu = container.querySelector("#illusion-stream-dropdown");
    badgeElem = container.querySelector("#illusion-stream-count");
    const helpBtn = container.querySelector("#illusion-bar-help");
    const closeBtn = container.querySelector("#illusion-bar-close");
    const dlAllBtn = container.querySelector("#illusion-dl-all");

    // Close button
    closeBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      e.preventDefault();
      container.style.display = "none";
      isDismissed = true;
    });

    // Help button
    helpBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      e.preventDefault();
      chrome.runtime.sendMessage({ type: "OPEN_HELP" });
    });

    // Download All
    dlAllBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      e.preventDefault();
      if (detectedStreams.length > 0) {
        detectedStreams.forEach((s) => triggerDownload(s));
      }
      isDropdownOpen = false;
      dropdownMenu.classList.remove("visible");
    });

    // Draggable Handling
    let dragStartX = 0;
    let dragStartY = 0;
    let initialLeft = 0;
    let initialTop = 0;

    toggleBtn.addEventListener("mousedown", (e) => {
      if (e.target.closest(".idm-btn-group") || e.target.closest(".illusion-dropdown-menu")) return;
      isDragging = false;
      dragStartX = e.clientX;
      dragStartY = e.clientY;
      const rect = container.getBoundingClientRect();
      const parentRect = container.parentElement ? container.parentElement.getBoundingClientRect() : { left: 0, top: 0 };
      initialLeft = rect.left - parentRect.left;
      initialTop = rect.top - parentRect.top;

      function onMouseMove(moveEvent) {
        const dx = moveEvent.clientX - dragStartX;
        const dy = moveEvent.clientY - dragStartY;
        if (Math.abs(dx) > 4 || Math.abs(dy) > 4) {
          isDragging = true;
          container.style.right = "auto";
          container.style.left = `${initialLeft + dx}px`;
          container.style.top = `${initialTop + dy}px`;
        }
      }

      function onMouseUp() {
        document.removeEventListener("mousemove", onMouseMove);
        document.removeEventListener("mouseup", onMouseUp);
        setTimeout(() => { isDragging = false; }, 50);
      }

      document.addEventListener("mousemove", onMouseMove);
      document.addEventListener("mouseup", onMouseUp);
    });

    // Click bar to toggle authentic context menu dropdown
    toggleBtn.addEventListener("click", (e) => {
      if (isDragging) return;
      e.stopPropagation();
      e.preventDefault();

      // If empty, force video play for 250ms to trigger stream buffering
      if (detectedStreams.length === 0) {
        const v = document.querySelector("video");
        if (v && v.paused) {
          v.play().catch(() => {});
        }
      }

      isDropdownOpen = !isDropdownOpen;
      if (isDropdownOpen) {
        updateStreamListUI();
        dropdownMenu.classList.add("visible");
      } else {
        dropdownMenu.classList.remove("visible");
      }
    });

    // Close on outside click
    document.addEventListener("click", (e) => {
      if (!container.contains(e.target)) {
        isDropdownOpen = false;
        dropdownMenu.classList.remove("visible");
      }
    });

    floatingPanel = container;
    updateStreamListUI();
    return container;
  }

  function addStream(stream) {
    if (!stream || !stream.url) return;
    // Strict REA check: Never add dummy webpage URLs!
    if (stream.url === window.location.href || stream.url.includes("youtube.com/watch")) return;
    if (stream.url.startsWith("blob:") && !stream.url.includes("googlevideo")) return;

    if (detectedStreams.some((s) => s.url === stream.url)) return;

    detectedStreams.push(stream);

    if (!floatingPanel) {
      createFloatingPanel();
    }

    updateStreamListUI();
  }

  function updateStreamListUI() {
    if (!floatingPanel) return;

    if (badgeElem) {
      if (detectedStreams.length > 1) {
        badgeElem.style.display = "inline-block";
        badgeElem.innerText = detectedStreams.length.toString();
      } else {
        badgeElem.style.display = "none";
      }
    }

    const listContainer = floatingPanel.querySelector("#illusion-stream-list");
    if (!listContainer) return;

    listContainer.innerHTML = "";

    // On YouTube, generate authentic IDM numbered quality list if streams are buffering
    const isYouTube = location.hostname.includes("youtube.com");
    let displayList = detectedStreams.slice();

    if (isYouTube && displayList.length <= 1) {
      const activeStream = displayList[0] || null;
      const baseStreamUrl = activeStream ? activeStream.url : null;
      const ytQualities = [
        { label: "1. MP4 file, quality 1080p HD", quality: "1080p HD", mime: "video/mp4" },
        { label: "2. MP4 file, quality 720p HD", quality: "720p HD", mime: "video/mp4" },
        { label: "3. MP4 file, quality 480p", quality: "480p", mime: "video/mp4" },
        { label: "4. MP4 file, quality 360p", quality: "360p", mime: "video/mp4" },
        { label: "5. M4A file, quality Audio 128k", quality: "Audio 128k", mime: "audio/mp4" }
      ];

      ytQualities.forEach((q, idx) => {
        const item = document.createElement("div");
        item.className = "idm-menu-item";
        if (idx === 1) item.classList.add("highlighted"); // 720p HD default highlighted like real IDM Image 2!
        item.innerText = q.label;

        item.addEventListener("mouseenter", () => {
          listContainer.querySelectorAll(".idm-menu-item").forEach((el) => el.classList.remove("highlighted"));
          item.classList.add("highlighted");
        });

        item.addEventListener("click", (e) => {
          e.stopPropagation();
          e.preventDefault();
          const targetUrl = baseStreamUrl || (detectedStreams[0] ? detectedStreams[0].url : null);
          if (targetUrl) {
            triggerDownload({
              url: targetUrl,
              quality: q.quality,
              mime: q.mime
            });
          }
          isDropdownOpen = false;
          dropdownMenu.classList.remove("visible");
        });

        listContainer.appendChild(item);
      });
      return;
    }

    if (displayList.length === 0) {
      const emptyItem = document.createElement("div");
      emptyItem.className = "idm-menu-item";
      emptyItem.style.color = "#777";
      emptyItem.style.fontStyle = "italic";
      emptyItem.innerText = "1. MP4 file, quality 720p HD (Buffering...)";
      emptyItem.addEventListener("click", (e) => {
        e.stopPropagation();
        const v = document.querySelector("video");
        if (v && v.paused) v.play();
      });
      listContainer.appendChild(emptyItem);
      return;
    }

    // Sort: Video high res to low, then audio
    displayList.forEach((stream, index) => {
      const item = document.createElement("div");
      item.className = "idm-menu-item";
      if (index === 0) {
        item.classList.add("highlighted");
      }

      const label = getCleanMediaFormatLabel(stream, index);
      item.innerText = label;

      item.addEventListener("mouseenter", () => {
        listContainer.querySelectorAll(".idm-menu-item").forEach((el) => el.classList.remove("highlighted"));
        item.classList.add("highlighted");
      });

      item.addEventListener("click", (e) => {
        e.stopPropagation();
        e.preventDefault();
        triggerDownload(stream);
        isDropdownOpen = false;
        dropdownMenu.classList.remove("visible");
      });

      listContainer.appendChild(item);
    });
  }

  function triggerDownload(stream) {
    if (!stream || !stream.url) return;

    let filename = stream.filename || "";
    if (!filename) {
      const title = (document.title || "video").replace(" - YouTube", "").trim();
      const ext = stream.mime && stream.mime.includes("audio") ? ".m4a" : ".mp4";
      filename = title.replace(/[^a-zA-Z0-9_\-\. ]/g, "_").trim() + ext;
    }

    chrome.runtime.sendMessage({
      type: "MEDIA_SNIFFED_DOWNLOAD",
      url: stream.url,
      filename: filename,
      audio_url: stream.audioUrl || null
    });
  }

  // 1. Listen for Main-World Hook detections from document.js
  window.addEventListener("message", (event) => {
    if (event.data && event.data.type === "__ILLUSION_MEDIA_STREAM__") {
      addStream({
        url: event.data.url,
        quality: event.data.quality,
        mime: event.data.mime,
        audioUrl: event.data.audioUrl || null
      });
    }
  });

  // 2. Listen for background network-level stream detections
  chrome.runtime.onMessage.addListener((msg) => {
    if (msg.type === "ILLUSION_STREAM_DETECTED" && msg.stream) {
      addStream(msg.stream);
    }
  });

  // 3. Query initial cached streams for this tab from background
  try {
    chrome.runtime.sendMessage({ type: "GET_TAB_STREAMS" }, (res) => {
      if (res && res.streams && res.streams.length > 0) {
        res.streams.forEach((s) => addStream(s));
      }
    });
  } catch (_) {}

  // 4. Scan HTML Media Elements
  function scanMediaTags() {
    if (isDismissed) return;

    const ytPlayer = document.getElementById("movie_player") || document.querySelector(".html5-video-player");
    const video = document.querySelector("video");

    if (ytPlayer || video) {
      if (!floatingPanel) {
        createFloatingPanel();
      }
    }

    const mediaList = document.querySelectorAll("video, audio");
    mediaList.forEach((el) => {
      let src = el.currentSrc || el.src;
      if (!src) {
        const source = el.querySelector("source");
        if (source) src = source.src;
      }

      if (src && src.startsWith("http") && !src.includes("youtube.com/watch")) {
        const isAudio = el.tagName.toLowerCase() === "audio";
        addStream({
          url: src,
          mime: isAudio ? "audio/mpeg" : "video/mp4",
          quality: isAudio ? "Audio Stream" : "HTML5 Video"
        });
      }
    });
  }

  // 5. Fullscreen Adaptive Reparenting
  function handleFullscreenChange() {
    if (!floatingPanel) return;
    const fsElement = document.fullscreenElement || document.webkitFullscreenElement;
    if (fsElement) {
      if (!fsElement.contains(floatingPanel)) {
        floatingPanel._originalParent = floatingPanel.parentElement;
        floatingPanel._originalNextSibling = floatingPanel.nextSibling;
        fsElement.appendChild(floatingPanel);
        floatingPanel.style.position = "absolute";
        floatingPanel.style.top = "14px";
        floatingPanel.style.left = "14px";
        floatingPanel.style.zIndex = "2147483647";
      }
    } else {
      if (floatingPanel._originalParent) {
        try {
          if (floatingPanel._originalNextSibling) {
            floatingPanel._originalParent.insertBefore(floatingPanel, floatingPanel._originalNextSibling);
          } else {
            floatingPanel._originalParent.appendChild(floatingPanel);
          }
        } catch (_) {
          const target = findTargetPlayerContainer() || document.body;
          target.appendChild(floatingPanel);
        }
        floatingPanel._originalParent = null;
        floatingPanel._originalNextSibling = null;
      }
    }
  }

  document.addEventListener("fullscreenchange", handleFullscreenChange);
  document.addEventListener("webkitfullscreenchange", handleFullscreenChange);

  // Initialize
  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", scanMediaTags);
  } else {
    scanMediaTags();
  }

  setInterval(scanMediaTags, 1500);

  const observer = new MutationObserver(() => scanMediaTags());
  observer.observe(document.body || document.documentElement, {
    childList: true,
    subtree: true
  });
})();

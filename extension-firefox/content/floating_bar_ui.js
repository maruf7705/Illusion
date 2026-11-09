// Illusion IDM Floating Bar UI (Top Frame Only, Closed Shadow DOM)
// Renders the authentic IDM Aero bar and Windows context menu with complete CSS isolation
(function () {
  "use strict";

  if (window !== window.top) return; // Top frame only

  let hostElement = null;
  let shadowRoot = null;
  let barContainer = null;
  let dropdownMenu = null;
  let isDropdownOpen = false;
  let isDragging = false;
  let isDismissed = false;
  let currentStreams = [];
  let hideTimer = null;
  let targetVideoRect = null;
  let pollInterval = null;

  function cleanMediaUrl(url) {
    if (!url || typeof url !== "string") return "";
    try {
      const u = new URL(url);
      u.searchParams.delete("bytestart");
      u.searchParams.delete("byteend");
      u.searchParams.delete("range");
      u.searchParams.delete("rn");
      u.searchParams.delete("alr");
      u.searchParams.delete("sq");
      u.searchParams.delete("segment");
      u.searchParams.delete("frag");
      u.searchParams.delete("sabr");
      u.searchParams.delete("rqh");
      u.searchParams.delete("ump");
      u.searchParams.delete("len");
      u.searchParams.delete("begin");
      return u.toString();
    } catch (_) {
      return url;
    }
  }

  function addStream(stream) {
    if (!stream || !stream.url) return;
    const url = cleanMediaUrl(stream.url);
    stream.url = url;
    if (stream.audioUrl) {
      stream.audioUrl = cleanMediaUrl(stream.audioUrl);
    }

    let itag = stream.itag || null;
    if (!itag) {
      try {
        itag = new URL(url).searchParams.get("itag");
      } catch (_) {}
    }
    if (itag) {
      stream.itag = itag.toString();
    }

    const idx = currentStreams.findIndex((s) => {
      if (s.url === url) return true;
      if (itag && s.itag && s.itag.toString() === itag.toString()) {
        return true;
      }
      return false;
    });

    if (idx >= 0) {
      const existing = currentStreams[idx];
      currentStreams[idx] = {
        ...existing,
        ...stream,
        url: stream.url || existing.url,
        audioUrl: stream.audioUrl || existing.audioUrl,
        size: Math.max(existing.size || 0, stream.size || 0),
        quality: (stream.quality && stream.quality !== "720p HD") ? stream.quality : (existing.quality || stream.quality)
      };
    } else {
      currentStreams.push(stream);
    }

    window.__ILLUSION_DISCOVERED_STREAMS__ = currentStreams;
    updateBadge();
    if (isDropdownOpen) {
      renderStreamList();
    }
  }

  function updateBadge() {
    if (!barContainer) return;
    const badge = barContainer.querySelector("#illusion-stream-count");
    if (badge) {
      if (currentStreams.length > 1) {
        badge.style.display = "inline-block";
        badge.innerText = currentStreams.length.toString();
      } else {
        badge.style.display = "none";
      }
    }
  }

  function requestStreamsUpdate() {
    try {
      window.dispatchEvent(new CustomEvent("__ILLUSION_TRIGGER_INSPECT__"));
      document.dispatchEvent(new CustomEvent("__ILLUSION_TRIGGER_INSPECT__"));
      window.postMessage({ type: "__ILLUSION_TRIGGER_INSPECT__" }, "*");
    } catch (_) {}

    // Check page window via Firefox Xray Vision wrappedJSObject
    try {
      const pageWin = window.wrappedJSObject || window;
      const pageStreams = pageWin.__ILLUSION_DISCOVERED_STREAMS__;
      if (pageStreams && Array.isArray(pageStreams)) {
        pageStreams.forEach((s) => addStream(s));
      }

      // Direct read ytInitialPlayerResponse or movie_player from page context
      let pr = null;
      try {
        const mp = pageWin.document ? pageWin.document.getElementById("movie_player") : null;
        if (mp && typeof mp.getPlayerResponse === "function") {
          pr = mp.getPlayerResponse();
        }
      } catch (_) {}
      if (!pr) {
        pr = pageWin.ytInitialPlayerResponse;
      }
      if (!pr && pageWin.ytplayer && pageWin.ytplayer.config && pageWin.ytplayer.config.args) {
        try {
          pr = JSON.parse(pageWin.ytplayer.config.args.raw_player_response || pageWin.ytplayer.config.args.player_response);
        } catch (_) {}
      }

      if (pr && pr.streamingData) {
        const streaming = pr.streamingData;
        const formats = streaming.formats || [];
        const adaptive = streaming.adaptiveFormats || [];

        let bestAudio = null;
        for (const f of adaptive) {
          if (f.mimeType && f.mimeType.includes("audio")) {
            let aUrl = f.url;
            if (!aUrl && (f.signatureCipher || f.cipher)) {
              try { aUrl = new URLSearchParams(f.signatureCipher || f.cipher).get("url"); } catch (_) {}
            }
            if (aUrl) {
              bestAudio = cleanMediaUrl(aUrl);
              break;
            }
          }
        }

        formats.forEach((f) => {
          let u = f.url;
          if (!u && (f.signatureCipher || f.cipher)) {
            try { u = new URLSearchParams(f.signatureCipher || f.cipher).get("url"); } catch (_) {}
          }
          if (u) {
            addStream({
              url: cleanMediaUrl(u),
              itag: f.itag ? f.itag.toString() : null,
              quality: f.qualityLabel || `${f.height || 360}p`,
              mime: f.mimeType || "video/mp4",
              size: parseInt(f.contentLength || "0", 10) || 0,
              audioUrl: null
            });
          }
        });

        adaptive.forEach((f) => {
          const isAudio = f.mimeType && f.mimeType.includes("audio");
          let u = f.url;
          if (!u && (f.signatureCipher || f.cipher)) {
            try { u = new URLSearchParams(f.signatureCipher || f.cipher).get("url"); } catch (_) {}
          }
          if (u) {
            const qLabel = f.qualityLabel || (isAudio ? (f.bitrate ? `Audio ${(f.bitrate/1000).toFixed(0)}k` : "Audio 128k") : `${f.height || 720}p`);
            addStream({
              url: cleanMediaUrl(u),
              itag: f.itag ? f.itag.toString() : null,
              quality: qLabel,
              mime: f.mimeType || (isAudio ? "audio/mp4" : "video/mp4"),
              size: parseInt(f.contentLength || "0", 10) || 0,
              audioUrl: isAudio ? null : bestAudio
            });
          }
        });
      }
    } catch (_) {}

    const videos = document.querySelectorAll("video");
    videos.forEach((v) => {
      const src = v.currentSrc || v.src;
      if (src && !src.startsWith("blob:") && !src.startsWith("data:") && src.startsWith("http")) {
        addStream({
          url: cleanMediaUrl(src),
          quality: v.videoHeight ? `${v.videoHeight}p` : "Video Stream",
          mime: src.includes(".m3u8") ? "application/vnd.apple.mpegurl" : "video/mp4",
          size: 0
        });
      }
    });

    if (typeof chrome !== "undefined" && chrome.runtime?.id) {
      try {
        chrome.runtime.sendMessage({ type: "GET_TAB_STREAMS" }, (resp) => {
          if (!chrome.runtime?.id || chrome.runtime.lastError) return;
          if (resp && Array.isArray(resp.streams) && resp.streams.length > 0) {
            resp.streams.forEach((s) => addStream(s));
          }
        });
      } catch (_) {}
    }
  }

  function initShadowDOM() {
    if (document.getElementById("illusion-ext-root")) {
      hostElement = document.getElementById("illusion-ext-root");
      return;
    }

    hostElement = document.createElement("div");
    hostElement.id = "illusion-ext-root";
    hostElement.style.all = "initial";
    hostElement.style.position = "fixed";
    hostElement.style.top = "0px";
    hostElement.style.left = "0px";
    hostElement.style.width = "0px";
    hostElement.style.height = "0px";
    hostElement.style.zIndex = "2147483647";
    hostElement.style.pointerEvents = "none";

    const targetMount = document.fullscreenElement || document.documentElement || document.body;
    targetMount.appendChild(hostElement);

    // Create Closed Shadow Root
    shadowRoot = hostElement.attachShadow({ mode: "closed" });

    // Inject isolated IDM Aero & Windows context menu CSS
    const style = document.createElement("style");
    style.textContent = `
      * {
        box-sizing: border-box;
        margin: 0;
        padding: 0;
        user-select: none;
      }

      .illusion-bar-wrapper {
        position: fixed;
        top: 20px;
        left: 20px;
        z-index: 2147483647;
        font-family: Tahoma, "Segoe UI", Arial, sans-serif;
        font-size: 11px;
        line-height: normal;
        pointer-events: auto;
        display: inline-flex;
        flex-direction: column;
        direction: ltr;
        text-align: left;
        transition: opacity 0.25s ease;
      }

      .illusion-bar-btn {
        display: inline-flex;
        align-items: center;
        height: 22px;
        padding: 0 4px 0 6px;
        background: linear-gradient(to bottom, #f2f7fd 0%, #e1edf9 40%, #c4ddf7 50%, #d4e7fa 100%);
        border: 1px solid #7599cb;
        border-radius: 3px;
        box-shadow: 0 1px 3px rgba(0, 0, 0, 0.35), inset 0 1px 0 rgba(255, 255, 255, 0.9);
        cursor: pointer;
        gap: 5px;
      }

      .illusion-bar-btn:hover {
        background: linear-gradient(to bottom, #f8fbfe 0%, #ebf3fc 40%, #d2e4f9 50%, #e0effc 100%);
        border-color: #5884bf;
      }

      .illusion-bar-btn:active {
        background: linear-gradient(to bottom, #c6ddf7 0%, #b8d4f4 50%, #d0e4f9 100%);
      }

      .idm-play-triangle {
        width: 0;
        height: 0;
        border-top: 5px solid transparent;
        border-bottom: 5px solid transparent;
        border-left: 9px solid #1ba829;
        filter: drop-shadow(0 1px 0 #0d610d);
        margin-right: 2px;
        flex-shrink: 0;
      }

      .idm-title-text {
        font-family: Tahoma, "Segoe UI", Arial, sans-serif;
        font-size: 11px;
        font-weight: bold;
        color: #1a3b68;
        text-shadow: 0 1px 0 rgba(255, 255, 255, 0.85);
        white-space: nowrap;
        line-height: 1;
        letter-spacing: -0.1px;
      }

      .idm-badge {
        background: #2b6cb0;
        color: #ffffff;
        font-size: 9px;
        font-weight: bold;
        padding: 0 4px;
        border-radius: 8px;
        line-height: 13px;
        margin-left: 1px;
      }

      .idm-btn-controls {
        display: inline-flex;
        align-items: center;
        gap: 2px;
        margin-left: 3px;
      }

      .idm-sub-btn {
        width: 14px;
        height: 14px;
        display: inline-flex;
        align-items: center;
        justify-content: center;
        font-family: Tahoma, "Segoe UI", Arial, sans-serif;
        font-size: 9px;
        font-weight: bold;
        color: #1a3b68;
        background: linear-gradient(to bottom, #fbfdfe 0%, #d8e8f8 100%);
        border: 1px solid #7599cb;
        border-radius: 2px;
        cursor: pointer;
        line-height: 1;
      }

      .idm-sub-btn:hover {
        background: #dbeaf9;
        border-color: #5b87c1;
      }

      .idm-btn-close:hover {
        background: #fde8e8;
        border-color: #e07070;
        color: #c5221f;
      }

      /* Authentic Windows IDM Context Menu Dropdown */
      .illusion-dropdown-menu {
        position: absolute;
        top: 23px;
        left: 0;
        background: #ffffff;
        border: 1px solid #7f9db9;
        box-shadow: 2px 2px 4px rgba(0, 0, 0, 0.3);
        padding: 2px 0;
        min-width: 210px;
        z-index: 2147483647;
        display: none;
        flex-direction: column;
        font-family: Tahoma, "Segoe UI", Arial, sans-serif;
        font-size: 11px;
      }

      .illusion-dropdown-menu.visible {
        display: flex;
      }

      .idm-menu-row {
        padding: 3px 18px 3px 20px;
        font-size: 11px;
        font-family: Tahoma, "Segoe UI", Arial, sans-serif;
        color: #000000;
        background: transparent;
        cursor: pointer;
        white-space: nowrap;
        line-height: 16px;
      }

      .idm-menu-row:hover,
      .idm-menu-row.highlighted {
        background: #b6d6f2;
        outline: 1px solid #7da2ce;
        outline-offset: -1px;
        color: #000000;
      }

      .idm-menu-divider {
        height: 1px;
        background: #e0e0e0;
        margin: 2px 1px;
      }
    `;
    shadowRoot.appendChild(style);

    barContainer = document.createElement("div");
    barContainer.className = "illusion-bar-wrapper";
    barContainer.innerHTML = `
      <div class="illusion-bar-btn" id="illusion-bar-toggle">
        <span class="idm-play-triangle"></span>
        <span class="idm-title-text">Download this video</span>
        <span class="idm-badge" id="illusion-stream-count" style="display:none;">0</span>
        <div class="idm-btn-controls">
          <span class="idm-sub-btn idm-btn-help" id="illusion-bar-help" title="Illusion Help">?</span>
          <span class="idm-sub-btn idm-btn-close" id="illusion-bar-close" title="Close this bar">✕</span>
        </div>
      </div>
      <div class="illusion-dropdown-menu" id="illusion-stream-dropdown">
        <div class="idm-menu-row" id="illusion-dl-all">Download all</div>
        <div class="idm-menu-divider"></div>
        <div id="illusion-stream-list"></div>
      </div>
    `;

    shadowRoot.appendChild(barContainer);

    const toggleBtn = barContainer.querySelector("#illusion-bar-toggle");
    dropdownMenu = barContainer.querySelector("#illusion-stream-dropdown");
    const helpBtn = barContainer.querySelector("#illusion-bar-help");
    const closeBtn = barContainer.querySelector("#illusion-bar-close");
    const dlAllBtn = barContainer.querySelector("#illusion-dl-all");

    // Close button
    closeBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      barContainer.style.display = "none";
      isDismissed = true;
    });

    // Help button
    helpBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      if (typeof chrome !== "undefined" && chrome.runtime?.id) {
        try {
          chrome.runtime.sendMessage({ type: "OPEN_HELP" });
        } catch (_) {}
      }
    });

    // Download All
    dlAllBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      if (currentStreams.length > 0) {
        currentStreams.forEach((s) => triggerDownload(s));
      }
      isDropdownOpen = false;
      dropdownMenu.classList.remove("visible");
    });

    // Click toggle to show/hide dropdown or trigger instant download
    toggleBtn.addEventListener("click", (e) => {
      if (isDragging) return;
      if (e.target.closest(".idm-btn-controls")) return;
      e.stopPropagation();

      requestStreamsUpdate();

      const isBadgeClick = e.target.id === "illusion-stream-count";

      // If user clicked the main button (not badge) and we only have 1 stream, download immediately!
      if (!isBadgeClick && currentStreams.length === 1 && !isDropdownOpen) {
        triggerDownload(currentStreams[0]);
        return;
      }

      isDropdownOpen = !isDropdownOpen;
      if (isDropdownOpen) {
        renderStreamList();
        dropdownMenu.classList.add("visible");
      } else {
        dropdownMenu.classList.remove("visible");
      }
    });

    // Draggable handling
    let dragStartX = 0, dragStartY = 0, initialLeft = 0, initialTop = 0;
    toggleBtn.addEventListener("mousedown", (e) => {
      if (e.target.closest(".idm-btn-controls") || e.target.closest(".illusion-dropdown-menu")) return;
      isDragging = false;
      dragStartX = e.clientX;
      dragStartY = e.clientY;
      const rect = barContainer.getBoundingClientRect();
      initialLeft = rect.left;
      initialTop = rect.top;

      function onMouseMove(moveEvt) {
        const dx = moveEvt.clientX - dragStartX;
        const dy = moveEvt.clientY - dragStartY;
        if (Math.abs(dx) > 4 || Math.abs(dy) > 4) {
          isDragging = true;
          barContainer.style.left = `${initialLeft + dx}px`;
          barContainer.style.top = `${initialTop + dy}px`;
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

    // Close dropdown on click outside
    document.addEventListener("click", (e) => {
      if (hostElement && !e.composedPath().includes(barContainer)) {
        isDropdownOpen = false;
        dropdownMenu.classList.remove("visible");
      }
    });

    // Test & automation dispatch hooks (supports both CustomEvent and cross-world postMessage)
    function handleTestExpand() {
      toggleBtn.click();
    }
    function handleTestDownload(idx) {
      const list = dropdownMenu.querySelector("#illusion-stream-list");
      if (list) {
        const rows = list.querySelectorAll(".idm-menu-row");
        if (rows && rows[idx]) rows[idx].click();
      }
    }

    window.addEventListener("__ILLUSION_TEST_EXPAND__", handleTestExpand);
    document.addEventListener("__ILLUSION_TEST_EXPAND__", handleTestExpand);
    window.addEventListener("__ILLUSION_TEST_DOWNLOAD__", (e) => {
      handleTestDownload((e.detail && e.detail.index) || 0);
    });
    document.addEventListener("__ILLUSION_TEST_DOWNLOAD__", (e) => {
      handleTestDownload((e.detail && e.detail.index) || 0);
    });

    window.addEventListener("message", (e) => {
      if (!e.data) return;
      if (e.data.type === "__ILLUSION_TEST_EXPAND__") {
        handleTestExpand();
      } else if (e.data.type === "__ILLUSION_TEST_DOWNLOAD__") {
        handleTestDownload(e.data.index || 0);
      }
    });
  }

  function renderStreamList() {
    if (!dropdownMenu) return;
    const listContainer = dropdownMenu.querySelector("#illusion-stream-list");
    if (!listContainer) return;
    listContainer.innerHTML = "";

    let displayList = currentStreams.slice();

    function getResolutionRank(quality) {
      const q = (quality || "").toLowerCase();
      if (q.includes("2160") || q.includes("4k")) return 2160;
      if (q.includes("1440") || q.includes("2k")) return 1440;
      if (q.includes("1080")) return 1080;
      if (q.includes("720")) return 720;
      if (q.includes("480")) return 480;
      if (q.includes("360")) return 360;
      if (q.includes("240")) return 240;
      if (q.includes("144")) return 144;
      return 0;
    }

    // Sort streams like IDM: resolution descending, file size descending, audio tracks at bottom
    displayList.sort((a, b) => {
      const aAudio = (a.mime || "").includes("audio") || (a.quality || "").toLowerCase().includes("audio");
      const bAudio = (b.mime || "").includes("audio") || (b.quality || "").toLowerCase().includes("audio");
      if (aAudio !== bAudio) return aAudio ? 1 : -1;
      if (aAudio && bAudio) return (b.size || 0) - (a.size || 0);
      const aRank = getResolutionRank(a.quality);
      const bRank = getResolutionRank(b.quality);
      if (aRank !== bRank) return bRank - aRank;
      return (b.size || 0) - (a.size || 0);
    });

    if (displayList.length === 0) {
      const emptyRow = document.createElement("div");
      emptyRow.className = "idm-menu-row";
      emptyRow.style.color = "#777";
      emptyRow.style.fontStyle = "italic";
      emptyRow.innerText = "1. Finding media streams (Buffering...)";
      emptyRow.addEventListener("click", (e) => {
        e.stopPropagation();
        requestStreamsUpdate();
        if (currentStreams.length > 0) {
          triggerDownload(currentStreams[0]);
          isDropdownOpen = false;
          dropdownMenu.classList.remove("visible");
        } else {
          showFeedback("Buffering video...");
        }
      });
      listContainer.appendChild(emptyRow);

      if (!pollInterval && isDropdownOpen) {
        let attempts = 0;
        pollInterval = setInterval(() => {
          attempts++;
          requestStreamsUpdate();
          if (currentStreams.length > 0 || !isDropdownOpen || attempts > 10) {
            clearInterval(pollInterval);
            pollInterval = null;
            if (currentStreams.length > 0 && isDropdownOpen) {
              renderStreamList();
            }
          }
        }, 300);
      }
      return;
    }

    if (pollInterval) {
      clearInterval(pollInterval);
      pollInterval = null;
    }

    displayList.forEach((stream, index) => {
      const row = document.createElement("div");
      row.className = "idm-menu-row";
      if (index === 0) row.classList.add("highlighted");

      const label = formatMediaLabel(stream, index);
      row.innerText = label;

      row.addEventListener("mouseenter", () => {
        listContainer.querySelectorAll(".idm-menu-row").forEach((el) => el.classList.remove("highlighted"));
        row.classList.add("highlighted");
      });

      row.addEventListener("click", (e) => {
        e.stopPropagation();
        triggerDownload(stream);
        isDropdownOpen = false;
        dropdownMenu.classList.remove("visible");
      });

      listContainer.appendChild(row);
    });
  }

  function formatMediaLabel(stream, index) {
    const url = stream.url || "";
    let itag = stream.itag || null;
    if (!itag) {
      try {
        itag = new URL(url).searchParams.get("itag");
      } catch (_) {}
    }

    let sizeStr = "";
    if (stream.size && stream.size > 0) {
      if (stream.size > 1024 * 1024 * 1024) sizeStr = ` (${(stream.size / (1024 * 1024 * 1024)).toFixed(1)} GB)`;
      else if (stream.size > 1024 * 1024) sizeStr = ` (${(stream.size / (1024 * 1024)).toFixed(1)} MB)`;
      else if (stream.size > 1024) sizeStr = ` (${(stream.size / 1024).toFixed(0)} KB)`;
    }

    if (itag) {
      const itagMap = {
        "248": "WebM file, quality 1080p",
        "137": "MP4 file, quality 1080p HD",
        "399": "MP4 file, quality 1080p",
        "247": "WebM file, quality 720p",
        "136": "MP4 file, quality 720p HD",
        "398": "MP4 file, quality 720p HD",
        "303": "WebM file, quality 1080p60 HD",
        "298": "MP4 file, quality 720p60 HD",
        "244": "WebM file, quality 480p",
        "135": "MP4 file, quality 480p",
        "397": "MP4 file, quality 480p",
        "243": "WebM file, quality 360p",
        "134": "MP4 file, quality 360p",
        "396": "MP4 file, quality 360p",
        "242": "WebM file, quality 240p",
        "133": "MP4 file, quality 240p",
        "395": "MP4 file, quality 240p",
        "278": "WebM file, quality 144p",
        "160": "MP4 file, quality 144p",
        "394": "MP4 file, quality 144p",
        "18": "MP4 file, quality 360p",
        "22": "MP4 file, quality 720p HD",
        "313": "WebM file, quality 2160p 4K",
        "271": "WebM file, quality 1440p 2K",
        "251": "WebM file, quality Audio 160k",
        "140": "M4A file, quality Audio 128k",
        "250": "WebM file, quality Audio 70k",
        "249": "WebM file, quality Audio 50k"
      };
      if (itagMap[itag]) {
        return `${index + 1}. ${itagMap[itag]}${sizeStr}`;
      }
    }

    let format = "MP4";
    const mime = (stream.mime || "").toLowerCase();
    if (mime.includes("webm") || url.includes(".webm")) format = "WebM";
    else if (mime.includes("mkv") || url.includes(".mkv")) format = "MKV";
    else if (mime.includes("m4a") || url.includes(".m4a")) format = "M4A";
    else if (mime.includes("mp3") || url.includes(".mp3")) format = "MP3";
    else if (mime.includes("mpegurl") || url.includes(".m3u8")) format = "HLS (.m3u8)";
    else if (mime.includes("dash") || url.includes(".mpd")) format = "DASH (.mpd)";

    let quality = stream.quality || "720p HD";
    return `${index + 1}. ${format} file, quality ${quality}${sizeStr}`;
  }

  function showFeedback(text, isError = false) {
    if (!barContainer) return;
    const titleEl = barContainer.querySelector(".idm-title-text");
    if (!titleEl) return;
    const origText = titleEl.innerText;
    titleEl.innerText = text;
    titleEl.style.color = isError ? "#e06c75" : "#1ba829";
    setTimeout(() => {
      if (titleEl) {
        titleEl.innerText = origText;
        titleEl.style.color = "";
      }
    }, 2500);
  }

  async function triggerDownload(stream) {
    if (!stream || !stream.url) return;
    const title = (document.title || "video").replace(" - YouTube", "").trim();
    const ext = stream.mime && stream.mime.includes("audio") ? ".m4a" : ".mp4";
    const filename = title.replace(/[^a-zA-Z0-9_\-\. ]/g, "_").trim() + ext;

    // Guarantee audio track pairing for YouTube adaptive streams (IDM parity)
    let effectiveAudioUrl = stream.audioUrl || null;
    const isVideo = !stream.mime || !stream.mime.includes("audio");
    if (!effectiveAudioUrl && isVideo && (stream.url.includes("googlevideo.com") || window.location.hostname.includes("youtube.com"))) {
      const audioMatch = currentStreams.find(s => (s.mime && s.mime.includes("audio")) || ["140", "251", "250", "249"].includes(s.itag));
      if (audioMatch && audioMatch.url && audioMatch.url !== stream.url) {
        effectiveAudioUrl = audioMatch.url;
      }
    }

    const payload = {
      action: "add_download",
      url: stream.url,
      filename: filename,
      referer: window.location.href,
      audio_url: effectiveAudioUrl,
      quality: stream.quality || "",
      headers: {
        Referer: window.location.href,
        "User-Agent": navigator.userAgent
      }
    };

    let dispatched = false;

    // 1. Primary Channel: Extension Background Service (enriches with Cookies & Header Ring Buffer)
    if (typeof chrome !== "undefined" && chrome.runtime && chrome.runtime.id) {
      try {
        const res = await new Promise((resolve) => {
          chrome.runtime.sendMessage({
            type: "START_VIDEO_DOWNLOAD",
            url: stream.url,
            filename: filename,
            audio_url: effectiveAudioUrl,
            page_url: window.location.href,
            quality: stream.quality || ""
          }, (response) => {
            if (chrome.runtime.lastError || (response && response.status === "error")) {
              resolve(false);
            } else {
              resolve(true);
            }
          });
        });
        if (res) {
          dispatched = true;
        }
      } catch (e) {
        console.warn("[Illusion] Extension runtime message failed:", e);
      }
    }

    // 2. Secondary Channel: Direct HTTP Bridge Fallback (only if runtime disconnected)
    if (!dispatched) {
      try {
        const resp = await fetch("http://127.0.0.1:9095/add_download", {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify(payload)
        });
        if (resp.ok) {
          dispatched = true;
        }
      } catch (_) {}
    }

    if (dispatched) {
      showFeedback("✓ Queued to Illusion!", false);
    } else {
      showFeedback("✕ Connection Failed — Launch Illusion", true);
    }
  }

  function positionOverVideo(rect) {
    if (!barContainer || isDismissed) return;
    targetVideoRect = rect;
    // Align to top-left of video player with 14px padding
    barContainer.style.left = `${Math.max(10, rect.left + 14)}px`;
    barContainer.style.top = `${Math.max(10, rect.top + 14)}px`;
    barContainer.style.display = "inline-flex";
    barContainer.style.opacity = "1";
  }

  // Handle Fullscreen adaptive reparenting
  function handleFullscreen() {
    if (!hostElement) return;
    const fsElement = document.fullscreenElement || document.webkitFullscreenElement;
    if (fsElement && !fsElement.contains(hostElement)) {
      fsElement.appendChild(hostElement);
    } else if (!fsElement && hostElement.parentElement !== document.documentElement) {
      document.documentElement.appendChild(hostElement);
    }
  }

  document.addEventListener("fullscreenchange", handleFullscreen);
  document.addEventListener("webkitfullscreenchange", handleFullscreen);

  // Auto-hide bar after 4s inactivity over video
  document.addEventListener("mousemove", (e) => {
    if (!barContainer || !targetVideoRect || isDismissed) return;
    const r = targetVideoRect;
    const isOver = e.clientX >= r.left && e.clientX <= r.right && e.clientY >= r.top && e.clientY <= r.bottom;
    if (isOver) {
      barContainer.style.opacity = "1";
      if (hideTimer) clearTimeout(hideTimer);
      hideTimer = setTimeout(() => {
        if (!isDropdownOpen) {
          barContainer.style.opacity = "0.2";
        }
      }, 4000);
    }
  });

  // Direct stream discovery listeners from page / main-world hook
  window.addEventListener("__ILLUSION_STREAM_DISCOVERED__", (e) => {
    if (e.detail) addStream(e.detail);
  });
  document.addEventListener("__ILLUSION_STREAM_DISCOVERED__", (e) => {
    if (e.detail) addStream(e.detail);
  });
  window.addEventListener("message", (e) => {
    if (e.data && (e.data.type === "__ILLUSION_STREAM_DISCOVERED__" || e.data.type === "ILLUSION_STREAM_DISCOVERED")) {
      const s = e.data.detail || e.data.stream;
      if (s) addStream(s);
    }
  });

  // Listen to background messages
  chrome.runtime.onMessage.addListener((msg) => {
    if (msg.type === "POSITION_BAR" && msg.rect) {
      initShadowDOM();
      positionOverVideo(msg.rect);
    }

    if (msg.type === "STREAM_REGISTRY_UPDATED" && Array.isArray(msg.streams)) {
      msg.streams.forEach((s) => addStream(s));
    }
  });

  // Periodic fallback check for direct video tags in top frame
  function checkTopVideo() {
    if (isDismissed) return;
    const v = document.querySelector("video");
    const ytPlayer = document.getElementById("movie_player") || document.querySelector(".html5-video-player");
    const target = ytPlayer || v;
    if (target) {
      const rect = target.getBoundingClientRect();
      if (rect.width >= 160 && rect.height >= 90) {
        initShadowDOM();
        positionOverVideo(rect);
      }
    }
  }

  // Request already detected streams from background safely
  if (typeof chrome !== "undefined" && chrome.runtime?.id) {
    try {
      chrome.runtime.sendMessage({ type: "GET_TAB_STREAMS" }, (resp) => {
        if (!chrome.runtime?.id || chrome.runtime.lastError) return;
        if (resp && Array.isArray(resp.streams) && resp.streams.length > 0) {
          currentStreams = resp.streams;
          if (barContainer) {
            const badge = barContainer.querySelector("#illusion-stream-count");
            if (badge && currentStreams.length > 1) {
              badge.style.display = "inline-block";
              badge.innerText = currentStreams.length.toString();
            }
          }
        }
      });
    } catch (_) {}
  }

  const topVideoInterval = setInterval(() => {
    checkTopVideo();
  }, 1500);

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", checkTopVideo);
  } else {
    checkTopVideo();
  }
})();

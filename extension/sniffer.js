// Illusion IDM-Grade Video & Audio Sniffer
// Reverse Engineered from IDM Integration Architecture
(function () {
  "use strict";

  const detectedStreams = [];
  let floatingPanel = null;
  let dropdownMenu = null;
  let badgeElem = null;
  let isDropdownOpen = false;

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

  function createFloatingPanel() {
    if (document.getElementById("illusion-video-bar")) {
      floatingPanel = document.getElementById("illusion-video-bar");
      return floatingPanel;
    }

    const container = document.createElement("div");
    container.id = "illusion-video-bar";
    container.className = "illusion-video-bar-container";

    container.innerHTML = `
      <div class="illusion-video-bar-btn" id="illusion-bar-toggle">
        <svg class="illusion-video-icon" viewBox="0 0 24 24">
          <path d="M19.35 10.04C18.67 6.59 15.64 4 12 4 9.11 4 6.6 5.64 5.35 8.04 2.34 8.36 0 10.91 0 14c0 3.31 2.69 6 6 6h13c2.76 0 5-2.24 5-5 0-2.64-2.05-4.78-4.65-4.96zM17 13l-5 5-5-5h3V9h4v4h3z"/>
        </svg>
        <span class="illusion-video-text">Download this video</span>
        <span class="illusion-badge" id="illusion-stream-count">1</span>
        <span class="illusion-arrow" id="illusion-bar-arrow">▼</span>
      </div>
      <div class="illusion-stream-dropdown" id="illusion-stream-dropdown">
        <div class="illusion-stream-header">Available Qualities & Streams</div>
        <div id="illusion-stream-list"></div>
      </div>
    `;

    // Prevent player click events from triggering pause/play
    ["click", "mousedown", "mouseup", "dblclick", "keydown", "keyup"].forEach((evt) => {
      container.addEventListener(evt, (e) => e.stopPropagation(), true);
    });

    const targetContainer = findTargetPlayerContainer();
    if (targetContainer) {
      const pos = window.getComputedStyle(targetContainer).position;
      if (pos === "static") targetContainer.style.position = "relative";
      targetContainer.appendChild(container);
      container.style.position = "absolute";
      container.style.top = "16px";
      container.style.right = "16px";
      container.style.zIndex = "2147483647";
    } else {
      (document.body || document.documentElement).appendChild(container);
      container.style.position = "fixed";
      container.style.top = "20px";
      container.style.right = "20px";
      container.style.zIndex = "2147483647";
    }

    const toggleBtn = container.querySelector("#illusion-bar-toggle");
    dropdownMenu = container.querySelector("#illusion-stream-dropdown");
    badgeElem = container.querySelector("#illusion-stream-count");
    const arrowElem = container.querySelector("#illusion-bar-arrow");

    toggleBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      e.preventDefault();

      if (detectedStreams.length === 1 && !isDropdownOpen) {
        triggerDownload(detectedStreams[0]);
        return;
      }

      isDropdownOpen = !isDropdownOpen;
      if (isDropdownOpen) {
        dropdownMenu.classList.add("visible");
        arrowElem.classList.add("open");
      } else {
        dropdownMenu.classList.remove("visible");
        arrowElem.classList.remove("open");
      }
    });

    document.addEventListener("click", (e) => {
      if (!container.contains(e.target)) {
        isDropdownOpen = false;
        dropdownMenu.classList.remove("visible");
        arrowElem.classList.remove("open");
      }
    });

    floatingPanel = container;
    updateStreamListUI();
    return container;
  }

  function addStream(stream) {
    if (!stream || !stream.url) return;
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
      badgeElem.innerText = Math.max(1, detectedStreams.length).toString();
    }

    const listContainer = floatingPanel.querySelector("#illusion-stream-list");
    if (!listContainer) return;

    listContainer.innerHTML = "";

    // If no streams sniffed yet (e.g. YouTube initial load before playback request)
    if (detectedStreams.length === 0) {
      const defaultItem = document.createElement("div");
      defaultItem.className = "illusion-stream-item";
      defaultItem.innerHTML = `
        <div class="illusion-stream-info">
          <span class="illusion-stream-title">Best Available Quality</span>
          <span class="illusion-stream-meta">${document.title || "Video Stream"}</span>
        </div>
        <svg class="illusion-dl-action-icon" viewBox="0 0 24 24">
          <path d="M19 9h-4V3H9v6H5l7 7 7-7zM5 18v2h14v-2H5z"/>
        </svg>
      `;
      defaultItem.addEventListener("click", (e) => {
        e.stopPropagation();
        triggerDownload({
          url: window.location.href,
          quality: "Best Available Quality"
        });
        isDropdownOpen = false;
        dropdownMenu.classList.remove("visible");
      });
      listContainer.appendChild(defaultItem);
      return;
    }

    detectedStreams.forEach((stream, index) => {
      const item = document.createElement("div");
      item.className = "illusion-stream-item";

      const title = stream.quality || `Stream #${index + 1}`;
      let filename = stream.filename || "";
      if (!filename) {
        try {
          filename = new URL(stream.url).pathname.split("/").pop();
        } catch (_) {}
      }
      if (!filename || filename.length === 0 || filename.includes("videoplayback")) {
        const ext = stream.mime && stream.mime.includes("audio") ? ".m4a" : ".mp4";
        filename = (document.title || "video").replace(" - YouTube", "").replace(/[^a-zA-Z0-9_-]/g, "_") + ext;
      }

      item.innerHTML = `
        <div class="illusion-stream-info">
          <span class="illusion-stream-title">${title}</span>
          <span class="illusion-stream-meta">${filename}</span>
        </div>
        <svg class="illusion-dl-action-icon" viewBox="0 0 24 24">
          <path d="M19 9h-4V3H9v6H5l7 7 7-7zM5 18v2h14v-2H5z"/>
        </svg>
      `;

      item.addEventListener("click", (e) => {
        e.stopPropagation();
        triggerDownload(stream);
        isDropdownOpen = false;
        dropdownMenu.classList.remove("visible");
      });

      listContainer.appendChild(item);
    });
  }

  function triggerDownload(stream) {
    let filename = stream.filename || "";
    if (!filename) {
      try {
        filename = new URL(stream.url).pathname.split("/").pop();
      } catch (_) {}
    }

    if (!filename || filename.length === 0 || filename.includes("videoplayback")) {
      const ext = stream.mime && stream.mime.includes("audio") ? ".m4a" : ".mp4";
      filename = (document.title || "video").replace(" - YouTube", "").replace(/[^a-zA-Z0-9_-]/g, "_") + ext;
    }

    chrome.runtime.sendMessage({
      type: "MEDIA_SNIFFED_DOWNLOAD",
      url: stream.url,
      filename: filename
    });
  }

  // 1. Listen for background network-level stream detections
  chrome.runtime.onMessage.addListener((msg) => {
    if (msg.type === "ILLUSION_STREAM_DETECTED" && msg.stream) {
      addStream(msg.stream);
    }
  });

  // 2. Query initial cached streams for this tab from background
  try {
    chrome.runtime.sendMessage({ type: "GET_TAB_STREAMS" }, (res) => {
      if (res && res.streams && res.streams.length > 0) {
        res.streams.forEach((s) => addStream(s));
      }
    });
  } catch (_) {}

  // 3. YouTube Special Extractor
  function checkYouTubePlayer() {
    if (!location.hostname.includes("youtube.com")) return;

    const ytPlayer = document.getElementById("movie_player") || document.querySelector(".html5-video-player");
    const video = document.querySelector("video");

    if (ytPlayer || video) {
      if (!floatingPanel) {
        createFloatingPanel();
      }

      // Add high quality download options if list is currently empty
      if (detectedStreams.length === 0) {
        const title = (document.title || "YouTube Video").replace(" - YouTube", "").trim();
        addStream({
          url: window.location.href,
          mime: "video/mp4",
          quality: "YouTube High Quality Video (MP4)",
          filename: title.replace(/[^a-zA-Z0-9_-]/g, "_") + ".mp4"
        });
        addStream({
          url: window.location.href,
          mime: "audio/mp4",
          quality: "Audio Track (M4A)",
          filename: title.replace(/[^a-zA-Z0-9_-]/g, "_") + ".m4a"
        });
      }
    }
  }

  // 4. Recursive DOM and Shadow DOM Video/Audio Inspection
  function findMediaElements(root = document) {
    const elements = [];
    function traverse(node) {
      if (!node) return;
      if (node.nodeType === Node.ELEMENT_NODE) {
        const tag = node.tagName.toLowerCase();
        if (tag === "video" || tag === "audio") {
          elements.push(node);
        }
        if (node.shadowRoot) {
          traverse(node.shadowRoot);
        }
      }
      let child = node.firstElementChild;
      while (child) {
        traverse(child);
        child = child.nextElementSibling;
      }
    }
    try {
      traverse(root);
    } catch (_) {}
    return elements;
  }

  function scanMediaTags() {
    checkYouTubePlayer();

    const mediaElements = findMediaElements(document);
    mediaElements.forEach((el) => {
      // If ANY video is found on page, ensure floating panel is present!
      if (!floatingPanel) {
        createFloatingPanel();
      }

      let src = el.currentSrc || el.src;
      if (!src) {
        const source = el.querySelector("source");
        if (source) src = source.src;
      }

      if (src && src.startsWith("http")) {
        const isAudio = el.tagName.toLowerCase() === "audio";
        addStream({
          url: src,
          mime: isAudio ? "audio/mpeg" : "video/mp4",
          quality: isAudio ? "Audio Stream" : "HTML5 Video Stream"
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
        floatingPanel.style.top = "20px";
        floatingPanel.style.right = "20px";
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

  // Periodic check & mutation observer for dynamic players (YouTube SPA navigation)
  setInterval(scanMediaTags, 1500);

  const observer = new MutationObserver(() => scanMediaTags());
  observer.observe(document.body || document.documentElement, {
    childList: true,
    subtree: true
  });
})();
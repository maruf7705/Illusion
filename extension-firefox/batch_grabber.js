// Illusion Batch Media Grabber Controller
(function () {
  "use strict";

  let mediaItems = [];
  let currentFilter = "all";

  async function init() {
    const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
    if (!tab) return;

    // 1. Get cached sniffed streams
    chrome.runtime.sendMessage({ type: "GET_TAB_STREAMS" }, async (res) => {
      const sniffed = (res && res.streams) || [];
      sniffed.forEach((s) => {
        mediaItems.push({
          url: s.url,
          filename: s.filename || (s.url.split("/").pop().split("?")[0] || "media"),
          type: (s.mime && s.mime.includes("audio")) ? "audio" : "video",
          quality: s.quality || "Media Stream",
          selected: true
        });
      });

      // 2. Scan page DOM for video, audio, images and links
      try {
        const results = await chrome.scripting.executeScript({
          target: { tabId: tab.id },
          func: () => {
            const list = [];
            // Videos
            document.querySelectorAll("video").forEach((v) => {
              const src = v.currentSrc || v.src;
              if (src && src.startsWith("http")) {
                list.push({ url: src, type: "video", title: document.title || "Video" });
              }
            });
            // Audio
            document.querySelectorAll("audio").forEach((a) => {
              const src = a.currentSrc || a.src;
              if (src && src.startsWith("http")) {
                list.push({ url: src, type: "audio", title: document.title || "Audio" });
              }
            });
            // Media links
            document.querySelectorAll("a[href]").forEach((a) => {
              const href = a.href;
              const ext = href.split("?")[0].split(".").pop().toLowerCase();
              if (["mp4", "mkv", "webm", "avi", "mov", "ts"].includes(ext)) {
                list.push({ url: href, type: "video", title: a.innerText.trim() || href });
              } else if (["mp3", "wav", "m4a", "flac", "aac", "ogg"].includes(ext)) {
                list.push({ url: href, type: "audio", title: a.innerText.trim() || href });
              } else if (["zip", "tar", "gz", "bz2", "7z", "rar", "iso", "bin", "exe"].includes(ext)) {
                list.push({ url: href, type: "files", title: a.innerText.trim() || href });
              }
            });
            return list;
          }
        });

        if (results && results[0] && results[0].result) {
          results[0].result.forEach((item) => {
            if (!mediaItems.some((m) => m.url === item.url)) {
              mediaItems.push({
                url: item.url,
                filename: item.url.split("/").pop().split("?")[0] || "download",
                type: item.type,
                quality: item.title || item.type.toUpperCase(),
                selected: true
              });
            }
          });
        }
      } catch (_) {}

      renderList();
    });
  }

  function renderList() {
    const container = document.getElementById("items-list");
    const countBadge = document.getElementById("found-count");

    const filtered = mediaItems.filter((item) => {
      if (currentFilter === "all") return true;
      return item.type === currentFilter;
    });

    countBadge.innerText = `${filtered.length} Items Found`;

    if (filtered.length === 0) {
      container.innerHTML = `<div style="padding: 24px; text-align: center; color: var(--subtext);">No media files detected for this category.</div>`;
      return;
    }

    container.innerHTML = "";
    filtered.forEach((item, index) => {
      const row = document.createElement("div");
      row.className = "item-row";

      const icon = item.type === "video" ? "🎬" : item.type === "audio" ? "🎵" : item.type === "images" ? "🖼️" : "📦";

      row.innerHTML = `
        <input type="checkbox" class="item-check" data-index="${index}" ${item.selected ? "checked" : ""}>
        <span class="item-icon">${icon}</span>
        <div class="item-info">
          <span class="item-title">${item.quality || item.filename}</span>
          <span class="item-meta">${item.url}</span>
        </div>
      `;

      const checkbox = row.querySelector(".item-check");
      checkbox.addEventListener("change", (e) => {
        item.selected = e.target.checked;
      });

      container.appendChild(row);
    });
  }

  // Filter Buttons
  document.querySelectorAll(".filter-btn").forEach((btn) => {
    btn.addEventListener("click", () => {
      document.querySelectorAll(".filter-btn").forEach((b) => b.classList.remove("active"));
      btn.classList.add("active");
      currentFilter = btn.dataset.type;
      renderList();
    });
  });

  // Select / Deselect All
  document.getElementById("btn-select-all").addEventListener("click", () => {
    mediaItems.forEach((m) => (m.selected = true));
    renderList();
  });

  document.getElementById("btn-deselect-all").addEventListener("click", () => {
    mediaItems.forEach((m) => (m.selected = false));
    renderList();
  });

  // Download Selected
  document.getElementById("btn-download-selected").addEventListener("click", () => {
    const selected = mediaItems.filter((m) => m.selected);
    if (selected.length === 0) {
      alert("Please select at least one media item to download.");
      return;
    }

    selected.forEach((item) => {
      chrome.runtime.sendMessage({
        type: "MEDIA_SNIFFED_DOWNLOAD",
        url: item.url,
        filename: item.filename
      });
    });

    window.close();
  });

  init();
})();

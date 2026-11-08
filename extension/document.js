// Illusion Document Main-World Injection (Reverse-Engineered from IDM document.js)
(function () {
  "use strict";

  if (window.__illusion_main_world__) return;
  window.__illusion_main_world__ = true;

  function notifyMediaFound(url, quality, mime, audioUrl) {
    if (!url || typeof url !== "string") return;
    if (url.startsWith("blob:") && !url.includes("youtube.com")) return;

    window.postMessage(
      {
        type: "__ILLUSION_MEDIA_STREAM__",
        url: url,
        quality: quality || "Detected Media Stream",
        mime: mime || "video/mp4",
        audioUrl: audioUrl || null
      },
      "*"
    );
  }

  // 1. Hook window.fetch
  const originalFetch = window.fetch;
  window.fetch = async function (...args) {
    try {
      const resource = args[0];
      const url = typeof resource === "string" ? resource : (resource && resource.url ? resource.url : "");
      if (url && (url.includes("googlevideo.com/videoplayback") || url.includes("/videoplayback") || url.match(/\.(mp4|webm|m3u8|mpd|m4s|mp3|m4a|wav)(\?|$)/i))) {
        notifyMediaFound(url);
      }
    } catch (_) {}
    return originalFetch.apply(this, args);
  };

  // 2. Hook XMLHttpRequest.prototype.open
  const originalXHROpen = XMLHttpRequest.prototype.open;
  XMLHttpRequest.prototype.open = function (...args) {
    try {
      const url = args[1];
      if (url && typeof url === "string" && (url.includes("googlevideo.com/videoplayback") || url.includes("/videoplayback") || url.match(/\.(mp4|webm|m3u8|mpd|m4s|mp3|m4a|wav)(\?|$)/i))) {
        notifyMediaFound(url);
      }
    } catch (_) {}
    return originalXHROpen.apply(this, args);
  };

  // 3. Hook HTMLMediaElement.prototype.play
  const originalPlay = HTMLMediaElement.prototype.play;
  HTMLMediaElement.prototype.play = function (...args) {
    try {
      const src = this.currentSrc || this.src;
      if (src) {
        notifyMediaFound(src, this.tagName.toLowerCase() === "audio" ? "Audio Playback" : "Video Playback");
      }
    } catch (_) {}
    return originalPlay.apply(this, args);
  };

  // 4. YouTube In-Page Player Data Introspection
  function checkYouTubeData() {
    if (!window.location.hostname.includes("youtube.com")) return;

    try {
      const playerResp = window.ytInitialPlayerResponse;
      if (playerResp && playerResp.streamingData) {
        const title = playerResp.videoDetails ? playerResp.videoDetails.title : (document.title || "YouTube Video");
        const streaming = playerResp.streamingData;
        const formats = streaming.formats || [];
        const adaptive = streaming.adaptiveFormats || [];

        // Check for dedicated audio track to pair with high-res video
        let bestAudioUrl = null;
        for (const fmt of adaptive) {
          if (fmt.mimeType && fmt.mimeType.includes("audio") && fmt.url) {
            bestAudioUrl = fmt.url;
            break;
          }
        }

        // Process standard muxed formats
        formats.forEach((fmt) => {
          if (fmt.url) {
            notifyMediaFound(fmt.url, `${fmt.qualityLabel || "720p"} (Video+Audio MP4)`, fmt.mimeType, null);
          }
        });

        // Process high-resolution adaptive formats (1080p, 1440p, 4K) paired with bestAudioUrl
        adaptive.forEach((fmt) => {
          if (fmt.url && fmt.qualityLabel) {
            notifyMediaFound(
              fmt.url,
              `${fmt.qualityLabel} Full Stream (Muxed via Illusion)`,
              fmt.mimeType || "video/mp4",
              bestAudioUrl
            );
          }
        });
      }
    } catch (_) {}
  }

  // Check immediately and on YouTube SPA page navigation
  checkYouTubeData();
  window.addEventListener("yt-navigate-finish", checkYouTubeData);
  window.addEventListener("load", checkYouTubeData);
  setInterval(checkYouTubeData, 3000);
})();

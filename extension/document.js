// Illusion Document Main-World Injection (Reverse-Engineered from IDM document.js)
(function () {
  "use strict";

  if (window.__illusion_main_world__) return;
  window.__illusion_main_world__ = true;

  const capturedBuffers = {
    video: [],
    audio: [],
    videoMime: "video/mp4",
    audioMime: "audio/mp4",
    totalVideoBytes: 0,
    totalAudioBytes: 0
  };
  window.__illusion_captured_buffers__ = capturedBuffers;

  function cleanUrl(url) {
    if (!url || typeof url !== "string") return "";
    try {
      const u = new URL(url);
      if (u.hostname.includes("googlevideo.com")) {
        u.searchParams.delete("range");
        u.searchParams.delete("rn");
        u.searchParams.delete("alr");
        return u.toString();
      }
    } catch (_) {}
    return url;
  }

  function notifyMediaFound(url, quality, mime, audioUrl) {
    if (!url || typeof url !== "string") return;
    if (url.startsWith("blob:") && !url.includes("googlevideo")) return;

    const targetUrl = cleanUrl(url);

    window.postMessage(
      {
        type: "__ILLUSION_MEDIA_STREAM__",
        url: targetUrl,
        quality: quality || "720p HD",
        mime: mime || "video/mp4",
        audioUrl: audioUrl ? cleanUrl(audioUrl) : null
      },
      "*"
    );
  }

  // 1. Hook MediaSource & SourceBuffer (IDM-Grade MSE Stream Interception)
  if (typeof SourceBuffer !== "undefined" && SourceBuffer.prototype.appendBuffer) {
    const originalAppendBuffer = SourceBuffer.prototype.appendBuffer;
    SourceBuffer.prototype.appendBuffer = function (buf) {
      try {
        if (buf) {
          const byteLen = buf.byteLength || (buf.buffer ? buf.buffer.byteLength : 0);
          const isAudio = this.track && this.track.kind === "audio" || (this._mime && this._mime.includes("audio"));
          if (isAudio) {
            capturedBuffers.audio.push(buf instanceof ArrayBuffer ? buf : buf.slice(0));
            capturedBuffers.totalAudioBytes += byteLen;
          } else {
            capturedBuffers.video.push(buf instanceof ArrayBuffer ? buf : buf.slice(0));
            capturedBuffers.totalVideoBytes += byteLen;
          }
        }
      } catch (_) {}
      return originalAppendBuffer.apply(this, arguments);
    };

    if (MediaSource.prototype.addSourceBuffer) {
      const origAddSourceBuffer = MediaSource.prototype.addSourceBuffer;
      MediaSource.prototype.addSourceBuffer = function (mimeType) {
        const sb = origAddSourceBuffer.apply(this, arguments);
        try {
          sb._mime = mimeType;
          if (mimeType.includes("audio")) {
            capturedBuffers.audioMime = mimeType;
          } else {
            capturedBuffers.videoMime = mimeType;
          }
        } catch (_) {}
        return sb;
      };
    }
  }

  // 2. Hook window.fetch
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

  // 3. Hook XMLHttpRequest.prototype.open
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

  // 4. Hook HTMLMediaElement.prototype.play
  const originalPlay = HTMLMediaElement.prototype.play;
  HTMLMediaElement.prototype.play = function (...args) {
    try {
      const src = this.currentSrc || this.src;
      if (src && src.startsWith("http")) {
        notifyMediaFound(src, this.tagName.toLowerCase() === "audio" ? "Audio Playback" : "Video Playback");
      }
    } catch (_) {}
    return originalPlay.apply(this, args);
  };

  // 5. In-Page Player Inspection
  function checkYouTubeData() {
    if (!window.location.hostname.includes("youtube.com")) return;

    try {
      const playerResp = window.ytInitialPlayerResponse;
      if (playerResp && playerResp.streamingData) {
        const streaming = playerResp.streamingData;
        const formats = streaming.formats || [];
        const adaptive = streaming.adaptiveFormats || [];

        let bestAudioUrl = null;
        for (const fmt of adaptive) {
          if (fmt.mimeType && fmt.mimeType.includes("audio") && fmt.url) {
            bestAudioUrl = fmt.url;
            break;
          }
        }

        formats.forEach((fmt) => {
          if (fmt.url) {
            notifyMediaFound(fmt.url, `${fmt.qualityLabel || "720p"} HD`, fmt.mimeType, null);
          }
        });

        adaptive.forEach((fmt) => {
          if (fmt.url && fmt.qualityLabel) {
            notifyMediaFound(
              fmt.url,
              `${fmt.qualityLabel} HD`,
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

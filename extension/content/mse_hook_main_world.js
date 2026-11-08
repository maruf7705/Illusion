// Illusion Main-World MSE & Network Instrumentation Hook
// Injected at document_start in world: MAIN (all_frames: true)
(function () {
  "use strict";

  if (window.__illusion_mse_hook__) return;
  window.__illusion_mse_hook__ = true;

  const blobMediaMap = new Map();
  const capturedCodecs = [];

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

  const ytItagMap = {
    "18": { q: "360p", m: "video/mp4" },
    "22": { q: "720p HD", m: "video/mp4" },
    "137": { q: "1080p HD", m: "video/mp4" },
    "136": { q: "720p HD", m: "video/mp4" },
    "135": { q: "480p", m: "video/mp4" },
    "134": { q: "360p", m: "video/mp4" },
    "140": { q: "Audio 128k", m: "audio/mp4" },
    "251": { q: "Audio 160k", m: "audio/webm" },
    "250": { q: "Audio 70k", m: "audio/webm" },
    "249": { q: "Audio 50k", m: "audio/webm" },
    "313": { q: "2160p 4K", m: "video/webm" },
    "271": { q: "1440p 2K", m: "video/webm" },
    "303": { q: "1080p60 HD", m: "video/webm" },
    "298": { q: "720p60 HD", m: "video/mp4" }
  };

  function emitMediaDiscovery(detail) {
    if (!detail || !detail.url) return;
    const cleaned = cleanUrl(detail.url);

    let quality = detail.quality || "";
    let mime = detail.mime || "video/mp4";

    try {
      const u = new URL(cleaned);
      const itag = u.searchParams.get("itag");
      if (itag && ytItagMap[itag]) {
        if (!quality || quality === "720p HD") quality = ytItagMap[itag].q;
        if (!detail.mime) mime = ytItagMap[itag].m;
      }
    } catch (_) {}

    window.dispatchEvent(
      new CustomEvent("__ILLUSION_STREAM_DISCOVERED__", {
        detail: {
          url: cleaned,
          quality: quality || "720p HD",
          mime: mime,
          audioUrl: detail.audioUrl ? cleanUrl(detail.audioUrl) : null,
          codecs: detail.codecs || null
        }
      })
    );
  }

  // 1. Monkey-patch URL.createObjectURL for MediaSource tracking
  const origCreateObjectURL = URL.createObjectURL;
  URL.createObjectURL = function (obj) {
    const url = origCreateObjectURL.apply(this, arguments);
    try {
      if (typeof MediaSource !== "undefined" && obj instanceof MediaSource) {
        blobMediaMap.set(url, obj);
        window.dispatchEvent(
          new CustomEvent("__ILLUSION_MSE_DETECTED__", {
            detail: { blobUrl: url }
          })
        );
      }
    } catch (_) {}
    return url;
  };

  // 2. Monkey-patch MediaSource.prototype.addSourceBuffer
  if (typeof MediaSource !== "undefined" && MediaSource.prototype.addSourceBuffer) {
    const origAddSourceBuffer = MediaSource.prototype.addSourceBuffer;
    MediaSource.prototype.addSourceBuffer = function (mimeCodec) {
      const sb = origAddSourceBuffer.apply(this, arguments);
      try {
        if (mimeCodec && typeof mimeCodec === "string") {
          capturedCodecs.push(mimeCodec);
          window.dispatchEvent(
            new CustomEvent("__ILLUSION_CODEC_DETECTED__", {
              detail: { mimeCodec }
            })
          );
        }
      } catch (_) {}
      return sb;
    };
  }

  // 3. Monkey-patch window.fetch
  const origFetch = window.fetch;
  window.fetch = async function (...args) {
    try {
      const resource = args[0];
      const url = typeof resource === "string" ? resource : (resource && resource.url ? resource.url : "");
      if (url) {
        const isMediaStream =
          url.includes("googlevideo.com/videoplayback") ||
          url.includes("/videoplayback") ||
          url.match(/\.(mp4|webm|m3u8|mpd|m4s|mp3|m4a|wav|ts)(\?|$)/i);
        if (isMediaStream) {
          emitMediaDiscovery({ url });
        }
      }
    } catch (_) {}
    return origFetch.apply(this, args);
  };

  // 4. Monkey-patch XMLHttpRequest.prototype.open
  const origXHROpen = XMLHttpRequest.prototype.open;
  XMLHttpRequest.prototype.open = function (...args) {
    try {
      const url = args[1];
      if (url && typeof url === "string") {
        const isMediaStream =
          url.includes("googlevideo.com/videoplayback") ||
          url.includes("/videoplayback") ||
          url.match(/\.(mp4|webm|m3u8|mpd|m4s|mp3|m4a|wav|ts)(\?|$)/i);
        if (isMediaStream) {
          emitMediaDiscovery({ url });
        }
      }
    } catch (_) {}
    return origXHROpen.apply(this, args);
  };

  // 5. Global Player Response Introspection (YouTube & SPA Players)
  function inspectGlobalPlayerState() {
    try {
      // YouTube ytInitialPlayerResponse
      const pr = window.ytInitialPlayerResponse || (window.ytplayer && window.ytplayer.config && window.ytplayer.config.args && JSON.parse(window.ytplayer.config.args.raw_player_response));
      if (pr && pr.streamingData) {
        const streaming = pr.streamingData;
        const formats = streaming.formats || [];
        const adaptive = streaming.adaptiveFormats || [];

        let bestAudioUrl = null;
        for (const fmt of adaptive) {
          if (fmt.mimeType && fmt.mimeType.includes("audio") && fmt.url) {
            bestAudioUrl = fmt.url;
            break;
          }
        }

        formats.forEach((f) => {
          if (f.url) {
            emitMediaDiscovery({
              url: f.url,
              quality: f.qualityLabel || "720p HD",
              mime: f.mimeType || "video/mp4",
              audioUrl: null
            });
          }
        });

        adaptive.forEach((f) => {
          if (f.url && f.qualityLabel) {
            emitMediaDiscovery({
              url: f.url,
              quality: f.qualityLabel,
              mime: f.mimeType || "video/mp4",
              audioUrl: bestAudioUrl
            });
          }
        });
      }
    } catch (_) {}
  }

  inspectGlobalPlayerState();
  window.addEventListener("yt-navigate-finish", inspectGlobalPlayerState);
  window.addEventListener("load", inspectGlobalPlayerState);
  setInterval(inspectGlobalPlayerState, 3000);
})();

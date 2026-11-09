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
      const sparams = (u.searchParams.get("sparams") || "").split(",");
      // CRITICAL: Never strip parameters that are signed by Google Video sparams HMAC!
      ["bytestart", "byteend", "range", "rn", "alr", "sq", "segment", "frag"].forEach((p) => {
        if (!sparams.includes(p)) {
          u.searchParams.delete(p);
        }
      });
      return u.toString();
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

  let lastActivePlaybackUrl = null;
  const liveItagUrls = new Map();
  let lastActiveAudioUrl = null;
  let lastActiveVideoUrl = null;

  function trackPlaybackUrl(url) {
    if (!url || typeof url !== "string") return;
    try {
      const u = new URL(url);
      if (u.pathname.includes("videoplayback") || u.hostname.includes("googlevideo.com")) {
        const itag = u.searchParams.get("itag");
        const cleaned = cleanUrl(url);
        if (itag) {
          liveItagUrls.set(itag, cleaned);
          const isAudio = ["140", "251", "250", "249"].includes(itag);
          if (isAudio) {
            lastActiveAudioUrl = cleaned;
          } else {
            lastActiveVideoUrl = cleaned;
          }

          // Emit the live, deciphered stream immediately to detector & floating bar
          const info = ytItagMap[itag] || { q: isAudio ? "Audio Track" : "720p HD", m: isAudio ? "audio/mp4" : "video/mp4" };
          emitMediaDiscovery({
            url: cleaned,
            quality: info.q,
            mime: info.m,
            size: 0,
            audioUrl: isAudio ? null : lastActiveAudioUrl
          });

          // If this is an audio track and we already have an active video stream, link them for dual-stream download
          if (isAudio && lastActiveVideoUrl) {
            try {
              const vItag = new URL(lastActiveVideoUrl).searchParams.get("itag");
              const vInfo = ytItagMap[vItag] || { q: "720p HD", m: "video/mp4" };
              emitMediaDiscovery({
                url: lastActiveVideoUrl,
                quality: vInfo.q,
                mime: vInfo.m,
                size: 0,
                audioUrl: cleaned
              });
            } catch (_) {}
          }
        }
        lastActivePlaybackUrl = cleaned;
      }
    } catch (_) {}
  }

  function emitMediaDiscovery(detail) {
    if (!detail || !detail.url) return;
    const cleaned = cleanUrl(detail.url);

    let quality = detail.quality || "";
    let mime = detail.mime || "video/mp4";
    let size = detail.size || 0;

    try {
      const u = new URL(cleaned);
      const itag = u.searchParams.get("itag");
      if (itag && ytItagMap[itag]) {
        if (!quality || quality === "720p HD") quality = ytItagMap[itag].q;
        if (!detail.mime) mime = ytItagMap[itag].m;
      }
    } catch (_) {}

    window.__ILLUSION_DISCOVERED_STREAMS__ = window.__ILLUSION_DISCOVERED_STREAMS__ || [];
    if (!window.__ILLUSION_DISCOVERED_STREAMS__.some((s) => s.url === cleaned)) {
      window.__ILLUSION_DISCOVERED_STREAMS__.push({
        url: cleaned,
        quality: quality || "720p HD",
        mime: mime,
        size: size
      });
    }

    const payload = {
      url: cleaned,
      quality: quality || "720p HD",
      mime: mime,
      size: size,
      audioUrl: detail.audioUrl ? cleanUrl(detail.audioUrl) : null,
      codecs: detail.codecs || null
    };

    window.dispatchEvent(
      new CustomEvent("__ILLUSION_STREAM_DISCOVERED__", {
        detail: payload
      })
    );

    // Cross-world postMessage (matches official IDM document.js -> content.js bridging)
    window.postMessage({
      source: "illusion_mse_hook",
      type: "__ILLUSION_STREAM_DISCOVERED__",
      detail: payload
    }, "*");
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

  function extractFormatUrl(f) {
    if (!f) return null;
    if (f.url) return f.url;
    // Check if we already intercepted the live deciphered URL for this itag from network playback
    if (f.itag && liveItagUrls.has(f.itag.toString())) {
      return liveItagUrls.get(f.itag.toString());
    }
    // Never emit fake undeciphered URLs with raw cipher signatures (triggers 403 Forbidden)
    return null;
  }

  function inspectStreamingData(streaming, activeUrl) {
    if (!streaming) return;
    const formats = streaming.formats || [];
    const adaptive = streaming.adaptiveFormats || [];

    // Find best audio URL
    let bestAudioUrl = lastActiveAudioUrl || liveItagUrls.get("140") || liveItagUrls.get("251");
    if (!bestAudioUrl) {
      for (const fmt of adaptive) {
        if (fmt.mimeType && fmt.mimeType.includes("audio")) {
          const u = liveItagUrls.get(fmt.itag?.toString()) || extractFormatUrl(fmt);
          if (u) {
            bestAudioUrl = cleanUrl(u);
            break;
          }
        }
      }
    }

    // 1. Process regular muxed formats
    formats.forEach((f) => {
      let streamUrl = liveItagUrls.get(f.itag?.toString()) || extractFormatUrl(f);
      if (streamUrl) {
        emitMediaDiscovery({
          url: cleanUrl(streamUrl),
          quality: f.qualityLabel || `${f.height || 360}p`,
          mime: f.mimeType || "video/mp4",
          size: parseInt(f.contentLength || "0", 10) || 0,
          audioUrl: null
        });
      }
    });

    // 2. Process adaptive video & audio formats
    adaptive.forEach((f) => {
      const isAudio = f.mimeType && f.mimeType.includes("audio");
      let streamUrl = liveItagUrls.get(f.itag?.toString()) || extractFormatUrl(f);
      if (!streamUrl && activeUrl) {
        try {
          const u = new URL(activeUrl);
          if (u.searchParams.get("itag") === f.itag?.toString()) {
            streamUrl = cleanUrl(activeUrl);
          }
        } catch (_) {}
      }

      if (streamUrl) {
        const qLabel = f.qualityLabel || (isAudio ? (f.bitrate ? `Audio ${(f.bitrate/1000).toFixed(0)}k` : "Audio 128k") : `${f.height || 720}p`);
        emitMediaDiscovery({
          url: cleanUrl(streamUrl),
          quality: qLabel,
          mime: f.mimeType || (isAudio ? "audio/mp4" : "video/mp4"),
          size: parseInt(f.contentLength || "0", 10) || 0,
          audioUrl: isAudio ? null : bestAudioUrl
        });
      }
    });
  }

  // 3. Monkey-patch window.fetch
  const origFetch = window.fetch;
  window.fetch = async function (...args) {
    try {
      const resource = args[0];
      const url = typeof resource === "string" ? resource : (resource && resource.url ? resource.url : "");
      if (url) {
        if (url.includes("/youtubei/v1/player")) {
          const res = await origFetch.apply(this, args);
          try {
            const clone = res.clone();
            clone.json().then((data) => {
              if (data && data.streamingData) {
                inspectStreamingData(data.streamingData, lastActivePlaybackUrl);
              }
            }).catch(() => {});
          } catch (_) {}
          return res;
        } else if (url.includes("googlevideo.com/videoplayback") || url.includes("/videoplayback")) {
          trackPlaybackUrl(url);
          inspectGlobalPlayerState(url);
        } else if (url.match(/\.(mp4|webm|m3u8|mpd|m4s|mp3|m4a|wav|ts)(\?|$)/i)) {
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
        if (url.includes("googlevideo.com/videoplayback") || url.includes("/videoplayback")) {
          trackPlaybackUrl(url);
          inspectGlobalPlayerState(url);
        } else if (url.match(/\.(mp4|webm|m3u8|mpd|m4s|mp3|m4a|wav|ts)(\?|$)/i)) {
          emitMediaDiscovery({ url });
        }
      }
    } catch (_) {}
    return origXHROpen.apply(this, args);
  };

  // 5. Global Player Response Introspection (YouTube & SPA Players)
  function inspectGlobalPlayerState(overrideUrl) {
    try {
      let pr = window.ytInitialPlayerResponse;
      if (!pr) {
        const mp = document.getElementById("movie_player");
        if (mp && typeof mp.getPlayerResponse === "function") {
          pr = mp.getPlayerResponse();
        }
      }
      if (!pr && window.ytplayer && window.ytplayer.config && window.ytplayer.config.args) {
        try {
          pr = JSON.parse(window.ytplayer.config.args.raw_player_response);
        } catch (_) {}
      }
      if (!pr) {
        // Fallback: search inline scripts for ytInitialPlayerResponse
        const scripts = document.getElementsByTagName("script");
        for (const s of scripts) {
          if (s.textContent && s.textContent.includes("ytInitialPlayerResponse = {")) {
            const m = s.textContent.match(/ytInitialPlayerResponse\s*=\s*(\{.+?\});/);
            if (m && m[1]) {
              try {
                pr = JSON.parse(m[1]);
                break;
              } catch (_) {}
            }
          }
        }
      }

      if (!pr || !pr.streamingData) return;
      inspectStreamingData(pr.streamingData, overrideUrl || lastActivePlaybackUrl);
    } catch (_) {}
  }

  // 6. Facebook / Instagram / Vimeo / Generic React Fiber Video Sniffer
  function inspectSocialMediaPlayers() {
    try {
      const isFB = window.location.hostname.includes("facebook.com") || window.location.hostname.includes("fb.watch");
      const isIG = window.location.hostname.includes("instagram.com");
      const isVimeo = window.location.hostname.includes("vimeo.com");

      // A. Scan all video elements for direct sources or React Fiber props
      const videos = document.querySelectorAll("video");
      videos.forEach((v) => {
        // Direct src
        const directSrc = v.currentSrc || v.src;
        if (directSrc && !directSrc.startsWith("blob:") && !directSrc.startsWith("data:") && directSrc.startsWith("http")) {
          emitMediaDiscovery({
            url: directSrc,
            quality: v.videoHeight ? `${v.videoHeight}p` : "HD Video",
            mime: directSrc.includes(".m3u8") ? "application/vnd.apple.mpegurl" : "video/mp4"
          });
        }

        // Facebook & Instagram React Fiber traversal
        if (isFB || isIG) {
          for (const key of Object.keys(v)) {
            if (key.startsWith("__reactFiber$") || key.startsWith("__reactInternalInstance$")) {
              let fiber = v[key];
              let depth = 0;
              while (fiber && depth < 25) {
                const props = fiber.memoizedProps;
                if (props) {
                  const meta = props.coreVideoPlayerMetaData || props.videoFBID || props.videoData;
                  const src = props.playbackUrl || props.hd_src || props.sd_src || (meta && (meta.playbackUrl || meta.hd_src || meta.sd_src));
                  if (src && typeof src === "string" && src.startsWith("http")) {
                    emitMediaDiscovery({
                      url: src,
                      quality: (props.hd_src || (meta && meta.hd_src)) ? "HD 1080p" : "SD 720p",
                      mime: src.includes(".m3u8") ? "application/vnd.apple.mpegurl" : "video/mp4"
                    });
                  }
                }
                fiber = fiber.return;
                depth++;
              }
            }
          }
        }
      });

      // B. Vimeo player config
      if (isVimeo && window.__VIMEO_CONFIG__) {
        const files = window.__VIMEO_CONFIG__.request?.files;
        if (files) {
          if (files.hls?.default_cdn) {
            emitMediaDiscovery({
              url: files.hls.default_cdn,
              quality: "HLS Adaptive (1080p/4K)",
              mime: "application/vnd.apple.mpegurl"
            });
          }
          if (Array.isArray(files.progressive)) {
            files.progressive.forEach((f) => {
              if (f.url) {
                emitMediaDiscovery({
                  url: f.url,
                  quality: `${f.quality || f.height || 720}p`,
                  mime: f.mime || "video/mp4"
                });
              }
            });
          }
        }
      }
    } catch (_) {}
  }

  inspectGlobalPlayerState();
  inspectSocialMediaPlayers();
  window.addEventListener("yt-navigate-finish", inspectGlobalPlayerState);
  window.addEventListener("load", () => {
    inspectGlobalPlayerState();
    inspectSocialMediaPlayers();
  });
  setInterval(() => {
    inspectGlobalPlayerState();
    inspectSocialMediaPlayers();
  }, 2500);
})();

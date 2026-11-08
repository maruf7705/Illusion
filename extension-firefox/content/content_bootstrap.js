// Illusion Firefox/Zen Main World Injector Bootstrap
// Ensures mse_hook_main_world.js runs in page context without CSP inline script violations
(function () {
  "use strict";

  if (typeof window !== "undefined" && window.__illusion_mse_hook__) return;

  try {
    const runtime = typeof chrome !== "undefined" && chrome.runtime ? chrome.runtime : (typeof browser !== "undefined" ? browser.runtime : null);
    if (!runtime || !runtime.getURL) return;

    const script = document.createElement("script");
    script.src = runtime.getURL("content/mse_hook_main_world.js");
    script.async = false;
    (document.head || document.documentElement).appendChild(script);
    script.onload = () => script.remove();
  } catch (_) {}
})();

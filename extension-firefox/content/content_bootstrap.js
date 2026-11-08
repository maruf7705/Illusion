// Illusion Firefox/Zen Main World Injector Bootstrap
// Ensures mse_hook_main_world.js runs in page context if not already loaded via manifest world: MAIN
(function () {
  "use strict";

  if (typeof window !== "undefined" && window.__illusion_mse_hook__) return;

  try {
    const xhr = new XMLHttpRequest();
    const runtime = typeof chrome !== "undefined" && chrome.runtime ? chrome.runtime : (typeof browser !== "undefined" ? browser.runtime : null);
    if (!runtime) return;

    const url = runtime.getURL("content/mse_hook_main_world.js");
    xhr.open("GET", url, false);
    xhr.send();
    if (xhr.status === 200 && xhr.responseText) {
      const script = document.createElement("script");
      script.textContent = xhr.responseText;
      (document.head || document.documentElement).appendChild(script);
      script.remove();
    }
  } catch (_) {}
})();

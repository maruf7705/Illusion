// Illusion Popup Logic
document.addEventListener("DOMContentLoaded", async () => {
  const statusPill = document.getElementById("status-pill");
  const interceptToggle = document.getElementById("intercept-toggle");
  const snifferToggle = document.getElementById("sniffer-toggle");
  const minSizeInput = document.getElementById("min-size");
  const addUrlBtn = document.getElementById("add-url-btn");

  // Load existing settings
  const data = await chrome.storage.local.get("settings");
  const settings = data.settings || {};

  if (settings.interceptDownloads !== undefined) {
    interceptToggle.checked = settings.interceptDownloads;
  }
  if (settings.snifferEnabled !== undefined) {
    snifferToggle.checked = settings.snifferEnabled;
  }
  if (settings.minFileSizeMB !== undefined) {
    minSizeInput.value = settings.minFileSizeMB;
  }

  // Check connection to native host
  chrome.runtime.sendMessage({ type: "PING_HOST" }, (res) => {
    if (chrome.runtime.lastError || !res || !res.connected) {
      statusPill.className = "pill disconnected";
      statusPill.innerText = "Host Disconnected";
    } else {
      statusPill.className = "pill connected";
      statusPill.innerText = "Connected";
    }
  });

  // Save changes
  async function saveSettings() {
    const current = (await chrome.storage.local.get("settings")).settings || {};
    current.interceptDownloads = interceptToggle.checked;
    current.snifferEnabled = snifferToggle.checked;
    current.minFileSizeMB = parseInt(minSizeInput.value, 10) || 0;
    await chrome.storage.local.set({ settings: current });
  }

  interceptToggle.addEventListener("change", saveSettings);
  snifferToggle.addEventListener("change", saveSettings);
  minSizeInput.addEventListener("change", saveSettings);

  addUrlBtn.addEventListener("click", () => {
    const url = prompt("Enter URL to download with Illusion:");
    if (url && url.startsWith("http")) {
      chrome.runtime.sendMessage({
        type: "MEDIA_SNIFFED_DOWNLOAD",
        url: url
      });
      alert("Download task sent to Illusion!");
    }
  });
});

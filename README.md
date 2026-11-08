# ⚡ Illusion

**Next-Generation High-Performance Download Manager for Arch Linux & Hyprland (Wayland)**  
*Inspired by the dynamic multi-segment acceleration architecture of Internet Download Manager (IDM), re-engineered from the ground up in 100% Rust.*

---

## 🌟 Highlights

- 🚀 **Dynamic Multi-Connection Engine**:
  - Dynamically divides remaining byte ranges among active connection threads when one finishes early (IDM's iconic dynamic splitting algorithm).
  - Maximizes bandwidth saturation across HTTP/1.1 and HTTP/2 connections.
- 💾 **Instant Zero-Copy Storage Allocation**:
  - Employs Linux kernel `fallocate` (`posix_fallocate`) to instantly allocate target file space.
  - Workers write directly to their assigned byte offsets via asynchronous file pointers, eliminating tedious post-download file chunk stitching (`.part` concatenation).
- 🎨 **Wayland-Native GUI (Hyprland Optimized)**:
  - 100% pure Rust desktop GUI powered by `eframe` / `egui`.
  - Crisp HiDPI rendering with zero XWayland blur.
  - Catppuccin Mocha / Sapphire dark theme matching modern Hyprland styling.
  - **Iconic Segment Visualizer**: Real-time multi-color segmented progress bar showing each active connection thread in action.
- 🌐 **Chrome / Chromium & Firefox Extension (Manifest V3)**:
  - Automatic download interception with full header, cookie, referer, and user-agent mirroring.
  - Right-click context menu ("Download with Illusion", "Download Media with Illusion").
  - **Media Sniffer**: Injects a floating download badge on web video/audio streams (`.mp4`, `.webm`, `.m3u8`, `.mpd`).
- ⚡ **Native Messaging Bridge**:
  - Ultra-fast native communication between browser extensions and Illusion via stdio JSON protocol.
- 🪟 **Hyprland Window Rules**:
  - Pre-configured window rules for floating download dialogs, smooth centering, and tiling compatibility.
- 💻 **Dual GUI & CLI Support**:
  - Seamless control from the desktop GUI (`illusion`) or command line (`illusion-cli`).

---

## 🔬 Architecture

```
[Browser / Extension] ──(Native Messaging stdio)──> [illusion-native-host]
                                                             │
                                                             ▼
                                                    [Illusion State]
                                                             │
             ┌───────────────────────────────────────────────┴──────────────────────────────────────────────┐
             ▼                                                                                              ▼
    [illusion-gui]                                                                                   [illusion-core]
 (Wayland / Hyprland)                                                                         (Dynamic Segment Engine)
         │                                                                                                  │
         ├── Real-time Segment Visualizer                                                                  ├── HTTP Range Prober
         ├── Category Filter (Video, Compressed, etc.)                                                     ├── fallocate Pre-allocator
         └── Floating Download Dialogs                                                                     └── Concurrent Workers
```

---

## 📦 Project Structure

```
.
├── Cargo.toml                              # Cargo workspace manifest
├── crates/
│   ├── illusion-core/                      # Core download engine & dynamic segmentation
│   │   ├── src/downloader.rs
│   │   ├── src/engine.rs                   # Multi-worker async execution & rebalancing
│   │   ├── src/prober.rs                   # Range & Content-Disposition prober
│   │   ├── src/segment.rs                  # Segment model & dynamic splitting
│   │   ├── src/state.rs                    # Persistent JSON task store
│   │   ├── src/storage.rs                  # fallocate & direct byte-offset I/O
│   │   └── src/task.rs                     # Task models & category categorization
│   ├── illusion-gui/                       # Wayland / Hyprland native GUI application
│   │   └── src/main.rs                     # Modern Catppuccin dark GUI & segment visualizer
│   ├── illusion-cli/                       # Terminal CLI interface
│   │   └── src/main.rs                     # illusion-cli add/list/resume/remove
│   └── illusion-native-host/               # WebExtensions Native Messaging Host
│       ├── src/main.rs                     # Stdio length-prefixed protocol bridge
│       └── com.illusion.downloader.json    # Native messaging manifest
├── extension/                              # Chrome / Chromium / Firefox Manifest V3 Extension
│   ├── manifest.json
│   ├── background.js                       # Download interception & native messaging
│   ├── content.js
│   ├── sniffer.js                          # Floating video/audio sniffer widget
│   ├── sniffer.css
│   ├── popup/                              # Extension popup controller
│   └── icons/
├── hyprland/
│   └── illusion.conf                       # Hyprland window rules
└── scripts/
    └── install.sh                          # One-click Arch Linux installer
```

---

## 🚀 Quick Start

### 1. Build and Install

Run the automated installer script:
```bash
./scripts/install.sh
```

This will:
1. Compile the release binaries (`illusion`, `illusion-cli`, `illusion-native-host`).
2. Install them to `~/.local/bin/`.
3. Register the Native Messaging host for Chromium, Google Chrome, and Firefox.
4. Install the desktop entry (`illusion.desktop`).
5. Copy Hyprland window rules to `~/.config/hypr/illusion.conf`.

### 2. Configure Hyprland

Add this line to your `~/.config/hypr/hyprland.conf`:
```ini
source = ~/.config/hypr/illusion.conf
```

### 3. Install the Browser Extension

1. Open **Chromium** or **Google Chrome**.
2. Navigate to `chrome://extensions/`.
3. Toggle **Developer mode** (top right).
4. Click **Load unpacked**.
5. Select the [`extension/`](extension/) directory from this repository.

---

## 💻 CLI Usage

You can also use Illusion entirely from your terminal:

```bash
# Download a file with 16 connections
illusion-cli add "https://example.com/largefile.iso" --conns 16

# Custom filename and destination folder
illusion-cli add "https://example.com/video.mp4" -f "my_video.mp4" -d ~/Videos

# List all downloads
illusion-cli list

# Resume a paused download
illusion-cli resume <task-id>

# Remove a download from queue
illusion-cli remove <task-id>
```

---

## 📄 License

MIT License. Crafted for the Arch Linux and Wayland community.

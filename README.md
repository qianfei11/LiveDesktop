# LivePhoto Viewer

A cross-platform desktop app for previewing **Live Photos** — image files paired with a same-name video clip (e.g. `IMG_001.jpg` + `IMG_001.mov`).

Built with **Tauri v2** (Rust backend + HTML/CSS/JS frontend).

---

## Features

- Hover over a photo card to instantly preview the video
- Click a card to open a fullscreen lightbox; press **Space** to toggle video / **Esc** to close
- Folder picker dialog or drag-and-drop a folder onto the window
- Live search by filename
- Grid and compact list views
- Dark glassmorphism UI with animated background

## Supported formats

| Image | Video |
|-------|-------|
| `.jpg` / `.jpeg` | `.mov` / `.MOV` |
| `.heic` / `.heif` | `.mp4` / `.MP4` |
| `.png` | `.m4v` / `.M4V` |

A Live Photo is detected when an image file and a video file share the **exact same filename stem** in the same folder.

---

## Prerequisites

| Tool | Version |
|------|---------|
| Rust + Cargo | 1.70+ |
| Node.js | 18+ |
| npm / pnpm | any |
| Platform libs | see below |

**Linux** — install WebKit2GTK and build tools:
```bash
sudo apt install libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev \
                 librsvg2-dev patchelf build-essential curl
```

**macOS** — Xcode Command Line Tools:
```bash
xcode-select --install
```

**Windows** — install [Visual Studio Build Tools](https://visualstudio.microsoft.com/downloads/) (C++ workload) and [WebView2](https://developer.microsoft.com/en-us/microsoft-edge/webview2/).

---

## Development

```bash
# Install Tauri CLI
npm install

# Run in dev mode (hot-reload)
npm run dev
# or: npx tauri dev
```

---

## Build (release)

### Native (current machine)
```bash
npm run build
# Binary / installer appears in src-tauri/target/release/bundle/
```

### Cross-compilation targets

First add the Rust target:
```bash
rustup target add <TARGET>
```

Then build:
```bash
npx tauri build --target <TARGET>
```

| Platform | Architecture | Target triple |
|----------|-------------|---------------|
| Linux | x86-64 | `x86_64-unknown-linux-gnu` |
| Linux | ARM64 | `aarch64-unknown-linux-gnu` |
| Linux | ARMv7 | `armv7-unknown-linux-gnueabihf` |
| Windows | x86-64 | `x86_64-pc-windows-msvc` |
| Windows | ARM64 | `aarch64-pc-windows-msvc` |
| macOS | x86-64 (Intel) | `x86_64-apple-darwin` |
| macOS | ARM64 (Apple Silicon) | `aarch64-apple-darwin` |
| macOS | Universal binary | `universal-apple-darwin` |

> **Linux cross-compile** requires a cross-linker, e.g. `gcc-aarch64-linux-gnu`.
> Add to `~/.cargo/config.toml`:
> ```toml
> [target.aarch64-unknown-linux-gnu]
> linker = "aarch64-linux-gnu-gcc"
> ```

---

## Project structure

```
LiveDesktop/
├── src/
│   └── index.html          # Full UI — HTML + CSS + JS (no build step)
├── src-tauri/
│   ├── src/
│   │   ├── main.rs         # Entry point
│   │   └── lib.rs          # Rust commands (list_live_photos)
│   ├── capabilities/
│   │   └── default.json    # Tauri v2 permissions
│   ├── Cargo.toml
│   ├── build.rs
│   └── tauri.conf.json
├── package.json
└── README.md
```

## Icons

Before release, generate icons from a 1024×1024 PNG source:
```bash
npx tauri icon path/to/icon.png
```
This creates all required sizes in `src-tauri/icons/`.

---

## License

MIT

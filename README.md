# Raijin

<div align="center">

**A native, high-performance desktop download manager built in Rust using GPUI.**

[![UI](https://img.shields.io/badge/UI-GPUI_Kit-blue.svg)](https://github.com/zed-industries/zed)
[![Platform](https://img.shields.io/badge/Platform-Windows%20%7C%20Linux%20%7C%20macOS-lightgrey.svg)](#installation)
[![License](https://img.shields.io/badge/License-Apache--2.0-green.svg)](#license)

</div>

---

## Overview

Raijin is an open-source, cross-platform desktop download manager inspired by AB Download Manager and XDM (Xtreme Download Manager). Implemented entirely in Rust, Raijin combines an asynchronous actor-based HTTP engine with a hardware-accelerated user interface rendered via GPUI Kit.

---

## Features

- Multi-connection segmented downloading for accelerated transfer speeds
- Resilient pause and resume with remote integrity verification
- Global and per-download bandwidth and speed limiter
- Modern, hardware-accelerated desktop UI with dark and light themes
- System tray integration with background downloading
- Browser extension integration for Chromium and Firefox
- Automatic file categorization
- Multi-platform support (Windows, Linux, macOS)

---

## Installation

> [!NOTE]
> Prebuilt binary packages (installers and portable archives) will be published in upcoming releases. Currently, Raijin must be compiled from source.

### Building From Source

1. **Clone the repository:**
   ```bash
   git clone https://github.com/username/raijin.git
   cd raijin
   ```

2. **Build and execute the desktop application:**
   ```bash
   cargo run --release -p raijin-ui
   ```

3. **Compile the release binary:**
   ```bash
   cargo build --release -p raijin-ui
   ```
   The resulting binary will be located in the `target/release/` directory:
   - **Windows:** `target/release/raijin.exe`
   - **Linux / macOS:** `target/release/raijin`

---

## Command Line Interface

Raijin supports execution arguments for integration and automation:

```bash
# Launch the desktop interface
raijin

# Launch directly into the system tray without displaying the main window
raijin --minimized

# Open the download creation dialog with a specified URL
raijin --add "https://example.com/file.iso"
```

### Environment Variables

Configuration paths and operational parameters can be overridden using environment variables:

| Variable | Description |
| :--- | :--- |
| `RAIJIN_DATA_DIR` | Specifies the base directory for application data and database files. |
| `RAIJIN_DOWNLOAD_DIR` | Sets the default destination directory for completed downloads. |
| `RAIJIN_DOWNLOAD_ROOT` | Sets the root path used for category-based automatic sorting. |
| `RAIJIN_DATABASE_URL` | Explicit SQLite connection string override. |
| `RUST_LOG` | Tracing filter directive (for example, `info`, `warn`, or `raijin_core=debug`). |

---

## Browser Extension

A companion extension source is located in the [`browser-extension/`](./browser-extension) directory.

### Build Instructions

1. **Requirements:** Node.js (v18 or higher) and [pnpm](https://pnpm.io/):
   ```bash
   npm install -g pnpm
   ```

2. **Install dependencies and compile:**
   ```bash
   cd browser-extension
   pnpm install
   pnpm build          # Builds both Chromium (MV3) and Firefox (MV2)
   pnpm build:firefox  # Builds only the Firefox-compatible extension
   ```

3. **Load in Browser:**
   - **Chromium (Chrome, Brave, Edge):** Navigate to `chrome://extensions`, enable **Developer mode**, select **Load unpacked**, and choose `browser-extension/.output/chrome-mv3`.
   - **Firefox (development):** Navigate to `about:debugging#/runtime/this-firefox`, select **Load Temporary Add-on**, and choose `browser-extension/.output/firefox-mv2/manifest.json`. Alternatively run `pnpm dev:firefox`, which loads it automatically.
   - **Firefox (permanent):** Firefox release only installs signed add-ons; loading the unsigned zip from `pnpm zip` fails with "appears to be corrupt". Sign it with your AMO credentials first:
     ```bash
     WEB_EXT_API_KEY=... WEB_EXT_API_SECRET=... pnpm build:firefox && pnpm sign:firefox
     ```

---

## Development and Verification

Raijin is structured as a Cargo workspace:
- `crates/core`: Headless download engine, database models, monitor projection, and platform abstractions.
- `crates/ui`: GPUI Kit application shell, view components, and presentation logic.

Execute the test suites and verification checks:

```powershell
# Run unit and integration tests for core logic
cargo test -p raijin-core

# Run all workspace test suites
cargo test --workspace

# Run Clippy checks across all targets
cargo clippy --all-targets -- -D warnings

# Verify formatting compliance
cargo fmt --all --check
```

---

## Project Status and Roadmap

- [x] Multi-connection HTTP engine with dynamic chunk splitting and probe validation.
- [x] SQLite persistence using Write-Ahead Logging (WAL) and transactional updates.
- [x] Hardware-accelerated user interface built with GPUI Kit (`DataTable`, category filtering).
- [x] Global and per-item token-bucket rate limiting.
- [x] System tray integration with low-resource background execution.
- [x] Local REST API and companion browser extension for Chromium and Firefox.
- [ ] Queue management and timed download scheduling.
- [ ] Precompiled binary packaging (Windows portable zip/installer, Linux tar/deb/AppImage, macOS DMG).
- [ ] Streaming media protocol support (HLS), proxy configurations, and cryptographic checksum validation.

---

## Acknowledgments

- [AB Download Manager](https://github.com/amir1376/ab-download-manager) for modern desktop UI/UX design patterns and user interaction workflows.
- [Xtreme Download Manager (XDM)](https://github.com/subhra74/xdm) for architectural inspiration in multi-connection segmented downloading and core engine design.

---

## License

This project is licensed under either the [Apache License, Version 2.0](https://www.apache.org/licenses/LICENSE-2.0).

# Local API & Browser Integration

Status: **Active development (M7 partial)**.

---

## 1. Local HTTP API (`axum`)

The local API server provides IPC between external consumers (the `wxt` browser extension, CLI, and third-party tools) and the Raijin desktop engine.

### Security Invariants:
- **Localhost Only**: Binds strictly to loopback (`127.0.0.1:[port]`). Never expose on `0.0.0.0`.
- **Token Authentication**: All mutating endpoints (such as `POST /add`) require a valid Bearer token matching `integration.api_token`.
- **No Arbitrary File Operations**: The API must never allow external clients to read, write, or delete arbitrary files outside configured download directories.

### Endpoints (Unversioned MVP):
- `GET /ping` — Health check, returns engine status and version.
- `POST /add` — Add a new download job (URL, headers, referrer, optional destination).
- `GET /downloads` — List active downloads (delegates to monitor projection).
- `GET /queues` — List queues (read-only until queue milestone is unheld).

*Note: `/api/v1` aliases may be added in the future once the API contract stabilizes.*

---

## 2. Browser Extension (`wxt`)

- Located in `browser-extension/`.
- Single codebase targeting both Chromium and Firefox using `wxt`.
- Stores the local API port and Bearer token in extension storage.
- Handles browser download interception and context menu triggers ("Download with Raijin").


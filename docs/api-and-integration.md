# Local API & Browser Integration

Status: **Active development (M7 partial)**.

## 1. Access model

The `axum` API connects the browser extension, CLI, and third-party tools to the desktop app through commands. The engine and projected monitor state remain separate from HTTP handlers.

**Decision:** enable zero-setup Basic access when browser integration is running, without disabling authentication globally. Basic access may only submit downloads for explicit desktop confirmation. Silent operations and private snapshots require `Authorization: Bearer <token>` matching `integration.api_token`.

| Endpoint | Basic access (no Authorization header) | Valid token |
|---|---|---|
| `POST /ping` | Health check; returns `{"basicAccess":true}` | Same |
| `POST /add` | Always opens confirmation; ignores silent flags | Confirmation unless `silentAdd` is true |
| `POST /start-headless-download` | `401` | Adds and starts downloads without confirmation |
| `GET /downloads` | `401` | Compact projected download list |
| `GET /queues` | `401` | Read-only queue snapshot |

An invalid, empty, malformed, or duplicated Authorization header on `/add` is rejected with `401`; it never falls back to Basic access. `/ping` is intentionally public and is not a credential check.

For authenticated `/add`, `silentAdd: true` bypasses confirmation; `silentStart` determines whether to enqueue immediately. Basic access cannot bypass confirmation through either flag or the headless endpoint. `202 Accepted` for confirmation only acknowledges handing the request to the desktop, not user approval or download creation.

## 2. Security boundaries

- Bind strictly to `127.0.0.1:<port>`, never `0.0.0.0`.
- Require exactly one Host header matching `127.0.0.1:<bound-port>` or `localhost:<bound-port>`. Reject conflicting absolute request authority. Do not trust forwarded headers. This blocks attacker-controlled hostnames used in DNS rebinding.
- If Origin is present, require exactly one syntactically valid Chromium or Firefox extension origin. Reject website origins, opaque `null`, malformed origins, and multiple values before dispatch.
- This is an **extension-origin class restriction**, not an allowlist identifying the official extension. Chromium development IDs vary and Firefox extension origins use installation-specific UUIDs. Requests without Origin remain supported for extension background clients and local tools. Neither Origin nor its absence authenticates local processes; sensitive capabilities always require the token.
- Do not enable CORS or return wildcard/reflected CORS headers. Extension background requests use browser host permissions. Ordinary web pages are not supported API clients. Test actual Chromium and Firefox builds before release; browser policy is not the sole security boundary.
- Require JSON for add requests. Limit bodies to 256 KiB, Basic batches to 8 items, and authenticated batches to 32 items. Reject empty batches and unsupported/non-HTTP(S) items before dispatch.
- Basic access permits only one pending batch across all clients. A shared permit remains held until every confirmation window from that batch closes, including cancellation. Allow at most one accepted Basic batch per five seconds. Excess requests receive `429`, without opening windows.
- Basic confirmation windows do not steal focus. Integration confirmation does not probe the remote URL or start network activity before user approval. Supplied headers and referrer are retained for the approved URL, but not reused if the user changes it.
- External clients cannot select arbitrary destination paths. Desktop confirmation shows the URL and resolved destination; existing filename and engine validation still apply. Authentication does not authorize arbitrary file operations.
- Generate tokens with OS-backed cryptographic randomness and persist them in SQLite. Token rotation invalidates existing credentials immediately. Never place tokens in URLs, query strings, logs, or JSON sidecars.

Basic access still trusts local clients to request user attention. Limits reduce confirmation spam but do not sandbox malicious software running as the user. No download is approved merely because its request reached loopback.

## 3. Browser extension

The `wxt` extension lives in `browser-extension/` and targets Chromium and Firefox. Network calls run in its background context, not in page content scripts.

- Empty token: show **Connected — Basic access**, disable silent options, and submit through `/add` for desktop confirmation.
- Valid token: show **Connected — Authenticated** after an authenticated snapshot probe.
- Rejected token: report the error without silently clearing it or retrying unauthenticated.
- Only advertise Basic connectivity when `/ping` explicitly reports support; older desktop builds do not gain unauthenticated capability merely by answering ping.
- Store the port and optional token in extension-local storage, never browser sync.
- While the popup is open, check connectivity immediately and schedule the next check 250 ms after the previous check completes. Read the latest port/token each time, preserve the last status while checking, and do not clear download action messages. Closing/unmounting the popup stops scheduling; an already-dispatched background request may finish, but cannot restart polling. No persistent background polling, WebSocket, or new endpoint is introduced. The short interval is chosen for responsive app-start detection; request latency and timeouts still affect detection time.

### Automatic download capture

Download listeners register synchronously during background startup, before asynchronous context-menu initialization. Capture follows browser download events, not arbitrary page clicks or URL-extension guesses.

A new download is tracked by download ID. When the browser filename is absent or has no extension, or a configured minimum size cannot yet be checked, `downloads.onChanged` updates the candidate instead of permanently rejecting it. The browser-resolved filename is reduced to its basename and sent as `suggestedName`; desktop sanitization and validation still apply. This uses metadata already obtained by the browser and adds no pre-approval remote probe.

Tracking is limited to 128 downloads and 20 seconds per item. Historical, terminal, erased, and expired items are not revived by change events. Unsupported file types, text responses, blacklist matches (including the final redirect URL), bypass, and disabled capture remain exclusions. Unknown size is only a reason to wait when a minimum size is configured.

Each tracked ID has at most one submission attempt, including while metadata events arrive during API I/O. Only accepted submissions cancel the browser download. Failure or incomplete metadata leaves it with the browser; automatic retry after an ambiguous API timeout is deliberately avoided to prevent duplicate confirmation windows. Diagnostics contain decision/error categories and download IDs, not URLs, paths, headers, or credentials.

The submission keeps the original download URL and only gathers headers for that URL, never by transferring original credentials to the final redirect host. Cold-start, Native Messaging, and remote metadata probing in the confirmation form remain outside this change. Browser smoke testing is still required, particularly for redirect URLs without a filename such as the VS Code installer endpoint.

## 4. Rationale and deferred work

Rejected: a global authentication-OFF toggle that exposes silent downloads and private lists to every local client. It simplifies setup by removing the security boundary rather than narrowing the unauthenticated capability.

Chosen: Basic submission plus mandatory confirmation. This makes the core browser action usable without copying a token, while automation and private data retain authentication. The existing token copy/paste flow remains available for advanced access.

Deferred: Native Messaging, automatic pairing, per-client credentials, and official extension identity pinning. These are not required for Basic access and must not be simulated by an endpoint that gives out the API token.

No schema changes are required. The existing integration token and extension configuration remain valid.

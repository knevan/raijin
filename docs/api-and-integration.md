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

For authenticated `/add`, `silentAdd: true` bypasses confirmation; `silentStart` determines whether to enqueue immediately. Basic access cannot bypass confirmation through either flag or the headless endpoint. `202 Accepted` for confirmation acknowledges successful creation of every confirmation window, not user approval or download creation. If any window fails to open, windows already created for that batch are closed and the handoff fails.

## 2. Security boundaries

- Bind strictly to `127.0.0.1:<port>`, never `0.0.0.0`.
- Require exactly one Host header matching `127.0.0.1:<bound-port>` or `localhost:<bound-port>`. Reject conflicting absolute request authority. Do not trust forwarded headers. This blocks attacker-controlled hostnames used in DNS rebinding.
- If Origin is present, require exactly one syntactically valid Chromium or Firefox extension origin. Reject website origins, opaque `null`, malformed origins, and multiple values before dispatch.
- This is an **extension-origin class restriction**, not an allowlist identifying the official extension. Chromium development IDs vary and Firefox extension origins use installation-specific UUIDs. Requests without Origin remain supported for extension background clients and local tools. Neither Origin nor its absence authenticates local processes; sensitive capabilities always require the token.
- Do not enable CORS or return wildcard/reflected CORS headers. Extension background requests use browser host permissions. Ordinary web pages are not supported API clients. Test actual Chromium and Firefox builds before release; browser policy is not the sole security boundary.
- Require JSON for add requests. Limit bodies to 256 KiB, Basic batches to 8 items, and authenticated batches to 32 items. Reject empty batches and unsupported/non-HTTP(S) items before dispatch.
- Basic access permits only one pending batch across all clients. A shared permit remains held until every confirmation window from that batch closes, including cancellation. Allow at most one accepted Basic batch per five seconds. Excess requests receive `429`, without opening windows.
- Basic confirmation windows do not steal focus. Confirmation windows automatically probe missing metadata before download approval; accepting a confirmation request therefore permits bounded remote metadata requests, not file creation or transfer approval. The same rule applies to Basic clients, which cannot be reliably distinguished from an explicit extension context-menu action. Probes use no captured credentials, a ten-second timeout, the existing bounded-body HTTP probe, and at most ten redirects. Changing the URL or closing the window aborts the probe; stale results cannot overwrite newer state. Supplied headers and referrer remain retained for the approved download URL, but are not reused if the user changes it.
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

Request and download listeners register synchronously during background startup, before asynchronous context-menu initialization. Primary detection uses non-blocking `webRequest.onHeadersReceived`, with the `webRequest` permission declared on both browser targets. Successful GET frame responses are classified using response filename, final URL, MIME type, size, blacklist, and bypass policy. `onSendHeaders` retains the original URL and headers while tracking redirect URL aliases; credentials from a redirect host are never attached to the original URL. Detection does not depend on arbitrary page clicks or a filename-determination hook.

A response can submit the desktop handoff before a browser download ID or destination filename exists. `downloads.onCreated` associates the ID with the request and waits for that same handoff before cancellation. Response and download paths share the submission outcome to prevent duplicate windows, including failures and ambiguous API timeouts. On Chromium MV3, observing headers does not block the response. Where `downloads.onDeterminingFilename` exists, a synchronous listener returns `true` and defers its `suggest()` callback while the shared handoff completes. This gates destination determination instead of relying on cancellation winning a race with Save As. The callback is completed exactly once after successful browser cancellation, or when capture is rejected, incomplete, fails, or the item terminates. Cancellation failure leaves browser history and files intact.

The filename gate is bounded to nine seconds (the eight-second API timeout plus one second of scheduling headroom). Expiry releases normal browser handling, prevents new automatic submissions for that item, and prevents a late acceptance from starting cancellation. An already-dispatched HTTP request cannot be rolled back: an ambiguous timeout can still produce a late desktop window alongside browser fallback, so it is never automatically retried. Downloads that leave filename determination with insufficient metadata are also released without later automatic takeover. Historical, terminal, non-HTTP(S), and extension-created items bypass the gate synchronously; other ineligible items resume as soon as their capture policy is evaluated. Firefox does not implement this hook and retains the existing response/download lifecycle path; no global browser Save As preference is changed.

Chosen: gate filename determination and cancel only after desktop acceptance. Rejected: cancel immediately when response classification succeeds, because desktop failure could otherwise strand the download without a working browser recovery path. The gate's actual native Save As behavior still requires Brave smoke testing; it is not an atomic browser/desktop transaction.

Download events remain a metadata fallback. When the filename is absent or has no extension, or a configured minimum size cannot yet be checked, `downloads.onChanged` updates the candidate instead of permanently rejecting it. Browser-resolved basenames are sent as `suggestedName`, and optional non-negative `totalBytes` hints populate the confirmation badge. A partial response uses the total from `Content-Range`, never its range's `Content-Length`. Hints do not replace engine validation.

Tracking is limited to 128 downloads and 20 seconds per item. Historical, terminal, erased, and expired items are not revived by change events. Unsupported file types, text responses, blacklist matches (including the final redirect URL), bypass, and disabled capture remain exclusions. Unknown size is only a reason to wait when a minimum size is configured.

Each tracked ID has at most one submission attempt, including while metadata events arrive during API I/O. Only accepted submissions cancel the browser download. Failure or incomplete metadata leaves it with the browser; automatic retry after an ambiguous API timeout is deliberately avoided to prevent duplicate confirmation windows. Diagnostics contain decision/error categories and download IDs, not URLs, paths, headers, or credentials.

The submission keeps the original download URL and only gathers headers for that URL. Confirmation opens without waiting for metadata resolution. Complete filename/size hints avoid another request; missing metadata triggers an automatic probe, including context-menu submissions with only a URL. URL-derived fallback names are not treated as user edits. Filename, category, and folder updates respect manual edits; a failed probe leaves the form submittable with unknown size. Credential-dependent endpoints may not resolve through the unauthenticated probe, but captured credentials remain available for the approved download.

Cold-start and Native Messaging remain deferred. Browser smoke testing is required with Save As enabled, especially for redirect endpoints such as the VS Code installer: verify response handoff before filename selection, one window per download, size display, context-menu probing, and browser fallback when the app rejects or cannot accept a handoff.

## 4. Rationale and deferred work

Rejected: a global authentication-OFF toggle that exposes silent downloads and private lists to every local client. It simplifies setup by removing the security boundary rather than narrowing the unauthenticated capability.

Chosen: Basic submission plus mandatory confirmation. This makes the core browser action usable without copying a token, while automation and private data retain authentication. The existing token copy/paste flow remains available for advanced access.

Deferred: Native Messaging, automatic pairing, per-client credentials, and official extension identity pinning. These are not required for Basic access and must not be simulated by an endpoint that gives out the API token.

No schema changes are required. The existing integration token and extension configuration remain valid.

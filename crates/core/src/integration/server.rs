//! Localhost-only integration server.
//!
//! Binds `127.0.0.1` and never `0.0.0.0`. Basic access can only request
//! confirmation; silent operations and snapshots require a Bearer token.
//!
//! The server holds no engine state. It translates HTTP into
//! [`IntegrationCommand`] values and awaits a [`IntegrationReply`], so the UI
//! (or a headless client) stays the only thing that touches the download engine.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Serialize;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use crate::download::{Bytes, DownloadItem, NewDownload};
use crate::integration::auth::TokenCell;
use crate::integration::{
    AddDownloadRequest, ApiErrorBody, DownloadRequestItem, DownloadRequestOptions, DownloadSummary,
    DownloadType, QueueSummary, file_name_for,
};

/// Bound channel capacity for inbound integration commands.
pub const DEFAULT_COMMAND_BUFFER: usize = 32;

/// Reply channel for one integration command.
pub type IntegrationResponder = oneshot::Sender<IntegrationReply>;

/// Command raised by an HTTP request and handled by the app.
#[derive(Debug)]
pub enum IntegrationCommand {
    /// Non-silent add: the app must confirm with the user before downloading.
    ConfirmAdd {
        /// Requested items.
        items: Vec<DownloadRequestItem>,
        /// Behavior flags from the request.
        options: DownloadRequestOptions,
        /// Held until all basic confirmation windows close.
        pending: Option<Arc<OwnedSemaphorePermit>>,
        /// Reply channel.
        reply: IntegrationResponder,
    },
    /// Headless add: enqueue without any dialog.
    HeadlessAdd {
        /// Requested items.
        items: Vec<DownloadRequestItem>,
        /// Behavior flags from the request.
        options: DownloadRequestOptions,
        /// Reply channel.
        reply: IntegrationResponder,
    },
    /// Compact download list for the extension popup.
    ListDownloads {
        /// Reply channel.
        reply: IntegrationResponder,
    },
    /// Queue list for the extension popup.
    ListQueues {
        /// Reply channel.
        reply: IntegrationResponder,
    },
}

/// Result the app sends back for an [`IntegrationCommand`].
#[derive(Debug, Clone)]
pub enum IntegrationReply {
    /// Items were accepted (already persisted or queued).
    Accepted {
        /// Ids assigned to the created downloads.
        ids: Vec<i64>,
    },
    /// The user declined the confirmation dialog.
    Rejected,
    /// Downloads snapshot.
    Downloads(Vec<DownloadSummary>),
    /// Queues snapshot.
    Queues(Vec<QueueSummary>),
    /// Handler failed with a client-visible message.
    Failed(String),
}

/// Cloneable handle used by HTTP handlers to reach the app.
#[derive(Debug, Clone)]
pub struct IntegrationHandle {
    commands: mpsc::Sender<IntegrationCommand>,
}

impl IntegrationHandle {
    /// Creates a handle plus the receiving end the app must drain.
    #[must_use]
    pub fn channel(buffer: usize) -> (Self, mpsc::Receiver<IntegrationCommand>) {
        let (commands, receiver) = mpsc::channel(buffer.max(1));
        (Self { commands }, receiver)
    }

    async fn request(
        &self,
        build: impl FnOnce(IntegrationResponder) -> IntegrationCommand,
    ) -> Result<IntegrationReply, ApiFailure> {
        let (reply, receiver) = oneshot::channel();
        self.commands
            .send(build(reply))
            .await
            .map_err(|_| ApiFailure::internal("app_unavailable"))?;
        receiver
            .await
            .map_err(|_| ApiFailure::internal("app_unavailable"))
    }
}

/// Failure surfaced to the extension.
#[derive(Debug)]
pub struct ApiFailure {
    status: StatusCode,
    message: &'static str,
}

impl ApiFailure {
    fn new(status: StatusCode, message: &'static str) -> Self {
        Self { status, message }
    }

    fn internal(message: &'static str) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    fn bad_request(message: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    /// Machine-readable reason, also used as the JSON `error` field.
    #[must_use]
    pub const fn message(&self) -> &'static str {
        self.message
    }
}

impl IntoResponse for ApiFailure {
    fn into_response(self) -> Response {
        (
            self.status,
            axum::Json(ApiErrorBody {
                error: self.message.to_owned(),
            }),
        )
            .into_response()
    }
}

/// Converted request ready to hand to the engine.
#[derive(Debug, Clone)]
pub struct PreparedItem {
    /// Engine request.
    pub request: NewDownload,
    /// Original payload, kept for dialog display and validation messaging.
    pub source: DownloadRequestItem,
}

/// Builds a [`NewDownload`] from an integration item.
///
/// Rejects `hls` items: the desktop engine speaks HTTP only until the media
/// pipeline lands, and silently degrading an HLS link to a raw HTTP download
/// would save a playlist instead of a video.
///
/// # Errors
///
/// Returns [`ApiFailure`] when the URL is not http(s) or the type is unsupported.
pub fn prepare_item(item: &DownloadRequestItem, folder: &Path) -> Result<PreparedItem, ApiFailure> {
    if item.r#type == DownloadType::Hls {
        return Err(ApiFailure::bad_request("unsupported_type"));
    }
    let link = item.link.trim();
    if !is_http_url(link) {
        return Err(ApiFailure::bad_request("invalid_url"));
    }
    let headers: BTreeMap<String, String> = item
        .headers
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|(name, value)| !name.trim().is_empty() && !value.is_empty())
        .collect();
    let request = NewDownload {
        url: link.to_owned(),
        download_page: item.download_page.clone(),
        headers,
        file_name: file_name_for(item),
        folder: folder.to_path_buf(),
        preferred_connections: None,
        speed_limit: None,
    };
    Ok(PreparedItem {
        request,
        source: item.clone(),
    })
}

/// Validates and converts a full request body.
///
/// # Errors
///
/// Returns [`ApiFailure`] for empty item lists or invalid items.
pub fn prepare_request(
    request: &AddDownloadRequest,
    folder: &Path,
) -> Result<Vec<PreparedItem>, ApiFailure> {
    if request.items.is_empty() {
        return Err(ApiFailure::bad_request("no_items"));
    }
    request
        .items
        .iter()
        .map(|item| prepare_item(item, folder))
        .collect()
}

/// Maps a persisted download into the wire summary.
#[must_use]
pub fn summarize_item(item: &DownloadItem) -> DownloadSummary {
    DownloadSummary {
        id: item.id.get(),
        url: item.url.clone(),
        file_name: item.file_name.clone(),
        status: item.status.as_str().to_owned(),
        downloaded_bytes: item.downloaded_bytes.get(),
        total_bytes: item.total_bytes.map(Bytes::get),
    }
}

fn is_http_url(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://"))
        && reqwest::Url::parse(url).is_ok_and(|url| url.host_str().is_some())
}

/// Shared state for the router.
#[derive(Clone)]
struct ServerState {
    handle: IntegrationHandle,
    token: TokenCell,
    port: u16,
    basic_pending: Arc<Semaphore>,
    basic_last_request: Arc<Mutex<Option<Instant>>>,
}

/// Binds the server to `127.0.0.1:port` and serves until the task is dropped.
///
/// The token is read from `token` on every request, so regenerating it takes
/// effect without rebinding the listener.
///
/// Returns the bound address so the caller can log the effective port.
///
/// # Errors
///
/// Returns an error when the address cannot be bound.
pub async fn serve(
    handle: IntegrationHandle,
    token: TokenCell,
    port: u16,
) -> std::io::Result<SocketAddr> {
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    let address = listener.local_addr()?;
    let state = ServerState {
        handle,
        token,
        port: address.port(),
        basic_pending: Arc::new(Semaphore::new(1)),
        basic_last_request: Arc::new(Mutex::new(None)),
    };

    let protected = Router::new()
        .route("/start-headless-download", post(start_headless))
        .route("/downloads", get(list_downloads))
        .route("/queues", get(list_queues))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token));
    let app = Router::new()
        .route("/ping", post(ping))
        .route("/add", post(add))
        .merge(protected)
        .layer(DefaultBodyLimit::max(256 * 1024))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_local_request,
        ))
        .with_state(state);
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::warn!(?error, "integration server stopped");
        }
    });
    Ok(address)
}

async fn ping() -> Response {
    axum::Json(serde_json::json!({"basicAccess": true})).into_response()
}

async fn require_local_request(
    State(state): State<ServerState>,
    request: Request,
    next: Next,
) -> Response {
    let hosts = request
        .headers()
        .get_all(header::HOST)
        .iter()
        .collect::<Vec<_>>();
    let valid_host = hosts.len() == 1
        && hosts[0].to_str().ok().is_some_and(|host| {
            host == format!("127.0.0.1:{}", state.port)
                || host == format!("localhost:{}", state.port)
        });
    let valid_authority = request.uri().authority().is_none_or(|authority| {
        hosts.first().and_then(|host| host.to_str().ok()) == Some(authority.as_str())
    });
    let origins = request
        .headers()
        .get_all(header::ORIGIN)
        .iter()
        .collect::<Vec<_>>();
    let valid_origin = origins.is_empty()
        || (origins.len() == 1 && origins[0].to_str().ok().is_some_and(extension_origin));
    if !valid_host || !valid_authority || !valid_origin {
        return ApiFailure::new(StatusCode::FORBIDDEN, "untrusted_request").into_response();
    }
    next.run(request).await
}

fn extension_origin(origin: &str) -> bool {
    if let Some(id) = origin.strip_prefix("chrome-extension://") {
        return id.len() == 32 && id.bytes().all(|byte| (b'a'..=b'p').contains(&byte));
    }
    if let Some(id) = origin.strip_prefix("moz-extension://") {
        return id.len() == 36
            && id.bytes().enumerate().all(|(index, byte)| {
                if [8, 13, 18, 23].contains(&index) {
                    byte == b'-'
                } else {
                    byte.is_ascii_hexdigit()
                }
            });
    }
    false
}

fn authenticated(headers: &HeaderMap, token: &TokenCell) -> bool {
    let expected = token.get();
    !expected.is_empty()
        && presented_token(headers)
            .is_some_and(|presented| crate::integration::auth::verify(&expected, &presented))
}

async fn add(
    State(state): State<ServerState>,
    headers: HeaderMap,
    body: Result<axum::Json<AddDownloadRequest>, JsonRejection>,
) -> Result<Response, ApiFailure> {
    let trusted = authenticated(&headers, &state.token);
    if headers.contains_key(header::AUTHORIZATION) && !trusted {
        return Err(ApiFailure::new(StatusCode::UNAUTHORIZED, "unauthorized"));
    }
    let axum::Json(mut request) = body.map_err(json_failure)?;
    validate_items(&request, if trusted { 32 } else { 8 })?;
    let pending = if trusted {
        None
    } else {
        let permit = Arc::clone(&state.basic_pending)
            .try_acquire_owned()
            .map_err(|_| ApiFailure::new(StatusCode::TOO_MANY_REQUESTS, "confirmation_pending"))?;
        {
            let mut last = state
                .basic_last_request
                .lock()
                .map_err(|_| ApiFailure::internal("rate_limit_unavailable"))?;
            let now = Instant::now();
            if last.is_some_and(|instant| now.duration_since(instant) < Duration::from_secs(5)) {
                return Err(ApiFailure::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "rate_limited",
                ));
            }
            *last = Some(now);
        }
        request.options = DownloadRequestOptions::default();
        Some(Arc::new(permit))
    };
    let reply = state
        .handle
        .request(|reply| {
            if trusted && request.options.silent_add {
                IntegrationCommand::HeadlessAdd {
                    items: request.items,
                    options: request.options,
                    reply,
                }
            } else {
                IntegrationCommand::ConfirmAdd {
                    items: request.items,
                    options: request.options,
                    pending,
                    reply,
                }
            }
        })
        .await?;
    accepted(reply)
}

fn json_failure(rejection: JsonRejection) -> ApiFailure {
    ApiFailure::new(rejection.status(), "bad_request")
}

fn validate_items(request: &AddDownloadRequest, limit: usize) -> Result<(), ApiFailure> {
    if request.items.is_empty() || request.items.len() > limit {
        return Err(ApiFailure::bad_request("invalid_item_count"));
    }
    for item in &request.items {
        if item.r#type != DownloadType::Http || !is_http_url(item.link.trim()) {
            return Err(ApiFailure::bad_request("invalid_item"));
        }
    }
    Ok(())
}

async fn start_headless(
    State(state): State<ServerState>,
    body: Result<axum::Json<AddDownloadRequest>, JsonRejection>,
) -> Result<Response, ApiFailure> {
    let axum::Json(mut request) = body.map_err(json_failure)?;
    validate_items(&request, 32)?;
    request.options.silent_start = true;
    let reply = state
        .handle
        .request(|reply| IntegrationCommand::HeadlessAdd {
            items: request.items,
            options: request.options,
            reply,
        })
        .await?;
    accepted(reply)
}

async fn list_downloads(State(state): State<ServerState>) -> Result<Response, ApiFailure> {
    let reply = state
        .handle
        .request(|reply| IntegrationCommand::ListDownloads { reply })
        .await?;
    match reply {
        IntegrationReply::Downloads(items) => Ok(axum::Json(items).into_response()),
        IntegrationReply::Failed(message) => {
            tracing::warn!(%message, "integration list downloads failed");
            Err(ApiFailure::internal("internal"))
        }
        _ => Err(ApiFailure::internal("internal")),
    }
}

async fn list_queues(State(state): State<ServerState>) -> Result<Response, ApiFailure> {
    let reply = state
        .handle
        .request(|reply| IntegrationCommand::ListQueues { reply })
        .await?;
    match reply {
        IntegrationReply::Queues(queues) => Ok(axum::Json(queues).into_response()),
        IntegrationReply::Failed(message) => {
            tracing::warn!(%message, "integration list queues failed");
            Err(ApiFailure::internal("internal"))
        }
        _ => Err(ApiFailure::internal("internal")),
    }
}

fn accepted(reply: IntegrationReply) -> Result<Response, ApiFailure> {
    match reply {
        IntegrationReply::Accepted { ids } => {
            Ok((StatusCode::ACCEPTED, axum::Json(AcceptedBody { ids })).into_response())
        }
        IntegrationReply::Rejected => {
            Ok((StatusCode::OK, axum::Json(RejectedBody { accepted: false })).into_response())
        }
        IntegrationReply::Failed(message) => {
            tracing::warn!(%message, "integration command failed");
            Err(ApiFailure::internal("internal"))
        }
        _ => Err(ApiFailure::internal("internal")),
    }
}

#[derive(Serialize)]
struct AcceptedBody {
    ids: Vec<i64>,
}

#[derive(Serialize)]
struct RejectedBody {
    accepted: bool,
}

async fn require_token(State(state): State<ServerState>, request: Request, next: Next) -> Response {
    if authenticated(request.headers(), &state.token) {
        return next.run(request).await;
    }
    (
        StatusCode::UNAUTHORIZED,
        axum::Json(ApiErrorBody {
            error: "unauthorized".to_owned(),
        }),
    )
        .into_response()
}

fn presented_token(headers: &HeaderMap) -> Option<String> {
    if headers.get_all(header::AUTHORIZATION).iter().count() != 1 {
        return None;
    }
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = value
        .strip_prefix("Bearer ")
        .or_else(|| value.strip_prefix("bearer "))?;
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    Some(token.to_owned())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn item(link: &str, kind: DownloadType) -> DownloadRequestItem {
        DownloadRequestItem {
            link: link.to_owned(),
            download_page: Some("https://example.com/watch".to_owned()),
            headers: None,
            description: None,
            suggested_name: None,
            r#type: kind,
        }
    }

    fn folder() -> PathBuf {
        PathBuf::from("C:/Downloads")
    }

    #[test]
    fn prepare_should_reject_hls_until_the_media_pipeline_lands() {
        let failure = prepare_item(
            &item("https://cdn.example.com/master.m3u8", DownloadType::Hls),
            &folder(),
        )
        .expect_err("hls is unsupported");
        assert_eq!(failure.status, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn prepare_should_reject_non_http_links() {
        for link in ["", "ftp://example.com/a.zip", "not-a-url", "https://"] {
            assert!(
                prepare_item(&item(link, DownloadType::Http), &folder()).is_err(),
                "must reject {link:?}"
            );
        }
    }

    #[test]
    fn prepare_should_drop_blank_headers() {
        let mut request = item("https://cdn.example.com/a.zip", DownloadType::Http);
        let mut headers = BTreeMap::new();
        headers.insert("Cookie".to_owned(), "a=b".to_owned());
        headers.insert("  ".to_owned(), "junk".to_owned());
        headers.insert("Empty".to_owned(), String::new());
        request.headers = Some(headers);

        let prepared = prepare_item(&request, &folder()).expect("valid item");
        assert_eq!(prepared.request.headers.len(), 1);
        assert_eq!(
            prepared.request.headers.get("Cookie"),
            Some(&"a=b".to_owned())
        );
    }

    #[test]
    fn prepare_should_keep_page_and_derive_the_file_name() {
        let prepared = prepare_item(
            &item("https://cdn.example.com/path/clip.mp4", DownloadType::Http),
            &folder(),
        )
        .expect("valid item");
        assert_eq!(prepared.request.file_name, "clip.mp4");
        assert_eq!(
            prepared.request.download_page.as_deref(),
            Some("https://example.com/watch")
        );
    }

    #[test]
    fn prepare_request_should_reject_empty_item_lists() {
        let request = AddDownloadRequest {
            items: Vec::new(),
            options: DownloadRequestOptions::default(),
        };
        assert!(prepare_request(&request, &folder()).is_err());
    }

    #[test]
    fn bearer_token_should_be_extracted_from_the_authorization_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer abc123".parse().expect("valid header"),
        );
        assert_eq!(presented_token(&headers), Some("abc123".to_owned()));
    }

    #[test]
    fn bearer_token_should_reject_missing_or_malformed_values() {
        let mut headers = HeaderMap::new();
        assert_eq!(presented_token(&headers), None);
        headers.insert(header::AUTHORIZATION, "abc123".parse().expect("valid"));
        assert_eq!(presented_token(&headers), None);
        headers.insert(header::AUTHORIZATION, "Bearer ".parse().expect("valid"));
        assert_eq!(presented_token(&headers), None);
    }

    /// Spawns the server on an ephemeral port with a fixed token.
    async fn spawn_server(token: &str) -> (String, mpsc::Receiver<IntegrationCommand>) {
        let (handle, receiver) = IntegrationHandle::channel(4);
        let address = serve(handle, TokenCell::new(token), 0)
            .await
            .expect("server binds");
        (format!("http://{address}"), receiver)
    }

    #[tokio::test]
    async fn ping_should_be_reachable_without_a_token() {
        let (base, _receiver) = spawn_server("secret").await;
        let response = reqwest::Client::new()
            .post(format!("{base}/ping"))
            .send()
            .await
            .expect("ping reachable");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn protected_routes_should_reject_missing_and_wrong_tokens() {
        let (base, _receiver) = spawn_server("secret").await;
        let client = reqwest::Client::new();
        let body = r#"{"items":[],"options":{"silentAdd":true,"silentStart":false}}"#;

        let missing = client
            .post(format!("{base}/start-headless-download"))
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .expect("request sent");
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

        let wrong = client
            .post(format!("{base}/add"))
            .header("Content-Type", "application/json")
            .header("Authorization", "Bearer nope")
            .body(body)
            .send()
            .await
            .expect("request sent");
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

        let downloads = client
            .get(format!("{base}/downloads"))
            .send()
            .await
            .expect("request sent");
        assert_eq!(downloads.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn add_should_forward_the_payload_and_answer_accepted() {
        let (base, mut receiver) = spawn_server("secret").await;
        // Answer the command as the app would.
        let app = tokio::spawn(async move {
            let command = receiver.recv().await.expect("command arrives");
            match command {
                IntegrationCommand::ConfirmAdd { items, reply, .. } => {
                    assert_eq!(items.len(), 1);
                    assert_eq!(items[0].link, "https://cdn.example.com/clip.mp4");
                    let _ = reply.send(IntegrationReply::Accepted { ids: Vec::new() });
                }
                other => panic!("unexpected command: {other:?}"),
            }
        });

        let response = reqwest::Client::new()
            .post(format!("{base}/add"))
            .header("Content-Type", "application/json")
            .header("Authorization", "Bearer secret")
            .body(
                r#"{"items":[{"link":"https://cdn.example.com/clip.mp4","downloadPage":null,
                    "headers":null,"description":null,"suggestedName":null,"type":"http"}],
                    "options":{"silentAdd":false,"silentStart":false}}"#,
            )
            .send()
            .await
            .expect("request sent");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        app.await.expect("app task finished");
    }

    #[tokio::test]
    async fn list_downloads_should_answer_from_the_app_reply() {
        let (base, mut receiver) = spawn_server("secret").await;
        let app = tokio::spawn(async move {
            let command = receiver.recv().await.expect("command arrives");
            match command {
                IntegrationCommand::ListDownloads { reply } => {
                    let _ = reply.send(IntegrationReply::Downloads(Vec::new()));
                }
                other => panic!("unexpected command: {other:?}"),
            }
        });

        let response = reqwest::Client::new()
            .get(format!("{base}/downloads"))
            .header("Authorization", "Bearer secret")
            .send()
            .await
            .expect("request sent");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.expect("body");
        assert_eq!(body, "[]");
        app.await.expect("app task finished");
    }

    #[tokio::test]
    async fn regenerated_tokens_should_apply_without_a_rebind() {
        let (handle, mut receiver) = IntegrationHandle::channel(4);
        // Drain commands like the app does, so an authorized request answers.
        let app = tokio::spawn(async move {
            while let Some(command) = receiver.recv().await {
                match command {
                    IntegrationCommand::ListDownloads { reply } => {
                        let _ = reply.send(IntegrationReply::Downloads(Vec::new()));
                    }
                    other => panic!("unexpected command: {other:?}"),
                }
            }
        });
        let token = TokenCell::new("old");
        let address = serve(handle, token.clone(), 0).await.expect("server binds");
        let base = format!("http://{address}");
        let client = reqwest::Client::new();

        let with_old = client
            .get(format!("{base}/downloads"))
            .header("Authorization", "Bearer old")
            .send()
            .await
            .expect("request sent");
        assert_eq!(with_old.status(), StatusCode::OK);

        token.set("new");

        let stale = client
            .get(format!("{base}/downloads"))
            .header("Authorization", "Bearer old")
            .send()
            .await
            .expect("request sent");
        assert_eq!(stale.status(), StatusCode::UNAUTHORIZED);

        let fresh = client
            .get(format!("{base}/downloads"))
            .header("Authorization", "Bearer new")
            .send()
            .await
            .expect("request sent");
        assert_eq!(fresh.status(), StatusCode::OK);

        drop(token);
        app.abort();
    }

    #[test]
    fn authentication_should_reject_empty_server_tokens_and_duplicate_credentials() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            "Bearer secret".parse().expect("header"),
        );
        assert!(!authenticated(&headers, &TokenCell::default()));
        headers.append(
            header::AUTHORIZATION,
            "Bearer secret".parse().expect("header"),
        );
        assert!(!authenticated(&headers, &TokenCell::new("secret")));
    }

    fn valid_body(count: usize) -> String {
        let items = (0..count)
            .map(|_| item("https://cdn.example.com/a.zip", DownloadType::Http))
            .collect();
        serde_json::to_string(&AddDownloadRequest {
            items,
            options: DownloadRequestOptions {
                silent_add: true,
                silent_start: true,
            },
        })
        .expect("serialize")
    }

    #[tokio::test]
    async fn basic_access_should_force_confirmation_and_bound_pending_requests() {
        let (base, mut receiver) = spawn_server("secret").await;
        let app = tokio::spawn(async move {
            let command = receiver.recv().await.expect("confirmation command");
            match command {
                IntegrationCommand::ConfirmAdd {
                    options,
                    pending,
                    reply,
                    ..
                } => {
                    assert!(!options.silent_add);
                    assert!(!options.silent_start);
                    reply
                        .send(IntegrationReply::Accepted { ids: Vec::new() })
                        .expect("reply");
                    pending.expect("basic permit")
                }
                other => panic!("unexpected command: {other:?}"),
            }
        });
        let client = reqwest::Client::new();
        let send = || {
            client
                .post(format!("{base}/add"))
                .header("Content-Type", "application/json")
                .body(valid_body(1))
                .send()
        };
        assert_eq!(
            send().await.expect("basic add").status(),
            StatusCode::ACCEPTED
        );
        let pending = app.await.expect("app task");
        assert_eq!(
            send().await.expect("pending add").status(),
            StatusCode::TOO_MANY_REQUESTS
        );
        let semaphore = Arc::clone(pending.semaphore());
        let second_window = Arc::clone(&pending);
        drop(pending);
        assert_eq!(semaphore.available_permits(), 0);
        drop(second_window);
        assert_eq!(semaphore.available_permits(), 1);
        assert_eq!(
            send().await.expect("rate limited add").status(),
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[tokio::test]
    async fn basic_access_should_not_expose_private_routes_or_downgrade_bad_credentials() {
        let (base, mut receiver) = spawn_server("secret").await;
        let client = reqwest::Client::new();
        for path in ["downloads", "queues"] {
            let response = client
                .get(format!("{base}/{path}"))
                .send()
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let response = client
            .post(format!("{base}/start-headless-download"))
            .header("Content-Type", "application/json")
            .body(valid_body(1))
            .send()
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        for value in ["Bearer wrong", "Bearer ", "Basic secret"] {
            let response = client
                .post(format!("{base}/add"))
                .header("Authorization", value)
                .header("Content-Type", "application/json")
                .body(valid_body(1))
                .send()
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn local_boundary_should_reject_web_origins_and_rebinding_hosts_even_with_a_token() {
        let (base, mut receiver) = spawn_server("secret").await;
        let client = reqwest::Client::new();
        for host in [
            "attacker.example:35343",
            "127.0.0.1.evil:35343",
            "127.0.0.1:0",
            "localhost",
        ] {
            let response = client
                .post(format!("{base}/ping"))
                .header("Host", host)
                .send()
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        for origin in [
            "https://attacker.example",
            "null",
            "http://localhost",
            "chrome-extension://invalid",
            "moz-extension://bad",
        ] {
            let response = client
                .post(format!("{base}/add"))
                .header("Origin", origin)
                .bearer_auth("secret")
                .header("Content-Type", "application/json")
                .body(valid_body(1))
                .send()
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        assert!(receiver.try_recv().is_err());
        for origin in [
            "chrome-extension://aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "moz-extension://12345678-1234-1234-1234-123456789abc",
        ] {
            let response = client
                .post(format!("{base}/ping"))
                .header("Origin", origin)
                .send()
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK);
            assert!(
                !response
                    .headers()
                    .contains_key("access-control-allow-origin")
            );
            assert_eq!(
                response.text().await.expect("body"),
                "{\"basicAccess\":true}"
            );
        }
        let response = client
            .post(format!("{base}/ping"))
            .header("Host", base.replace("http://127.0.0.1", "localhost"))
            .send()
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn add_should_bound_payloads_before_dispatch() {
        let (base, mut receiver) = spawn_server("secret").await;
        let client = reqwest::Client::new();
        for body in [valid_body(0), valid_body(9)] {
            let response = client
                .post(format!("{base}/add"))
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }
        let response = client
            .post(format!("{base}/add"))
            .header("Content-Type", "application/json")
            .body(" ".repeat(256 * 1024 + 1))
            .send()
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let response = client
            .post(format!("{base}/add"))
            .header("Content-Type", "text/plain")
            .body(valid_body(1))
            .send()
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn authenticated_silent_add_should_forward_start_policy() {
        for start in [false, true] {
            let (base, mut receiver) = spawn_server("secret").await;
            let app = tokio::spawn(async move {
                match receiver.recv().await.expect("command") {
                    IntegrationCommand::HeadlessAdd { options, reply, .. } => {
                        assert_eq!(options.silent_start, start);
                        reply
                            .send(IntegrationReply::Accepted { ids: vec![1] })
                            .expect("reply");
                    }
                    other => panic!("unexpected command: {other:?}"),
                }
            });
            let body =
                valid_body(1).replace("\"silentStart\":true", &format!("\"silentStart\":{start}"));
            let response = reqwest::Client::new()
                .post(format!("{base}/add"))
                .bearer_auth("secret")
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            app.await.expect("app task");
        }
    }

    #[test]
    fn summarize_should_expose_stable_status_tokens() {
        let item = DownloadItem {
            id: crate::download::DownloadId::new(7),
            kind: crate::download::DownloadKind::Http,
            url: "https://example.com/a.zip".to_owned(),
            download_page: None,
            headers: BTreeMap::new(),
            file_name: "a.zip".to_owned(),
            folder: folder(),
            status: crate::download::DownloadStatus::Downloading,
            total_bytes: Some(Bytes::new(2048)),
            downloaded_bytes: Bytes::new(512),
            etag: None,
            last_modified: None,
            preferred_connections: None,
            speed_limit: None,
            failure: None,
            created_at: 1,
            started_at: None,
            completed_at: None,
            updated_at: 1,
        };
        let summary = summarize_item(&item);
        assert_eq!(summary.id, 7);
        assert_eq!(summary.downloaded_bytes, 512);
        assert_eq!(summary.total_bytes, Some(2048));
        assert_eq!(summary.status, "downloading");
    }
}

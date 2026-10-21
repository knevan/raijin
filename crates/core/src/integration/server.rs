//! Localhost-only integration server.
//!
//! Binds `127.0.0.1` and never `0.0.0.0`. Mutating routes require
//! `Authorization: Bearer <token>`; `POST /ping` stays open so the extension can
//! distinguish "server down" from "token rejected".
//!
//! The server holds no engine state. It translates HTTP into
//! [`IntegrationCommand`] values and awaits a [`IntegrationReply`], so the UI
//! (or a headless client) stays the only thing that touches the download engine.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};

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
    (lower.starts_with("http://") || lower.starts_with("https://")) && url.len() > "https://".len()
}

/// Shared state for the router.
#[derive(Clone)]
struct ServerState {
    handle: IntegrationHandle,
    token: TokenCell,
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
    let state = ServerState { handle, token };

    // The token middleware is applied before `/ping` is merged in, so the
    // health check stays reachable without credentials.
    let protected = Router::new()
        .route("/add", post(add))
        .route("/start-headless-download", post(start_headless))
        .route("/downloads", get(list_downloads))
        .route("/queues", get(list_queues))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token));
    let app = Router::new()
        .route("/ping", post(ping))
        .merge(protected)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
    let address = listener.local_addr()?;
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            tracing::warn!(?error, "integration server stopped");
        }
    });
    Ok(address)
}

async fn ping() -> Response {
    StatusCode::OK.into_response()
}

async fn add(
    State(state): State<ServerState>,
    body: Result<axum::Json<AddDownloadRequest>, JsonRejection>,
) -> Result<Response, ApiFailure> {
    let axum::Json(request) = body.map_err(|_| ApiFailure::bad_request("bad_request"))?;
    let reply = state
        .handle
        .request(|reply| IntegrationCommand::ConfirmAdd {
            items: request.items,
            options: request.options,
            reply,
        })
        .await?;
    accepted(reply)
}

async fn start_headless(
    State(state): State<ServerState>,
    body: Result<axum::Json<AddDownloadRequest>, JsonRejection>,
) -> Result<Response, ApiFailure> {
    let axum::Json(request) = body.map_err(|_| ApiFailure::bad_request("bad_request"))?;
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
    let expected = state.token.get();
    if presented_token(request.headers())
        .is_some_and(|token| crate::integration::auth::verify(&expected, &token))
    {
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
            .post(format!("{base}/add"))
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

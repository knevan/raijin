//! Local integration API models shared with the browser extension.
//!
//! Field names and shapes must stay in sync with `browser-extension/utils/types.ts`.
//! Everything here is UI-free: the server only produces requests and responses,
//! the UI decides how (and whether) to surface them.

pub mod auth;
pub mod server;

use std::collections::BTreeMap;

pub use auth::{TokenCell, load_or_create as load_or_create_token, regenerate as regenerate_token};
use serde::{Deserialize, Serialize};
pub use server::{
    ApiFailure, DEFAULT_COMMAND_BUFFER, IntegrationCommand, IntegrationHandle, IntegrationReply,
    IntegrationResponder, PreparedItem, prepare_item, prepare_request, serve, summarize_item,
};

/// Default localhost port for the integration server.
pub const DEFAULT_PORT: u16 = 35343;

/// Settings-table key holding the persisted API token.
pub const API_TOKEN_KEY: &str = "integration.api_token";

/// Transport kind requested by the extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DownloadType {
    /// Plain HTTP(S) download.
    Http,
    /// HLS playlist (desktop support lands with the media pipeline).
    Hls,
}

/// One requested download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadRequestItem {
    /// Source URL.
    pub link: String,
    /// Page the download was discovered on.
    pub download_page: Option<String>,
    /// Captured request headers. Sent to localhost only.
    pub headers: Option<BTreeMap<String, String>>,
    /// Human-readable label from the page.
    pub description: Option<String>,
    /// Server- or page-suggested file name.
    pub suggested_name: Option<String>,
    /// Transport kind.
    pub r#type: DownloadType,
}

/// Per-request behavior flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub struct DownloadRequestOptions {
    /// Skip the confirmation dialog.
    pub silent_add: bool,
    /// Start downloading immediately without waiting for the user.
    pub silent_start: bool,
}

/// `POST /add` and `POST /start-headless-download` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddDownloadRequest {
    /// Requested items, in page order.
    pub items: Vec<DownloadRequestItem>,
    /// Behavior flags.
    #[serde(default)]
    pub options: DownloadRequestOptions,
}

/// Compact download summary for the extension popup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadSummary {
    /// Stable download id.
    pub id: i64,
    /// Source URL.
    pub url: String,
    /// File name shown in the UI.
    pub file_name: String,
    /// Durable status token (matches the persisted enum).
    pub status: String,
    /// Downloaded bytes.
    pub downloaded_bytes: u64,
    /// Total bytes when the server reported a length.
    pub total_bytes: Option<u64>,
}

/// `GET /queues` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QueueSummary {
    /// Stable queue id.
    pub id: i64,
    /// Display name.
    pub name: String,
    /// Configured concurrency limit.
    pub max_concurrent: u16,
    /// Number of items currently in the queue.
    pub item_count: usize,
}

/// Error body returned by the integration server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiErrorBody {
    /// Short machine-readable reason.
    pub error: String,
}

/// Extracts a usable file name for an extension-originated link.
///
/// Prefers the extension's suggestion (which already follows the browser's
/// `Content-Disposition` handling), then the last URL segment, then a fallback.
/// Query strings and fragments are stripped and percent-escapes decoded.
#[must_use]
pub fn file_name_for(item: &DownloadRequestItem) -> String {
    if let Some(name) = item.suggested_name.as_deref() {
        let cleaned = sanitize_file_name(name);
        if !cleaned.is_empty() {
            return cleaned;
        }
    }
    let from_url = file_name_from_url(&item.link);
    if !from_url.is_empty() {
        return from_url;
    }
    "download.bin".to_owned()
}

/// Derives a file name from the last path segment of an URL.
#[must_use]
pub fn file_name_from_url(url: &str) -> String {
    let without_scheme = url.split("://").nth(1).unwrap_or(url);
    let path = without_scheme.split(['?', '#']).next().unwrap_or_default();
    let segment = path.rsplit('/').next().unwrap_or_default().trim();
    if !path.contains('/') || segment.is_empty() || segment.contains(['\\', ':']) {
        return String::new();
    }
    sanitize_file_name(&crate::download::decode_percent_filename(segment))
}

/// Strips path separators and control characters from a suggested name.
///
/// Extension-supplied names are untrusted input; a name containing `/` or `..`
/// would otherwise escape the destination folder.
#[must_use]
pub fn sanitize_file_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw).trim();
    let cleaned: String = base.chars().filter(|c| !c.is_control()).take(128).collect();
    let cleaned = cleaned.trim();
    if cleaned == "." || cleaned == ".." {
        return String::new();
    }
    cleaned.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(link: &str, suggested: Option<&str>) -> DownloadRequestItem {
        DownloadRequestItem {
            link: link.to_owned(),
            download_page: None,
            headers: None,
            description: None,
            suggested_name: suggested.map(str::to_owned),
            r#type: DownloadType::Http,
        }
    }

    #[test]
    fn file_name_should_prefer_extension_suggestion() {
        let name = file_name_for(&item("https://cdn.example.com/a/b.zip", Some("custom.zip")));
        assert_eq!(name, "custom.zip");
    }

    #[test]
    fn file_name_should_fall_back_to_url_segment() {
        let name = file_name_for(&item("https://cdn.example.com/a/b.zip?v=2#frag", None));
        assert_eq!(name, "b.zip");
    }

    #[test]
    fn file_name_should_decode_percent_escapes() {
        let name = file_name_for(&item("https://cdn.example.com/My%20Video.mp4", None));
        assert_eq!(name, "My Video.mp4");
    }

    #[test]
    fn file_name_should_fallback_when_nothing_is_usable() {
        assert_eq!(
            file_name_for(&item("https://cdn.example.com/", None)),
            "download.bin"
        );
        assert_eq!(
            file_name_for(&item("https://cdn.example.com", None)),
            "download.bin"
        );
    }

    #[test]
    fn sanitize_should_strip_path_separators() {
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name("a\\b\\c.zip"), "c.zip");
        assert_eq!(sanitize_file_name(".."), "");
        assert_eq!(sanitize_file_name("  "), "");
    }

    #[test]
    fn request_should_deserialize_the_extension_payload() {
        let raw = r#"{
            "items": [{
                "link": "https://cdn.example.com/clip.mp4",
                "downloadPage": "https://example.com/watch",
                "headers": {"Cookie": "a=b"},
                "description": "Clip",
                "suggestedName": "clip.mp4",
                "type": "http"
            }],
            "options": {"silentAdd": true, "silentStart": false}
        }"#;
        let request: AddDownloadRequest =
            serde_json::from_str(raw).expect("extension payload must deserialize");
        assert_eq!(request.items.len(), 1);
        assert!(request.options.silent_add);
        assert!(!request.options.silent_start);
        let first = &request.items[0];
        assert_eq!(first.link, "https://cdn.example.com/clip.mp4");
        assert_eq!(first.r#type, DownloadType::Http);
        assert_eq!(
            first.headers.as_ref().and_then(|h| h.get("Cookie")),
            Some(&"a=b".to_owned())
        );
    }

    #[test]
    fn options_should_default_when_omitted() {
        let raw = r#"{"items": []}"#;
        let request: AddDownloadRequest = serde_json::from_str(raw).expect("options are optional");
        assert_eq!(request.options, DownloadRequestOptions::default());
    }
}

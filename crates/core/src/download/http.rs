use std::collections::BTreeMap;
use std::future::Future;
use std::str::FromStr;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::{StatusCode, header};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::download::{Bytes, ResumeSupport};

const PROBE_RANGE_START: u64 = 0;
const PROBE_RANGE_END: u64 = 255;
const MAX_PROBE_BODY_BYTES: usize = 4096;
const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
pub(crate) const ACCEPT_ENCODING_IDENTITY: &str = "identity";

/// Inclusive byte range used by HTTP requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteRange {
    /// Inclusive start byte.
    pub start: Bytes,
    /// Inclusive end byte.
    pub end: Bytes,
}

impl ByteRange {
    /// Creates an inclusive byte range.
    #[must_use]
    pub const fn new(start: Bytes, end: Bytes) -> Self {
        Self { start, end }
    }

    fn header_value(self) -> String {
        format!("bytes={}-{}", self.start.get(), self.end.get())
    }
}

/// Request sent through the HTTP client abstraction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpRequest {
    /// Request URL.
    pub url: String,
    /// Extra request headers.
    pub headers: BTreeMap<String, String>,
    /// Optional range request.
    pub range: Option<ByteRange>,
}

/// Response returned by the HTTP client abstraction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpResponse {
    /// Numeric HTTP status code.
    pub status: u16,
    /// Response headers with lowercase names.
    pub headers: BTreeMap<String, String>,
    /// Response body bytes captured for metadata checks.
    pub body: Vec<u8>,
}

impl HttpResponse {
    fn status_code(&self) -> Result<StatusCode, HttpProbeError> {
        StatusCode::from_u16(self.status)
            .map_err(|_| HttpProbeError::InvalidStatusCode(self.status))
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }
}

/// Abstraction over HTTP clients used by probes and future jobs.
pub trait HttpClient: Clone + Send + Sync + 'static {
    /// Executes one HTTP request.
    fn execute(
        &self,
        request: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, HttpProbeError>> + Send + '_;
}

/// Reqwest-backed HTTP client implementation.
#[derive(Clone)]
pub struct ReqwestHttpClient {
    client: reqwest::Client,
}

impl ReqwestHttpClient {
    /// Builds a default reqwest HTTP client.
    ///
    /// # Errors
    ///
    /// Returns an error if reqwest client construction fails.
    pub fn new() -> Result<Self, HttpProbeError> {
        let client = reqwest::Client::builder()
            .user_agent(DEFAULT_USER_AGENT)
            .default_headers(default_http_headers())
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()?;
        Ok(Self { client })
    }

    /// Wraps an existing reqwest client.
    #[must_use]
    pub fn from_client(client: reqwest::Client) -> Self {
        Self { client }
    }

    pub(crate) fn client(&self) -> &reqwest::Client {
        &self.client
    }
}

fn default_http_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static(ACCEPT_ENCODING_IDENTITY),
    );
    headers
}

impl std::fmt::Debug for ReqwestHttpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReqwestHttpClient").finish_non_exhaustive()
    }
}

impl HttpClient for ReqwestHttpClient {
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, HttpProbeError> {
        let mut builder = self
            .client
            .get(&request.url)
            .headers(request_headers(&request)?);
        if let Some(range) = request.range {
            builder = builder.header(header::RANGE, range.header_value());
        }

        let mut response = builder.send().await?;
        let status = response.status().as_u16();
        let headers = response_headers(response.headers());
        let body = read_probe_body(&mut response).await?;

        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }
}

async fn read_probe_body(response: &mut reqwest::Response) -> Result<Vec<u8>, HttpProbeError> {
    let mut body = Vec::new();
    while body.len() < MAX_PROBE_BODY_BYTES {
        let Some(chunk) = response.chunk().await? else {
            break;
        };
        let remaining = MAX_PROBE_BODY_BYTES - body.len();
        let writable = chunk.len().min(remaining);
        body.extend_from_slice(&chunk[..writable]);
        if writable < chunk.len() {
            break;
        }
    }
    Ok(body)
}

/// Request for metadata probing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeRequest {
    /// Request URL.
    pub url: String,
    /// Extra request headers.
    pub headers: BTreeMap<String, String>,
    /// Expected target file name, used to detect accidental HTML pages.
    pub file_name: Option<String>,
}

impl ProbeRequest {
    /// Creates a probe request with no extra headers.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            headers: BTreeMap::new(),
            file_name: None,
        }
    }
}

/// Expected persisted metadata used for resume validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeValidation {
    /// Previously persisted total size.
    pub total_bytes: Option<Bytes>,
    /// Previously persisted ETag.
    pub etag: Option<String>,
    /// Previously known resume support.
    pub resume_support: ResumeSupport,
}

/// Parsed HTTP Content-Range header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentRange {
    /// Inclusive start byte.
    pub start: Bytes,
    /// Inclusive end byte.
    pub end: Bytes,
    /// Total resource size, if provided.
    pub total: Option<Bytes>,
}

/// Remote HTTP metadata collected by probe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpMetadata {
    /// Final probed URL.
    pub url: String,
    /// Resume capability detected by probe.
    pub resume_support: ResumeSupport,
    /// Total resource size, if known.
    pub total_bytes: Option<Bytes>,
    /// Content length of the response that supplied metadata.
    pub response_content_length: Option<Bytes>,
    /// Parsed Content-Range value for ranged responses.
    pub content_range: Option<ContentRange>,
    /// Entity tag header.
    pub etag: Option<String>,
    /// Last-Modified header.
    pub last_modified: Option<String>,
    /// Content-Type header.
    pub content_type: Option<String>,
    /// HTTP status code used for metadata.
    pub status: u16,
}

/// HTTP probe and validation errors.
#[derive(Debug, Error)]
pub enum HttpProbeError {
    /// Reqwest failed to build or execute a request.
    #[error(transparent)]
    Request(#[from] reqwest::Error),
    /// Header name was invalid.
    #[error("invalid HTTP header name `{0}`")]
    InvalidHeaderName(String),
    /// Header value was invalid.
    #[error("invalid HTTP header value for `{0}`")]
    InvalidHeaderValue(String),
    /// HTTP status code was not valid.
    #[error("invalid HTTP status code `{0}`")]
    InvalidStatusCode(u16),
    /// Server returned a status that cannot be used for download metadata.
    #[error("unexpected HTTP status `{0}`")]
    UnexpectedStatus(u16),
    /// Content-Length header could not be parsed.
    #[error("invalid Content-Length `{0}`")]
    InvalidContentLength(String),
    /// Content-Range header could not be parsed.
    #[error("invalid Content-Range `{0}`")]
    InvalidContentRange(String),
    /// Content-Range header was required but missing.
    #[error("missing Content-Range header for ranged response")]
    MissingContentRange,
    /// Content-Range did not match the requested probe range.
    #[error("Content-Range `{actual}` does not match requested range `{expected}`")]
    ContentRangeMismatch { expected: String, actual: String },
    /// Persisted content length no longer matches remote metadata.
    #[error("remote content length changed from `{expected}` to `{actual}`")]
    ContentLengthChanged { expected: Bytes, actual: Bytes },
    /// Persisted ETag no longer matches remote metadata.
    #[error("remote ETag changed from `{expected}` to `{actual}`")]
    EtagChanged { expected: String, actual: String },
    /// Server previously supported ranges but no longer does.
    #[error("remote server no longer supports byte ranges")]
    RangeSupportDisappeared,
    /// Response appears to be an HTML page instead of the expected file.
    #[error("response appears to be an HTML page, not requested file `{file_name}`")]
    WebpageMismatch { file_name: String },
}

/// Probes a URL for HTTP metadata.
///
/// # Errors
///
/// Returns an error when the server response is invalid or metadata validation fails.
pub async fn probe_http<C>(
    client: &C,
    request: ProbeRequest,
) -> Result<HttpMetadata, HttpProbeError>
where
    C: HttpClient,
{
    let range = ByteRange::new(Bytes::new(PROBE_RANGE_START), Bytes::new(PROBE_RANGE_END));
    let range_response = client
        .execute(HttpRequest {
            url: request.url.clone(),
            headers: request.headers.clone(),
            range: Some(range),
        })
        .await?;

    let status = range_response.status_code()?;
    let metadata = if status == StatusCode::PARTIAL_CONTENT {
        metadata_from_range_response(&request.url, range, range_response)?
    } else {
        let fallback_response = if should_fallback(status) {
            client
                .execute(HttpRequest {
                    url: request.url.clone(),
                    headers: request.headers.clone(),
                    range: None,
                })
                .await?
        } else {
            range_response
        };
        metadata_from_full_response(&request.url, fallback_response)?
    };

    validate_webpage_mismatch(&metadata, request.file_name.as_deref())?;
    Ok(metadata)
}

/// Validates remote metadata against previously persisted resume state.
///
/// # Errors
///
/// Returns validation errors when persisted state no longer matches remote metadata.
pub fn validate_resume_metadata(
    previous: &ResumeValidation,
    metadata: &HttpMetadata,
) -> Result<(), HttpProbeError> {
    if let (Some(expected), Some(actual)) = (previous.total_bytes, metadata.total_bytes)
        && expected != actual
    {
        return Err(HttpProbeError::ContentLengthChanged { expected, actual });
    }

    if let (Some(expected), Some(actual)) = (previous.etag.as_deref(), metadata.etag.as_deref())
        && expected != actual
    {
        return Err(HttpProbeError::EtagChanged {
            expected: expected.to_owned(),
            actual: actual.to_owned(),
        });
    }

    if previous.resume_support == ResumeSupport::Supported
        && metadata.resume_support != ResumeSupport::Supported
    {
        return Err(HttpProbeError::RangeSupportDisappeared);
    }

    Ok(())
}

fn metadata_from_range_response(
    url: &str,
    requested_range: ByteRange,
    response: HttpResponse,
) -> Result<HttpMetadata, HttpProbeError> {
    let content_range_header = response
        .header("content-range")
        .ok_or(HttpProbeError::MissingContentRange)?;
    let content_range = parse_content_range(content_range_header)?;
    validate_content_range(requested_range, content_range, content_range_header)?;
    let response_content_length = parse_optional_content_length(response.header("content-length"))?;

    Ok(HttpMetadata {
        url: url.to_owned(),
        resume_support: ResumeSupport::Supported,
        total_bytes: content_range.total,
        response_content_length,
        content_range: Some(content_range),
        etag: response.header("etag").map(ToOwned::to_owned),
        last_modified: response.header("last-modified").map(ToOwned::to_owned),
        content_type: response.header("content-type").map(ToOwned::to_owned),
        status: response.status,
    })
}

fn metadata_from_full_response(
    url: &str,
    response: HttpResponse,
) -> Result<HttpMetadata, HttpProbeError> {
    let status = response.status_code()?;
    if !status.is_success() {
        return Err(HttpProbeError::UnexpectedStatus(response.status));
    }
    let content_length = parse_optional_content_length(response.header("content-length"))?;

    Ok(HttpMetadata {
        url: url.to_owned(),
        resume_support: ResumeSupport::Unsupported,
        total_bytes: content_length,
        response_content_length: content_length,
        content_range: None,
        etag: response.header("etag").map(ToOwned::to_owned),
        last_modified: response.header("last-modified").map(ToOwned::to_owned),
        content_type: response.header("content-type").map(ToOwned::to_owned),
        status: response.status,
    })
}

fn should_fallback(status: StatusCode) -> bool {
    status == StatusCode::OK
        || status == StatusCode::RANGE_NOT_SATISFIABLE
        || status == StatusCode::BAD_REQUEST
        || status == StatusCode::METHOD_NOT_ALLOWED
        || status == StatusCode::NOT_IMPLEMENTED
}

fn validate_content_range(
    requested_range: ByteRange,
    content_range: ContentRange,
    actual_header: &str,
) -> Result<(), HttpProbeError> {
    if content_range.start != requested_range.start || content_range.end < content_range.start {
        return Err(HttpProbeError::ContentRangeMismatch {
            expected: requested_range.header_value(),
            actual: actual_header.to_owned(),
        });
    }
    Ok(())
}

fn validate_webpage_mismatch(
    metadata: &HttpMetadata,
    file_name: Option<&str>,
) -> Result<(), HttpProbeError> {
    let Some(file_name) = file_name else {
        return Ok(());
    };
    if is_html_file_name(file_name) {
        return Ok(());
    }
    if metadata
        .content_type
        .as_deref()
        .is_some_and(is_html_content_type)
    {
        return Err(HttpProbeError::WebpageMismatch {
            file_name: file_name.to_owned(),
        });
    }
    Ok(())
}

fn parse_optional_content_length(value: Option<&str>) -> Result<Option<Bytes>, HttpProbeError> {
    value
        .map(|value| {
            value
                .trim()
                .parse::<u64>()
                .map(Bytes::new)
                .map_err(|_| HttpProbeError::InvalidContentLength(value.to_owned()))
        })
        .transpose()
}

pub(crate) fn parse_content_range(value: &str) -> Result<ContentRange, HttpProbeError> {
    let value = value.trim();
    let Some(range) = value.strip_prefix("bytes ") else {
        return Err(HttpProbeError::InvalidContentRange(value.to_owned()));
    };
    let Some((byte_range, total)) = range.split_once('/') else {
        return Err(HttpProbeError::InvalidContentRange(value.to_owned()));
    };
    let Some((start, end)) = byte_range.split_once('-') else {
        return Err(HttpProbeError::InvalidContentRange(value.to_owned()));
    };
    let start = start
        .parse::<u64>()
        .map_err(|_| HttpProbeError::InvalidContentRange(value.to_owned()))?;
    let end = end
        .parse::<u64>()
        .map_err(|_| HttpProbeError::InvalidContentRange(value.to_owned()))?;
    if end < start {
        return Err(HttpProbeError::InvalidContentRange(value.to_owned()));
    }
    let total = if total == "*" {
        None
    } else {
        Some(
            total
                .parse::<u64>()
                .map(Bytes::new)
                .map_err(|_| HttpProbeError::InvalidContentRange(value.to_owned()))?,
        )
    };

    Ok(ContentRange {
        start: Bytes::new(start),
        end: Bytes::new(end),
        total,
    })
}

fn request_headers(request: &HttpRequest) -> Result<HeaderMap, HttpProbeError> {
    let mut headers = HeaderMap::with_capacity(request.headers.len());
    for (name, value) in &request.headers {
        if name.eq_ignore_ascii_case(header::HOST.as_str()) {
            continue;
        }
        let header_name = HeaderName::from_str(name)
            .map_err(|_| HttpProbeError::InvalidHeaderName(name.clone()))?;
        let header_value = HeaderValue::from_str(value)
            .map_err(|_| HttpProbeError::InvalidHeaderValue(name.clone()))?;
        headers.insert(header_name, header_value);
    }
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static(ACCEPT_ENCODING_IDENTITY),
    );
    Ok(headers)
}

fn response_headers(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    for (name, value) in headers {
        if let Ok(value) = value.to_str() {
            result.insert(name.as_str().to_ascii_lowercase(), value.to_owned());
        }
    }
    result
}

fn is_html_content_type(content_type: &str) -> bool {
    content_type
        .split(';')
        .next()
        .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/html"))
}

fn is_html_file_name(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    lower.ends_with(".html") || lower.ends_with(".htm")
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::sync::Arc;

    use reqwest::header::{CONTENT_LENGTH, ETAG, LAST_MODIFIED};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::task::JoinHandle;

    use super::*;

    #[derive(Debug, Clone)]
    enum ServerMode {
        RangeSupported,
        RangeUnsupported,
        RangeUnsupportedLargeBody,
        ChangedLength,
        ChangedEtag,
        HtmlPage,
    }

    #[derive(Debug)]
    struct TestServer {
        addr: SocketAddr,
        task: JoinHandle<()>,
    }

    impl TestServer {
        async fn spawn(mode: ServerMode) -> std::io::Result<Self> {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let addr = listener.local_addr()?;
            let mode = Arc::new(mode);
            let task = tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let mode = Arc::clone(&mode);
                    tokio::spawn(async move {
                        let _ = handle_connection(stream, &mode).await;
                    });
                }
            });

            Ok(Self { addr, task })
        }

        fn url(&self) -> String {
            format!("http://{}/file.bin", self.addr)
        }
    }

    impl Drop for TestServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn handle_connection(mut stream: TcpStream, mode: &ServerMode) -> std::io::Result<()> {
        let mut request = vec![0_u8; 4096];
        let read = stream.read(&mut request).await?;
        let request = String::from_utf8_lossy(&request[..read]);
        let has_range = request
            .lines()
            .any(|line| line.eq_ignore_ascii_case("range: bytes=0-255"));

        let response = response_for(mode, has_range);
        stream.write_all(response.as_bytes()).await?;
        stream.shutdown().await
    }

    fn response_for(mode: &ServerMode, has_range: bool) -> String {
        match mode {
            ServerMode::RangeSupported if has_range => response(
                206,
                "Partial Content",
                &[
                    (CONTENT_LENGTH.as_str(), "256"),
                    ("Content-Range", "bytes 0-255/1024"),
                    (ETAG.as_str(), "\"etag-1\""),
                    (LAST_MODIFIED.as_str(), "Sat, 08 Aug 2026 00:00:00 GMT"),
                    ("Content-Type", "application/octet-stream"),
                ],
                &vec![b'a'; 256],
            ),
            ServerMode::RangeSupported => response(
                200,
                "OK",
                &[
                    (CONTENT_LENGTH.as_str(), "1024"),
                    (ETAG.as_str(), "\"etag-1\""),
                    ("Content-Type", "application/octet-stream"),
                ],
                &vec![b'a'; 1024],
            ),
            ServerMode::RangeUnsupported => response(
                200,
                "OK",
                &[
                    (CONTENT_LENGTH.as_str(), "1024"),
                    (ETAG.as_str(), "\"etag-1\""),
                    ("Content-Type", "application/octet-stream"),
                ],
                &vec![b'a'; 1024],
            ),
            ServerMode::RangeUnsupportedLargeBody => response(
                200,
                "OK",
                &[
                    (CONTENT_LENGTH.as_str(), "8192"),
                    (ETAG.as_str(), "\"etag-1\""),
                    ("Content-Type", "application/octet-stream"),
                ],
                &vec![b'a'; 8192],
            ),
            ServerMode::ChangedLength if has_range => response(
                206,
                "Partial Content",
                &[
                    (CONTENT_LENGTH.as_str(), "256"),
                    ("Content-Range", "bytes 0-255/2048"),
                    (ETAG.as_str(), "\"etag-1\""),
                    ("Content-Type", "application/octet-stream"),
                ],
                &vec![b'a'; 256],
            ),
            ServerMode::ChangedLength => response(
                200,
                "OK",
                &[(CONTENT_LENGTH.as_str(), "2048")],
                &vec![b'a'; 2048],
            ),
            ServerMode::ChangedEtag if has_range => response(
                206,
                "Partial Content",
                &[
                    (CONTENT_LENGTH.as_str(), "256"),
                    ("Content-Range", "bytes 0-255/1024"),
                    (ETAG.as_str(), "\"etag-2\""),
                    ("Content-Type", "application/octet-stream"),
                ],
                &vec![b'a'; 256],
            ),
            ServerMode::ChangedEtag => response(
                200,
                "OK",
                &[
                    (CONTENT_LENGTH.as_str(), "1024"),
                    (ETAG.as_str(), "\"etag-2\""),
                ],
                &vec![b'a'; 1024],
            ),
            ServerMode::HtmlPage => response(
                200,
                "OK",
                &[
                    (CONTENT_LENGTH.as_str(), "28"),
                    ("Content-Type", "text/html; charset=utf-8"),
                ],
                b"<!doctype html><html></html>",
            ),
        }
    }

    fn response(status: u16, reason: &str, headers: &[(&str, &str)], body: &[u8]) -> String {
        let mut response = format!(
            "HTTP/1.1 {status} {reason}\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        for (name, value) in headers {
            if !name.eq_ignore_ascii_case(CONTENT_LENGTH.as_str()) {
                response.push_str(name);
                response.push_str(": ");
                response.push_str(value);
                response.push_str("\r\n");
            }
        }
        response.push_str("\r\n");
        response.push_str(&String::from_utf8_lossy(body));
        response
    }

    fn reqwest_client() -> Result<ReqwestHttpClient, HttpProbeError> {
        ReqwestHttpClient::new()
    }

    #[tokio::test]
    async fn probe_should_detect_range_supported_metadata() -> Result<(), Box<dyn std::error::Error>>
    {
        let server = TestServer::spawn(ServerMode::RangeSupported).await?;
        let metadata = probe_http(&reqwest_client()?, ProbeRequest::new(server.url())).await?;

        assert_eq!(metadata.resume_support, ResumeSupport::Supported);
        assert_eq!(metadata.total_bytes, Some(Bytes::new(1024)));
        assert_eq!(metadata.response_content_length, Some(Bytes::new(256)));
        assert_eq!(metadata.etag.as_deref(), Some("\"etag-1\""));
        assert_eq!(
            metadata.content_range.map(|range| range.end),
            Some(Bytes::new(255))
        );
        Ok(())
    }

    #[tokio::test]
    async fn probe_should_fallback_when_range_unsupported() -> Result<(), Box<dyn std::error::Error>>
    {
        let server = TestServer::spawn(ServerMode::RangeUnsupported).await?;
        let metadata = probe_http(&reqwest_client()?, ProbeRequest::new(server.url())).await?;

        assert_eq!(metadata.resume_support, ResumeSupport::Unsupported);
        assert_eq!(metadata.total_bytes, Some(Bytes::new(1024)));
        assert_eq!(metadata.content_range, None);
        Ok(())
    }

    #[tokio::test]
    async fn reqwest_probe_client_should_cap_captured_body()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = TestServer::spawn(ServerMode::RangeUnsupportedLargeBody).await?;
        let client = reqwest_client()?;
        let response = client
            .execute(HttpRequest {
                url: server.url(),
                headers: BTreeMap::new(),
                range: Some(ByteRange::new(Bytes::ZERO, Bytes::new(255))),
            })
            .await?;

        assert_eq!(response.status, 200);
        assert_eq!(response.body.len(), MAX_PROBE_BODY_BYTES);
        Ok(())
    }

    #[tokio::test]
    async fn validation_should_reject_changed_length() -> Result<(), Box<dyn std::error::Error>> {
        let server = TestServer::spawn(ServerMode::ChangedLength).await?;
        let metadata = probe_http(&reqwest_client()?, ProbeRequest::new(server.url())).await?;
        let error = validate_resume_metadata(
            &ResumeValidation {
                total_bytes: Some(Bytes::new(1024)),
                etag: Some("\"etag-1\"".to_owned()),
                resume_support: ResumeSupport::Supported,
            },
            &metadata,
        );

        assert!(matches!(
            error,
            Err(HttpProbeError::ContentLengthChanged { .. })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn validation_should_reject_changed_etag() -> Result<(), Box<dyn std::error::Error>> {
        let server = TestServer::spawn(ServerMode::ChangedEtag).await?;
        let metadata = probe_http(&reqwest_client()?, ProbeRequest::new(server.url())).await?;
        let error = validate_resume_metadata(
            &ResumeValidation {
                total_bytes: Some(Bytes::new(1024)),
                etag: Some("\"etag-1\"".to_owned()),
                resume_support: ResumeSupport::Supported,
            },
            &metadata,
        );

        assert!(matches!(error, Err(HttpProbeError::EtagChanged { .. })));
        Ok(())
    }

    #[tokio::test]
    async fn validation_should_reject_disappeared_range_support()
    -> Result<(), Box<dyn std::error::Error>> {
        let server = TestServer::spawn(ServerMode::RangeUnsupported).await?;
        let metadata = probe_http(&reqwest_client()?, ProbeRequest::new(server.url())).await?;
        let error = validate_resume_metadata(
            &ResumeValidation {
                total_bytes: Some(Bytes::new(1024)),
                etag: Some("\"etag-1\"".to_owned()),
                resume_support: ResumeSupport::Supported,
            },
            &metadata,
        );

        assert!(matches!(
            error,
            Err(HttpProbeError::RangeSupportDisappeared)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn probe_should_reject_html_mismatch() -> Result<(), Box<dyn std::error::Error>> {
        let server = TestServer::spawn(ServerMode::HtmlPage).await?;
        let mut request = ProbeRequest::new(server.url());
        request.file_name = Some("archive.zip".to_owned());
        let error = probe_http(&reqwest_client()?, request).await;

        assert!(matches!(error, Err(HttpProbeError::WebpageMismatch { .. })));
        Ok(())
    }

    #[test]
    fn content_range_parser_should_parse_valid_range() -> Result<(), HttpProbeError> {
        let range = parse_content_range("bytes 10-19/100")?;

        assert_eq!(range.start, Bytes::new(10));
        assert_eq!(range.end, Bytes::new(19));
        assert_eq!(range.total, Some(Bytes::new(100)));
        Ok(())
    }
}

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
    /// Final URL after redirects.
    pub url: String,
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
        let url = response.url().to_string();
        let headers = response_headers(response.headers());
        let body = read_probe_body(&mut response).await?;

        Ok(HttpResponse {
            status,
            url,
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
    /// Server-suggested file name from `Content-Disposition`, if present.
    #[serde(default)]
    pub file_name: Option<String>,
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
        let url = range_response.url.clone();
        metadata_from_range_response(&url, range, range_response)?
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
        let url = fallback_response.url.clone();
        metadata_from_full_response(&url, fallback_response)?
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
        file_name: response
            .header("content-disposition")
            .and_then(extract_filename_from_content_disposition),
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
        file_name: response
            .header("content-disposition")
            .and_then(extract_filename_from_content_disposition),
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

/// Decodes `%XX` runs as UTF-8 without touching `+`.
///
/// Mirrors ABDM `FilenameDecoder`: consecutive escapes form one UTF-8 run,
/// stray/invalid escapes stay literal so no name is lost.
#[must_use]
pub fn decode_percent_filename(segment: &str) -> String {
    if !segment.contains('%') {
        return segment.to_owned();
    }
    let bytes = segment.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let is_escape =
            bytes[i] == b'%' && is_hex(bytes.get(i + 1)) && is_hex(bytes.get(i + 2));
        if !is_escape {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let raw_start = i;
        let mut pending: Vec<u8> = Vec::new();
        while i < bytes.len()
            && bytes[i] == b'%'
            && is_hex(bytes.get(i + 1))
            && is_hex(bytes.get(i + 2))
        {
            pending.push((hex_val(bytes[i + 1]) << 4) | hex_val(bytes[i + 2]));
            i += 3;
        }
        match String::from_utf8(pending) {
            Ok(text) => out.extend_from_slice(text.as_bytes()),
            Err(_) => out.extend_from_slice(&bytes[raw_start..i]),
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| segment.to_owned())
}

/// Extracts a server-suggested file name from a `Content-Disposition` value.
///
/// Priority mirrors ABDM `fileNameExtractor`: extended `filename*=UTF-8''...`
/// first, then plain `filename="..."` (with RFC 2047 MIME-word and percent
/// decoding). Returns `None` when nothing usable remains; callers fall back
/// to the URL-derived name.
#[must_use]
pub fn extract_filename_from_content_disposition(value: &str) -> Option<String> {
    extended_filename(value)
        .or_else(|| plain_filename(value))
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
}

/// Finds an extended `filename*=charset'lang'percent-encoded` parameter.
fn extended_filename(value: &str) -> Option<String> {
    for param in split_header_params(value) {
        // Non-parameter segments (e.g. leading `attachment`) are skipped.
        let Some((name, param_value)) = split_param(&param) else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("filename*") {
            continue;
        }
        let encoded = param_value.trim().trim_matches('"').trim();
        // RFC 5987 shape is `charset'lang'value`; accept a bare value too.
        let encoded = encoded.rsplit("''").next().unwrap_or(encoded);
        let decoded = decode_percent_filename(encoded);
        let decoded = decoded.trim();
        if !decoded.is_empty() {
            return Some(decoded.to_owned());
        }
    }
    None
}

/// Finds a plain `filename="..."` parameter with MIME-word support.
fn plain_filename(value: &str) -> Option<String> {
    for param in split_header_params(value) {
        // Non-parameter segments (e.g. leading `attachment`) are skipped.
        let Some((name, param_value)) = split_param(&param) else {
            continue;
        };
        if !name.trim().eq_ignore_ascii_case("filename") {
            continue;
        }
        let mut filename = param_value.trim().to_owned();
        if filename.len() >= 2
            && filename.starts_with('"')
            && filename.ends_with('"')
        {
            filename = filename[1..filename.len() - 1].to_owned();
        }
        let filename = decode_mime_words(&filename);
        let filename = decode_percent_filename(&filename);
        let filename = filename.trim();
        if !filename.is_empty() {
            return Some(filename.to_owned());
        }
    }
    None
}

/// Splits a header value on `;` outside quoted strings.
fn split_header_params(value: &str) -> Vec<String> {
    let mut params = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    for ch in value.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                current.push(ch);
            }
            ';' if !in_quotes => {
                params.push(current);
                current = String::new();
            }
            _ => current.push(ch),
        }
    }
    params.push(current);
    params
}

/// Splits one `name=value` parameter on its first `=`.
fn split_param(param: &str) -> Option<(String, String)> {
    let (name, param_value) = param.split_once('=')?;
    Some((name.to_owned(), param_value.to_owned()))
}

/// Decodes RFC 2047 `=?charset?B|Q?text?=` words, leaving unknown ones raw.
fn decode_mime_words(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(token) = mime_word_at(&bytes[i..]) {
            out.push_str(&token.text);
            i += token.len;
        } else if bytes[i] < 0x80 {
            out.push(bytes[i] as char);
            i += 1;
        } else {
            // Copy one full UTF-8 char so multibyte text survives intact.
            let ch = value[i..].chars().next().unwrap_or('\u{FFFD}');
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

struct MimeWord {
    text: String,
    len: usize,
}

/// Scans one encoded word at the slice start; `None` means literal text.
fn mime_word_at(input: &[u8]) -> Option<MimeWord> {
    if input.len() < 8 || &input[..2] != b"=?" {
        return None;
    }
    let rest = &input[2..];
    let charset_end = rest.iter().position(|&byte| byte == b'?')?;
    let charset = std::str::from_utf8(&rest[..charset_end]).ok()?;
    let rest = &rest[charset_end + 1..];
    if rest.is_empty() {
        return None;
    }
    let encoding = rest[0];
    if rest.len() < 3 || rest[1] != b'?' {
        return None;
    }
    let rest = &rest[2..];
    let end = find_mime_end(rest)?;
    let encoded = std::str::from_utf8(&rest[..end]).ok()?;
    let decoded = match encoding {
        b'B' | b'b' => decode_mime_base64(encoded, charset)?,
        b'Q' | b'q' => decode_mime_quoted(encoded, charset)?,
        _ => return None,
    };
    Some(MimeWord {
        len: input.len() - rest.len() + end + 2,
        text: decoded,
    })
}

/// Finds the closing `?=` of an encoded word.
fn find_mime_end(input: &[u8]) -> Option<usize> {
    input
        .windows(2)
        .position(|window| window == b"?=")
}

/// Decodes `B` (base64) MIME-word bytes into text.
fn decode_mime_base64(encoded: &str, charset: &str) -> Option<String> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let bytes = STANDARD.decode(encoded.trim()).ok()?;
    bytes_to_text(&bytes, charset)
}

/// Decodes `Q` (quoted-printable) MIME-word bytes into text.
fn decode_mime_quoted(encoded: &str, charset: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut raw: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // `_` represents a space in Q encoding.
            b'_' => {
                raw.push(b' ');
                i += 1;
            }
            b'=' if is_hex(bytes.get(i + 1)) && is_hex(bytes.get(i + 2)) => {
                raw.push((hex_val(bytes[i + 1]) << 4) | hex_val(bytes[i + 2]));
                i += 3;
            }
            byte => {
                raw.push(byte);
                i += 1;
            }
        }
    }
    bytes_to_text(&raw, charset)
}

/// Converts decoded bytes using the declared charset.
///
/// Resolves the label per the WHATWG Encoding Standard (covers Shift_JIS,
/// GBK, windows-1252, and friends). Malformed input yields `None` so the
/// caller keeps the raw token instead of a replacement-damaged name. Note
/// the standard maps `iso-8859-1` onto `windows-1252` by design.
fn bytes_to_text(bytes: &[u8], charset: &str) -> Option<String> {
    let encoding = encoding_rs::Encoding::for_label(charset.as_bytes())?;
    let (text, _, had_errors) = encoding.decode(bytes);
    (!had_errors).then(|| text.into_owned())
}

fn is_hex(value: Option<&u8>) -> bool {
    matches!(value, Some(b'0'..=b'9' | b'a'..=b'f' | b'A'..=b'F'))
}

fn hex_val(value: u8) -> u8 {
    match value {
        b'0'..=b'9' => value - b'0',
        b'a'..=b'f' => value - b'a' + 10,
        b'A'..=b'F' => value - b'A' + 10,
        _ => 0,
    }
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
        RangeSupportedWithDisposition,
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
            ServerMode::RangeSupportedWithDisposition if has_range => response(
                206,
                "Partial Content",
                &[
                    (CONTENT_LENGTH.as_str(), "256"),
                    ("Content-Range", "bytes 0-255/1024"),
                    (ETAG.as_str(), "\"etag-1\""),
                    ("Content-Type", "application/octet-stream"),
                    (
                        "Content-Disposition",
                        "attachment; filename*=UTF-8''My%20App.exe",
                    ),
                ],
                &vec![b'a'; 256],
            ),
            ServerMode::RangeSupportedWithDisposition => response(
                200,
                "OK",
                &[
                    (CONTENT_LENGTH.as_str(), "1024"),
                    (ETAG.as_str(), "\"etag-1\""),
                    ("Content-Type", "application/octet-stream"),
                    (
                        "Content-Disposition",
                        "attachment; filename*=UTF-8''My%20App.exe",
                    ),
                ],
                &vec![b'a'; 1024],
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

    #[test]
    fn disposition_should_prefer_extended_over_plain_filename() {
        assert_eq!(
            extract_filename_from_content_disposition(
                "attachment; filename=\"fallback.exe\"; filename*=UTF-8''My%20App.exe"
            ),
            Some("My App.exe".to_owned())
        );
        assert_eq!(
            extract_filename_from_content_disposition("attachment; filename=\"My App.exe\""),
            Some("My App.exe".to_owned())
        );
        assert_eq!(
            extract_filename_from_content_disposition("attachment; filename=setup.exe"),
            Some("setup.exe".to_owned())
        );
    }

    #[test]
    fn disposition_should_decode_mime_words_and_keep_plus_literal() {
        assert_eq!(
            extract_filename_from_content_disposition(
                "attachment; filename=\"=?UTF-8?Q?My_App=2Eexe?=\""
            ),
            Some("My App.exe".to_owned())
        );
        assert_eq!(
            extract_filename_from_content_disposition("attachment; filename=\"a%2Bb.exe\""),
            Some("a+b.exe".to_owned())
        );
    }

    #[test]
    fn disposition_should_decode_legacy_charsets_via_whatwg() {
        // Shift_JIS `あ` (0x82 0xA0) carried as base64 `gqA=`.
        assert_eq!(
            extract_filename_from_content_disposition(
                "attachment; filename=\"=?Shift_JIS?B?gqA=?=.txt\""
            ),
            Some("あ.txt".to_owned())
        );
        // WHATWG maps ISO-8859-1 onto windows-1252, so 0x80 is `€`.
        assert_eq!(
            extract_filename_from_content_disposition(
                "attachment; filename=\"=?ISO-8859-1?Q?=80?=\""
            ),
            Some("€".to_owned())
        );
    }

    #[test]
    fn disposition_should_keep_raw_token_for_unknown_or_broken_charsets() {
        assert_eq!(
            extract_filename_from_content_disposition(
                "attachment; filename=\"=?X-UNKNOWN?Q?hello?=\""
            ),
            Some("=?X-UNKNOWN?Q?hello?=".to_owned())
        );
        // 0xFF is malformed UTF-8: no replacement-damaged name.
        assert_eq!(
            extract_filename_from_content_disposition(
                "attachment; filename=\"=?UTF-8?Q?=FF?=\""
            ),
            Some("=?UTF-8?Q?=FF?=".to_owned())
        );
    }

    #[test]
    fn disposition_should_reject_unusable_values() {
        assert_eq!(extract_filename_from_content_disposition("attachment"), None);
        assert_eq!(
            extract_filename_from_content_disposition("attachment; filename=\"\""),
            None
        );
        assert_eq!(
            extract_filename_from_content_disposition("attachment; filename=\"   \""),
            None
        );
    }

    #[test]
    fn percent_decoder_should_keep_stray_escapes_literal() {
        assert_eq!(
            decode_percent_filename("Antigravity%20IDE.exe"),
            "Antigravity IDE.exe"
        );
        assert_eq!(decode_percent_filename("100%.exe"), "100%.exe");
        assert_eq!(decode_percent_filename("a+b.zip"), "a+b.zip");
    }

    #[tokio::test]
    async fn probe_should_capture_disposition_filename_and_final_url(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let server = TestServer::spawn(ServerMode::RangeSupportedWithDisposition).await?;
        let metadata = probe_http(&reqwest_client()?, ProbeRequest::new(server.url())).await?;

        assert_eq!(metadata.file_name, Some("My App.exe".to_owned()));
        assert_eq!(metadata.url, server.url());
        Ok(())
    }

    #[tokio::test]
    async fn probe_should_leave_filename_empty_without_disposition(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let server = TestServer::spawn(ServerMode::RangeSupported).await?;
        let metadata = probe_http(&reqwest_client()?, ProbeRequest::new(server.url())).await?;

        assert_eq!(metadata.file_name, None);
        assert_eq!(metadata.url, server.url());
        Ok(())
    }
}

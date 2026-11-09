use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Client, RequestBuilder, Response, StatusCode, Url, redirect};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::db::{DbError, DownloadRepository, HlsPlanRecord, HlsRepository, HlsSegmentRecord};
use crate::download::job::LiveDownloadProgress;
use crate::download::{
    Bytes, DownloadConfig, DownloadFailure, DownloadItem, DownloadStatus, FailureKind, SpeedLimiter,
};

mod playlist;

#[cfg(test)]
#[path = "hls/tests.rs"]
mod tests;

use playlist::{MediaPlan, Playlist, PlaylistError, parse_playlist};

const TS_PACKET_SIZE: u64 = 188;
const TS_SYNC_BYTE: u8 = 0x47;
const MAX_PLAYLIST_BYTES: usize = 2 * 1024 * 1024;
const MAX_PLAYLIST_DEPTH: u32 = 4;
const MAX_REDIRECTS: usize = 10;
const DEFAULT_HLS_CONNECTIONS: u16 = 4;
const MAX_VARIANT_FALLBACKS: u32 = 8;
const PLAYLIST_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_PROGRESS_INTERVAL: Duration = Duration::from_millis(500);
const RETRY_BASE_DELAY: Duration = Duration::from_millis(250);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(10);
const IO_CHUNK: usize = 64 * 1024;
const SEGMENT_TEMP_SUFFIX: &str = ".raijin-part";
const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// Captured headers that must never be forwarded, either because reqwest derives
/// them or because they only describe the original browser request.
const DROPPED_HEADERS: &[&str] = &[
    "host",
    "range",
    "if-range",
    "content-length",
    "content-type",
    "content-encoding",
    "transfer-encoding",
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authorization",
    "te",
    "trailer",
    "upgrade",
    "accept-encoding",
];

/// Errors returned by HLS VOD jobs.
///
/// Messages never include request URLs so signed playlist and segment URLs stay
/// out of logs, the database, and the UI.
#[derive(Debug, Error)]
pub enum HlsDownloadJobError {
    /// Database operation failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// Filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Playlist was rejected by the parser.
    #[error("unsupported or invalid playlist: {0}")]
    Playlist(String),
    /// Network request failed.
    #[error("network request failed: {0}")]
    Network(String),
    /// URL is not an absolute HTTP(S) URL with a host.
    #[error("download URL is not a valid HTTP(S) URL")]
    InvalidUrl,
    /// URL embeds user credentials.
    #[error("URLs must not embed credentials")]
    EmbeddedCredentials,
    /// URL carries a fragment.
    #[error("URLs must not contain fragments")]
    FragmentNotAllowed,
    /// Redirect target is not an absolute HTTP(S) URL.
    #[error("redirect target is not a valid HTTP(S) URL")]
    InvalidRedirect,
    /// Redirect chain is longer than allowed.
    #[error("too many redirects")]
    TooManyRedirects,
    /// Redirect response carried no usable `Location`.
    #[error("redirect response is missing a Location header")]
    MissingRedirectLocation,
    /// Header name or value could not be used for an outgoing request.
    #[error("request header `{0}` is invalid")]
    InvalidHeader(String),
    /// Playlist nesting is deeper than allowed.
    #[error("playlist nesting exceeded {limit} levels")]
    PlaylistDepth { limit: u32 },
    /// Playlist chain loops back to an already visited URL.
    #[error("playlist redirect loop detected")]
    PlaylistLoop,
    /// Playlist body exceeds the hard cap.
    #[error("playlist exceeds the {limit} byte limit")]
    PlaylistTooLarge { limit: usize },
    /// Server returned a status that must not be retried.
    #[error("unexpected HTTP status `{0}`")]
    UnexpectedStatus(u16),
    /// Server returned a status worth retrying.
    #[error("server returned HTTP status `{status}`")]
    TransientStatus {
        /// Numeric HTTP status.
        status: u16,
        /// Delay requested by the server, already bounded.
        retry_after: Option<Duration>,
    },
    /// Server answered a full-resource request with a partial response.
    #[error("server returned a partial response for a full resource")]
    PartialResponse,
    /// Response body was content-encoded.
    #[error("content-encoded responses are not supported")]
    ContentEncoding,
    /// `Content-Length` header could not be parsed.
    #[error("invalid Content-Length header")]
    InvalidContentLength,
    /// Segment is not an MPEG-TS packet stream.
    #[error("segment `{index}` is not an MPEG-TS packet stream")]
    NotTransportStream { index: u32 },
    /// Received byte count does not match the declared length.
    #[error("segment `{index}` length mismatch: expected {expected} bytes, received {actual}")]
    LengthMismatch {
        index: u32,
        expected: u64,
        actual: u64,
    },
    /// Local segment payload no longer matches its persisted checksum.
    #[error("segment `{index}` checksum does not match the stored checksum")]
    ChecksumMismatch { index: u32 },
    /// Retained segment no longer matches what the server reports.
    #[error("segment `{index}` changed remotely; manual validation required")]
    SegmentChanged { index: u32 },
    /// Selected plan changed on the server since it was persisted.
    #[error("selected HLS plan changed remotely ({0}); manual validation required")]
    PlanChanged(&'static str),
    /// Plan row disappeared while the job held it.
    #[error("HLS plan is missing for this download")]
    MissingPlan,
    /// Destination name could not be disambiguated.
    #[error("destination file conflict for `{0}`")]
    DestinationConflict(String),
    /// Filesystem cannot publish atomically without overwriting.
    #[error("could not publish output safely: {0}")]
    PublishUnsupported(#[source] std::io::Error),
    /// Retry budget was exhausted for a segment.
    #[error("too many retries: {0}")]
    TooManyRetries(String),
    /// Segment worker task failed to join.
    #[error("segment worker task failed: {0}")]
    WorkerJoin(#[from] tokio::task::JoinError),
    /// Cancellation token was triggered.
    #[error("download was canceled")]
    Cancelled,
    /// System clock is earlier than Unix epoch.
    #[error("system clock is earlier than Unix epoch")]
    ClockBeforeEpoch,
    /// System clock value does not fit in database timestamp range.
    #[error("system clock timestamp is out of range")]
    ClockOutOfRange,
    /// Byte accounting overflowed.
    #[error("download byte counter overflowed")]
    ByteCountOverflow,
    /// Segment directory name could not be derived from the download id.
    #[error("segment directory path is not valid UTF-8")]
    NonUtf8Path,
}

impl From<reqwest::Error> for HlsDownloadJobError {
    fn from(error: reqwest::Error) -> Self {
        Self::Network(describe_request_error(&error))
    }
}

impl From<PlaylistError> for HlsDownloadJobError {
    fn from(error: PlaylistError) -> Self {
        Self::Playlist(describe_playlist_error(&error))
    }
}

impl HlsDownloadJobError {
    /// Converts the job error into persisted failure details.
    #[must_use]
    pub fn to_failure(&self) -> DownloadFailure {
        DownloadFailure {
            kind: failure_kind(self),
            message: self.to_string(),
        }
    }

    /// Returns true when the error must pause for user validation instead of retrying.
    #[must_use]
    pub fn requires_manual_action(&self) -> bool {
        matches!(
            self,
            Self::Playlist(_)
                | Self::PlanChanged(_)
                | Self::SegmentChanged { .. }
                | Self::MissingPlan
                | Self::PartialResponse
                | Self::ContentEncoding
                | Self::InvalidContentLength
                | Self::NotTransportStream { .. }
                | Self::InvalidUrl
                | Self::EmbeddedCredentials
                | Self::FragmentNotAllowed
                | Self::InvalidRedirect
                | Self::PlaylistDepth { .. }
                | Self::PlaylistLoop
                | Self::PlaylistTooLarge { .. }
                | Self::NonUtf8Path
        )
    }

    fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Network(_) | Self::TransientStatus { .. } | Self::LengthMismatch { .. }
        )
    }

    fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::TransientStatus { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

fn describe_request_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "request timed out".to_owned()
    } else if error.is_connect() {
        "connection failed".to_owned()
    } else if error.is_body() || error.is_decode() {
        "response body failed".to_owned()
    } else if let Some(status) = error.status() {
        format!("request failed with status `{}`", status.as_u16())
    } else {
        "request failed".to_owned()
    }
}

fn describe_playlist_error(error: &PlaylistError) -> String {
    match error {
        PlaylistError::InvalidUrl(_) => "playlist contains an invalid URL".to_owned(),
        other => other.to_string(),
    }
}

fn failure_kind(error: &HlsDownloadJobError) -> FailureKind {
    match error {
        HlsDownloadJobError::Cancelled => FailureKind::Canceled,
        HlsDownloadJobError::TooManyRetries(_) => FailureKind::TooManyRetries,
        HlsDownloadJobError::Io(error) | HlsDownloadJobError::PublishUnsupported(error)
            if error.kind() == std::io::ErrorKind::StorageFull =>
        {
            FailureKind::NoSpace
        }
        HlsDownloadJobError::Io(_)
        | HlsDownloadJobError::Db(_)
        | HlsDownloadJobError::ChecksumMismatch { .. }
        | HlsDownloadJobError::WorkerJoin(_)
        | HlsDownloadJobError::DestinationConflict(_)
        | HlsDownloadJobError::PublishUnsupported(_)
        | HlsDownloadJobError::ClockBeforeEpoch
        | HlsDownloadJobError::ClockOutOfRange
        | HlsDownloadJobError::ByteCountOverflow
        | HlsDownloadJobError::NonUtf8Path => FailureKind::Disk,
        HlsDownloadJobError::Network(_)
        | HlsDownloadJobError::TransientStatus { .. }
        | HlsDownloadJobError::UnexpectedStatus(_)
        | HlsDownloadJobError::LengthMismatch { .. } => FailureKind::Network,
        HlsDownloadJobError::Playlist(_)
        | HlsDownloadJobError::PlanChanged(_)
        | HlsDownloadJobError::SegmentChanged { .. }
        | HlsDownloadJobError::MissingPlan
        | HlsDownloadJobError::PartialResponse
        | HlsDownloadJobError::ContentEncoding
        | HlsDownloadJobError::InvalidContentLength
        | HlsDownloadJobError::NotTransportStream { .. }
        | HlsDownloadJobError::InvalidUrl
        | HlsDownloadJobError::EmbeddedCredentials
        | HlsDownloadJobError::FragmentNotAllowed
        | HlsDownloadJobError::InvalidRedirect
        | HlsDownloadJobError::TooManyRedirects
        | HlsDownloadJobError::MissingRedirectLocation
        | HlsDownloadJobError::InvalidHeader(_)
        | HlsDownloadJobError::PlaylistDepth { .. }
        | HlsDownloadJobError::PlaylistLoop
        | HlsDownloadJobError::PlaylistTooLarge { .. } => FailureKind::Validation,
    }
}

/// Result type returned by HLS VOD jobs.
pub type HlsDownloadJobResult<T> = Result<T, HlsDownloadJobError>;

/// Native HLS VOD job that resolves a playlist into MPEG-TS segments and assembles one file.
///
/// Media requests use an internally built HTTP client so redirects are walked
/// manually: captured browser headers must never follow a redirect to another
/// origin, which reqwest's automatic redirect handling cannot express.
#[derive(Debug, Clone)]
pub struct HlsDownloadJob {
    download_repo: DownloadRepository,
    hls_repo: HlsRepository,
    item: DownloadItem,
    config: HlsJobConfig,
    live_progress: Option<Arc<LiveDownloadProgress>>,
    /// Shared global throughput limiter; `None` keeps the zero-cost path.
    global_limiter: Option<Arc<SpeedLimiter>>,
}

#[derive(Debug, Clone)]
struct HlsJobConfig {
    incomplete_extension: String,
    progress_interval: Duration,
    max_retries: u32,
    preferred_connections: u16,
    max_connections: u16,
    max_playlist_bytes: usize,
    max_playlist_depth: u32,
}

impl Default for HlsJobConfig {
    fn default() -> Self {
        let download_config = DownloadConfig::default();
        Self {
            incomplete_extension: download_config.incomplete_extension,
            progress_interval: DEFAULT_PROGRESS_INTERVAL,
            max_retries: download_config.max_retries.get(),
            preferred_connections: DEFAULT_HLS_CONNECTIONS,
            max_connections: download_config.max_connections.get(),
            max_playlist_bytes: MAX_PLAYLIST_BYTES,
            max_playlist_depth: MAX_PLAYLIST_DEPTH,
        }
    }
}

impl HlsDownloadJob {
    /// Creates an HLS job for one persisted download.
    #[must_use]
    pub fn new(download_repo: DownloadRepository, item: DownloadItem) -> Self {
        let hls_repo = download_repo.hls_repository();
        Self {
            download_repo,
            hls_repo,
            item,
            config: HlsJobConfig::default(),
            live_progress: None,
            global_limiter: None,
        }
    }

    /// Attaches a shared global throughput limiter layered over the per-download limit.
    ///
    /// `None` disables global shaping and keeps the zero-cost path.
    #[must_use]
    pub fn with_global_limiter(mut self, limiter: Option<Arc<SpeedLimiter>>) -> Self {
        self.global_limiter = limiter;
        self
    }

    pub(crate) fn with_live_progress(mut self, live_progress: Arc<LiveDownloadProgress>) -> Self {
        self.live_progress = Some(live_progress);
        self
    }

    /// Runs the job until completion, cancellation, or error.
    ///
    /// Returns `Ok` with a paused item when the job needs user validation or was
    /// canceled, so the caller only sees `Err` for terminal failures. Partial media
    /// never reaches the final destination.
    ///
    /// # Errors
    ///
    /// Returns an error when network, validation, persistence, or disk operations fail.
    pub async fn run(
        mut self,
        cancellation: CancellationToken,
    ) -> HlsDownloadJobResult<DownloadItem> {
        let result = self.run_inner(&cancellation).await;
        match result {
            Ok(item) => Ok(item),
            Err(_error) if cancellation.is_cancelled() => {
                self.persist_paused().await?;
                self.sync_live_progress().await?;
                Ok(self.item.clone())
            }
            Err(error) if error.requires_manual_action() => {
                self.persist_paused_with_failure(&error).await?;
                self.sync_live_progress().await?;
                Ok(self.item.clone())
            }
            Err(error) => {
                self.persist_error(&error).await?;
                self.sync_live_progress().await?;
                Err(error)
            }
        }
    }

    async fn run_inner(
        &mut self,
        cancellation: &CancellationToken,
    ) -> HlsDownloadJobResult<DownloadItem> {
        let client = build_client()?;
        let origin =
            Origin::of(&parse_http_url(&self.item.url)?).ok_or(HlsDownloadJobError::InvalidUrl)?;
        let directory = segment_directory(&self.item);
        fs::create_dir_all(&directory).await?;
        remove_stale_temps(&directory).await?;

        let (urls, mut progress) = self
            .load_or_resolve_plan(&client, &origin, &directory, cancellation)
            .await?;
        let pending = urls
            .iter()
            .enumerate()
            .filter(|(index, _)| !completed_contains(&progress, *index))
            .map(|(index, _)| {
                u32::try_from(index).map_err(|_| HlsDownloadJobError::ByteCountOverflow)
            })
            .collect::<HlsDownloadJobResult<VecDeque<u32>>>()?;

        self.persist_status(DownloadStatus::Downloading, Bytes::new(progress.byte_total))
            .await?;
        if let Some(live_progress) = &self.live_progress {
            live_progress.set_downloaded(Bytes::new(progress.byte_total));
        }

        self.download_segments(&urls, pending, &directory, cancellation, &mut progress)
            .await?;
        if cancellation.is_cancelled() {
            let _ = remove_stale_temps(&directory).await;
            self.persist_paused().await?;
            self.sync_live_progress().await?;
            return Ok(self.item.clone());
        }

        self.persist_status(
            DownloadStatus::PreparingFile,
            Bytes::new(progress.byte_total),
        )
        .await?;
        let total = self.assemble(&directory, cancellation).await?;
        self.item.total_bytes = Some(Bytes::new(total));
        self.persist_status(DownloadStatus::Completed, Bytes::new(total))
            .await?;
        if let Some(live_progress) = &self.live_progress {
            live_progress.set_downloaded(Bytes::new(total));
        }
        // Segments are retained until the output is published; only then are they disposable.
        let _ = fs::remove_dir_all(&directory).await;
        Ok(self.item.clone())
    }

    /// Resolves segment URLs and the reusable progress to start from.
    ///
    /// A plan whose every segment already exists locally and matches its persisted
    /// length and checksum is used as-is, so an assembly retry never touches the
    /// network.
    async fn load_or_resolve_plan(
        &self,
        client: &Client,
        origin: &Origin,
        directory: &Path,
        cancellation: &CancellationToken,
    ) -> HlsDownloadJobResult<(Vec<String>, SegmentProgress)> {
        let Some(record) = self.hls_repo.load(self.item.id).await? else {
            let plan = self.resolve_plan(client, origin, cancellation).await?;
            self.hls_repo
                .save_plan(self.item.id, &record_for(&plan))
                .await?;
            let urls = plan
                .segments
                .iter()
                .map(|segment| segment.url.clone())
                .collect();
            return Ok((urls, SegmentProgress::default()));
        };

        if let Some(progress) = verify_all_local(&record, directory, cancellation).await? {
            tracing::debug!(
                download_id = self.item.id.get(),
                segments = record.segments.len(),
                "HLS segments verified locally; skipping network plan refresh"
            );
            let urls = record
                .segments
                .iter()
                .map(|segment| segment.url.clone())
                .collect();
            return Ok((urls, progress));
        }

        let (plan, persisted) = self
            .resume_plan(client, origin, record, cancellation)
            .await?;
        let progress = self
            .reuse_completed_segments(&plan, &persisted, directory, cancellation)
            .await?;
        // Retained bytes are only mixed with fresh ones after a remote identity check.
        self.validate_reused_segments(client, origin, &persisted, &progress, cancellation)
            .await?;
        if let Some(live_progress) = &self.live_progress {
            live_progress.set_downloaded(Bytes::new(progress.byte_total));
        }
        let urls = plan
            .segments
            .iter()
            .map(|segment| segment.url.clone())
            .collect();
        Ok((urls, progress))
    }

    /// Refetches the persisted playlist and strictly compares it with the stored plan.
    async fn resume_plan(
        &self,
        client: &Client,
        origin: &Origin,
        record: HlsPlanRecord,
        cancellation: &CancellationToken,
    ) -> HlsDownloadJobResult<(MediaPlan, Vec<HlsSegmentRecord>)> {
        let (body, final_url) = self
            .fetch_playlist(client, &record.playlist_url, origin, cancellation)
            .await?;
        let plan = match parse_playlist(&body, &final_url)? {
            Playlist::Media(plan) => plan,
            Playlist::Master(_) => {
                return Err(HlsDownloadJobError::PlanChanged(
                    "persisted media playlist now resolves to a master playlist",
                ));
            }
        };
        if !plan_matches(&plan, &record) {
            return Err(HlsDownloadJobError::PlanChanged(
                "segment sequence, URI, or duration changed",
            ));
        }
        Ok((plan, record.segments))
    }

    /// Resolves the initial segment plan from the download URL.
    ///
    /// Variants arrive sorted by bandwidth, highest first. The first variant whose
    /// media playlist is inside the supported subset wins; a variant that parses but
    /// is unsupported (fMP4, encryption, live, byte ranges, ...) is skipped for the
    /// next one, bounded by `MAX_VARIANT_FALLBACKS` so a mislabeled master cannot
    /// turn into an unbounded request loop. Network, authentication and redirect
    /// failures are never masked: they abort resolution instead of silently
    /// selecting another rendition.
    ///
    /// This fallback only exists on first resolve. Resume reuses the persisted plan,
    /// so a later failure cannot move the selection.
    async fn resolve_plan(
        &self,
        client: &Client,
        origin: &Origin,
        cancellation: &CancellationToken,
    ) -> HlsDownloadJobResult<MediaPlan> {
        let mut stack = vec![(parse_http_url(&self.item.url)?, 0_u32)];
        let mut visited = HashSet::new();
        let mut fallbacks = 0_u32;

        while let Some((url, depth)) = stack.pop() {
            if !visited.insert(url.as_str().to_owned()) {
                return Err(HlsDownloadJobError::PlaylistLoop);
            }
            let (body, final_url) = self
                .fetch_playlist(client, url.as_str(), origin, cancellation)
                .await?;
            // A redirect re-visiting an earlier playlist is a loop; a plain refetch
            // of the same URL is not.
            if final_url != url.as_str() && !visited.insert(final_url.clone()) {
                return Err(HlsDownloadJobError::PlaylistLoop);
            }
            match parse_playlist(&body, &final_url) {
                Ok(Playlist::Media(plan)) => return Ok(plan),
                Ok(Playlist::Master(variants)) => {
                    if variants.is_empty() {
                        return Err(HlsDownloadJobError::Playlist(
                            "master playlist has no usable variant".to_owned(),
                        ));
                    }
                    // Variants are sorted highest bandwidth first; the stack is LIFO,
                    // so push in reverse to try the best rendition first.
                    let child_depth = depth.saturating_add(1);
                    if child_depth > self.config.max_playlist_depth {
                        return Err(HlsDownloadJobError::PlaylistDepth {
                            limit: self.config.max_playlist_depth,
                        });
                    }
                    tracing::debug!(
                        download_id = self.item.id.get(),
                        depth,
                        variants = variants.len(),
                        "resolving HLS master playlist"
                    );
                    for variant in variants.into_iter().rev() {
                        stack.push((parse_http_url(&variant.url)?, child_depth));
                    }
                }
                Err(error)
                    if is_skippable_variant_error(&error) && fallbacks < MAX_VARIANT_FALLBACKS =>
                {
                    fallbacks += 1;
                    tracing::debug!(
                        download_id = self.item.id.get(),
                        %error,
                        "skipping unsupported HLS variant"
                    );
                }
                Err(error) => return Err(error.into()),
            }
        }

        Err(HlsDownloadJobError::Playlist(
            "no variant playlist is inside the supported HLS subset".to_owned(),
        ))
    }

    async fn fetch_playlist(
        &self,
        client: &Client,
        url: &str,
        origin: &Origin,
        cancellation: &CancellationToken,
    ) -> HlsDownloadJobResult<(Vec<u8>, String)> {
        let mut response = send_following_redirects(
            client,
            RequestKind::Playlist,
            url,
            &self.item.headers,
            origin,
            cancellation,
        )
        .await?;
        validate_success_status(&response)?;
        check_content_encoding(response.headers())?;
        let final_url = response.url().as_str().to_owned();
        let cap = self.config.max_playlist_bytes;
        let mut body = Vec::new();
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return Err(HlsDownloadJobError::Cancelled),
                chunk = response.chunk() => {
                    let Some(chunk) = chunk? else { break };
                    if body.len().saturating_add(chunk.len()) > cap {
                        return Err(HlsDownloadJobError::PlaylistTooLarge { limit: cap });
                    }
                    body.extend_from_slice(&chunk);
                }
            }
        }
        Ok((body, final_url))
    }

    /// Re-verifies persisted completed segments and returns the accounting they justify.
    async fn reuse_completed_segments(
        &self,
        plan: &MediaPlan,
        persisted: &[HlsSegmentRecord],
        directory: &Path,
        cancellation: &CancellationToken,
    ) -> HlsDownloadJobResult<SegmentProgress> {
        let mut progress = SegmentProgress::default();
        for record in persisted {
            let (Some(length), Some(sha256)) = (record.length, record.sha256.as_deref()) else {
                continue;
            };
            let Ok(index) = usize::try_from(record.index) else {
                continue;
            };
            if index >= plan.segments.len() {
                continue;
            }
            let path = segment_path(directory, record.index);
            if verify_file(&path, length, sha256, cancellation).await? {
                progress.completed.insert(record.index);
                progress.byte_total = progress
                    .byte_total
                    .checked_add(length)
                    .ok_or(HlsDownloadJobError::ByteCountOverflow)?;
            } else {
                self.hls_repo
                    .invalidate_segment(self.item.id, record.index)
                    .await?;
                let _ = fs::remove_file(&path).await;
            }
        }
        Ok(progress)
    }

    /// Confirms retained segments still describe the same remote resource.
    ///
    /// Reused bytes are only mixed with freshly downloaded bytes when the server
    /// confirms identity through a matching validator; silent remote
    /// replacement would otherwise splice two recordings into one output.
    ///
    /// Known limitation: a server exposing neither `ETag` nor `Last-Modified`
    /// leaves retained bytes unverifiable, so a partial resume pauses with a
    /// validation failure and needs a user reset. Restarting from zero on the
    /// server's word alone was rejected because it silently discards verified
    /// progress. When every segment is already complete locally, no probe happens at
    /// all, so an expired source does not block assembly retry.
    async fn validate_reused_segments(
        &self,
        client: &Client,
        origin: &Origin,
        persisted: &[HlsSegmentRecord],
        progress: &SegmentProgress,
        cancellation: &CancellationToken,
    ) -> HlsDownloadJobResult<()> {
        for record in persisted {
            if !progress.completed.contains(&record.index) {
                continue;
            }
            // shortcut: validators are probed sequentially, one response at a time;
            // parallelize if resume of very long VODs becomes latency-bound.
            let identity = send_following_redirects(
                client,
                RequestKind::IdentityProbe,
                &record.url,
                &self.item.headers,
                origin,
                cancellation,
            )
            .await
            .and_then(remote_identity);
            let identity = identity?;

            let etag_matches = match (&record.etag, &identity.etag) {
                (Some(stored), Some(remote)) if stored == remote => true,
                (Some(_), Some(_)) => {
                    return Err(HlsDownloadJobError::SegmentChanged {
                        index: record.index,
                    });
                }
                _ => false,
            };
            let last_modified_matches = match (&record.last_modified, &identity.last_modified) {
                (Some(stored), Some(remote)) if stored == remote => true,
                (Some(_), Some(_)) => {
                    return Err(HlsDownloadJobError::SegmentChanged {
                        index: record.index,
                    });
                }
                _ => false,
            };
            if let (Some(stored), Some(remote)) = (record.length, identity.length)
                && stored != remote
            {
                return Err(HlsDownloadJobError::SegmentChanged {
                    index: record.index,
                });
            }
            let confirmed = etag_matches || last_modified_matches;
            if !confirmed {
                tracing::warn!(
                    download_id = self.item.id.get(),
                    segment = record.index,
                    "server exposes no matching validator for a retained segment"
                );
                return Err(HlsDownloadJobError::SegmentChanged {
                    index: record.index,
                });
            }
        }
        Ok(())
    }

    /// Downloads pending segments with bounded concurrency and a single JoinSet owner.
    async fn download_segments(
        &mut self,
        urls: &[String],
        pending: VecDeque<u32>,
        directory: &Path,
        cancellation: &CancellationToken,
        progress: &mut SegmentProgress,
    ) -> HlsDownloadJobResult<()> {
        let mut workers = JoinSet::new();
        let workers_cancellation = cancellation.child_token();
        let result = self
            .segment_loop(
                &mut workers,
                urls,
                pending,
                directory,
                &workers_cancellation,
                progress,
            )
            .await;
        workers_cancellation.cancel();
        drain_workers(&mut workers).await;
        result
    }

    async fn segment_loop(
        &mut self,
        workers: &mut JoinSet<HlsDownloadJobResult<SegmentOutcome>>,
        urls: &[String],
        mut pending: VecDeque<u32>,
        directory: &Path,
        cancellation: &CancellationToken,
        progress: &mut SegmentProgress,
    ) -> HlsDownloadJobResult<()> {
        let limit = usize::from(self.connection_limit());
        let captured = Arc::new(self.item.headers.clone());
        let origin =
            Origin::of(&parse_http_url(&self.item.url)?).ok_or(HlsDownloadJobError::InvalidUrl)?;
        let client = build_client()?;
        let speed_limiter = self.item.speed_limit.and_then(SpeedLimiter::new);
        let mut ticker = tokio::time::interval(self.config.progress_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await;

        loop {
            while workers.len() < limit {
                let Some(index) = pending.pop_front() else {
                    break;
                };
                let url = urls
                    .get(
                        usize::try_from(index)
                            .map_err(|_| HlsDownloadJobError::ByteCountOverflow)?,
                    )
                    .ok_or(HlsDownloadJobError::MissingPlan)?;
                workers.spawn(download_segment(SegmentRequest {
                    client: client.clone(),
                    cancellation: cancellation.clone(),
                    captured: Arc::clone(&captured),
                    origin: origin.clone(),
                    directory: directory.to_path_buf(),
                    index,
                    url: url.clone(),
                    max_retries: self.config.max_retries,
                    speed_limiter: speed_limiter.clone(),
                    global_limiter: self.global_limiter.clone(),
                    live_progress: self.live_progress.clone(),
                }));
            }

            if workers.is_empty() {
                break;
            }

            tokio::select! {
                () = cancellation.cancelled() => break,
                _ = ticker.tick() => {
                    // Persist progress on a timer, never per chunk.
                    self.persist_status(
                        DownloadStatus::Downloading,
                        Bytes::new(progress.byte_total),
                    )
                    .await?;
                }
                joined = workers.join_next() => {
                    let Some(joined) = joined else { break };
                    let outcome = joined??;
                    progress.byte_total = progress
                        .byte_total
                        .checked_add(outcome.length)
                        .ok_or(HlsDownloadJobError::ByteCountOverflow)?;
                    progress.completed.insert(outcome.index);
                    self.hls_repo
                        .complete_segment(
                            self.item.id,
                            outcome.index,
                            outcome.length,
                            &outcome.sha256,
                            outcome.etag.as_deref(),
                            outcome.last_modified.as_deref(),
                        )
                        .await?;
                    // Only verified, persisted bytes reach the live counter.
                    if let Some(live_progress) = &self.live_progress {
                        live_progress.add_downloaded(outcome.length);
                    }
                }
            }
        }
        Ok(())
    }

    /// Concatenates completed segments in plan order and publishes the final file.
    async fn assemble(
        &mut self,
        directory: &Path,
        cancellation: &CancellationToken,
    ) -> HlsDownloadJobResult<u64> {
        let record = self
            .hls_repo
            .load(self.item.id)
            .await?
            .ok_or(HlsDownloadJobError::MissingPlan)?;

        fs::create_dir_all(&self.item.folder).await?;
        let incomplete = self.incomplete_path();
        let total =
            match write_assembled(&record.segments, directory, &incomplete, cancellation).await {
                Ok(total) => total,
                Err(error) => {
                    let _ = fs::remove_file(&incomplete).await;
                    return Err(error);
                }
            };

        let final_path = self.publish(&incomplete).await?;
        if let Some(file_name) = final_path.file_name().and_then(|name| name.to_str())
            && file_name != self.item.file_name
        {
            self.item.file_name = file_name.to_owned();
        }
        Ok(total)
    }

    /// Publishes the assembled file without ever overwriting an existing output.
    ///
    /// A hard link creates the destination atomically and fails if the name is
    /// taken, so concurrent jobs and pre-existing files can never be clobbered.
    async fn publish(&mut self, incomplete: &Path) -> HlsDownloadJobResult<PathBuf> {
        let requested = self.item.folder.join(&self.item.file_name);
        for suffix in 0..64 {
            let candidate = suffixed_path(&requested, suffix);
            match fs::hard_link(incomplete, &candidate).await {
                Ok(()) => {
                    let _ = fs::remove_file(incomplete).await;
                    return Ok(candidate);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    tracing::warn!(
                        download_id = self.item.id.get(),
                        ?error,
                        "collision-safe publish failed"
                    );
                    return Err(HlsDownloadJobError::PublishUnsupported(error));
                }
            }
        }
        Err(HlsDownloadJobError::DestinationConflict(
            requested.display().to_string(),
        ))
    }

    fn connection_limit(&self) -> u16 {
        let preferred = self
            .item
            .preferred_connections
            .map_or(self.config.preferred_connections, |value| value.get());
        preferred.clamp(1, self.config.max_connections.max(1))
    }

    fn incomplete_path(&self) -> PathBuf {
        self.item.folder.join(format!(
            "{}{}",
            self.item.file_name, self.config.incomplete_extension
        ))
    }

    async fn persist_status(
        &mut self,
        status: DownloadStatus,
        downloaded: Bytes,
    ) -> HlsDownloadJobResult<()> {
        let now = now_ms()?;
        self.item.status = status;
        self.item.downloaded_bytes = downloaded;
        self.item.updated_at = now;
        if matches!(
            status,
            DownloadStatus::Downloading | DownloadStatus::PreparingFile | DownloadStatus::Completed
        ) {
            self.item.started_at = self.item.started_at.or(Some(now));
            self.item.failure = None;
        }
        if status == DownloadStatus::Completed {
            self.item.completed_at = Some(now);
        }
        self.download_repo.update(&self.item).await?;
        Ok(())
    }

    async fn persist_paused(&mut self) -> HlsDownloadJobResult<()> {
        let downloaded = self.completed_bytes().await?;
        self.persist_status(DownloadStatus::Paused, Bytes::new(downloaded))
            .await
    }

    async fn persist_paused_with_failure(
        &mut self,
        error: &HlsDownloadJobError,
    ) -> HlsDownloadJobResult<()> {
        let downloaded = self.completed_bytes().await?;
        self.item.failure = Some(error.to_failure());
        self.persist_status(DownloadStatus::Paused, Bytes::new(downloaded))
            .await
    }

    async fn persist_error(&mut self, error: &HlsDownloadJobError) -> HlsDownloadJobResult<()> {
        let downloaded = self.completed_bytes().await?;
        self.item.failure = Some(error.to_failure());
        self.persist_status(DownloadStatus::Error, Bytes::new(downloaded))
            .await
    }

    /// Sums verified persisted segment lengths so counters never include partial payloads.
    async fn completed_bytes(&self) -> HlsDownloadJobResult<u64> {
        let Some(plan) = self.hls_repo.load(self.item.id).await? else {
            return Ok(0);
        };
        plan.segments
            .iter()
            .filter_map(|segment| segment.length)
            .try_fold(0_u64, |total, length| {
                total
                    .checked_add(length)
                    .ok_or(HlsDownloadJobError::ByteCountOverflow)
            })
    }

    async fn sync_live_progress(&self) -> HlsDownloadJobResult<()> {
        if let Some(live_progress) = &self.live_progress {
            live_progress.set_downloaded(Bytes::new(self.completed_bytes().await?));
        }
        Ok(())
    }
}

/// Verified completed segment indices and their total length.
#[derive(Debug, Default)]
struct SegmentProgress {
    byte_total: u64,
    completed: HashSet<u32>,
}

fn completed_contains(progress: &SegmentProgress, index: usize) -> bool {
    u32::try_from(index).is_ok_and(|index| progress.completed.contains(&index))
}

/// Returns progress when every persisted segment verifies from disk.
async fn verify_all_local(
    record: &HlsPlanRecord,
    directory: &Path,
    cancellation: &CancellationToken,
) -> HlsDownloadJobResult<Option<SegmentProgress>> {
    let mut progress = SegmentProgress::default();
    for segment in &record.segments {
        let (Some(length), Some(sha256)) = (segment.length, segment.sha256.as_deref()) else {
            return Ok(None);
        };
        let path = segment_path(directory, segment.index);
        if !verify_file(&path, length, sha256, cancellation).await? {
            return Ok(None);
        }
        progress.completed.insert(segment.index);
        progress.byte_total = progress
            .byte_total
            .checked_add(length)
            .ok_or(HlsDownloadJobError::ByteCountOverflow)?;
    }
    Ok(Some(progress))
}

struct SegmentRequest {
    client: Client,
    cancellation: CancellationToken,
    captured: Arc<BTreeMap<String, String>>,
    origin: Origin,
    directory: PathBuf,
    index: u32,
    url: String,
    max_retries: u32,
    speed_limiter: Option<Arc<SpeedLimiter>>,
    global_limiter: Option<Arc<SpeedLimiter>>,
    live_progress: Option<Arc<LiveDownloadProgress>>,
}

struct AttemptProgress {
    live: Option<Arc<LiveDownloadProgress>>,
    bytes: u64,
}

impl AttemptProgress {
    fn new(live: Option<Arc<LiveDownloadProgress>>) -> Self {
        if let Some(progress) = &live {
            progress.add_active_part();
        }
        Self { live, bytes: 0 }
    }

    fn record(&mut self, bytes: u64) {
        self.bytes = self.bytes.saturating_add(bytes);
        if let Some(progress) = &self.live {
            progress.add_downloaded(bytes);
        }
    }
}

impl Drop for AttemptProgress {
    fn drop(&mut self) {
        if let Some(progress) = &self.live {
            progress.subtract_downloaded(self.bytes);
            progress.remove_active_part();
        }
    }
}

#[derive(Debug)]
struct SegmentOutcome {
    index: u32,
    length: u64,
    sha256: String,
    etag: Option<String>,
    last_modified: Option<String>,
}

/// Remote identity of one retained segment, used to keep resume from mixing versions.
#[derive(Debug, Default)]
struct RemoteIdentity {
    length: Option<u64>,
    etag: Option<String>,
    last_modified: Option<String>,
}

async fn download_segment(request: SegmentRequest) -> HlsDownloadJobResult<SegmentOutcome> {
    let mut attempt = 0_u32;
    loop {
        match download_segment_once(&request).await {
            Ok(outcome) => return Ok(outcome),
            Err(error) if error.is_transient() && !request.cancellation.is_cancelled() => {
                if attempt >= request.max_retries {
                    return Err(HlsDownloadJobError::TooManyRetries(format!(
                        "segment `{}` failed after {} retries",
                        request.index, request.max_retries
                    )));
                }
                attempt += 1;
                let delay = error
                    .retry_after()
                    .unwrap_or_else(|| retry_backoff(attempt));
                tracing::debug!(
                    segment = request.index,
                    attempt,
                    %error,
                    "retrying HLS segment"
                );
                sleep_cancellable(delay, &request.cancellation).await;
                if request.cancellation.is_cancelled() {
                    return Err(HlsDownloadJobError::Cancelled);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

async fn download_segment_once(request: &SegmentRequest) -> HlsDownloadJobResult<SegmentOutcome> {
    let mut progress = AttemptProgress::new(request.live_progress.clone());
    let mut response = send_following_redirects(
        &request.client,
        RequestKind::Segment,
        &request.url,
        &request.captured,
        &request.origin,
        &request.cancellation,
    )
    .await?;
    validate_success_status(&response)?;
    check_content_encoding(response.headers())?;
    if let Some(content_type) = header_string(response.headers(), header::CONTENT_TYPE)
        && content_type.to_ascii_lowercase().contains("html")
    {
        return Err(HlsDownloadJobError::NotTransportStream {
            index: request.index,
        });
    }
    let expected = content_length(response.headers())?;
    let etag = header_string(response.headers(), header::ETAG);
    let last_modified = header_string(response.headers(), header::LAST_MODIFIED);

    let temp = segment_temp_path(&request.directory, request.index);
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&temp)
        .await?;
    let mut hasher = Sha256::new();
    let mut written = 0_u64;
    let result = stream_segment_body(
        request,
        &mut response,
        &mut file,
        &mut hasher,
        &mut written,
        expected,
        &mut progress,
    )
    .await;
    if let Err(error) = result {
        drop(file);
        let _ = fs::remove_file(&temp).await;
        return Err(error);
    }
    file.flush().await?;
    file.sync_all().await?;
    drop(file);

    let outcome = SegmentOutcome {
        index: request.index,
        length: written,
        sha256: hex_digest(hasher.finalize()),
        etag,
        last_modified,
    };
    if let Err(error) = fs::rename(&temp, segment_path(&request.directory, request.index)).await {
        let _ = fs::remove_file(&temp).await;
        return Err(error.into());
    }
    Ok(outcome)
}

async fn stream_segment_body(
    request: &SegmentRequest,
    response: &mut Response,
    file: &mut tokio::fs::File,
    hasher: &mut Sha256,
    written: &mut u64,
    expected: Option<u64>,
    progress: &mut AttemptProgress,
) -> HlsDownloadJobResult<()> {
    loop {
        let chunk = tokio::select! {
            () = request.cancellation.cancelled() => return Err(HlsDownloadJobError::Cancelled),
            chunk = response.chunk() => chunk?,
        };
        let Some(chunk) = chunk else { break };
        if chunk.is_empty() {
            continue;
        }
        let length =
            u64::try_from(chunk.len()).map_err(|_| HlsDownloadJobError::ByteCountOverflow)?;
        let next = written
            .checked_add(length)
            .ok_or(HlsDownloadJobError::ByteCountOverflow)?;
        if let Some(expected) = expected
            && next > expected
        {
            return Err(HlsDownloadJobError::LengthMismatch {
                index: request.index,
                expected,
                actual: next,
            });
        }
        if !ts_sync_ok(*written, &chunk) {
            return Err(HlsDownloadJobError::NotTransportStream {
                index: request.index,
            });
        }
        if let Some(limiter) = &request.speed_limiter {
            acquire_cancellable(limiter, length, &request.cancellation).await?;
        }
        if let Some(limiter) = &request.global_limiter {
            acquire_cancellable(limiter, length, &request.cancellation).await?;
        }
        file.write_all(&chunk).await?;
        hasher.update(&chunk);
        *written = next;
        progress.record(length);
    }

    if *written == 0 || !(*written).is_multiple_of(TS_PACKET_SIZE) {
        return Err(HlsDownloadJobError::NotTransportStream {
            index: request.index,
        });
    }
    if let Some(expected) = expected
        && *written != expected
    {
        return Err(HlsDownloadJobError::LengthMismatch {
            index: request.index,
            expected,
            actual: *written,
        });
    }
    Ok(())
}

async fn write_assembled(
    segments: &[HlsSegmentRecord],
    directory: &Path,
    incomplete: &Path,
    cancellation: &CancellationToken,
) -> HlsDownloadJobResult<u64> {
    match fs::remove_file(incomplete).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(incomplete)
        .await?;
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; IO_CHUNK];
    for segment in segments {
        let (Some(length), Some(sha256)) = (segment.length, segment.sha256.as_deref()) else {
            return Err(HlsDownloadJobError::MissingPlan);
        };
        let mut input = fs::File::open(segment_path(directory, segment.index)).await?;
        let mut hasher = Sha256::new();
        let mut read_total = 0_u64;
        loop {
            let read = tokio::select! {
                () = cancellation.cancelled() => return Err(HlsDownloadJobError::Cancelled),
                read = input.read(&mut buffer) => read?,
            };
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            output.write_all(&buffer[..read]).await?;
            read_total = read_total
                .checked_add(
                    u64::try_from(read).map_err(|_| HlsDownloadJobError::ByteCountOverflow)?,
                )
                .ok_or(HlsDownloadJobError::ByteCountOverflow)?;
        }
        if read_total != length || hex_digest(hasher.finalize()) != sha256 {
            return Err(HlsDownloadJobError::ChecksumMismatch {
                index: segment.index,
            });
        }
        total = total
            .checked_add(length)
            .ok_or(HlsDownloadJobError::ByteCountOverflow)?;
    }
    output.flush().await?;
    output.sync_all().await?;
    Ok(total)
}

/// What a manual redirect walk is fetching; it decides the headers and expected status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestKind {
    /// Playlist body, capped and fully read.
    Playlist,
    /// Segment body, streamed straight to a temp file.
    Segment,
    /// Metadata-only probe of a retained segment.
    IdentityProbe,
}

async fn send_following_redirects(
    client: &Client,
    kind: RequestKind,
    url: &str,
    captured: &BTreeMap<String, String>,
    origin: &Origin,
    cancellation: &CancellationToken,
) -> HlsDownloadJobResult<Response> {
    let mut current = parse_http_url(url)?;
    for _ in 0..=MAX_REDIRECTS {
        let mut request: RequestBuilder = client
            .get(current.clone())
            .headers(header_map_for(&current, captured, origin)?);
        if kind == RequestKind::IdentityProbe {
            request = request.header(header::RANGE, "bytes=0-0");
        }
        if kind == RequestKind::Playlist {
            request = request.timeout(PLAYLIST_TIMEOUT);
        }
        let response = tokio::select! {
            () = cancellation.cancelled() => return Err(HlsDownloadJobError::Cancelled),
            response = request.send() => response?,
        };
        if !is_redirect(response.status()) {
            return Ok(response);
        }
        let location = header_string(response.headers(), header::LOCATION)
            .ok_or(HlsDownloadJobError::MissingRedirectLocation)?;
        let target = current
            .join(location.trim())
            .map_err(|_| HlsDownloadJobError::InvalidRedirect)?;
        current =
            parse_http_url(target.as_str()).map_err(|_| HlsDownloadJobError::InvalidRedirect)?;
    }
    Err(HlsDownloadJobError::TooManyRedirects)
}

/// Reads validators and the resource length from an identity probe response.
///
/// A `206` is expected here: `Content-Range` carries the full resource length. A
/// server that ignores the range answers `200` with the full `Content-Length`; the
/// body is dropped unread so the probe stays bounded.
fn remote_identity(response: Response) -> HlsDownloadJobResult<RemoteIdentity> {
    let status = response.status();
    let headers = response.headers();
    match status {
        StatusCode::PARTIAL_CONTENT => Ok(RemoteIdentity {
            length: content_range_total(headers),
            etag: header_string(headers, header::ETAG),
            last_modified: header_string(headers, header::LAST_MODIFIED),
        }),
        status if status.is_success() => Ok(RemoteIdentity {
            length: content_length(headers)?,
            etag: header_string(headers, header::ETAG),
            last_modified: header_string(headers, header::LAST_MODIFIED),
        }),
        status if status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS => {
            Err(HlsDownloadJobError::TransientStatus {
                status: status.as_u16(),
                retry_after: retry_after(headers),
            })
        }
        status => Err(HlsDownloadJobError::UnexpectedStatus(status.as_u16())),
    }
}

fn content_range_total(headers: &HeaderMap) -> Option<u64> {
    let value = header_string(headers, header::CONTENT_RANGE)?;
    value.rsplit_once('/')?.1.trim().parse().ok()
}

fn validate_success_status(response: &Response) -> HlsDownloadJobResult<()> {
    let status = response.status();
    if status == StatusCode::PARTIAL_CONTENT {
        return Err(HlsDownloadJobError::PartialResponse);
    }
    if status.is_success() {
        return Ok(());
    }
    if status.is_server_error()
        || status == StatusCode::TOO_MANY_REQUESTS
        || status == StatusCode::REQUEST_TIMEOUT
    {
        return Err(HlsDownloadJobError::TransientStatus {
            status: status.as_u16(),
            retry_after: retry_after(response.headers()),
        });
    }
    Err(HlsDownloadJobError::UnexpectedStatus(status.as_u16()))
}

fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?;
    let seconds: u64 = value.trim().parse().ok()?;
    Some(Duration::from_secs(seconds).min(MAX_RETRY_AFTER))
}

fn check_content_encoding(headers: &HeaderMap) -> HlsDownloadJobResult<()> {
    for value in headers.get_all(header::CONTENT_ENCODING) {
        let value = value.to_str().unwrap_or_default().trim();
        if !value.is_empty() && !value.eq_ignore_ascii_case("identity") {
            return Err(HlsDownloadJobError::ContentEncoding);
        }
    }
    Ok(())
}

fn content_length(headers: &HeaderMap) -> HlsDownloadJobResult<Option<u64>> {
    let Some(value) = headers.get(header::CONTENT_LENGTH) else {
        return Ok(None);
    };
    value
        .to_str()
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Some)
        .ok_or(HlsDownloadJobError::InvalidContentLength)
}

fn header_string(headers: &HeaderMap, name: HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Checks every 188-byte boundary that starts inside `chunk`, accounting for `offset`.
fn ts_sync_ok(offset: u64, chunk: &[u8]) -> bool {
    let remainder = offset % TS_PACKET_SIZE;
    let first = if remainder == 0 {
        0
    } else {
        usize::try_from(TS_PACKET_SIZE - remainder).unwrap_or(TS_PACKET_SIZE as usize)
    };
    (first..chunk.len())
        .step_by(TS_PACKET_SIZE as usize)
        .all(|position| chunk[position] == TS_SYNC_BYTE)
}

fn retry_backoff(attempt: u32) -> Duration {
    RETRY_BASE_DELAY.saturating_mul(1_u32 << attempt.min(5))
}

async fn acquire_cancellable(
    limiter: &SpeedLimiter,
    bytes: u64,
    cancellation: &CancellationToken,
) -> HlsDownloadJobResult<()> {
    tokio::select! {
        () = cancellation.cancelled() => Err(HlsDownloadJobError::Cancelled),
        () = limiter.acquire(bytes) => Ok(()),
    }
}

async fn sleep_cancellable(delay: Duration, cancellation: &CancellationToken) {
    tokio::select! {
        () = cancellation.cancelled() => {}
        () = tokio::time::sleep(delay) => {}
    }
}

async fn drain_workers(workers: &mut JoinSet<HlsDownloadJobResult<SegmentOutcome>>) {
    while workers.join_next().await.is_some() {}
}

async fn verify_file(
    path: &Path,
    length: u64,
    sha256: &str,
    cancellation: &CancellationToken,
) -> HlsDownloadJobResult<bool> {
    match fs::metadata(path).await {
        Ok(metadata) if metadata.len() == length => {}
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    }
    let mut file = fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; IO_CHUNK];
    let mut total = 0_u64;
    loop {
        let read = tokio::select! {
            () = cancellation.cancelled() => return Err(HlsDownloadJobError::Cancelled),
            read = file.read(&mut buffer) => read?,
        };
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total = total
            .checked_add(u64::try_from(read).map_err(|_| HlsDownloadJobError::ByteCountOverflow)?)
            .ok_or(HlsDownloadJobError::ByteCountOverflow)?;
    }
    Ok(total == length && hex_digest(hasher.finalize()) == sha256)
}

async fn remove_stale_temps(directory: &Path) -> HlsDownloadJobResult<()> {
    let mut entries = fs::read_dir(directory).await?;
    while let Some(entry) = entries.next_entry().await? {
        if entry
            .file_name()
            .to_string_lossy()
            .ends_with(SEGMENT_TEMP_SUFFIX)
        {
            let _ = fs::remove_file(entry.path()).await;
        }
    }
    Ok(())
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    use std::fmt::Write;

    let bytes = digest.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

/// Playlist rejections that mean "this rendition is out of scope", which may be
/// skipped for the next variant. Everything else (malformed playlists, unsafe
/// URLs) is a real error and must not be masked.
fn is_skippable_variant_error(error: &PlaylistError) -> bool {
    matches!(
        error,
        PlaylistError::Encrypted
            | PlaylistError::ByteRange
            | PlaylistError::FragmentedMp4
            | PlaylistError::Discontinuity
            | PlaylistError::MissingEndList
            | PlaylistError::UnsupportedTag(_)
            | PlaylistError::NoSegments
            | PlaylistError::TooManySegments { .. }
            | PlaylistError::NoUsableVariants
    )
}

fn plan_matches(plan: &MediaPlan, record: &HlsPlanRecord) -> bool {
    plan.segments.len() == record.segments.len()
        && plan
            .segments
            .iter()
            .zip(&record.segments)
            .all(|(fresh, stored)| {
                fresh.sequence == stored.sequence
                    && fresh.url == stored.url
                    && fresh.duration == stored.duration
            })
}

fn record_for(plan: &MediaPlan) -> HlsPlanRecord {
    HlsPlanRecord {
        playlist_url: plan.playlist_url.clone(),
        segments: plan
            .segments
            .iter()
            .enumerate()
            .filter_map(|(index, segment)| {
                Some(HlsSegmentRecord {
                    index: u32::try_from(index).ok()?,
                    sequence: segment.sequence,
                    url: segment.url.clone(),
                    duration: segment.duration,
                    length: None,
                    sha256: None,
                    etag: None,
                    last_modified: None,
                })
            })
            .collect(),
    }
}

fn segment_path(directory: &Path, index: u32) -> PathBuf {
    directory.join(format!("{index:08}.ts"))
}

fn segment_temp_path(directory: &Path, index: u32) -> PathBuf {
    directory.join(format!("{index:08}.ts{SEGMENT_TEMP_SUFFIX}"))
}

/// Temporary segment directory for one HLS download, derived only from app identifiers.
#[must_use]
pub(crate) fn segment_directory(item: &DownloadItem) -> PathBuf {
    item.folder.join(format!(".raijin-hls-{}", item.id.get()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Origin {
    scheme: String,
    host: String,
    port: Option<u16>,
}

impl Origin {
    fn of(url: &Url) -> Option<Self> {
        Some(Self {
            scheme: url.scheme().to_owned(),
            host: url.host_str()?.to_ascii_lowercase(),
            port: url.port_or_known_default(),
        })
    }
}

fn is_http_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
}

/// Parses an HTTP(S) resource URL, rejecting credentials and fragments the same
/// way the playlist parser does.
fn parse_http_url(value: &str) -> HlsDownloadJobResult<Url> {
    let url = Url::parse(value)
        .ok()
        .filter(is_http_url)
        .filter(|url| url.host_str().is_some())
        .ok_or(HlsDownloadJobError::InvalidUrl)?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(HlsDownloadJobError::EmbeddedCredentials);
    }
    if url.fragment().is_some() {
        return Err(HlsDownloadJobError::FragmentNotAllowed);
    }
    Ok(url)
}

/// Builds request headers, forwarding captured headers only for their original origin.
fn header_map_for(
    url: &Url,
    captured: &BTreeMap<String, String>,
    origin: &Origin,
) -> HlsDownloadJobResult<HeaderMap> {
    let mut map = HeaderMap::new();
    map.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static(crate::download::http::ACCEPT_ENCODING_IDENTITY),
    );
    if Origin::of(url).as_ref() != Some(origin) {
        return Ok(map);
    }
    for (name, value) in captured {
        if DROPPED_HEADERS
            .iter()
            .any(|dropped| name.eq_ignore_ascii_case(dropped))
        {
            continue;
        }
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| HlsDownloadJobError::InvalidHeader(name.clone()))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| HlsDownloadJobError::InvalidHeader(name.to_string()))?;
        map.insert(name, value);
    }
    Ok(map)
}

fn is_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

fn build_client() -> HlsDownloadJobResult<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static(crate::download::http::ACCEPT_ENCODING_IDENTITY),
    );
    Ok(Client::builder()
        .user_agent(DEFAULT_USER_AGENT)
        .default_headers(headers)
        .redirect(redirect::Policy::none())
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .build()?)
}

/// Applies the repo's `name(N).ext` collision convention; `0` keeps the requested name.
fn suffixed_path(requested: &Path, suffix: u32) -> PathBuf {
    if suffix == 0 {
        return requested.to_path_buf();
    }
    let parent = requested
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let stem = requested
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("video");
    let file_name = match requested.extension().and_then(|value| value.to_str()) {
        Some(extension) => format!("{stem}({suffix}).{extension}"),
        None => format!("{stem}({suffix})"),
    };
    parent.join(file_name)
}

fn now_ms() -> HlsDownloadJobResult<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| HlsDownloadJobError::ClockBeforeEpoch)?;
    i64::try_from(duration.as_millis()).map_err(|_| HlsDownloadJobError::ClockOutOfRange)
}

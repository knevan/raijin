use std::io::SeekFrom;
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::StatusCode;
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use thiserror::Error;
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::db::{DbError, DownloadRepository, PartRepository};
use crate::download::http::ACCEPT_ENCODING_IDENTITY;
use crate::download::part::split_part_at;
use crate::download::speed::SpeedLimiter;
use crate::download::{
    Bytes, DownloadFailure, DownloadItem, DownloadPart, DownloadStatus, FailureKind, PartId,
    PartStatus, ProbeRequest, ReqwestHttpClient, ResumeSupport, ResumeValidation, probe_http,
    split_fixed_ranges_with_min_part_size, validate_resume_metadata,
};

const DEFAULT_PROGRESS_INTERVAL: Duration = Duration::from_millis(500);
const SPLIT_REPLY_TIMEOUT: Duration = Duration::from_millis(25);
const SPLIT_RETRY_INTERVAL: Duration = Duration::from_millis(50);
const SINGLE_PART_INDEX: u32 = 0;

#[cfg(test)]
#[path = "job/connections_tests.rs"]
mod connections_tests;

/// Result type returned by single-connection HTTP jobs.
pub type HttpDownloadJobResult<T> = Result<T, HttpDownloadJobError>;

#[derive(Debug, Default)]
pub(crate) struct LiveDownloadProgress {
    downloaded_bytes: AtomicU64,
    active_part_count: AtomicU16,
}

impl LiveDownloadProgress {
    pub(crate) fn new(downloaded_bytes: Bytes) -> Self {
        Self {
            downloaded_bytes: AtomicU64::new(downloaded_bytes.get()),
            active_part_count: AtomicU16::new(0),
        }
    }

    pub(crate) fn snapshot(&self) -> (Bytes, u16) {
        (
            Bytes::new(self.downloaded_bytes.load(Ordering::Relaxed)),
            self.active_part_count.load(Ordering::Relaxed),
        )
    }

    pub(crate) fn clear_active_parts(&self) {
        self.active_part_count.store(0, Ordering::Relaxed);
    }

    fn set_downloaded(&self, downloaded_bytes: Bytes) {
        self.downloaded_bytes
            .store(downloaded_bytes.get(), Ordering::Relaxed);
    }

    fn add_downloaded(&self, bytes: u64) {
        let _ = self
            .downloaded_bytes
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_add(bytes))
            });
    }

    fn add_active_part(&self) {
        let _ =
            self.active_part_count
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    Some(current.saturating_add(1))
                });
    }

    fn remove_active_part(&self) {
        let _ =
            self.active_part_count
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    Some(current.saturating_sub(1))
                });
    }
}

#[derive(Debug)]
struct ActivePartGuard {
    live_progress: Arc<LiveDownloadProgress>,
}

impl ActivePartGuard {
    fn new(live_progress: Arc<LiveDownloadProgress>) -> Self {
        live_progress.add_active_part();
        Self { live_progress }
    }
}

impl Drop for ActivePartGuard {
    fn drop(&mut self) {
        self.live_progress.remove_active_part();
    }
}

/// Errors returned by single-connection HTTP jobs.
#[derive(Debug, Error)]
pub enum HttpDownloadJobError {
    /// Database operation failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// HTTP probe failed.
    #[error(transparent)]
    Probe(#[from] crate::download::HttpProbeError),
    /// HTTP request or body streaming failed.
    #[error(transparent)]
    Request(#[from] reqwest::Error),
    /// Filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// HTTP header name is invalid.
    #[error("invalid HTTP header name `{0}`")]
    InvalidHeaderName(String),
    /// HTTP header value is invalid.
    #[error("invalid HTTP header value for `{0}`")]
    InvalidHeaderValue(String),
    /// Remote server returned an unusable status.
    #[error("unexpected HTTP status `{0}`")]
    UnexpectedStatus(u16),
    /// Remote server ignored required resume range.
    #[error("remote server ignored byte range resume request")]
    RangeIgnored,
    /// Remote server cannot resume partial file.
    #[error("remote server does not support byte range resume")]
    ResumeUnsupported,
    /// System clock is earlier than Unix epoch.
    #[error("system clock is earlier than Unix epoch")]
    ClockBeforeEpoch,
    /// System clock value does not fit in database timestamp range.
    #[error("system clock timestamp is out of range")]
    ClockOutOfRange,
    /// Arithmetic overflow while tracking progress.
    #[error("download byte counter overflowed")]
    ByteCountOverflow,
    /// Range split failed.
    #[error(transparent)]
    RangeSplit(#[from] crate::download::RangeSplitError),
    /// Worker task failed to join.
    #[error("part worker task failed: {0}")]
    WorkerJoin(#[from] tokio::task::JoinError),
    /// Ranged worker received invalid content range.
    #[error("part `{index}` received invalid content range `{actual}`")]
    InvalidPartContentRange { index: u32, actual: String },
    /// Destination file already exists and could not be disambiguated.
    #[error("destination file conflict for `{0}`")]
    DestinationConflict(String),
    /// A ranged response ended before its current exclusive boundary.
    #[error("part `{index}` ended prematurely: expected offset {expected}, reached {actual}")]
    PrematureEof {
        index: u32,
        expected: u64,
        actual: u64,
    },
    /// Retry policy was exhausted.
    #[error("too many retries: {0}")]
    TooManyRetries(String),
}

impl HttpDownloadJobError {
    /// Converts the job error into persisted failure details.
    #[must_use]
    pub fn to_failure(&self) -> DownloadFailure {
        DownloadFailure {
            kind: failure_kind(self),
            message: self.to_string(),
        }
    }

    /// Returns true when the error should pause for manual validation/recovery instead of retrying.
    #[must_use]
    pub fn requires_manual_action(&self) -> bool {
        match self {
            Self::Probe(error) => probe_error_requires_manual_action(error),
            Self::ResumeUnsupported | Self::RangeIgnored | Self::InvalidPartContentRange { .. } => {
                true
            }
            _ => false,
        }
    }
}

/// Single-connection HTTP job that streams one persisted download into an incomplete file.
#[derive(Debug, Clone)]
pub struct HttpDownloadJob {
    download_repo: DownloadRepository,
    part_repo: PartRepository,
    client: ReqwestHttpClient,
    item: DownloadItem,
    config: HttpDownloadJobConfig,
    live_progress: Option<Arc<LiveDownloadProgress>>,
    /// Shared global throughput limiter; `None` keeps the zero-cost path.
    global_limiter: Option<Arc<SpeedLimiter>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HttpDownloadJobConfig {
    incomplete_extension: String,
    progress_interval: Duration,
    max_retries: u32,
    min_part_size: Bytes,
    adaptive_connections_enabled: bool,
    default_connections: u16,
    min_connections: u16,
    max_connections: u16,
    connection_probe_interval: Duration,
    connection_gain_threshold: u8,
}

impl Default for HttpDownloadJobConfig {
    fn default() -> Self {
        let download_config = crate::download::DownloadConfig::default();
        Self {
            incomplete_extension: download_config.incomplete_extension,
            progress_interval: DEFAULT_PROGRESS_INTERVAL,
            max_retries: download_config.max_retries.get(),
            min_part_size: download_config.min_part_size,
            adaptive_connections_enabled: download_config.adaptive_connections_enabled,
            default_connections: download_config.default_connections.get(),
            min_connections: download_config.min_connections.get(),
            max_connections: download_config.max_connections.get(),
            connection_probe_interval: Duration::from_millis(
                download_config.connection_probe_interval_ms.max(1),
            ),
            connection_gain_threshold: download_config.connection_gain_threshold,
        }
    }
}

impl HttpDownloadJob {
    /// Creates a job using default filesystem/progress settings.
    #[must_use]
    pub fn new(
        download_repo: DownloadRepository,
        part_repo: PartRepository,
        client: ReqwestHttpClient,
        item: DownloadItem,
    ) -> Self {
        Self {
            download_repo,
            part_repo,
            client,
            item,
            config: HttpDownloadJobConfig::default(),
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
    /// # Errors
    ///
    /// Returns an error when HTTP, validation, persistence, or disk operations fail.
    pub async fn run(
        mut self,
        cancellation: CancellationToken,
    ) -> HttpDownloadJobResult<DownloadItem> {
        let mut attempts = 0_u32;
        let result = loop {
            let result = self.run_inner(&cancellation).await;
            match result {
                Err(error)
                    if !cancellation.is_cancelled()
                        && !error.requires_manual_action()
                        && attempts < self.config.max_retries =>
                {
                    attempts += 1;
                    tracing::debug!(
                        download_id = self.item.id.get(),
                        attempts,
                        ?error,
                        "retrying failed download job"
                    );
                    sleep_retry_delay(attempts, &cancellation).await?;
                }
                result => break result,
            }
        };
        match result {
            Ok(item) => Ok(item),
            Err(_) if cancellation.is_cancelled() => {
                self.persist_paused().await?;
                Ok(self.item)
            }
            Err(error) if error.requires_manual_action() => {
                self.persist_paused_with_failure(&error).await?;
                Ok(self.item)
            }
            Err(error) => {
                self.persist_error(&error).await?;
                Err(error)
            }
        }
    }

    async fn run_inner(
        &mut self,
        cancellation: &CancellationToken,
    ) -> HttpDownloadJobResult<DownloadItem> {
        let mut final_path = self.final_path();
        let incomplete_path = self.incomplete_path();
        // Fail fast on unwritable destinations before spending a network probe;
        // errors persist as `Disk` failures visible in the monitor projection.
        check_destination_writable(&self.item.folder, &incomplete_path).await?;
        if existing_len(&incomplete_path).await? == 0 {
            final_path = self.reserve_unique_destination(&final_path).await?;
        }

        let resume_from = existing_len(&incomplete_path).await?;
        let metadata = self.probe(resume_from).await?;
        self.item.total_bytes = metadata.total_bytes.or(self.item.total_bytes);
        self.item.etag = metadata.etag.clone().or_else(|| self.item.etag.clone());
        self.item.last_modified = metadata
            .last_modified
            .clone()
            .or_else(|| self.item.last_modified.clone());

        let existing_parts = self.part_repo.list_for_download(self.item.id).await?;
        if self.should_use_ranged(&metadata, &existing_parts) {
            return self
                .run_ranged(cancellation, &incomplete_path, &final_path, existing_parts)
                .await;
        }

        if resume_from > 0 && metadata.resume_support != ResumeSupport::Supported {
            return Err(HttpDownloadJobError::ResumeUnsupported);
        }

        let range_start = (resume_from > 0).then_some(resume_from);
        let mut response = self.request_body(range_start).await?;
        validate_download_status(response.status(), range_start)?;

        let mut part = self.part_for_resume(resume_from).await?;
        self.persist_started(&mut part, resume_from).await?;
        if let Some(live_progress) = &self.live_progress {
            live_progress.set_downloaded(Bytes::new(resume_from));
            live_progress.add_active_part();
        }

        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(range_start.is_none())
            .open(&incomplete_path)
            .await?;
        if range_start.is_some() {
            file.seek(SeekFrom::Start(resume_from)).await?;
        }

        let mut downloaded = resume_from;
        let mut last_persisted = tokio::time::Instant::now();
        let speed_limiter = self.item.speed_limit.and_then(SpeedLimiter::new);
        let global_limiter = self.global_limiter.clone();
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => {
                    self.persist_progress(&mut part, downloaded, DownloadStatus::Paused, PartStatus::Canceled).await?;
                    return Ok(self.item.clone());
                }
                chunk = response.chunk() => {
                    let Some(chunk) = chunk? else {
                        break;
                    };
                    let chunk_len = u64::try_from(chunk.len()).map_err(|_| HttpDownloadJobError::ByteCountOverflow)?;
                    if let Some(limiter) = &speed_limiter {
                        limiter.acquire(chunk_len).await;
                    }
                    if let Some(limiter) = &global_limiter {
                        limiter.acquire(chunk_len).await;
                    }
                    file.write_all(&chunk).await?;
                    downloaded = downloaded
                        .checked_add(chunk_len)
                        .ok_or(HttpDownloadJobError::ByteCountOverflow)?;
                    self.item.downloaded_bytes = Bytes::new(downloaded);
                    part.current_byte = Bytes::new(downloaded);
                    if let Some(live_progress) = &self.live_progress {
                        live_progress.set_downloaded(Bytes::new(downloaded));
                    }
                    if last_persisted.elapsed() >= self.config.progress_interval {
                        self.persist_progress(&mut part, downloaded, DownloadStatus::Downloading, PartStatus::Receiving).await?;
                        last_persisted = tokio::time::Instant::now();
                    }
                }
            }
        }

        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        self.persist_progress(
            &mut part,
            downloaded,
            DownloadStatus::PreparingFile,
            PartStatus::Receiving,
        )
        .await?;

        if let Some(total) = self.item.total_bytes
            && downloaded != total.get()
        {
            return Err(HttpDownloadJobError::UnexpectedStatus(
                response.status().as_u16(),
            ));
        }

        self.finalize_destination(&incomplete_path, &final_path)
            .await?;
        self.persist_completed(&mut part, downloaded).await?;
        Ok(self.item.clone())
    }

    async fn run_ranged(
        &mut self,
        cancellation: &CancellationToken,
        incomplete_path: &PathBuf,
        final_path: &PathBuf,
        existing_parts: Vec<DownloadPart>,
    ) -> HttpDownloadJobResult<DownloadItem> {
        let total = self
            .item
            .total_bytes
            .ok_or(HttpDownloadJobError::ResumeUnsupported)?;
        let mut connection_controller = ConnectionController::new(
            &self.config,
            self.item.preferred_connections,
            total.get(),
            downloaded_from_parts(&existing_parts),
        );
        let mut connection_target = connection_controller.target();
        let mut retries_since_probe = 0_u32;
        let mut workers_changed_since_probe = false;
        let mut workers_finished_since_probe = false;
        let mut parts = if existing_parts.is_empty() || existing_parts.len() == 1 {
            let desired_parts = u16::try_from(connection_target).unwrap_or(u16::MAX);
            split_fixed_ranges_with_min_part_size(
                self.item.id,
                Some(total),
                desired_parts,
                self.config.min_part_size,
                now_ms()?,
            )?
        } else {
            existing_parts
        };
        self.part_repo
            .set_for_download(self.item.id, &parts)
            .await?;
        if self.live_progress.is_none() {
            self.live_progress = Some(Arc::new(LiveDownloadProgress::new(Bytes::new(
                downloaded_from_parts(&parts),
            ))));
        }
        if let Some(live_progress) = &self.live_progress {
            live_progress.set_downloaded(Bytes::new(downloaded_from_parts(&parts)));
        }

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(incomplete_path)
            .await?;
        file.set_len(total.get()).await?;
        file.sync_all().await?;
        drop(file);
        self.persist_ranged_started(&parts).await?;

        let mut next_part_index = next_part_index(&parts);
        let mut pending = parts
            .drain(..)
            .filter(|part| !part_is_complete(part))
            .collect::<std::collections::VecDeque<_>>();
        let mut workers = JoinSet::new();
        let mut worker_controls = std::collections::HashMap::new();
        let mut connection_probe = tokio::time::interval(self.config.connection_probe_interval);
        connection_probe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        connection_probe.tick().await;
        let speed_limiter = self.item.speed_limit.and_then(SpeedLimiter::new);
        let global_limiter = self.global_limiter.clone();
        let mut first_error = None;

        loop {
            while workers.len().saturating_add(pending.len()) < connection_target {
                if !split_pending_part(
                    &mut pending,
                    &self.part_repo,
                    &mut next_part_index,
                    self.config.min_part_size,
                    now_ms()?,
                )
                .await?
                {
                    break;
                }
            }

            while workers.len() < connection_target {
                if let Some(part) = pending.pop_front() {
                    let part_index = part.index;
                    let (control_tx, control_rx) = mpsc::channel(1);
                    let active_state = Arc::new(ActivePartState::new(&part));
                    worker_controls.insert(
                        part_index,
                        ActivePartControl {
                            tx: control_tx,
                            state: Arc::clone(&active_state),
                        },
                    );
                    let request = PartWorkerRequest {
                        part_repo: self.part_repo.clone(),
                        client: self.client.clone(),
                        url: self.item.url.clone(),
                        headers: self.item.headers.clone(),
                        path: incomplete_path.clone(),
                        part,
                        cancellation: cancellation.clone(),
                        progress_interval: self.config.progress_interval,
                        speed_limiter: speed_limiter.clone(),
                        global_limiter: global_limiter.clone(),
                        live_progress: self.live_progress.clone(),
                        control_rx,
                        active_state,
                    };
                    workers_changed_since_probe = true;
                    workers.spawn(async move {
                        let outcome = part_worker(request).await;
                        (part_index, outcome)
                    });
                    continue;
                }

                if workers.len() < connection_target {
                    let child = request_inflight_split(
                        &worker_controls,
                        &self.part_repo,
                        &mut next_part_index,
                        self.config.min_part_size,
                        now_ms()?,
                        cancellation,
                    )
                    .await?;
                    if let Some(child) = child {
                        pending.push_back(child);
                        continue;
                    }
                }

                break;
            }

            if workers.is_empty() {
                break;
            }

            let should_wait_for_split = workers.len() < connection_target
                && pending.is_empty()
                && !worker_controls.is_empty();
            let joined = tokio::select! {
                _ = connection_probe.tick(), if connection_controller.is_adaptive() => {
                    let (downloaded, active_part_count) = self
                        .live_progress
                        .as_ref()
                        .map(|progress| progress.snapshot())
                        .unwrap_or((Bytes::ZERO, u16::try_from(workers.len()).unwrap_or(u16::MAX)));
                    connection_target = connection_controller.observe(ConnectionSample {
                        now: Instant::now(),
                        locally_limited: speed_limiter.is_some() || global_limiter.is_some(),
                        workers_changed: workers_changed_since_probe,
                        workers_finished: workers_finished_since_probe,
                        downloaded_bytes: downloaded.get(),
                        active_part_count,
                        retry_count: retries_since_probe,
                        remaining_bytes: total.get().saturating_sub(downloaded.get()),
                    });
                    retries_since_probe = 0;
                    workers_changed_since_probe = false;
                    workers_finished_since_probe = false;
                    continue;
                }
                () = tokio::time::sleep(SPLIT_RETRY_INTERVAL), if should_wait_for_split => continue,
                joined = workers.join_next() => joined,
            };
            let (part_index, outcome) = joined.ok_or(HttpDownloadJobError::ResumeUnsupported)??;
            let outcome = outcome?;
            worker_controls.remove(&part_index);
            workers_changed_since_probe = true;
            workers_finished_since_probe = true;
            match outcome {
                PartWorkerOutcome::Completed => {}
                PartWorkerOutcome::Canceled => {
                    workers.abort_all();
                    self.persist_ranged_paused().await?;
                    return Ok(self.item.clone());
                }
                PartWorkerOutcome::Failed(mut part, error) => {
                    if error.requires_manual_action() {
                        first_error = Some(error);
                        workers.abort_all();
                        break;
                    } else if part.retry_count < self.config.max_retries {
                        part.retry_count += 1;
                        retries_since_probe = retries_since_probe.saturating_add(1);
                        part.status = PartStatus::Idle;
                        part.updated_at = now_ms()?;
                        self.part_repo.set(&part).await?;
                        pending.push_back(part);
                    } else {
                        part.status = PartStatus::Error;
                        part.updated_at = now_ms()?;
                        self.part_repo.set(&part).await?;
                        first_error = Some(HttpDownloadJobError::TooManyRetries(format!(
                            "part `{}` failed after {} retries: {error}",
                            part.index, self.config.max_retries
                        )));
                        workers.abort_all();
                        break;
                    }
                }
            }
        }

        if let Some(error) = first_error {
            self.persist_ranged_paused_with_failure(&error).await?;
            return Ok(self.item.clone());
        }
        if cancellation.is_cancelled() {
            self.persist_ranged_paused().await?;
            return Ok(self.item.clone());
        }

        let parts = self.part_repo.list_for_download(self.item.id).await?;
        if !parts.iter().all(part_is_complete) {
            self.persist_ranged_paused().await?;
            return Ok(self.item.clone());
        }

        let mut file = OpenOptions::new().write(true).open(incomplete_path).await?;
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        self.persist_preparing(total.get()).await?;
        self.finalize_destination(incomplete_path, final_path)
            .await?;
        self.persist_ranged_completed(total.get()).await?;
        Ok(self.item.clone())
    }

    fn should_use_ranged(
        &self,
        metadata: &crate::download::HttpMetadata,
        existing_parts: &[DownloadPart],
    ) -> bool {
        existing_parts.len() > 1
            || (metadata.resume_support == ResumeSupport::Supported
                && metadata.total_bytes.is_some_and(|bytes| bytes.get() > 0)
                && self.preferred_connections().get() > 1)
    }

    fn preferred_connections(&self) -> NonZeroU16 {
        self.item
            .preferred_connections
            .unwrap_or(crate::download::DownloadConfig::default().default_connections)
    }

    async fn probe(
        &self,
        resume_from: u64,
    ) -> HttpDownloadJobResult<crate::download::HttpMetadata> {
        let metadata = probe_http(
            &self.client,
            ProbeRequest {
                url: self.item.url.clone(),
                headers: self.item.headers.clone(),
                file_name: Some(self.item.file_name.clone()),
            },
        )
        .await?;

        if resume_from > 0 {
            validate_resume_metadata(
                &ResumeValidation {
                    total_bytes: self.item.total_bytes,
                    etag: self.item.etag.clone(),
                    resume_support: ResumeSupport::Supported,
                },
                &metadata,
            )?;
        }

        Ok(metadata)
    }

    async fn request_body(
        &self,
        range_start: Option<u64>,
    ) -> HttpDownloadJobResult<reqwest::Response> {
        let mut request = self
            .client
            .client()
            .get(&self.item.url)
            .headers(request_headers(&self.item.headers)?);
        if let Some(start) = range_start {
            request = request.header(header::RANGE, format!("bytes={start}-"));
        }
        Ok(request.send().await?)
    }

    async fn part_for_resume(&self, resume_from: u64) -> HttpDownloadJobResult<DownloadPart> {
        let mut parts = self.part_repo.list_for_download(self.item.id).await?;
        if let Some(part) = parts.pop() {
            return Ok(DownloadPart {
                current_byte: Bytes::new(resume_from),
                ..part
            });
        }

        Ok(DownloadPart {
            id: PartId::new(self.item.id.get()),
            download_id: self.item.id,
            index: SINGLE_PART_INDEX,
            start_byte: Bytes::ZERO,
            end_byte: self
                .item
                .total_bytes
                .and_then(|bytes| bytes.get().checked_sub(1).map(Bytes::new)),
            current_byte: Bytes::new(resume_from),
            status: PartStatus::Idle,
            retry_count: 0,
            updated_at: now_ms()?,
        })
    }

    async fn persist_started(
        &mut self,
        part: &mut DownloadPart,
        downloaded: u64,
    ) -> HttpDownloadJobResult<()> {
        let now = now_ms()?;
        self.item.status = DownloadStatus::Downloading;
        self.item.downloaded_bytes = Bytes::new(downloaded);
        self.item.started_at = self.item.started_at.or(Some(now));
        self.item.completed_at = None;
        self.item.failure = None;
        self.item.updated_at = now;
        part.status = PartStatus::Receiving;
        part.current_byte = Bytes::new(downloaded);
        part.updated_at = now;
        self.download_repo.update(&self.item).await?;
        self.part_repo.set(part).await?;
        Ok(())
    }

    async fn persist_progress(
        &mut self,
        part: &mut DownloadPart,
        downloaded: u64,
        status: DownloadStatus,
        part_status: PartStatus,
    ) -> HttpDownloadJobResult<()> {
        let now = now_ms()?;
        self.item.status = status;
        self.item.downloaded_bytes = Bytes::new(downloaded);
        self.item.updated_at = now;
        part.status = part_status;
        part.current_byte = Bytes::new(downloaded);
        part.updated_at = now;
        self.download_repo.update(&self.item).await?;
        self.part_repo.set(part).await?;
        Ok(())
    }

    async fn persist_paused(&mut self) -> HttpDownloadJobResult<()> {
        let now = now_ms()?;
        self.item.status = DownloadStatus::Paused;
        self.item.updated_at = now;
        self.download_repo.update(&self.item).await?;
        Ok(())
    }

    async fn persist_paused_with_failure(
        &mut self,
        error: &HttpDownloadJobError,
    ) -> HttpDownloadJobResult<()> {
        let now = now_ms()?;
        self.item.status = DownloadStatus::Paused;
        self.item.failure = Some(error.to_failure());
        self.item.updated_at = now;
        self.download_repo.update(&self.item).await?;
        Ok(())
    }

    async fn persist_completed(
        &mut self,
        part: &mut DownloadPart,
        downloaded: u64,
    ) -> HttpDownloadJobResult<()> {
        let now = now_ms()?;
        self.item.status = DownloadStatus::Completed;
        self.item.downloaded_bytes = Bytes::new(downloaded);
        self.item.completed_at = Some(now);
        self.item.failure = None;
        self.item.updated_at = now;
        part.status = PartStatus::Completed;
        part.current_byte = Bytes::new(downloaded);
        part.updated_at = now;
        self.download_repo.update(&self.item).await?;
        self.part_repo.set(part).await?;
        Ok(())
    }

    async fn persist_ranged_started(
        &mut self,
        parts: &[DownloadPart],
    ) -> HttpDownloadJobResult<()> {
        let now = now_ms()?;
        self.item.status = DownloadStatus::Downloading;
        self.item.downloaded_bytes = Bytes::new(downloaded_from_parts(parts));
        self.item.started_at = self.item.started_at.or(Some(now));
        self.item.completed_at = None;
        self.item.failure = None;
        self.item.updated_at = now;
        self.download_repo.update(&self.item).await?;
        Ok(())
    }

    async fn persist_ranged_paused(&mut self) -> HttpDownloadJobResult<()> {
        let parts = self.part_repo.list_for_download(self.item.id).await?;
        let now = now_ms()?;
        self.item.status = DownloadStatus::Paused;
        self.item.downloaded_bytes = Bytes::new(downloaded_from_parts(&parts));
        self.item.updated_at = now;
        self.download_repo.update(&self.item).await?;
        Ok(())
    }

    async fn persist_ranged_paused_with_failure(
        &mut self,
        error: &HttpDownloadJobError,
    ) -> HttpDownloadJobResult<()> {
        let parts = self.part_repo.list_for_download(self.item.id).await?;
        let now = now_ms()?;
        self.item.status = DownloadStatus::Paused;
        self.item.downloaded_bytes = Bytes::new(downloaded_from_parts(&parts));
        self.item.failure = Some(DownloadFailure {
            kind: failure_kind(error),
            message: error.to_string(),
        });
        self.item.updated_at = now;
        self.download_repo.update(&self.item).await?;
        Ok(())
    }

    async fn persist_preparing(&mut self, downloaded: u64) -> HttpDownloadJobResult<()> {
        let now = now_ms()?;
        self.item.status = DownloadStatus::PreparingFile;
        self.item.downloaded_bytes = Bytes::new(downloaded);
        self.item.updated_at = now;
        self.download_repo.update(&self.item).await?;
        Ok(())
    }

    async fn persist_ranged_completed(&mut self, downloaded: u64) -> HttpDownloadJobResult<()> {
        let now = now_ms()?;
        self.item.status = DownloadStatus::Completed;
        self.item.downloaded_bytes = Bytes::new(downloaded);
        self.item.completed_at = Some(now);
        self.item.failure = None;
        self.item.updated_at = now;
        self.download_repo.update(&self.item).await?;
        Ok(())
    }

    async fn persist_error(&mut self, error: &HttpDownloadJobError) -> HttpDownloadJobResult<()> {
        let now = now_ms()?;
        self.item.status = DownloadStatus::Error;
        self.item.failure = Some(DownloadFailure {
            kind: failure_kind(error),
            message: error.to_string(),
        });
        self.item.updated_at = now;
        self.download_repo.update(&self.item).await?;
        Ok(())
    }

    fn final_path(&self) -> PathBuf {
        self.item.folder.join(&self.item.file_name)
    }

    async fn reserve_unique_destination(
        &mut self,
        requested: &PathBuf,
    ) -> HttpDownloadJobResult<PathBuf> {
        let unique = unique_destination_path(requested, &self.config.incomplete_extension).await?;
        if unique != *requested {
            if let Some(file_name) = unique.file_name().and_then(|name| name.to_str()) {
                self.item.file_name = file_name.to_owned();
                self.item.updated_at = now_ms()?;
                self.download_repo.update(&self.item).await?;
            } else {
                return Err(HttpDownloadJobError::DestinationConflict(
                    unique.display().to_string(),
                ));
            }
        }
        Ok(unique)
    }

    async fn finalize_destination(
        &mut self,
        incomplete_path: &PathBuf,
        final_path: &PathBuf,
    ) -> HttpDownloadJobResult<()> {
        let final_path = self.reserve_unique_final_destination(final_path).await?;
        match fs::rename(incomplete_path, &final_path).await {
            Ok(()) => Ok(()),
            Err(error) if is_cross_device_error(&error) => {
                fs::copy(incomplete_path, &final_path).await?;
                fs::remove_file(incomplete_path).await?;
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn reserve_unique_final_destination(
        &mut self,
        requested: &PathBuf,
    ) -> HttpDownloadJobResult<PathBuf> {
        let unique = unique_final_destination_path(requested).await?;
        if unique != *requested {
            if let Some(file_name) = unique.file_name().and_then(|name| name.to_str()) {
                self.item.file_name = file_name.to_owned();
                self.item.updated_at = now_ms()?;
                self.download_repo.update(&self.item).await?;
            } else {
                return Err(HttpDownloadJobError::DestinationConflict(
                    unique.display().to_string(),
                ));
            }
        }
        Ok(unique)
    }

    fn incomplete_path(&self) -> PathBuf {
        self.item.folder.join(format!(
            "{}{}",
            self.item.file_name, self.config.incomplete_extension
        ))
    }
}

fn request_headers(
    headers: &std::collections::BTreeMap<String, String>,
) -> HttpDownloadJobResult<HeaderMap> {
    let mut map = HeaderMap::new();
    for (name, value) in headers {
        if name.eq_ignore_ascii_case(header::HOST.as_str()) {
            continue;
        }
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| HttpDownloadJobError::InvalidHeaderName(name.clone()))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| HttpDownloadJobError::InvalidHeaderValue(name.to_string()))?;
        map.insert(name, value);
    }
    map.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static(ACCEPT_ENCODING_IDENTITY),
    );
    Ok(map)
}

fn validate_download_status(
    status: StatusCode,
    range_start: Option<u64>,
) -> HttpDownloadJobResult<()> {
    match (range_start, status) {
        (Some(_), StatusCode::PARTIAL_CONTENT) => Ok(()),
        (Some(_), StatusCode::OK) => Err(HttpDownloadJobError::RangeIgnored),
        (None, status) if status.is_success() => Ok(()),
        (_, status) => Err(HttpDownloadJobError::UnexpectedStatus(status.as_u16())),
    }
}

fn failure_kind(error: &HttpDownloadJobError) -> FailureKind {
    match error {
        HttpDownloadJobError::Db(_) => FailureKind::Disk,
        HttpDownloadJobError::Probe(_)
        | HttpDownloadJobError::ResumeUnsupported
        | HttpDownloadJobError::RangeIgnored => FailureKind::Validation,
        HttpDownloadJobError::Request(_)
        | HttpDownloadJobError::UnexpectedStatus(_)
        | HttpDownloadJobError::PrematureEof { .. } => FailureKind::Network,
        // Full disks surface as `ErrorKind::StorageFull`; other filesystem
        // failures (permissions, missing media) stay under `Disk`.
        HttpDownloadJobError::Io(error) if error.kind() == std::io::ErrorKind::StorageFull => {
            FailureKind::NoSpace
        }
        HttpDownloadJobError::Io(_) => FailureKind::Disk,
        HttpDownloadJobError::InvalidHeaderName(_)
        | HttpDownloadJobError::InvalidHeaderValue(_) => FailureKind::Validation,
        HttpDownloadJobError::ClockBeforeEpoch
        | HttpDownloadJobError::ClockOutOfRange
        | HttpDownloadJobError::ByteCountOverflow
        | HttpDownloadJobError::RangeSplit(_)
        | HttpDownloadJobError::WorkerJoin(_) => FailureKind::Disk,
        HttpDownloadJobError::InvalidPartContentRange { .. } => FailureKind::Validation,
        HttpDownloadJobError::DestinationConflict(_) => FailureKind::Disk,
        HttpDownloadJobError::TooManyRetries(_) => FailureKind::TooManyRetries,
    }
}

async fn sleep_retry_delay(
    attempts: u32,
    cancellation: &CancellationToken,
) -> HttpDownloadJobResult<()> {
    let capped = attempts.min(5);
    let delay = Duration::from_millis(100 * 2_u64.pow(capped));
    tokio::select! {
        _ = cancellation.cancelled() => Ok(()),
        () = tokio::time::sleep(delay) => Ok(()),
    }
}

fn next_part_index(parts: &[DownloadPart]) -> u32 {
    parts
        .iter()
        .map(|part| part.index)
        .max()
        .unwrap_or(0)
        .saturating_add(1)
}

async fn request_inflight_split(
    worker_controls: &std::collections::HashMap<u32, ActivePartControl>,
    part_repo: &PartRepository,
    next_part_index: &mut u32,
    min_part_size: Bytes,
    now_ms: i64,
    cancellation: &CancellationToken,
) -> HttpDownloadJobResult<Option<DownloadPart>> {
    let mut candidates = worker_controls.values().collect::<Vec<_>>();
    candidates.sort_by_key(|control| std::cmp::Reverse(control.state.snapshot().remaining));

    for control in candidates {
        if control.state.snapshot().remaining < min_part_size.get().max(1).saturating_mul(2) {
            continue;
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        if control
            .tx
            .try_send(PartWorkerControl::RequestSplit {
                child_index: *next_part_index,
                min_part_size,
                now_ms,
                reply_tx,
            })
            .is_err()
        {
            continue;
        }

        let proposal = tokio::select! {
            _ = cancellation.cancelled() => return Ok(None),
            () = tokio::time::sleep(SPLIT_REPLY_TIMEOUT) => continue,
            proposal = reply_rx => proposal,
        };
        let PartSplitResponse::Proposed {
            parent,
            child,
            commit_tx,
        } = (match proposal {
            Ok(response) => response,
            Err(_) => continue,
        })
        else {
            continue;
        };

        if let Err(error) = part_repo.set_many(&[parent.clone(), child.clone()]).await {
            let _ = commit_tx.send(false);
            return Err(error.into());
        }
        *next_part_index = next_part_index.saturating_add(1);

        if commit_tx.send(true).is_err() {
            tracing::warn!(
                part_index = parent.index,
                child_index = child.index,
                "in-flight split committed but worker ack failed; child left persisted for resume"
            );
            return Ok(None);
        }
        return Ok(Some(child));
    }

    Ok(None)
}

async fn split_pending_part(
    pending: &mut std::collections::VecDeque<DownloadPart>,
    part_repo: &PartRepository,
    next_part_index: &mut u32,
    min_part_size: Bytes,
    now_ms: i64,
) -> HttpDownloadJobResult<bool> {
    let Some((candidate_index, split_start)) = pending
        .iter()
        .enumerate()
        .filter_map(|(index, part)| {
            let end = part.end_byte?;
            let remaining = end
                .get()
                .checked_add(1)?
                .checked_sub(part.current_byte.get())?;
            let split_start =
                valid_inflight_split_start(part.current_byte, end.get(), min_part_size)?;
            Some((index, remaining, split_start))
        })
        .max_by_key(|(_, remaining, _)| *remaining)
        .map(|(index, _, split_start)| (index, split_start))
    else {
        return Ok(false);
    };

    let Some(part) = pending.get(candidate_index).cloned() else {
        return Ok(false);
    };
    let (parent, child) = split_part_at(&part, *next_part_index, split_start, now_ms)?;
    part_repo.set_many(&[parent.clone(), child.clone()]).await?;
    *next_part_index = next_part_index.saturating_add(1);
    if let Some(slot) = pending.get_mut(candidate_index) {
        *slot = parent;
    }
    pending.push_back(child);
    Ok(true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionMode {
    Fixed,
    Adaptive,
}

#[derive(Debug, Clone, Copy)]
struct ConnectionSample {
    now: Instant,
    locally_limited: bool,
    workers_changed: bool,
    workers_finished: bool,
    downloaded_bytes: u64,
    active_part_count: u16,
    retry_count: u32,
    remaining_bytes: u64,
}

#[derive(Debug, Clone)]
struct ConnectionController {
    mode: ConnectionMode,
    min: usize,
    max: usize,
    min_part_size: u64,
    gain_threshold: u8,
    target: usize,
    interval: Duration,
    meter: ThroughputMeter,
    phase: ConnectionPhase,
}

impl ConnectionController {
    fn new(
        config: &HttpDownloadJobConfig,
        preferred_connections: Option<NonZeroU16>,
        total_bytes: u64,
        downloaded_bytes: u64,
    ) -> Self {
        let max = usize::from(config.max_connections.max(1));
        let min = usize::from(config.min_connections.max(1)).min(max);
        let remaining = total_bytes.saturating_sub(downloaded_bytes);
        let mode = if preferred_connections.is_some() || !config.adaptive_connections_enabled {
            ConnectionMode::Fixed
        } else {
            ConnectionMode::Adaptive
        };
        let requested = preferred_connections
            .map(NonZeroU16::get)
            .unwrap_or(config.default_connections);
        let target = Self::cap_by_remaining(
            usize::from(requested).clamp(min, max),
            min,
            config.min_part_size.get(),
            remaining,
        );

        Self {
            mode,
            min,
            max,
            min_part_size: config.min_part_size.get(),
            gain_threshold: config.connection_gain_threshold,
            target,
            interval: config
                .connection_probe_interval
                .max(Duration::from_millis(1)),
            meter: ThroughputMeter::default(),
            phase: ConnectionPhase::Baseline { active: 0 },
        }
    }

    fn is_adaptive(&self) -> bool {
        self.mode == ConnectionMode::Adaptive
    }

    fn target(&self) -> usize {
        self.target
    }

    fn observe(&mut self, sample: ConnectionSample) -> usize {
        if !self.is_adaptive() {
            return self.target;
        }

        let remaining_cap = Self::cap_by_remaining(
            self.max,
            self.min,
            self.min_part_size,
            sample.remaining_bytes,
        );
        let previous_target = self.target;
        self.target = self.target.min(remaining_cap);
        if sample.retry_count > 0 {
            self.target = self.target.saturating_sub(1).max(self.min);
        }
        if self.target != previous_target || sample.retry_count > 0 || sample.locally_limited {
            self.cooldown(sample, "bounds, retry, or local limiter");
            return self.target;
        }

        let active = usize::from(sample.active_part_count);
        match self.phase {
            ConnectionPhase::Baseline {
                active: baseline_active,
            } => {
                if active > self.target || active != baseline_active || sample.workers_changed {
                    self.meter.reset(sample.now, sample.downloaded_bytes);
                    self.phase = ConnectionPhase::Baseline { active };
                    return self.target;
                }
                let Some(rate) =
                    self.meter
                        .observe(sample.now, sample.downloaded_bytes, self.interval)
                else {
                    return self.target;
                };
                if self
                    .meter
                    .is_ready(sample.now, self.interval.saturating_mul(2))
                {
                    if active < self.target {
                        self.target = active.max(self.min);
                        self.cooldown(sample, "unfilled worker slots");
                        return self.target;
                    }
                    if rate.smoothed_bps <= 0.0 || self.target >= remaining_cap {
                        return self.target;
                    }
                    self.target += 1;
                    self.phase = ConnectionPhase::AwaitActivation {
                        baseline_bps: rate.smoothed_bps,
                        active,
                        since: sample.now,
                    };
                    tracing::debug!(
                        baseline_bps = rate.smoothed_bps,
                        active,
                        target = self.target,
                        "connection probe requested"
                    );
                }
            }
            ConnectionPhase::AwaitActivation {
                baseline_bps,
                active: previous_active,
                since,
            } => {
                if sample.workers_finished {
                    self.target = self.target.saturating_sub(1).max(self.min);
                    self.cooldown(sample, "workers finished before activation");
                } else if active == self.target && active == previous_active + 1 {
                    self.phase = ConnectionPhase::Settling {
                        baseline_bps,
                        since: sample.now,
                    };
                    tracing::debug!(active, "connection probe activated; settling");
                } else if active != previous_active
                    || sample.now.saturating_duration_since(since)
                        >= self.interval.saturating_mul(4)
                {
                    self.target = self.target.saturating_sub(1).max(self.min);
                    self.cooldown(sample, "probe activation unavailable");
                }
            }
            ConnectionPhase::Settling {
                baseline_bps,
                since,
            } => {
                if active != self.target || sample.workers_changed {
                    self.cooldown(sample, "workers changed during settling");
                } else if sample.now.saturating_duration_since(since)
                    >= self.interval.saturating_mul(2)
                {
                    self.meter.reset(sample.now, sample.downloaded_bytes);
                    self.phase = ConnectionPhase::Observe {
                        baseline_bps,
                        gain_samples: 0,
                    };
                }
            }
            ConnectionPhase::Observe {
                baseline_bps,
                mut gain_samples,
            } => {
                if active != self.target || sample.workers_changed {
                    self.cooldown(sample, "workers changed during observation");
                    return self.target;
                }
                let Some(rate) =
                    self.meter
                        .observe(sample.now, sample.downloaded_bytes, self.interval)
                else {
                    return self.target;
                };
                if has_meaningful_gain(baseline_bps, rate.raw_bps, self.gain_threshold)
                    && has_meaningful_gain(baseline_bps, rate.smoothed_bps, self.gain_threshold)
                {
                    gain_samples = gain_samples.saturating_add(1);
                } else {
                    gain_samples = 0;
                }
                self.phase = ConnectionPhase::Observe {
                    baseline_bps,
                    gain_samples,
                };
                if self
                    .meter
                    .is_ready(sample.now, self.interval.saturating_mul(2))
                {
                    let useful = gain_samples >= 2;
                    tracing::debug!(
                        baseline_bps,
                        post_bps = rate.smoothed_bps,
                        active,
                        useful,
                        "connection probe evaluated"
                    );
                    if useful {
                        self.meter.reset(sample.now, sample.downloaded_bytes);
                        self.phase = ConnectionPhase::Baseline { active };
                    } else {
                        self.cooldown(sample, "insufficient sustained gain");
                    }
                }
            }
            ConnectionPhase::Cooldown { since } => {
                if sample.now.saturating_duration_since(since) >= self.interval.saturating_mul(4) {
                    self.meter.reset(sample.now, sample.downloaded_bytes);
                    self.phase = ConnectionPhase::Baseline { active };
                }
            }
        }
        self.target
    }

    fn cooldown(&mut self, sample: ConnectionSample, reason: &'static str) {
        tracing::debug!(
            reason,
            target = self.target,
            active = sample.active_part_count,
            "connection expansion suspended"
        );
        self.phase = ConnectionPhase::Cooldown { since: sample.now };
        self.meter.reset(sample.now, sample.downloaded_bytes);
    }

    fn cap_by_remaining(
        target: usize,
        min: usize,
        min_part_size: u64,
        remaining_bytes: u64,
    ) -> usize {
        if remaining_bytes == 0 {
            return min;
        }

        let min_part_size = min_part_size.max(1);
        let range_limited = remaining_bytes.div_ceil(min_part_size).max(1);
        target
            .min(usize::try_from(range_limited).unwrap_or(usize::MAX))
            .max(min)
    }
}

#[derive(Debug, Clone, Copy)]
enum ConnectionPhase {
    Baseline {
        active: usize,
    },
    AwaitActivation {
        baseline_bps: f64,
        active: usize,
        since: Instant,
    },
    Settling {
        baseline_bps: f64,
        since: Instant,
    },
    Observe {
        baseline_bps: f64,
        gain_samples: u8,
    },
    Cooldown {
        since: Instant,
    },
}

#[derive(Debug, Clone, Default)]
struct ThroughputMeter {
    last: Option<(Instant, u64)>,
    started: Option<Instant>,
    smoothed_bps: Option<f64>,
    samples: u32,
}

#[derive(Debug, Clone, Copy)]
struct ThroughputRate {
    raw_bps: f64,
    smoothed_bps: f64,
}

impl ThroughputMeter {
    fn reset(&mut self, now: Instant, downloaded: u64) {
        *self = Self {
            last: Some((now, downloaded)),
            started: Some(now),
            ..Self::default()
        };
    }

    fn observe(
        &mut self,
        now: Instant,
        downloaded: u64,
        interval: Duration,
    ) -> Option<ThroughputRate> {
        let Some((previous_time, previous_bytes)) = self.last else {
            self.reset(now, downloaded);
            return None;
        };
        let elapsed = now.checked_duration_since(previous_time)?;
        if elapsed < (interval / 10).max(Duration::from_millis(1)) {
            return None;
        }
        let Some(delta) = downloaded.checked_sub(previous_bytes) else {
            self.reset(now, downloaded);
            return None;
        };
        let raw_bps = delta as f64 / elapsed.as_secs_f64();
        let tau = interval.max(Duration::from_millis(1)).as_secs_f64() * 2.0;
        let alpha = -(-elapsed.as_secs_f64() / tau).exp_m1();
        let smoothed_bps = self
            .smoothed_bps
            .map_or(raw_bps, |previous| previous + alpha * (raw_bps - previous));
        self.last = Some((now, downloaded));
        self.smoothed_bps = Some(smoothed_bps);
        self.samples = self.samples.saturating_add(1);
        Some(ThroughputRate {
            raw_bps,
            smoothed_bps,
        })
    }

    fn is_ready(&self, now: Instant, window: Duration) -> bool {
        self.samples >= 2
            && self
                .started
                .is_some_and(|start| now.saturating_duration_since(start) >= window)
    }
}

fn has_meaningful_gain(previous_bps: f64, current_bps: f64, threshold_percent: u8) -> bool {
    previous_bps > 0.0
        && current_bps > previous_bps
        && current_bps - previous_bps >= previous_bps * f64::from(threshold_percent) / 100.0
}

fn probe_error_requires_manual_action(error: &crate::download::HttpProbeError) -> bool {
    matches!(
        error,
        crate::download::HttpProbeError::ContentLengthChanged { .. }
            | crate::download::HttpProbeError::EtagChanged { .. }
            | crate::download::HttpProbeError::RangeSupportDisappeared
            | crate::download::HttpProbeError::WebpageMismatch { .. }
            | crate::download::HttpProbeError::ContentRangeMismatch { .. }
            | crate::download::HttpProbeError::MissingContentRange
            | crate::download::HttpProbeError::InvalidContentRange(_)
    )
}

pub(crate) async fn unique_destination_path(
    requested: &PathBuf,
    incomplete_extension: &str,
) -> HttpDownloadJobResult<PathBuf> {
    if !path_exists(requested).await?
        && !path_exists(&incomplete_path_for(requested, incomplete_extension)).await?
    {
        return Ok(requested.clone());
    }

    let parent = requested.parent().map(PathBuf::from).unwrap_or_default();
    let stem = requested
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            HttpDownloadJobError::DestinationConflict(requested.display().to_string())
        })?;
    let extension = requested.extension().and_then(|value| value.to_str());
    for suffix in 1_u32..=9_999 {
        let file_name = match extension {
            Some(extension) => format!("{stem}({suffix}).{extension}"),
            None => format!("{stem}({suffix})"),
        };
        let candidate = parent.join(file_name);
        if !path_exists(&candidate).await?
            && !path_exists(&incomplete_path_for(&candidate, incomplete_extension)).await?
        {
            return Ok(candidate);
        }
    }

    Err(HttpDownloadJobError::DestinationConflict(
        requested.display().to_string(),
    ))
}

async fn unique_final_destination_path(requested: &PathBuf) -> HttpDownloadJobResult<PathBuf> {
    if !path_exists(requested).await? {
        return Ok(requested.clone());
    }

    let parent = requested.parent().map(PathBuf::from).unwrap_or_default();
    let stem = requested
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            HttpDownloadJobError::DestinationConflict(requested.display().to_string())
        })?;
    let extension = requested.extension().and_then(|value| value.to_str());
    for suffix in 1_u32..=9_999 {
        let file_name = match extension {
            Some(extension) => format!("{stem}({suffix}).{extension}"),
            None => format!("{stem}({suffix})"),
        };
        let candidate = parent.join(file_name);
        if !path_exists(&candidate).await? {
            return Ok(candidate);
        }
    }

    Err(HttpDownloadJobError::DestinationConflict(
        requested.display().to_string(),
    ))
}

fn incomplete_path_for(path: &Path, incomplete_extension: &str) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .map(|value| format!("{value}{incomplete_extension}"))
        .unwrap_or_else(|| incomplete_extension.to_owned());
    path.with_file_name(file_name)
}

async fn path_exists(path: &PathBuf) -> HttpDownloadJobResult<bool> {
    match fs::metadata(path).await {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

/// Probes destination writability without touching the real incomplete file.
///
/// Creates the folder, then writes and removes a uniquely named probe file.
/// The incomplete path itself is never created here: creating it would make
/// the unique-destination logic mistake the download for a name collision and
/// rename the file.
async fn check_destination_writable(
    folder: &Path,
    incomplete_path: &Path,
) -> HttpDownloadJobResult<()> {
    fs::create_dir_all(folder).await?;
    let probe = incomplete_path.with_file_name(format!(
        ".raijin-write-test-{}-{}.tmp",
        std::process::id(),
        DESTINATION_PROBE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&probe, []).await?;
    // Best-effort cleanup: writability is already proven by the write above.
    let _ = fs::remove_file(&probe).await;
    Ok(())
}

static DESTINATION_PROBE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn is_cross_device_error(error: &std::io::Error) -> bool {
    matches!(error.raw_os_error(), Some(18) | Some(17))
        || error
            .to_string()
            .to_ascii_lowercase()
            .contains("cross-device")
}

#[derive(Debug, Clone)]
struct ActivePartControl {
    tx: mpsc::Sender<PartWorkerControl>,
    state: Arc<ActivePartState>,
}

#[derive(Debug)]
struct ActivePartState {
    current_byte: AtomicU64,
    target_end: AtomicU64,
}

impl ActivePartState {
    fn new(part: &DownloadPart) -> Self {
        Self {
            current_byte: AtomicU64::new(part.current_byte.get()),
            target_end: AtomicU64::new(part.end_byte.map(Bytes::get).unwrap_or(0)),
        }
    }

    fn set_current(&self, current_byte: Bytes) {
        self.current_byte
            .store(current_byte.get(), Ordering::Relaxed);
    }

    fn set_target_end(&self, target_end: Bytes) {
        self.target_end.store(target_end.get(), Ordering::Relaxed);
    }

    fn snapshot(&self) -> ActivePartSnapshot {
        let current = self.current_byte.load(Ordering::Relaxed);
        let target_end = self.target_end.load(Ordering::Relaxed);
        ActivePartSnapshot {
            remaining: target_end
                .checked_add(1)
                .and_then(|exclusive_end| exclusive_end.checked_sub(current))
                .unwrap_or(0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActivePartSnapshot {
    remaining: u64,
}

#[derive(Debug)]
struct PartWorkerRequest {
    part_repo: PartRepository,
    client: ReqwestHttpClient,
    url: String,
    headers: std::collections::BTreeMap<String, String>,
    path: PathBuf,
    part: DownloadPart,
    cancellation: CancellationToken,
    progress_interval: Duration,
    speed_limiter: Option<Arc<SpeedLimiter>>,
    /// Shared global limiter layered over the per-download limit; `None` skips shaping.
    global_limiter: Option<Arc<SpeedLimiter>>,
    live_progress: Option<Arc<LiveDownloadProgress>>,
    control_rx: mpsc::Receiver<PartWorkerControl>,
    active_state: Arc<ActivePartState>,
}

#[derive(Debug)]
enum PartWorkerControl {
    RequestSplit {
        child_index: u32,
        min_part_size: Bytes,
        now_ms: i64,
        reply_tx: oneshot::Sender<PartSplitResponse>,
    },
}

#[derive(Debug)]
enum PartSplitResponse {
    Proposed {
        parent: DownloadPart,
        child: DownloadPart,
        commit_tx: oneshot::Sender<bool>,
    },
    Rejected,
}

#[derive(Debug)]
enum PartWorkerOutcome {
    Completed,
    Canceled,
    Failed(DownloadPart, HttpDownloadJobError),
}

#[derive(Debug)]
struct PartWorkerFailure {
    part: DownloadPart,
    error: HttpDownloadJobError,
}

async fn part_worker(request: PartWorkerRequest) -> HttpDownloadJobResult<PartWorkerOutcome> {
    match part_worker_inner(request).await {
        Ok(outcome) => Ok(outcome),
        Err(failure) => Ok(PartWorkerOutcome::Failed(failure.part, failure.error)),
    }
}

async fn part_worker_inner(
    mut request: PartWorkerRequest,
) -> Result<PartWorkerOutcome, Box<PartWorkerFailure>> {
    if part_is_complete(&request.part) {
        return Ok(PartWorkerOutcome::Completed);
    }
    let _active_part_guard = request
        .live_progress
        .as_ref()
        .map(|live_progress| ActivePartGuard::new(Arc::clone(live_progress)));

    request.part.status = PartStatus::Connecting;
    request.part.updated_at = now_ms().map_err(|error| worker_failure(&request.part, error))?;
    request
        .part_repo
        .set(&request.part)
        .await
        .map_err(|error| worker_failure(&request.part, error.into()))?;

    let start = request.part.current_byte.get();
    let mut target_end = request
        .part
        .end_byte
        .ok_or_else(|| worker_failure(&request.part, HttpDownloadJobError::ResumeUnsupported))?
        .get();
    request.active_state.set_current(request.part.current_byte);
    request.active_state.set_target_end(Bytes::new(target_end));
    let mut response = request
        .client
        .client()
        .get(&request.url)
        .headers(
            request_headers(&request.headers)
                .map_err(|error| worker_failure(&request.part, error))?,
        )
        .header(header::RANGE, format!("bytes={start}-{target_end}"))
        .send()
        .await
        .map_err(|error| worker_failure(&request.part, error.into()))?;
    if response.status() != StatusCode::PARTIAL_CONTENT {
        let error = if response.status() == StatusCode::OK {
            HttpDownloadJobError::RangeIgnored
        } else {
            HttpDownloadJobError::UnexpectedStatus(response.status().as_u16())
        };
        return Err(Box::new(PartWorkerFailure {
            part: request.part,
            error,
        }));
    }
    validate_part_content_range(&request.part, response.headers())
        .map_err(|error| worker_failure(&request.part, error))?;

    let mut file = OpenOptions::new()
        .write(true)
        .open(&request.path)
        .await
        .map_err(|error| worker_failure(&request.part, error.into()))?;
    file.seek(SeekFrom::Start(start))
        .await
        .map_err(|error| worker_failure(&request.part, error.into()))?;
    request.part.status = PartStatus::Receiving;
    request.part.updated_at = now_ms().map_err(|error| worker_failure(&request.part, error))?;
    request
        .part_repo
        .set(&request.part)
        .await
        .map_err(|error| worker_failure(&request.part, error.into()))?;

    let mut last_persisted = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = request.cancellation.cancelled() => {
                request.part.status = PartStatus::Canceled;
                request.part.updated_at = now_ms().map_err(|error| worker_failure(&request.part, error))?;
                request.part_repo.set(&request.part).await.map_err(|error| worker_failure(&request.part, error.into()))?;
                return Ok(PartWorkerOutcome::Canceled);
            }
            control = request.control_rx.recv() => {
                if let Some(control) = control {
                    handle_part_worker_control(&mut request.part, &mut target_end, control, &request.cancellation).await;
                    request.active_state.set_target_end(Bytes::new(target_end));
                }
            }
            chunk = response.chunk() => {
                let Some(chunk) = chunk.map_err(|error| worker_failure(&request.part, error.into()))? else {
                    break;
                };
                if chunk.is_empty() {
                    continue;
                }
                let chunk_len = u64::try_from(chunk.len()).map_err(|_| worker_failure(&request.part, HttpDownloadJobError::ByteCountOverflow))?;
                let writable_len = writable_chunk_len(request.part.current_byte.get(), target_end, chunk_len)
                    .map_err(|error| worker_failure(&request.part, error))?;
                if writable_len == 0 {
                    break;
                }
                if let Some(limiter) = &request.speed_limiter {
                    limiter.acquire(writable_len).await;
                }
                if let Some(limiter) = &request.global_limiter {
                    limiter.acquire(writable_len).await;
                }
                let writable_len = usize::try_from(writable_len).map_err(|_| worker_failure(&request.part, HttpDownloadJobError::ByteCountOverflow))?;
                file.write_all(&chunk[..writable_len]).await.map_err(|error| worker_failure(&request.part, error.into()))?;
                let current = request.part.current_byte.get()
                    .checked_add(u64::try_from(writable_len).map_err(|_| worker_failure(&request.part, HttpDownloadJobError::ByteCountOverflow))?)
                    .ok_or_else(|| worker_failure(&request.part, HttpDownloadJobError::ByteCountOverflow))?;
                request.part.current_byte = Bytes::new(current);
                request.active_state.set_current(request.part.current_byte);
                if let Some(live_progress) = &request.live_progress {
                    live_progress.add_downloaded(u64::try_from(writable_len).map_err(|_| worker_failure(&request.part, HttpDownloadJobError::ByteCountOverflow))?);
                }
                if current == target_end.saturating_add(1) {
                    break;
                }
                if last_persisted.elapsed() >= request.progress_interval {
                    request.part.updated_at = now_ms().map_err(|error| worker_failure(&request.part, error))?;
                    request.part_repo.set(&request.part).await.map_err(|error| worker_failure(&request.part, error.into()))?;
                    last_persisted = tokio::time::Instant::now();
                }
            }
        }
    }

    file.flush()
        .await
        .map_err(|error| worker_failure(&request.part, error.into()))?;
    let expected_end = target_end.saturating_add(1);
    if request.part.current_byte.get() != expected_end {
        let actual = request.part.current_byte.get();
        return Err(worker_failure(
            &request.part,
            HttpDownloadJobError::PrematureEof {
                index: request.part.index,
                expected: expected_end,
                actual,
            },
        ));
    }

    request.part.status = PartStatus::Completed;
    request.part.updated_at = now_ms().map_err(|error| worker_failure(&request.part, error))?;
    request
        .part_repo
        .set(&request.part)
        .await
        .map_err(|error| worker_failure(&request.part, error.into()))?;
    Ok(PartWorkerOutcome::Completed)
}

async fn handle_part_worker_control(
    part: &mut DownloadPart,
    target_end: &mut u64,
    control: PartWorkerControl,
    _cancellation: &CancellationToken,
) {
    let PartWorkerControl::RequestSplit {
        child_index,
        min_part_size,
        now_ms,
        reply_tx,
    } = control;

    let Some(split_start) =
        valid_inflight_split_start(part.current_byte, *target_end, min_part_size)
    else {
        let _ = reply_tx.send(PartSplitResponse::Rejected);
        return;
    };

    let Ok((mut parent, child)) = split_part_at(part, child_index, split_start, now_ms) else {
        let _ = reply_tx.send(PartSplitResponse::Rejected);
        return;
    };
    parent.status = PartStatus::Receiving;
    let Some(parent_end) = parent.end_byte else {
        let _ = reply_tx.send(PartSplitResponse::Rejected);
        return;
    };
    let (commit_tx, commit_rx) = oneshot::channel();
    if reply_tx
        .send(PartSplitResponse::Proposed {
            parent,
            child,
            commit_tx,
        })
        .is_err()
    {
        return;
    }

    let committed = commit_rx.await.unwrap_or(false);
    if committed {
        part.end_byte = Some(parent_end);
        part.updated_at = now_ms;
        *target_end = parent_end.get();
    }
}

fn valid_inflight_split_start(
    current_byte: Bytes,
    target_end: u64,
    min_part_size: Bytes,
) -> Option<Bytes> {
    let exclusive_end = target_end.checked_add(1)?;
    let remaining = exclusive_end.checked_sub(current_byte.get())?;
    if remaining < min_part_size.get().max(1).saturating_mul(2) {
        return None;
    }
    current_byte
        .get()
        .checked_add(remaining / 2)
        .map(Bytes::new)
}

fn writable_chunk_len(
    current_byte: u64,
    target_end: u64,
    chunk_len: u64,
) -> HttpDownloadJobResult<u64> {
    let exclusive_end = target_end
        .checked_add(1)
        .ok_or(HttpDownloadJobError::ByteCountOverflow)?;
    let remaining = exclusive_end.saturating_sub(current_byte);
    Ok(chunk_len.min(remaining))
}

fn worker_failure(part: &DownloadPart, error: HttpDownloadJobError) -> Box<PartWorkerFailure> {
    Box::new(PartWorkerFailure {
        part: part.clone(),
        error,
    })
}

fn validate_part_content_range(
    part: &DownloadPart,
    headers: &HeaderMap,
) -> HttpDownloadJobResult<()> {
    let actual = headers
        .get(header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or(crate::download::HttpProbeError::MissingContentRange)?;
    let content_range = crate::download::http::parse_content_range(actual)?;
    if content_range.start != part.current_byte || Some(content_range.end) != part.end_byte {
        return Err(HttpDownloadJobError::InvalidPartContentRange {
            index: part.index,
            actual: actual.to_owned(),
        });
    }
    Ok(())
}

fn part_is_complete(part: &DownloadPart) -> bool {
    let Some(end) = part.end_byte else {
        return false;
    };
    part.current_byte.get() == end.get().saturating_add(1) && part.status == PartStatus::Completed
}

fn downloaded_from_parts(parts: &[DownloadPart]) -> u64 {
    parts
        .iter()
        .map(|part| {
            part.current_byte
                .get()
                .saturating_sub(part.start_byte.get())
        })
        .sum()
}

async fn existing_len(path: &PathBuf) -> HttpDownloadJobResult<u64> {
    match fs::metadata(path).await {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

fn now_ms() -> HttpDownloadJobResult<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| HttpDownloadJobError::ClockBeforeEpoch)?;
    i64::try_from(duration.as_millis()).map_err(|_| HttpDownloadJobError::ClockOutOfRange)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::task::JoinHandle;

    use super::*;
    use crate::db;
    use crate::download::{DownloadKind, split_fixed_ranges};

    struct TestDb {
        _dir: TempDir,
        download_repo: DownloadRepository,
        part_repo: PartRepository,
    }

    #[derive(Debug)]
    struct TestServer {
        addr: SocketAddr,
        task: JoinHandle<()>,
    }

    #[derive(Debug)]
    struct ServerState {
        body: Vec<u8>,
        range_supported: bool,
        slow_body: bool,
        transient_range_failures: AtomicUsize,
        premature_eof_failures: AtomicUsize,
    }

    impl TestServer {
        async fn spawn(
            body: Vec<u8>,
            range_supported: bool,
            slow_body: bool,
        ) -> std::io::Result<Self> {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let addr = listener.local_addr()?;
            let state = Arc::new(ServerState {
                body,
                range_supported,
                slow_body,
                transient_range_failures: AtomicUsize::new(0),
                premature_eof_failures: AtomicUsize::new(0),
            });
            let task = tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let state = Arc::clone(&state);
                    tokio::spawn(async move {
                        let _ = handle_connection(stream, state).await;
                    });
                }
            });
            Ok(Self { addr, task })
        }

        async fn spawn_with_transient_range_failures(
            body: Vec<u8>,
            failures: usize,
        ) -> std::io::Result<Self> {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let addr = listener.local_addr()?;
            let state = Arc::new(ServerState {
                body,
                range_supported: true,
                slow_body: false,
                transient_range_failures: AtomicUsize::new(failures),
                premature_eof_failures: AtomicUsize::new(0),
            });
            let task = tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let state = Arc::clone(&state);
                    tokio::spawn(async move {
                        let _ = handle_connection(stream, state).await;
                    });
                }
            });
            Ok(Self { addr, task })
        }

        async fn spawn_with_premature_eof_failures(
            body: Vec<u8>,
            failures: usize,
            slow_body: bool,
        ) -> std::io::Result<Self> {
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let addr = listener.local_addr()?;
            let state = Arc::new(ServerState {
                body,
                range_supported: true,
                slow_body,
                transient_range_failures: AtomicUsize::new(0),
                premature_eof_failures: AtomicUsize::new(failures),
            });
            let task = tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let state = Arc::clone(&state);
                    tokio::spawn(async move {
                        let _ = handle_connection(stream, state).await;
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

    async fn test_db() -> HttpDownloadJobResult<TestDb> {
        let dir = tempfile::tempdir()
            .map_err(sqlx::Error::Io)
            .map_err(DbError::from)?;
        let db_path = dir.path().join("raijin-job-test.sqlite");
        let database_url = format!("sqlite://{}", db_path.display());
        let pool = db::bootstrap(&database_url).await?;
        Ok(TestDb {
            _dir: dir,
            download_repo: DownloadRepository::new(pool.clone()),
            part_repo: PartRepository::new(pool),
        })
    }

    async fn handle_connection(
        mut stream: TcpStream,
        state: Arc<ServerState>,
    ) -> std::io::Result<()> {
        let mut request = vec![0_u8; 4096];
        let read = stream.read(&mut request).await?;
        let request = String::from_utf8_lossy(&request[..read]);
        let range = requested_range(&request);
        let (status, reason, body, content_range) = response_parts(&state, range);

        let mut headers = format!(
            "HTTP/1.1 {status} {reason}\r\nConnection: close\r\nContent-Length: {}\r\nETag: \"job-etag\"\r\nContent-Type: application/octet-stream\r\n",
            body.len()
        );
        if let Some(content_range) = content_range {
            headers.push_str("Content-Range: ");
            headers.push_str(&content_range);
            headers.push_str("\r\n");
        }
        headers.push_str("\r\n");
        stream.write_all(headers.as_bytes()).await?;

        if state.slow_body {
            for chunk in body.chunks(1024) {
                stream.write_all(chunk).await?;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        } else {
            stream.write_all(body).await?;
        }
        stream.shutdown().await
    }

    #[derive(Debug, Clone, Copy)]
    struct RequestedRange {
        start: u64,
        end: Option<u64>,
    }

    fn requested_range(request: &str) -> Option<RequestedRange> {
        request.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if !name.eq_ignore_ascii_case("range") {
                return None;
            }
            let (start, end) = value.trim().strip_prefix("bytes=")?.split_once('-')?;
            Some(RequestedRange {
                start: start.parse().ok()?,
                end: if end.is_empty() {
                    None
                } else {
                    Some(end.parse().ok()?)
                },
            })
        })
    }

    fn response_parts(
        state: &ServerState,
        range: Option<RequestedRange>,
    ) -> (u16, &'static str, &[u8], Option<String>) {
        let total = state.body.len();
        if let Some(range) = range
            && state.range_supported
            && let Ok(start) = usize::try_from(range.start)
            && start < total
        {
            if !is_probe_range(range)
                && state
                    .transient_range_failures
                    .try_update(
                        AtomicOrdering::Relaxed,
                        AtomicOrdering::Relaxed,
                        |current| current.checked_sub(1),
                    )
                    .is_ok()
            {
                return (500, "Internal Server Error", &[], None);
            }
            let end = range
                .end
                .and_then(|end| usize::try_from(end).ok())
                .unwrap_or(total - 1)
                .min(total - 1);
            let full_len = end.saturating_sub(start).saturating_add(1);
            if !is_probe_range(range)
                && state
                    .premature_eof_failures
                    .try_update(
                        AtomicOrdering::Relaxed,
                        AtomicOrdering::Relaxed,
                        |current| current.checked_sub(1),
                    )
                    .is_ok()
            {
                let truncated_len = (full_len / 2).min(full_len.saturating_sub(1));
                return (
                    206,
                    "Partial Content",
                    &state.body[start..start + truncated_len],
                    Some(format!("bytes {start}-{end}/{total}")),
                );
            }
            return (
                206,
                "Partial Content",
                &state.body[start..=end],
                Some(format!("bytes {start}-{end}/{total}")),
            );
        }

        (200, "OK", &state.body, None)
    }

    fn is_probe_range(range: RequestedRange) -> bool {
        range.start == 0 && range.end == Some(255)
    }

    fn one_connection() -> NonZeroU16 {
        match NonZeroU16::new(1) {
            Some(value) => value,
            None => panic!("test connection count must be non-zero"),
        }
    }

    fn four_connections() -> NonZeroU16 {
        match NonZeroU16::new(4) {
            Some(value) => value,
            None => panic!("test connection count must be non-zero"),
        }
    }

    fn adaptive_test_config() -> HttpDownloadJobConfig {
        HttpDownloadJobConfig {
            incomplete_extension: ".part".to_owned(),
            progress_interval: Duration::from_millis(10),
            max_retries: 3,
            min_part_size: Bytes::new(1_024),
            adaptive_connections_enabled: true,
            default_connections: 4,
            min_connections: 1,
            max_connections: 8,
            connection_probe_interval: Duration::from_millis(10),
            connection_gain_threshold: 10,
        }
    }

    fn assert_contiguous_completed_ranges(parts: &[DownloadPart], total_len: u64) {
        let mut parts = parts.to_vec();
        parts.sort_by_key(|part| part.start_byte);
        let mut next_start = 0;
        for part in &parts {
            assert_eq!(part.start_byte, Bytes::new(next_start));
            let end = match part.end_byte {
                Some(end) => end,
                None => panic!("test ranged part must have end byte"),
            };
            assert_eq!(part.current_byte, Bytes::new(end.get().saturating_add(1)));
            assert_eq!(part.status, PartStatus::Completed);
            next_start = end.get().saturating_add(1);
        }
        assert_eq!(next_start, total_len);
    }

    fn assert_contiguous_part_ranges(parts: &[DownloadPart], total_len: u64) {
        let mut parts = parts.to_vec();
        parts.sort_by_key(|part| part.start_byte);
        let mut next_start = 0;
        for part in &parts {
            assert_eq!(part.start_byte, Bytes::new(next_start));
            let end = match part.end_byte {
                Some(end) => end,
                None => panic!("test ranged part must have end byte"),
            };
            assert!(part.current_byte >= part.start_byte);
            assert!(part.current_byte.get() <= end.get().saturating_add(1));
            next_start = end.get().saturating_add(1);
        }
        assert_eq!(next_start, total_len);
    }

    fn sample_item(url: String, folder: PathBuf, downloaded: u64) -> DownloadItem {
        DownloadItem {
            id: crate::download::DownloadId::new(1),
            kind: DownloadKind::Http,
            url,
            download_page: None,
            headers: BTreeMap::new(),
            file_name: "file.bin".to_owned(),
            folder,
            category: None,
            status: DownloadStatus::Added,
            total_bytes: (downloaded > 0).then_some(Bytes::new(1024)),
            downloaded_bytes: Bytes::new(downloaded),
            etag: (downloaded > 0).then_some("\"job-etag\"".to_owned()),
            last_modified: None,
            preferred_connections: Some(one_connection()),
            speed_limit: None,
            failure: None,
            created_at: 1,
            started_at: None,
            completed_at: None,
            updated_at: 1,
        }
    }

    async fn run_job(
        db: &TestDb,
        item: DownloadItem,
        cancellation: CancellationToken,
    ) -> HttpDownloadJobResult<DownloadItem> {
        db.download_repo.add(&item).await?;
        HttpDownloadJob::new(
            db.download_repo.clone(),
            db.part_repo.clone(),
            ReqwestHttpClient::new()?,
            item,
        )
        .run(cancellation)
        .await
    }

    #[tokio::test]
    async fn job_should_download_small_file_from_local_server() -> HttpDownloadJobResult<()> {
        let body = vec![b'x'; 1024];
        let server = TestServer::spawn(body.clone(), true, false).await?;
        let db = test_db().await?;
        let folder = tempfile::tempdir()?;
        let item = sample_item(server.url(), folder.path().to_path_buf(), 0);

        let completed = run_job(&db, item.clone(), CancellationToken::new()).await?;
        let final_bytes = fs::read(folder.path().join("file.bin")).await?;

        assert_eq!(completed.status, DownloadStatus::Completed);
        assert_eq!(final_bytes, body);
        Ok(())
    }

    #[tokio::test]
    async fn job_should_pause_mid_download_and_persist_state() -> HttpDownloadJobResult<()> {
        let server = TestServer::spawn(vec![b'p'; 128 * 1024], true, true).await?;
        let db = test_db().await?;
        let folder = tempfile::tempdir()?;
        let item = sample_item(server.url(), folder.path().to_path_buf(), 0);
        db.download_repo.add(&item).await?;
        let cancellation = CancellationToken::new();
        let job = HttpDownloadJob::new(
            db.download_repo.clone(),
            db.part_repo.clone(),
            ReqwestHttpClient::new()?,
            item.clone(),
        );
        let cancel_clone = cancellation.clone();

        let task = tokio::spawn(async move { job.run(cancellation).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel_clone.cancel();
        task.await
            .map_err(|_| HttpDownloadJobError::ResumeUnsupported)??;
        let persisted = db
            .download_repo
            .get(item.id)
            .await?
            .ok_or(DbError::NotFound {
                entity: "download",
                id: item.id.get(),
            })?;

        assert_eq!(persisted.status, DownloadStatus::Paused);
        assert!(
            persisted.downloaded_bytes.get()
                < persisted.total_bytes.unwrap_or(Bytes::new(u64::MAX)).get()
        );
        Ok(())
    }

    #[tokio::test]
    async fn job_should_resume_partial_file_with_range() -> HttpDownloadJobResult<()> {
        let body = vec![b'r'; 1024];
        let server = TestServer::spawn(body.clone(), true, false).await?;
        let db = test_db().await?;
        let folder = tempfile::tempdir()?;
        let item = sample_item(server.url(), folder.path().to_path_buf(), 512);
        fs::write(folder.path().join("file.bin.raijin-part"), &body[..512]).await?;

        let completed = run_job(&db, item, CancellationToken::new()).await?;
        let final_bytes = fs::read(folder.path().join("file.bin")).await?;

        assert_eq!(completed.status, DownloadStatus::Completed);
        assert_eq!(final_bytes, body);
        Ok(())
    }

    #[tokio::test]
    async fn job_should_atomically_rename_incomplete_file_on_completion()
    -> HttpDownloadJobResult<()> {
        let server = TestServer::spawn(vec![b'a'; 1024], true, false).await?;
        let db = test_db().await?;
        let folder = tempfile::tempdir()?;
        let item = sample_item(server.url(), folder.path().to_path_buf(), 0);

        run_job(&db, item, CancellationToken::new()).await?;

        assert!(fs::metadata(folder.path().join("file.bin")).await.is_ok());
        assert!(
            fs::metadata(folder.path().join("file.bin.raijin-part"))
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn ranged_job_should_download_with_multiple_parts() -> HttpDownloadJobResult<()> {
        let body = (0..=255)
            .cycle()
            .take(16 * 1024 * 1024)
            .collect::<Vec<u8>>();
        let server = TestServer::spawn(body.clone(), true, false).await?;
        let db = test_db().await?;
        let folder = tempfile::tempdir()?;
        let mut item = sample_item(server.url(), folder.path().to_path_buf(), 0);
        item.preferred_connections = Some(four_connections());

        let completed = run_job(&db, item.clone(), CancellationToken::new()).await?;
        let final_bytes = fs::read(folder.path().join("file.bin")).await?;
        let parts = db.part_repo.list_for_download(item.id).await?;

        assert_eq!(completed.status, DownloadStatus::Completed);
        assert_eq!(final_bytes, body);
        assert_eq!(parts.len(), 4);
        assert!(
            parts
                .iter()
                .all(|part| part.status == PartStatus::Completed)
        );
        Ok(())
    }

    #[tokio::test]
    async fn ranged_job_should_resume_from_persisted_part_state() -> HttpDownloadJobResult<()> {
        let body = (0..=255).cycle().take(1024).collect::<Vec<u8>>();
        let server = TestServer::spawn(body.clone(), true, false).await?;
        let db = test_db().await?;
        let folder = tempfile::tempdir()?;
        let mut item = sample_item(server.url(), folder.path().to_path_buf(), 256);
        item.preferred_connections = Some(four_connections());
        item.total_bytes = Some(Bytes::new(1024));
        item.etag = Some("\"job-etag\"".to_owned());
        db.download_repo.add(&item).await?;
        let mut parts = split_fixed_ranges(item.id, Some(Bytes::new(1024)), 4, 1)?;
        parts[0].current_byte = Bytes::new(256);
        parts[0].status = PartStatus::Completed;
        db.part_repo.set_for_download(item.id, &parts).await?;
        let mut partial = vec![0_u8; 1024];
        partial[..256].copy_from_slice(&body[..256]);
        fs::write(folder.path().join("file.bin.raijin-part"), partial).await?;

        let completed = HttpDownloadJob::new(
            db.download_repo.clone(),
            db.part_repo.clone(),
            ReqwestHttpClient::new()?,
            item.clone(),
        )
        .run(CancellationToken::new())
        .await?;
        let final_bytes = fs::read(folder.path().join("file.bin")).await?;

        assert_eq!(completed.status, DownloadStatus::Completed);
        assert_eq!(final_bytes, body);
        Ok(())
    }

    #[tokio::test]
    async fn ranged_job_should_split_large_pending_parts_dynamically() -> HttpDownloadJobResult<()>
    {
        let completed_len = 1024 * 1024;
        let total_len = 8 * 1024 * 1024;
        let body = (0..=255).cycle().take(total_len).collect::<Vec<u8>>();
        let server = TestServer::spawn(body.clone(), true, false).await?;
        let db = test_db().await?;
        let folder = tempfile::tempdir()?;
        let mut item = sample_item(
            server.url(),
            folder.path().to_path_buf(),
            completed_len as u64,
        );
        item.preferred_connections = Some(four_connections());
        item.total_bytes = Some(Bytes::new(total_len as u64));
        item.etag = Some("\"job-etag\"".to_owned());
        db.download_repo.add(&item).await?;
        let now = now_ms()?;
        let parts = vec![
            DownloadPart {
                id: PartId::new(65_537),
                download_id: item.id,
                index: 0,
                start_byte: Bytes::ZERO,
                end_byte: Some(Bytes::new(completed_len as u64 - 1)),
                current_byte: Bytes::new(completed_len as u64),
                status: PartStatus::Completed,
                retry_count: 0,
                updated_at: now,
            },
            DownloadPart {
                id: PartId::new(65_538),
                download_id: item.id,
                index: 1,
                start_byte: Bytes::new(completed_len as u64),
                end_byte: Some(Bytes::new(total_len as u64 - 1)),
                current_byte: Bytes::new(completed_len as u64),
                status: PartStatus::Idle,
                retry_count: 0,
                updated_at: now,
            },
        ];
        db.part_repo.set_for_download(item.id, &parts).await?;
        let mut partial = vec![0_u8; total_len];
        partial[..completed_len].copy_from_slice(&body[..completed_len]);
        fs::write(folder.path().join("file.bin.raijin-part"), partial).await?;
        let mut job = HttpDownloadJob::new(
            db.download_repo.clone(),
            db.part_repo.clone(),
            ReqwestHttpClient::new()?,
            item.clone(),
        );
        job.config.min_part_size = Bytes::new(1024 * 1024);

        let completed = job.run(CancellationToken::new()).await?;
        let final_bytes = fs::read(folder.path().join("file.bin")).await?;
        let persisted_parts = db.part_repo.list_for_download(item.id).await?;

        assert_eq!(completed.status, DownloadStatus::Completed);
        assert_eq!(final_bytes, body);
        assert!(persisted_parts.len() > parts.len());
        assert!(
            persisted_parts
                .iter()
                .all(|part| part.status == PartStatus::Completed)
        );
        Ok(())
    }

    #[tokio::test]
    async fn ranged_job_should_split_active_worker_in_flight() -> HttpDownloadJobResult<()> {
        let completed_len = 1024 * 1024;
        let total_len = 8 * 1024 * 1024;
        let body = (0..=255).cycle().take(total_len).collect::<Vec<u8>>();
        let server = TestServer::spawn(body.clone(), true, true).await?;
        let db = test_db().await?;
        let folder = tempfile::tempdir()?;
        let mut item = sample_item(
            server.url(),
            folder.path().to_path_buf(),
            completed_len as u64,
        );
        item.preferred_connections = Some(four_connections());
        item.total_bytes = Some(Bytes::new(total_len as u64));
        item.etag = Some("\"job-etag\"".to_owned());
        db.download_repo.add(&item).await?;
        let now = now_ms()?;
        let parts = vec![
            DownloadPart {
                id: PartId::new(65_537),
                download_id: item.id,
                index: 0,
                start_byte: Bytes::ZERO,
                end_byte: Some(Bytes::new(completed_len as u64 - 1)),
                current_byte: Bytes::new(completed_len as u64),
                status: PartStatus::Completed,
                retry_count: 0,
                updated_at: now,
            },
            DownloadPart {
                id: PartId::new(65_538),
                download_id: item.id,
                index: 1,
                start_byte: Bytes::new(completed_len as u64),
                end_byte: Some(Bytes::new(total_len as u64 - 1)),
                current_byte: Bytes::new(completed_len as u64),
                status: PartStatus::Idle,
                retry_count: 0,
                updated_at: now,
            },
        ];
        db.part_repo.set_for_download(item.id, &parts).await?;
        let mut partial = vec![0_u8; total_len];
        partial[..completed_len].copy_from_slice(&body[..completed_len]);
        fs::write(folder.path().join("file.bin.raijin-part"), partial).await?;
        let mut job = HttpDownloadJob::new(
            db.download_repo.clone(),
            db.part_repo.clone(),
            ReqwestHttpClient::new()?,
            item.clone(),
        );
        job.config.min_part_size = Bytes::new(1024 * 1024);

        let completed = job.run(CancellationToken::new()).await?;
        let final_bytes = fs::read(folder.path().join("file.bin")).await?;
        let persisted_parts = db.part_repo.list_for_download(item.id).await?;

        assert_eq!(completed.status, DownloadStatus::Completed);
        assert_eq!(final_bytes, body);
        assert!(persisted_parts.len() > parts.len());
        assert_contiguous_completed_ranges(&persisted_parts, total_len as u64);
        Ok(())
    }

    #[tokio::test]
    async fn ranged_job_should_pause_cleanly_after_inflight_split_cancellation()
    -> HttpDownloadJobResult<()> {
        let completed_len = 1024 * 1024;
        let total_len = 8 * 1024 * 1024;
        let body = (0..=255).cycle().take(total_len).collect::<Vec<u8>>();
        let server = TestServer::spawn(body.clone(), true, true).await?;
        let db = test_db().await?;
        let folder = tempfile::tempdir()?;
        let mut item = sample_item(
            server.url(),
            folder.path().to_path_buf(),
            completed_len as u64,
        );
        item.preferred_connections = Some(four_connections());
        item.total_bytes = Some(Bytes::new(total_len as u64));
        item.etag = Some("\"job-etag\"".to_owned());
        db.download_repo.add(&item).await?;
        let now = now_ms()?;
        let parts = vec![
            DownloadPart {
                id: PartId::new(65_537),
                download_id: item.id,
                index: 0,
                start_byte: Bytes::ZERO,
                end_byte: Some(Bytes::new(completed_len as u64 - 1)),
                current_byte: Bytes::new(completed_len as u64),
                status: PartStatus::Completed,
                retry_count: 0,
                updated_at: now,
            },
            DownloadPart {
                id: PartId::new(65_538),
                download_id: item.id,
                index: 1,
                start_byte: Bytes::new(completed_len as u64),
                end_byte: Some(Bytes::new(total_len as u64 - 1)),
                current_byte: Bytes::new(completed_len as u64),
                status: PartStatus::Idle,
                retry_count: 0,
                updated_at: now,
            },
        ];
        db.part_repo.set_for_download(item.id, &parts).await?;
        let mut partial = vec![0_u8; total_len];
        partial[..completed_len].copy_from_slice(&body[..completed_len]);
        fs::write(folder.path().join("file.bin.raijin-part"), partial).await?;
        let cancellation = CancellationToken::new();
        let mut job = HttpDownloadJob::new(
            db.download_repo.clone(),
            db.part_repo.clone(),
            ReqwestHttpClient::new()?,
            item.clone(),
        );
        job.config.min_part_size = Bytes::new(1024 * 1024);
        let cancel_clone = cancellation.clone();

        let task = tokio::spawn(async move { job.run(cancellation).await });
        let mut saw_split = false;
        for _ in 0..100 {
            if db.part_repo.list_for_download(item.id).await?.len() > parts.len() {
                saw_split = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(saw_split);
        cancel_clone.cancel();
        let paused = task.await.map_err(HttpDownloadJobError::WorkerJoin)??;
        let persisted = db
            .download_repo
            .get(item.id)
            .await?
            .ok_or(DbError::NotFound {
                entity: "download",
                id: item.id.get(),
            })?;
        let persisted_parts = db.part_repo.list_for_download(item.id).await?;

        assert_eq!(paused.status, DownloadStatus::Paused);
        assert_eq!(persisted.status, DownloadStatus::Paused);
        assert_contiguous_part_ranges(&persisted_parts, total_len as u64);
        Ok(())
    }

    #[test]
    fn inflight_split_should_reject_when_remaining_is_too_small() {
        let split = valid_inflight_split_start(Bytes::new(900), 999, Bytes::new(128));

        assert_eq!(split, None);
    }

    #[test]
    fn inflight_split_should_reject_stale_split_point() {
        let part = DownloadPart {
            id: PartId::new(65_538),
            download_id: crate::download::DownloadId::new(1),
            index: 1,
            start_byte: Bytes::ZERO,
            end_byte: Some(Bytes::new(999)),
            current_byte: Bytes::new(700),
            status: PartStatus::Receiving,
            retry_count: 0,
            updated_at: 1,
        };

        let split = split_part_at(&part, 2, Bytes::new(600), 2);

        assert!(split.is_err());
    }

    #[tokio::test]
    async fn split_commit_failure_should_leave_worker_target_unchanged() {
        let mut part = DownloadPart {
            id: PartId::new(65_538),
            download_id: crate::download::DownloadId::new(1),
            index: 1,
            start_byte: Bytes::ZERO,
            end_byte: Some(Bytes::new(999)),
            current_byte: Bytes::new(100),
            status: PartStatus::Receiving,
            retry_count: 0,
            updated_at: 1,
        };
        let mut target_end = 999;
        let cancellation = CancellationToken::new();
        let (reply_tx, reply_rx) = oneshot::channel();
        let control = PartWorkerControl::RequestSplit {
            child_index: 2,
            min_part_size: Bytes::new(128),
            now_ms: 2,
            reply_tx,
        };

        let (_, proposed) = tokio::join!(
            handle_part_worker_control(&mut part, &mut target_end, control, &cancellation),
            async move {
                if let Ok(PartSplitResponse::Proposed { commit_tx, .. }) = reply_rx.await {
                    let _ = commit_tx.send(false);
                    true
                } else {
                    false
                }
            }
        );

        assert!(proposed);
        assert_eq!(part.end_byte, Some(Bytes::new(999)));
        assert_eq!(target_end, 999);
    }

    #[tokio::test]
    async fn split_ack_failure_should_not_return_live_child() -> HttpDownloadJobResult<()> {
        let db = test_db().await?;
        let folder = tempfile::tempdir()?;
        let item = sample_item(
            "http://127.0.0.1/file.bin".to_owned(),
            folder.path().to_path_buf(),
            0,
        );
        db.download_repo.add(&item).await?;
        let part = DownloadPart {
            id: PartId::new(65_538),
            download_id: item.id,
            index: 1,
            start_byte: Bytes::ZERO,
            end_byte: Some(Bytes::new(999)),
            current_byte: Bytes::ZERO,
            status: PartStatus::Receiving,
            retry_count: 0,
            updated_at: 1,
        };
        let (control_tx, mut control_rx) = mpsc::channel(1);
        let mut controls = std::collections::HashMap::new();
        controls.insert(
            part.index,
            ActivePartControl {
                tx: control_tx,
                state: Arc::new(ActivePartState::new(&part)),
            },
        );
        let worker_part = part.clone();
        let worker = tokio::spawn(async move {
            let Some(PartWorkerControl::RequestSplit {
                child_index,
                min_part_size: _,
                now_ms,
                reply_tx,
            }) = control_rx.recv().await
            else {
                return;
            };
            let (parent, child) =
                match split_part_at(&worker_part, child_index, Bytes::new(500), now_ms) {
                    Ok(parts) => parts,
                    Err(_) => return,
                };
            let (commit_tx, commit_rx) = oneshot::channel();
            let _ = reply_tx.send(PartSplitResponse::Proposed {
                parent,
                child,
                commit_tx,
            });
            drop(commit_rx);
        });
        let mut next_part_index = 2;

        let child = request_inflight_split(
            &controls,
            &db.part_repo,
            &mut next_part_index,
            Bytes::new(128),
            2,
            &CancellationToken::new(),
        )
        .await?;
        worker.await.map_err(HttpDownloadJobError::WorkerJoin)?;
        let persisted = db.part_repo.list_for_download(item.id).await?;

        assert_eq!(child, None);
        assert_eq!(next_part_index, 3);
        assert_eq!(persisted.len(), 2);
        assert_eq!(persisted[0].start_byte, Bytes::ZERO);
        assert_eq!(persisted[0].end_byte, Some(Bytes::new(499)));
        assert_eq!(persisted[1].start_byte, Bytes::new(500));
        assert_eq!(persisted[1].end_byte, Some(Bytes::new(999)));
        Ok(())
    }

    #[tokio::test]
    async fn cancellation_during_split_negotiation_should_keep_target_unchanged() {
        let mut part = DownloadPart {
            id: PartId::new(65_538),
            download_id: crate::download::DownloadId::new(1),
            index: 1,
            start_byte: Bytes::ZERO,
            end_byte: Some(Bytes::new(999)),
            current_byte: Bytes::new(100),
            status: PartStatus::Receiving,
            retry_count: 0,
            updated_at: 1,
        };
        let mut target_end = 999;
        let cancellation = CancellationToken::new();
        let cancel_clone = cancellation.clone();
        let (reply_tx, reply_rx) = oneshot::channel();
        let control = PartWorkerControl::RequestSplit {
            child_index: 2,
            min_part_size: Bytes::new(128),
            now_ms: 2,
            reply_tx,
        };

        let (_, proposed) = tokio::join!(
            handle_part_worker_control(&mut part, &mut target_end, control, &cancellation),
            async move {
                let proposed = matches!(reply_rx.await, Ok(PartSplitResponse::Proposed { .. }));
                cancel_clone.cancel();
                proposed
            }
        );

        assert!(proposed);
        assert_eq!(part.end_byte, Some(Bytes::new(999)));
        assert_eq!(target_end, 999);
    }

    #[tokio::test]
    async fn ranged_adaptive_job_should_preserve_local_and_global_speed_limits()
    -> HttpDownloadJobResult<()> {
        for global in [false, true] {
            let body = vec![b'l'; 512 * 1024];
            let server = TestServer::spawn(body.clone(), true, false).await?;
            let db = test_db().await?;
            let folder = tempfile::tempdir()?;
            let mut item = sample_item(server.url(), folder.path().to_path_buf(), 0);
            item.preferred_connections = None;
            let limit = crate::download::BytesPerSecond::new(1024 * 1024);
            if !global {
                item.speed_limit = Some(limit);
            }
            db.download_repo.add(&item).await?;
            let mut job = HttpDownloadJob::new(
                db.download_repo.clone(),
                db.part_repo.clone(),
                ReqwestHttpClient::new()?,
                item.clone(),
            );
            job.config = adaptive_test_config();
            if global {
                job = job.with_global_limiter(SpeedLimiter::new(limit));
            }
            let progress = Arc::new(LiveDownloadProgress::new(Bytes::ZERO));
            job = job.with_live_progress(Arc::clone(&progress));
            let started = Instant::now();
            let mut poll = tokio::time::interval(Duration::from_millis(1));
            let run = job.run(CancellationToken::new());
            tokio::pin!(run);
            let completed = loop {
                tokio::select! {
                    result = &mut run => break result?,
                    _ = poll.tick() => assert!(progress.snapshot().1 <= 4),
                }
            };
            assert_eq!(completed.status, DownloadStatus::Completed);
            assert!(started.elapsed() >= Duration::from_millis(250));
            assert_eq!(fs::read(folder.path().join("file.bin")).await?, body);
            let parts = db.part_repo.list_for_download(item.id).await?;
            assert!(parts.len() >= 4);
            assert_contiguous_completed_ranges(&parts, body.len() as u64);
        }
        Ok(())
    }

    #[tokio::test]
    async fn ranged_job_should_retry_transient_part_failures() -> HttpDownloadJobResult<()> {
        let body = (0..=255).cycle().take(8 * 1024 * 1024).collect::<Vec<u8>>();
        let server = TestServer::spawn_with_transient_range_failures(body.clone(), 2).await?;
        let db = test_db().await?;
        let folder = tempfile::tempdir()?;
        let mut item = sample_item(server.url(), folder.path().to_path_buf(), 0);
        item.preferred_connections = Some(four_connections());

        let completed = run_job(&db, item.clone(), CancellationToken::new()).await?;
        let final_bytes = fs::read(folder.path().join("file.bin")).await?;
        let parts = db.part_repo.list_for_download(item.id).await?;

        assert_eq!(completed.status, DownloadStatus::Completed);
        assert_eq!(final_bytes, body);
        assert!(parts.iter().any(|part| part.retry_count > 0));
        Ok(())
    }

    #[tokio::test]
    async fn ranged_job_should_pause_when_server_validation_fails() -> HttpDownloadJobResult<()> {
        let body = vec![b'v'; 1024];
        let server = TestServer::spawn(body, false, false).await?;
        let db = test_db().await?;
        let folder = tempfile::tempdir()?;
        let mut item = sample_item(server.url(), folder.path().to_path_buf(), 0);
        item.preferred_connections = Some(four_connections());
        item.total_bytes = Some(Bytes::new(1024));
        item.etag = Some("\"job-etag\"".to_owned());
        db.download_repo.add(&item).await?;
        let parts = split_fixed_ranges(item.id, Some(Bytes::new(1024)), 4, 1)?;
        db.part_repo.set_for_download(item.id, &parts).await?;

        let paused = HttpDownloadJob::new(
            db.download_repo.clone(),
            db.part_repo.clone(),
            ReqwestHttpClient::new()?,
            item.clone(),
        )
        .run(CancellationToken::new())
        .await?;

        assert_eq!(paused.status, DownloadStatus::Paused);
        assert!(paused.failure.is_some());
        let persisted_parts = db.part_repo.list_for_download(item.id).await?;
        assert!(persisted_parts.iter().all(|part| part.retry_count == 0));
        Ok(())
    }

    #[tokio::test]
    async fn unique_destination_paths_should_suffix_without_spaces() -> HttpDownloadJobResult<()> {
        let folder = tempfile::tempdir()?;
        for (name, first, second, third) in [
            ("setup.exe", "setup(1).exe", "setup(2).exe", "setup(3).exe"),
            ("README", "README(1)", "README(2)", "README(3)"),
        ] {
            let requested = folder.path().join(name);
            assert_eq!(
                unique_destination_path(&requested, ".raijin-part").await?,
                requested
            );
            fs::write(&requested, b"original").await?;
            fs::write(folder.path().join(first), b"existing").await?;
            fs::write(
                incomplete_path_for(&folder.path().join(second), ".raijin-part"),
                b"partial",
            )
            .await?;

            assert_eq!(
                unique_destination_path(&requested, ".raijin-part").await?,
                folder.path().join(third)
            );
            assert_eq!(
                unique_final_destination_path(&requested).await?,
                folder.path().join(second)
            );
            assert_eq!(fs::read(&requested).await?, b"original");
        }
        Ok(())
    }

    #[tokio::test]
    async fn job_should_choose_unique_final_name_when_destination_exists()
    -> HttpDownloadJobResult<()> {
        let body = vec![b'd'; 1024];
        let server = TestServer::spawn(body.clone(), true, false).await?;
        let db = test_db().await?;
        let folder = tempfile::tempdir()?;
        fs::write(folder.path().join("file.bin"), b"existing").await?;
        let item = sample_item(server.url(), folder.path().to_path_buf(), 0);

        let completed = run_job(&db, item, CancellationToken::new()).await?;
        let final_bytes = fs::read(folder.path().join("file(1).bin")).await?;

        assert_eq!(completed.file_name, "file(1).bin");
        assert_eq!(final_bytes, body);
        Ok(())
    }

    #[tokio::test]
    async fn job_should_pause_resume_validation_failures_for_manual_action()
    -> HttpDownloadJobResult<()> {
        let body = vec![b'n'; 1024];
        let server = TestServer::spawn(body.clone(), false, false).await?;
        let db = test_db().await?;
        let folder = tempfile::tempdir()?;
        let item = sample_item(server.url(), folder.path().to_path_buf(), 512);
        fs::write(folder.path().join("file.bin.raijin-part"), &body[..512]).await?;

        let paused = run_job(&db, item, CancellationToken::new()).await?;

        assert_eq!(paused.status, DownloadStatus::Paused);
        assert_eq!(
            paused.failure.as_ref().map(|failure| failure.kind),
            Some(FailureKind::Validation)
        );
        Ok(())
    }

    #[tokio::test]
    async fn job_should_persist_disk_error_for_unwritable_destination() -> HttpDownloadJobResult<()>
    {
        let server = TestServer::spawn(vec![b'x'; 64], true, false).await?;
        let db = test_db().await?;
        // A regular file as folder makes directory creation fail deterministically.
        let blocker = tempfile::NamedTempFile::new()?;
        let item = sample_item(server.url(), blocker.path().to_path_buf(), 0);

        let result = run_job(&db, item.clone(), CancellationToken::new()).await;

        let error = result.expect_err("job must fail for unwritable destination");
        assert_eq!(error.to_failure().kind, FailureKind::Disk);
        let persisted = db
            .download_repo
            .get(item.id)
            .await?
            .ok_or(DbError::NotFound {
                entity: "download",
                id: item.id.get(),
            })?;
        assert_eq!(persisted.status, DownloadStatus::Error);
        assert_eq!(
            persisted.failure.as_ref().map(|failure| failure.kind),
            Some(FailureKind::Disk)
        );
        Ok(())
    }

    #[test]
    fn storage_full_io_should_map_to_no_space_failure() {
        let error = HttpDownloadJobError::Io(std::io::Error::from(std::io::ErrorKind::StorageFull));

        assert!(!error.requires_manual_action());
        assert_eq!(error.to_failure().kind, FailureKind::NoSpace);
    }

    #[test]
    fn other_io_errors_should_stay_disk_failures() {
        let error =
            HttpDownloadJobError::Io(std::io::Error::from(std::io::ErrorKind::PermissionDenied));

        assert_eq!(error.to_failure().kind, FailureKind::Disk);
    }

    #[test]
    fn premature_eof_should_be_retryable_and_map_to_network_failure() {
        let error = HttpDownloadJobError::PrematureEof {
            index: 2,
            expected: 2048,
            actual: 1024,
        };

        assert!(!error.requires_manual_action());
        assert_eq!(error.to_failure().kind, FailureKind::Network);
        assert!(error.to_string().contains("part `2`"));
        assert!(error.to_string().contains("2048"));
        assert!(error.to_string().contains("1024"));
    }

    #[tokio::test]
    async fn ranged_job_should_retry_on_premature_eof_and_complete_successfully()
    -> HttpDownloadJobResult<()> {
        let body = (0..=255).cycle().take(8 * 1024 * 1024).collect::<Vec<u8>>();
        let server = TestServer::spawn_with_premature_eof_failures(body.clone(), 1, false).await?;
        let db = test_db().await?;
        let folder = tempfile::tempdir()?;
        let mut item = sample_item(server.url(), folder.path().to_path_buf(), 0);
        item.preferred_connections = Some(four_connections());

        let live_progress = Arc::new(LiveDownloadProgress::new(Bytes::ZERO));
        db.download_repo.add(&item).await?;
        let completed = HttpDownloadJob::new(
            db.download_repo.clone(),
            db.part_repo.clone(),
            ReqwestHttpClient::new()?,
            item.clone(),
        )
        .with_live_progress(Arc::clone(&live_progress))
        .run(CancellationToken::new())
        .await?;

        let final_bytes = fs::read(folder.path().join("file.bin")).await?;
        let parts = db.part_repo.list_for_download(item.id).await?;

        assert_eq!(completed.status, DownloadStatus::Completed);
        assert_eq!(final_bytes, body);
        assert!(parts.len() > 1);
        assert!(parts.iter().any(|part| part.retry_count > 0));
        assert_contiguous_completed_ranges(&parts, body.len() as u64);

        let (live_downloaded, active_parts) = live_progress.snapshot();
        assert_eq!(live_downloaded.get(), body.len() as u64);
        assert_eq!(active_parts, 0);
        Ok(())
    }

    #[tokio::test]
    async fn resumed_retry_after_premature_eof_should_not_double_count_live_progress()
    -> HttpDownloadJobResult<()> {
        let total_len = 2 * 1024 * 1024;
        let body = (0..=255).cycle().take(total_len).collect::<Vec<u8>>();
        let server = TestServer::spawn_with_premature_eof_failures(body.clone(), 2, false).await?;
        let db = test_db().await?;
        let folder = tempfile::tempdir()?;
        let mut item = sample_item(server.url(), folder.path().to_path_buf(), 0);
        item.preferred_connections = Some(four_connections());

        let live_progress = Arc::new(LiveDownloadProgress::new(Bytes::ZERO));
        db.download_repo.add(&item).await?;
        let completed = HttpDownloadJob::new(
            db.download_repo.clone(),
            db.part_repo.clone(),
            ReqwestHttpClient::new()?,
            item.clone(),
        )
        .with_live_progress(Arc::clone(&live_progress))
        .run(CancellationToken::new())
        .await?;

        let final_bytes = fs::read(folder.path().join("file.bin")).await?;
        let parts = db.part_repo.list_for_download(item.id).await?;

        assert_eq!(completed.status, DownloadStatus::Completed);
        assert_eq!(final_bytes, body);
        assert!(parts.iter().any(|part| part.retry_count > 0));

        let (live_downloaded, active_parts) = live_progress.snapshot();
        assert_eq!(live_downloaded.get(), total_len as u64);
        assert_eq!(active_parts, 0);
        Ok(())
    }

    #[tokio::test]
    async fn ranged_job_should_fail_when_premature_eof_retries_exhausted()
    -> HttpDownloadJobResult<()> {
        let body = (0..=255).cycle().take(8 * 1024 * 1024).collect::<Vec<u8>>();
        let server = TestServer::spawn_with_premature_eof_failures(body.clone(), 10, false).await?;
        let db = test_db().await?;
        let folder = tempfile::tempdir()?;
        let mut item = sample_item(server.url(), folder.path().to_path_buf(), 0);
        item.preferred_connections = Some(four_connections());

        let paused = run_job(&db, item.clone(), CancellationToken::new()).await?;
        let final_path = folder.path().join("file.bin");
        let parts = db.part_repo.list_for_download(item.id).await?;

        assert_eq!(paused.status, DownloadStatus::Paused);
        let failure = paused
            .failure
            .expect("must record failure on retry exhaustion");
        assert_eq!(failure.kind, FailureKind::TooManyRetries);
        assert!(failure.message.contains("too many retries"));
        assert!(failure.message.contains("ended prematurely"));
        assert!(!path_exists(&final_path).await?);
        assert!(!parts.iter().all(part_is_complete));
        assert!(parts.iter().any(|part| part.retry_count == 3));
        Ok(())
    }

    #[tokio::test]
    async fn ranged_job_should_resume_premature_eof_after_pending_split()
    -> HttpDownloadJobResult<()> {
        let completed_len = 1024 * 1024;
        let total_len = 8 * 1024 * 1024;
        let body = (0..=255).cycle().take(total_len).collect::<Vec<u8>>();
        let server = TestServer::spawn_with_premature_eof_failures(body.clone(), 1, false).await?;
        let db = test_db().await?;
        let folder = tempfile::tempdir()?;
        let mut item = sample_item(
            server.url(),
            folder.path().to_path_buf(),
            completed_len as u64,
        );
        item.preferred_connections = Some(four_connections());
        item.total_bytes = Some(Bytes::new(total_len as u64));
        item.etag = Some("\"job-etag\"".to_owned());
        db.download_repo.add(&item).await?;
        let now = now_ms()?;
        let parts = vec![
            DownloadPart {
                id: PartId::new(65_537),
                download_id: item.id,
                index: 0,
                start_byte: Bytes::ZERO,
                end_byte: Some(Bytes::new(completed_len as u64 - 1)),
                current_byte: Bytes::new(completed_len as u64),
                status: PartStatus::Completed,
                retry_count: 0,
                updated_at: now,
            },
            DownloadPart {
                id: PartId::new(65_538),
                download_id: item.id,
                index: 1,
                start_byte: Bytes::new(completed_len as u64),
                end_byte: Some(Bytes::new(total_len as u64 - 1)),
                current_byte: Bytes::new(completed_len as u64),
                status: PartStatus::Idle,
                retry_count: 0,
                updated_at: now,
            },
        ];
        db.part_repo.set_for_download(item.id, &parts).await?;
        let mut partial = vec![0_u8; total_len];
        partial[..completed_len].copy_from_slice(&body[..completed_len]);
        fs::write(folder.path().join("file.bin.raijin-part"), partial).await?;
        let mut job = HttpDownloadJob::new(
            db.download_repo.clone(),
            db.part_repo.clone(),
            ReqwestHttpClient::new()?,
            item.clone(),
        );
        job.config.min_part_size = Bytes::new(1024 * 1024);

        let completed = job.run(CancellationToken::new()).await?;
        let final_bytes = fs::read(folder.path().join("file.bin")).await?;
        let persisted_parts = db.part_repo.list_for_download(item.id).await?;

        assert_eq!(completed.status, DownloadStatus::Completed);
        assert_eq!(final_bytes, body);
        assert!(persisted_parts.len() > parts.len());
        assert!(persisted_parts.iter().any(|part| part.retry_count > 0));
        assert_contiguous_completed_ranges(&persisted_parts, total_len as u64);
        Ok(())
    }

    #[tokio::test]
    async fn part_worker_should_fail_with_committed_split_boundary_on_premature_eof()
    -> HttpDownloadJobResult<()> {
        let total_len = 256 * 1024;
        let original_end = (total_len - 1) as u64;
        let body = (0..=255).cycle().take(total_len).collect::<Vec<u8>>();
        let server = TestServer::spawn_with_premature_eof_failures(body.clone(), 1, true).await?;
        let db = test_db().await?;
        let folder = tempfile::tempdir()?;
        let incomplete_path = folder.path().join("file.bin.raijin-part");

        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&incomplete_path)
            .await?;
        file.set_len(total_len as u64).await?;
        drop(file);

        let mut item = sample_item(server.url(), folder.path().to_path_buf(), 0);
        item.total_bytes = Some(Bytes::new(total_len as u64));
        item.etag = Some("\"job-etag\"".to_owned());
        db.download_repo.add(&item).await?;

        let now = now_ms()?;
        let part = DownloadPart {
            id: PartId::new(65_537),
            download_id: item.id,
            index: 0,
            start_byte: Bytes::ZERO,
            end_byte: Some(Bytes::new(original_end)),
            current_byte: Bytes::ZERO,
            status: PartStatus::Idle,
            retry_count: 0,
            updated_at: now,
        };
        db.part_repo
            .set_for_download(item.id, std::slice::from_ref(&part))
            .await?;

        let (control_tx, control_rx) = mpsc::channel(1);
        let active_state = Arc::new(ActivePartState::new(&part));
        let mut worker_controls = std::collections::HashMap::new();
        worker_controls.insert(
            0,
            ActivePartControl {
                tx: control_tx,
                state: Arc::clone(&active_state),
            },
        );

        let cancellation = CancellationToken::new();
        let request = PartWorkerRequest {
            part_repo: db.part_repo.clone(),
            client: ReqwestHttpClient::new()?,
            url: item.url.clone(),
            headers: item.headers.clone(),
            path: incomplete_path.clone(),
            part,
            cancellation: cancellation.clone(),
            progress_interval: Duration::from_millis(10),
            speed_limiter: None,
            global_limiter: None,
            live_progress: None,
            control_rx,
            active_state: Arc::clone(&active_state),
        };

        let mut workers = JoinSet::new();
        workers.spawn(async move { part_worker(request).await });

        let mut progress_seen = false;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if active_state.current_byte.load(AtomicOrdering::Relaxed) > 0 {
                progress_seen = true;
                break;
            }
        }
        assert!(
            progress_seen,
            "worker must make progress before in-flight split"
        );

        let mut next_part_index = 1;
        let child = request_inflight_split(
            &worker_controls,
            &db.part_repo,
            &mut next_part_index,
            Bytes::new(4 * 1024),
            now_ms()?,
            &cancellation,
        )
        .await?;
        let child = child.expect("in-flight split must succeed");

        let worker_joined = tokio::time::timeout(Duration::from_secs(5), workers.join_next())
            .await
            .expect("worker must finish within timeout")
            .expect("worker joined")
            .expect("worker task must not panic")?;

        let (failed_parent, error) = match worker_joined {
            PartWorkerOutcome::Failed(parent, error) => (parent, error),
            other => panic!("expected worker to fail with premature EOF, got {other:?}"),
        };

        let (index, expected, actual) = match error {
            HttpDownloadJobError::PrematureEof {
                index,
                expected,
                actual,
            } => (index, expected, actual),
            other => panic!("expected PrematureEof error, got {other:?}"),
        };

        assert_eq!(index, 0);

        let persisted_parts = db.part_repo.list_for_download(item.id).await?;
        let persisted_parent = persisted_parts
            .iter()
            .find(|p| p.index == 0)
            .expect("parent part exists in database");
        let persisted_child = persisted_parts
            .iter()
            .find(|p| p.index == child.index)
            .expect("child part exists in database");

        let parent_end = persisted_parent
            .end_byte
            .expect("parent has end_byte")
            .get();
        let parent_exclusive_end = parent_end.saturating_add(1);

        assert_eq!(expected, parent_exclusive_end);
        assert_ne!(parent_end, original_end);
        assert!(actual < expected);
        assert_eq!(failed_parent.current_byte, Bytes::new(actual));
        assert_eq!(child.status, PartStatus::Idle);
        assert_eq!(persisted_child.status, PartStatus::Idle);
        assert_eq!(child.start_byte.get(), parent_exclusive_end);
        assert_eq!(
            child.end_byte.expect("child has end_byte").get(),
            original_end
        );
        assert_eq!(persisted_child.start_byte, child.start_byte);
        assert_eq!(persisted_child.end_byte, child.end_byte);
        assert_eq!(failed_parent.end_byte, persisted_parent.end_byte);
        assert_eq!(failed_parent.end_byte, Some(Bytes::new(parent_end)));

        Ok(())
    }
}

#[cfg(test)]
mod baseline;

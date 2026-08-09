use std::io::SeekFrom;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::StatusCode;
use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use thiserror::Error;
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::db::{DbError, DownloadRepository, PartRepository};
use crate::download::{
    Bytes, DownloadFailure, DownloadItem, DownloadPart, DownloadStatus, FailureKind, PartId,
    PartStatus, ProbeRequest, ReqwestHttpClient, ResumeSupport, ResumeValidation, probe_http,
    split_fixed_ranges, validate_resume_metadata,
};

const DEFAULT_PROGRESS_INTERVAL: Duration = Duration::from_millis(500);
const SINGLE_PART_INDEX: u32 = 0;

/// Result type returned by single-connection HTTP jobs.
pub type HttpDownloadJobResult<T> = Result<T, HttpDownloadJobError>;

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
}

/// Single-connection HTTP job that streams one persisted download into an incomplete file.
#[derive(Debug, Clone)]
pub struct HttpDownloadJob {
    download_repo: DownloadRepository,
    part_repo: PartRepository,
    client: ReqwestHttpClient,
    item: DownloadItem,
    config: HttpDownloadJobConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HttpDownloadJobConfig {
    incomplete_extension: String,
    progress_interval: Duration,
    max_retries: u32,
}

impl Default for HttpDownloadJobConfig {
    fn default() -> Self {
        Self {
            incomplete_extension: crate::download::DownloadConfig::default().incomplete_extension,
            progress_interval: DEFAULT_PROGRESS_INTERVAL,
            max_retries: crate::download::DownloadConfig::default().max_retries.get(),
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
        }
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
        let result = self.run_inner(&cancellation).await;
        match result {
            Ok(item) => Ok(item),
            Err(_) if cancellation.is_cancelled() => {
                self.persist_paused().await?;
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
        let final_path = self.final_path();
        let incomplete_path = self.incomplete_path();
        fs::create_dir_all(&self.item.folder).await?;

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
                    file.write_all(&chunk).await?;
                    downloaded = downloaded
                        .checked_add(u64::try_from(chunk.len()).map_err(|_| HttpDownloadJobError::ByteCountOverflow)?)
                        .ok_or(HttpDownloadJobError::ByteCountOverflow)?;
                    self.item.downloaded_bytes = Bytes::new(downloaded);
                    part.current_byte = Bytes::new(downloaded);
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

        fs::rename(&incomplete_path, &final_path).await?;
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
        let mut parts = if existing_parts.is_empty() || existing_parts.len() == 1 {
            let desired_parts = self.preferred_connections().get();
            split_fixed_ranges(self.item.id, Some(total), desired_parts, now_ms()?)?
        } else {
            existing_parts
        };
        self.part_repo
            .set_for_download(self.item.id, &parts)
            .await?;

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

        let mut pending = parts
            .drain(..)
            .filter(|part| !part_is_complete(part))
            .collect::<std::collections::VecDeque<_>>();
        let mut workers = JoinSet::new();
        let limit = usize::from(self.preferred_connections().get());
        let mut first_error = None;

        loop {
            while workers.len() < limit {
                let Some(part) = pending.pop_front() else {
                    break;
                };
                workers.spawn(part_worker(PartWorkerRequest {
                    part_repo: self.part_repo.clone(),
                    client: self.client.clone(),
                    url: self.item.url.clone(),
                    headers: self.item.headers.clone(),
                    path: incomplete_path.clone(),
                    part,
                    cancellation: cancellation.clone(),
                    progress_interval: self.config.progress_interval,
                }));
            }

            if workers.is_empty() {
                break;
            }

            let outcome = workers
                .join_next()
                .await
                .ok_or(HttpDownloadJobError::ResumeUnsupported)???;
            match outcome {
                PartWorkerOutcome::Completed => {}
                PartWorkerOutcome::Canceled => {
                    workers.abort_all();
                    self.persist_ranged_paused().await?;
                    return Ok(self.item.clone());
                }
                PartWorkerOutcome::Failed(mut part, error) => {
                    if part.retry_count < self.config.max_retries {
                        part.retry_count += 1;
                        part.status = PartStatus::Idle;
                        part.updated_at = now_ms()?;
                        self.part_repo.set(&part).await?;
                        pending.push_back(part);
                    } else {
                        first_error = Some(error);
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
        fs::rename(incomplete_path, final_path).await?;
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
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| HttpDownloadJobError::InvalidHeaderName(name.clone()))?;
        let value = HeaderValue::from_str(value)
            .map_err(|_| HttpDownloadJobError::InvalidHeaderValue(name.to_string()))?;
        map.insert(name, value);
    }
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
        HttpDownloadJobError::Request(_) | HttpDownloadJobError::UnexpectedStatus(_) => {
            FailureKind::Network
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
    }
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

    request.part.status = PartStatus::Connecting;
    request.part.updated_at = now_ms().map_err(|error| worker_failure(&request.part, error))?;
    request
        .part_repo
        .set(&request.part)
        .await
        .map_err(|error| worker_failure(&request.part, error.into()))?;

    let start = request.part.current_byte.get();
    let end = request
        .part
        .end_byte
        .ok_or_else(|| worker_failure(&request.part, HttpDownloadJobError::ResumeUnsupported))?
        .get();
    let mut response = request
        .client
        .client()
        .get(&request.url)
        .headers(
            request_headers(&request.headers)
                .map_err(|error| worker_failure(&request.part, error))?,
        )
        .header(header::RANGE, format!("bytes={start}-{end}"))
        .send()
        .await
        .map_err(|error| worker_failure(&request.part, error.into()))?;
    if response.status() != StatusCode::PARTIAL_CONTENT {
        return Err(Box::new(PartWorkerFailure {
            part: request.part,
            error: HttpDownloadJobError::UnexpectedStatus(response.status().as_u16()),
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
            chunk = response.chunk() => {
                let Some(chunk) = chunk.map_err(|error| worker_failure(&request.part, error.into()))? else {
                    break;
                };
                file.write_all(&chunk).await.map_err(|error| worker_failure(&request.part, error.into()))?;
                let current = request.part.current_byte.get()
                    .checked_add(u64::try_from(chunk.len()).map_err(|_| worker_failure(&request.part, HttpDownloadJobError::ByteCountOverflow))?)
                    .ok_or_else(|| worker_failure(&request.part, HttpDownloadJobError::ByteCountOverflow))?;
                request.part.current_byte = Bytes::new(current);
                if last_persisted.elapsed() >= request.progress_interval {
                    request.part.updated_at = now_ms().map_err(|error| worker_failure(&request.part, error))?;
                    request.part_repo.set(&request.part).await.map_err(|error| worker_failure(&request.part, error.into()))?;
                    last_persisted = tokio::time::Instant::now();
                }
            }
        }
    }

    request.part.status = PartStatus::Completed;
    request.part.updated_at = now_ms().map_err(|error| worker_failure(&request.part, error))?;
    request
        .part_repo
        .set(&request.part)
        .await
        .map_err(|error| worker_failure(&request.part, error.into()))?;
    file.flush()
        .await
        .map_err(|error| worker_failure(&request.part, error.into()))?;
    Ok(PartWorkerOutcome::Completed)
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

    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::task::JoinHandle;

    use super::*;
    use crate::db;
    use crate::download::DownloadKind;

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
            let end = range
                .end
                .and_then(|end| usize::try_from(end).ok())
                .unwrap_or(total - 1)
                .min(total - 1);
            return (
                206,
                "Partial Content",
                &state.body[start..=end],
                Some(format!("bytes {start}-{end}/{total}")),
            );
        }

        (200, "OK", &state.body, None)
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

    fn sample_item(url: String, folder: PathBuf, downloaded: u64) -> DownloadItem {
        DownloadItem {
            id: crate::download::DownloadId::new(1),
            kind: DownloadKind::Http,
            url,
            download_page: None,
            headers: BTreeMap::new(),
            file_name: "file.bin".to_owned(),
            folder,
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
        let body = (0..=255).cycle().take(1024).collect::<Vec<u8>>();
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
        Ok(())
    }
}

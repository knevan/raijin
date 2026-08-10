pub mod http;
pub mod job;
pub mod manager;
pub mod model;
pub mod part;
pub mod speed;

pub use http::{
    ByteRange, ContentRange, HttpClient, HttpMetadata, HttpProbeError, HttpRequest, HttpResponse,
    ProbeRequest, ReqwestHttpClient, ResumeValidation, probe_http, validate_resume_metadata,
};
pub use job::{HttpDownloadJob, HttpDownloadJobError, HttpDownloadJobResult};
pub use manager::{
    DEFAULT_COMMAND_BUFFER, DEFAULT_EVENT_BUFFER, DownloadCommand, DownloadEvent,
    DownloadManagerError, DownloadManagerHandle, DownloadManagerOptions, DownloadManagerResult,
    NewDownload,
};
pub use model::{
    Bytes, BytesPerSecond, DownloadConfig, DownloadFailure, DownloadId, DownloadItem, DownloadKind,
    DownloadPart, DownloadProgress, DownloadStatus, FailureKind, ParseDomainEnumError, PartId,
    PartStatus, QueueId, ResumeSupport,
};
pub use part::{RangeSplitError, split_fixed_ranges, split_fixed_ranges_with_min_part_size};
pub use speed::SpeedLimiter;

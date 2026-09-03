use std::collections::BTreeMap;
use std::fmt;
use std::num::{NonZeroU16, NonZeroU32};
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Stable identifier for a persisted download.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[repr(transparent)]
pub struct DownloadId(i64);

impl DownloadId {
    /// Creates a download identifier from its persisted representation.
    #[must_use]
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    /// Returns the persisted identifier value.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl From<i64> for DownloadId {
    fn from(value: i64) -> Self {
        Self::new(value)
    }
}

impl From<DownloadId> for i64 {
    fn from(value: DownloadId) -> Self {
        value.get()
    }
}

impl fmt::Display for DownloadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Stable identifier for a persisted queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[repr(transparent)]
pub struct QueueId(i64);

impl QueueId {
    /// Identifier reserved for the default main queue.
    pub const MAIN: Self = Self(0);

    /// Creates a queue identifier from its persisted representation.
    #[must_use]
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    /// Returns the persisted identifier value.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl From<i64> for QueueId {
    fn from(value: i64) -> Self {
        Self::new(value)
    }
}

impl From<QueueId> for i64 {
    fn from(value: QueueId) -> Self {
        value.get()
    }
}

impl fmt::Display for QueueId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Stable identifier for a persisted download part.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[repr(transparent)]
pub struct PartId(i64);

impl PartId {
    /// Creates a part identifier from its persisted representation.
    #[must_use]
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    /// Returns the persisted identifier value.
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
}

impl From<i64> for PartId {
    fn from(value: i64) -> Self {
        Self::new(value)
    }
}

impl From<PartId> for i64 {
    fn from(value: PartId) -> Self {
        value.get()
    }
}

impl fmt::Display for PartId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Byte count value.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[repr(transparent)]
pub struct Bytes(u64);

impl Bytes {
    /// Zero bytes.
    pub const ZERO: Self = Self(0);

    /// Creates a byte count.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the raw byte count.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for Bytes {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

impl From<Bytes> for u64 {
    fn from(value: Bytes) -> Self {
        value.get()
    }
}

impl fmt::Display for Bytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Byte throughput value.
#[derive(
    Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[repr(transparent)]
pub struct BytesPerSecond(u64);

impl BytesPerSecond {
    /// No throughput.
    pub const ZERO: Self = Self(0);

    /// Creates a throughput value in bytes per second.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the raw bytes-per-second value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for BytesPerSecond {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

impl From<BytesPerSecond> for u64 {
    fn from(value: BytesPerSecond) -> Self {
        value.get()
    }
}

impl fmt::Display for BytesPerSecond {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Error returned when a persisted enum value is unknown.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("unknown {kind} value `{value}`")]
pub struct ParseDomainEnumError {
    kind: &'static str,
    value: String,
}

impl ParseDomainEnumError {
    #[must_use]
    fn new(kind: &'static str, value: &str) -> Self {
        Self {
            kind,
            value: value.to_owned(),
        }
    }
}

macro_rules! persisted_enum {
    (
        $(#[$meta:meta])*
        pub enum $name:ident {
            $( $variant:ident => $value:literal, )+
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $( $variant, )+
        }

        impl $name {
            /// Returns the stable persisted string value.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self {
                    $( Self::$variant => $value, )+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = ParseDomainEnumError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $( $value => Ok(Self::$variant), )+
                    _ => Err(ParseDomainEnumError::new(stringify!($name), value)),
                }
            }
        }

        impl TryFrom<&str> for $name {
            type Error = ParseDomainEnumError;

            fn try_from(value: &str) -> Result<Self, ParseDomainEnumError> {
                value.parse()
            }
        }
    };
}

persisted_enum! {
    /// Type of download handled by the engine registry.
    pub enum DownloadKind {
        Http => "http",
        Hls => "hls",
        Aria2 => "aria2",
    }
}

persisted_enum! {
    /// Durable lifecycle state for a download item.
    pub enum DownloadStatus {
        Added => "added",
        Queued => "queued",
        Downloading => "downloading",
        Paused => "paused",
        Retrying => "retrying",
        PreparingFile => "preparing_file",
        Completed => "completed",
        Error => "error",
        Removed => "removed",
    }
}

persisted_enum! {
    /// Durable lifecycle state for a download part.
    pub enum PartStatus {
        Idle => "idle",
        Connecting => "connecting",
        Receiving => "receiving",
        Completed => "completed",
        Canceled => "canceled",
        Error => "error",
    }
}

persisted_enum! {
    /// Current resume capability known for a remote resource.
    pub enum ResumeSupport {
        Unknown => "unknown",
        Supported => "supported",
        Unsupported => "unsupported",
    }
}

persisted_enum! {
    /// Persisted failure category for a download.
    pub enum FailureKind {
        Network => "network",
        Server => "server",
        Validation => "validation",
        Disk => "disk",
        Canceled => "canceled",
        NoSpace => "no_space",
        TooManyRetries => "too_many_retries",
    }
}

/// Persisted failure details for a download.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadFailure {
    /// Machine-readable failure category.
    pub kind: FailureKind,
    /// Human-readable failure message suitable for logs and UI.
    pub message: String,
}

/// Persisted download metadata and aggregate progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadItem {
    /// Stable persisted identifier.
    pub id: DownloadId,
    /// Download implementation kind.
    pub kind: DownloadKind,
    /// Primary source URL.
    pub url: String,
    /// Optional page URL where the download was discovered.
    pub download_page: Option<String>,
    /// Request headers captured for authenticated or browser-originated downloads.
    pub headers: BTreeMap<String, String>,
    /// Final file name without parent folder.
    pub file_name: String,
    /// Destination folder.
    pub folder: PathBuf,
    /// Durable lifecycle state.
    pub status: DownloadStatus,
    /// Known total size, if available.
    pub total_bytes: Option<Bytes>,
    /// Aggregate downloaded bytes.
    pub downloaded_bytes: Bytes,
    /// Last validated ETag.
    pub etag: Option<String>,
    /// Last validated Last-Modified value.
    pub last_modified: Option<String>,
    /// Preferred worker count for ranged downloads.
    pub preferred_connections: Option<NonZeroU16>,
    /// Optional per-download throughput limit.
    pub speed_limit: Option<BytesPerSecond>,
    /// Last persisted failure.
    pub failure: Option<DownloadFailure>,
    /// Creation timestamp as Unix milliseconds.
    pub created_at: i64,
    /// Last start timestamp as Unix milliseconds.
    pub started_at: Option<i64>,
    /// Completion timestamp as Unix milliseconds.
    pub completed_at: Option<i64>,
    /// Last update timestamp as Unix milliseconds.
    pub updated_at: i64,
}

/// Persisted state for one ranged or blind download part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadPart {
    /// Stable persisted part identifier.
    pub id: PartId,
    /// Parent download identifier.
    pub download_id: DownloadId,
    /// Stable part order within a download.
    pub index: u32,
    /// Inclusive starting byte offset.
    pub start_byte: Bytes,
    /// Inclusive ending byte offset, when known.
    pub end_byte: Option<Bytes>,
    /// Next byte offset to write.
    pub current_byte: Bytes,
    /// Durable lifecycle state.
    pub status: PartStatus,
    /// Number of retries already attempted.
    pub retry_count: u32,
    /// Last update timestamp as Unix milliseconds.
    pub updated_at: i64,
}

/// Global download defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadConfig {
    /// Default worker count for ranged downloads.
    pub default_connections: NonZeroU16,
    /// Whether downloads without an explicit connection count use adaptive ranged workers.
    pub adaptive_connections_enabled: bool,
    /// Minimum adaptive ranged worker count.
    pub min_connections: NonZeroU16,
    /// Maximum ranged worker count, including explicit custom values.
    pub max_connections: NonZeroU16,
    /// Probe interval for adaptive connection decisions, in milliseconds.
    pub connection_probe_interval_ms: u64,
    /// Required aggregate throughput gain percentage before probing more workers.
    pub connection_gain_threshold: u8,
    /// Minimum bytes per ranged part.
    pub min_part_size: Bytes,
    /// Maximum retries per failing part or job stage.
    pub max_retries: NonZeroU32,
    /// Extension appended to incomplete files.
    pub incomplete_extension: String,
    /// Whether sparse file allocation may be used when supported safely.
    pub sparse_file_allocation: bool,
    /// Optional global throughput limit.
    pub global_speed_limit: Option<BytesPerSecond>,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            default_connections: non_zero_u16(8),
            adaptive_connections_enabled: true,
            min_connections: non_zero_u16(1),
            max_connections: non_zero_u16(16),
            connection_probe_interval_ms: 1_000,
            connection_gain_threshold: 10,
            min_part_size: Bytes::new(4 * 1024 * 1024),
            max_retries: non_zero_u32(3),
            incomplete_extension: ".raijin-part".to_owned(),
            sparse_file_allocation: false,
            global_speed_limit: None,
        }
    }
}

/// Aggregated progress for events and monitor projections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadProgress {
    /// Downloaded bytes across all parts.
    pub downloaded_bytes: Bytes,
    /// Known total size, if available.
    pub total_bytes: Option<Bytes>,
    /// Current aggregate speed.
    pub speed: BytesPerSecond,
    /// Estimated seconds until completion, if computable.
    pub eta_seconds: Option<u64>,
    /// Number of active part workers.
    pub active_part_count: u16,
}

const fn non_zero_u16(value: u16) -> NonZeroU16 {
    match NonZeroU16::new(value) {
        Some(value) => value,
        None => panic!("non-zero u16 default must be greater than zero"),
    }
}

const fn non_zero_u32(value: u32) -> NonZeroU32 {
    match NonZeroU32::new(value) {
        Some(value) => value,
        None => panic!("non-zero u32 default must be greater than zero"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! assert_enum_mapping {
        ($ty:ty, [$(($variant:path, $value:literal)),+ $(,)?]) => {
            $(
                assert_eq!($variant.as_str(), $value);
                assert_eq!($variant.to_string(), $value);
                assert_eq!(<$ty>::from_str($value), Ok($variant));
                assert_eq!(<$ty>::try_from($value), Ok($variant));
            )+
        };
    }

    #[test]
    fn download_kind_should_round_trip_persisted_values() {
        assert_enum_mapping!(
            DownloadKind,
            [
                (DownloadKind::Http, "http"),
                (DownloadKind::Hls, "hls"),
                (DownloadKind::Aria2, "aria2"),
            ]
        );
    }

    #[test]
    fn download_status_should_round_trip_persisted_values() {
        assert_enum_mapping!(
            DownloadStatus,
            [
                (DownloadStatus::Added, "added"),
                (DownloadStatus::Queued, "queued"),
                (DownloadStatus::Downloading, "downloading"),
                (DownloadStatus::Paused, "paused"),
                (DownloadStatus::Retrying, "retrying"),
                (DownloadStatus::PreparingFile, "preparing_file"),
                (DownloadStatus::Completed, "completed"),
                (DownloadStatus::Error, "error"),
                (DownloadStatus::Removed, "removed"),
            ]
        );
    }

    #[test]
    fn part_status_should_round_trip_persisted_values() {
        assert_enum_mapping!(
            PartStatus,
            [
                (PartStatus::Idle, "idle"),
                (PartStatus::Connecting, "connecting"),
                (PartStatus::Receiving, "receiving"),
                (PartStatus::Completed, "completed"),
                (PartStatus::Canceled, "canceled"),
                (PartStatus::Error, "error"),
            ]
        );
    }

    #[test]
    fn resume_support_should_round_trip_persisted_values() {
        assert_enum_mapping!(
            ResumeSupport,
            [
                (ResumeSupport::Unknown, "unknown"),
                (ResumeSupport::Supported, "supported"),
                (ResumeSupport::Unsupported, "unsupported"),
            ]
        );
    }

    #[test]
    fn failure_kind_should_round_trip_persisted_values() {
        assert_enum_mapping!(
            FailureKind,
            [
                (FailureKind::Network, "network"),
                (FailureKind::Server, "server"),
                (FailureKind::Validation, "validation"),
                (FailureKind::Disk, "disk"),
                (FailureKind::Canceled, "canceled"),
                (FailureKind::NoSpace, "no_space"),
                (FailureKind::TooManyRetries, "too_many_retries"),
            ]
        );
    }

    #[test]
    fn enum_parse_should_reject_unknown_value() {
        let error = DownloadStatus::from_str("finished").err();

        assert_eq!(
            error.map(|error| error.to_string()),
            Some("unknown DownloadStatus value `finished`".to_owned())
        );
    }
}

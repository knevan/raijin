//! Minimal performance baseline comparing Adaptive, Single, and Fixed-4 download strategies.
//! Detailed methodology and benchmarks documented in `docs/dynamic-splitting-improvements.md`.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::download::speed::SpeedLimiter;
use crate::download::{BytesPerSecond, DownloadId, DownloadKind};

const CHUNK_SIZE: usize = 256 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const RUN_TIMEOUT: Duration = Duration::from_secs(60);
const CSV_HEADER: &str = "scenario,run,strategy,first_byte_upper_ms,duration_ms,throughput_mib_s,probe_requests,range_requests,sampled_peak_workers,worker_transitions,initial_target,first_progress_workers";
const BASELINE_STRATEGIES: &[Strategy] = &[Strategy::Adaptive, Strategy::Single, Strategy::Fixed4];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Strategy {
    Adaptive,
    AdaptiveFloor,
    Single,
    Fixed4,
    Fixed8,
}

impl Strategy {
    fn name(self) -> &'static str {
        match self {
            Self::Adaptive => "Adaptive",
            Self::AdaptiveFloor => "Adaptive-Floor",
            Self::Single => "Single",
            Self::Fixed4 => "Fixed-4",
            Self::Fixed8 => "Fixed-8",
        }
    }

    fn preferred_connections(self) -> Option<NonZeroU16> {
        match self {
            Self::Adaptive | Self::AdaptiveFloor => None,
            Self::Single => NonZeroU16::new(1),
            Self::Fixed4 => NonZeroU16::new(4),
            Self::Fixed8 => NonZeroU16::new(8),
        }
    }
}

#[derive(Clone)]
enum ServerThrottle {
    Uncapped,
    Shared(BytesPerSecond),
    PerConnection(BytesPerSecond),
}

struct BaselineServer {
    addr: SocketAddr,
    task: JoinHandle<()>,
    probe_requests: Arc<AtomicUsize>,
    range_requests: Arc<AtomicUsize>,
}

impl BaselineServer {
    async fn spawn(
        body: Vec<u8>,
        throttle: ServerThrottle,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let probe_requests = Arc::new(AtomicUsize::new(0));
        let range_requests = Arc::new(AtomicUsize::new(0));
        let body = Arc::new(body);
        let shared_limiter = match throttle {
            ServerThrottle::Shared(rate) => SpeedLimiter::new(rate),
            _ => None,
        };
        let probes = Arc::clone(&probe_requests);
        let ranges = Arc::clone(&range_requests);

        let task = tokio::spawn(async move {
            let mut conn_tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accept_res = listener.accept() => {
                        let Ok((stream, _)) = accept_res else { break; };
                        let body = Arc::clone(&body);
                        let probes = Arc::clone(&probes);
                        let ranges = Arc::clone(&ranges);
                        let limiter = match throttle {
                            ServerThrottle::Uncapped => None,
                            ServerThrottle::Shared(_) => shared_limiter.clone(),
                            ServerThrottle::PerConnection(rate) => SpeedLimiter::new(rate),
                        };
                        conn_tasks.spawn(async move {
                            let _ = handle_connection(stream, body, limiter, probes, ranges).await;
                        });
                    }
                    Some(_) = conn_tasks.join_next(), if !conn_tasks.is_empty() => {}
                }
            }
            conn_tasks.abort_all();
        });

        Ok(Self {
            addr,
            task,
            probe_requests,
            range_requests,
        })
    }

    fn url(&self) -> String {
        format!("http://{}/file.bin", self.addr)
    }

    fn probe_requests(&self) -> usize {
        self.probe_requests.load(Ordering::Relaxed)
    }

    fn range_requests(&self) -> usize {
        self.range_requests.load(Ordering::Relaxed)
    }
}

impl Drop for BaselineServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<String>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    while buf.len() < 8192 {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(Some(String::from_utf8_lossy(&buf).into_owned()));
        }
    }
    if buf.is_empty() {
        Ok(None)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "incomplete or oversized request headers",
        ))
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    body: Arc<Vec<u8>>,
    limiter: Option<Arc<SpeedLimiter>>,
    probe_requests: Arc<AtomicUsize>,
    range_requests: Arc<AtomicUsize>,
) -> std::io::Result<()> {
    let Some(req) = read_request(&mut stream).await? else {
        return Ok(());
    };
    let range = parse_requested_range(&req);
    let is_probe = is_probe_range(range);
    if is_probe {
        probe_requests.fetch_add(1, Ordering::Relaxed);
    } else {
        range_requests.fetch_add(1, Ordering::Relaxed);
    }

    let total = body.len();
    let (status, reason, slice, cr_hdr) = if let Some(r) = range {
        let start = usize::try_from(r.start).unwrap_or(0);
        let end = r
            .end
            .and_then(|e| usize::try_from(e).ok())
            .unwrap_or(total.saturating_sub(1))
            .min(total.saturating_sub(1));
        if start < total && start <= end {
            (
                206,
                "Partial Content",
                &body[start..=end],
                Some(format!("bytes {start}-{end}/{total}")),
            )
        } else {
            (416, "Range Not Satisfiable", &[][..], None)
        }
    } else {
        (200, "OK", &body[..], None)
    };

    let mut headers = format!(
        "HTTP/1.1 {status} {reason}\r\nConnection: close\r\nContent-Length: {}\r\nETag: \"baseline-etag\"\r\nContent-Type: application/octet-stream\r\n",
        slice.len()
    );
    if let Some(cr) = cr_hdr {
        headers.push_str("Content-Range: ");
        headers.push_str(&cr);
        headers.push_str("\r\n");
    }
    headers.push_str("\r\n");
    stream.write_all(headers.as_bytes()).await?;

    if is_probe {
        stream.write_all(slice).await?;
        return stream.shutdown().await;
    }

    for chunk in slice.chunks(CHUNK_SIZE) {
        if let Some(limiter) = &limiter {
            limiter.acquire(chunk.len() as u64).await;
        }
        stream.write_all(chunk).await?;
    }
    stream.shutdown().await
}

#[derive(Debug, Clone, Copy)]
struct RequestedRange {
    start: u64,
    end: Option<u64>,
}

fn parse_requested_range(request: &str) -> Option<RequestedRange> {
    request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if !name.eq_ignore_ascii_case("range") {
            return None;
        }
        let (s, e) = value.trim().strip_prefix("bytes=")?.split_once('-')?;
        Some(RequestedRange {
            start: s.parse().ok()?,
            end: if e.is_empty() {
                None
            } else {
                Some(e.parse().ok()?)
            },
        })
    })
}

fn is_probe_range(range: Option<RequestedRange>) -> bool {
    matches!(
        range,
        Some(RequestedRange {
            start: 0,
            end: Some(255)
        })
    )
}

struct TestDb {
    _dir: tempfile::TempDir,
    download_repo: DownloadRepository,
    part_repo: PartRepository,
}

async fn create_test_db() -> Result<TestDb, Box<dyn std::error::Error + Send + Sync>> {
    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("baseline.sqlite");
    let pool = crate::db::bootstrap(&format!("sqlite://{}", db_path.display())).await?;
    Ok(TestDb {
        _dir: dir,
        download_repo: DownloadRepository::new(pool.clone()),
        part_repo: PartRepository::new(pool),
    })
}

fn create_download_item(
    url: String,
    folder: PathBuf,
    preferred_connections: Option<NonZeroU16>,
) -> DownloadItem {
    DownloadItem {
        id: DownloadId::new(1),
        kind: DownloadKind::Http,
        url,
        download_page: None,
        headers: BTreeMap::new(),
        file_name: "file.bin".to_owned(),
        folder,
        category: None,
        status: DownloadStatus::Added,
        total_bytes: None,
        downloaded_bytes: Bytes::ZERO,
        etag: None,
        last_modified: None,
        preferred_connections,
        speed_limit: None,
        failure: None,
        created_at: 1,
        started_at: None,
        completed_at: None,
        updated_at: 1,
    }
}

async fn run_single_baseline(
    scenario_id: &str,
    run_index: usize,
    strategy: Strategy,
    payload: &[u8],
    throttle: &ServerThrottle,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let server = BaselineServer::spawn(payload.to_vec(), throttle.clone()).await?;
    let db = create_test_db().await?;
    let folder = tempfile::tempdir()?;
    let item = create_download_item(
        server.url(),
        folder.path().to_path_buf(),
        strategy.preferred_connections(),
    );
    db.download_repo.add(&item).await?;

    let live_progress = Arc::new(LiveDownloadProgress::new(Bytes::ZERO));
    let mut job = HttpDownloadJob::new(
        db.download_repo.clone(),
        db.part_repo.clone(),
        ReqwestHttpClient::new()?,
        item,
    )
    .with_live_progress(Arc::clone(&live_progress));

    if strategy == Strategy::AdaptiveFloor {
        job.config.default_connections =
            floor_startup_connections(&job.config, u64::try_from(payload.len())?);
    }
    let initial_target = if job.preferred_connections().get() == 1 {
        1
    } else {
        ConnectionController::new(
            &job.config,
            job.item.preferred_connections,
            u64::try_from(payload.len())?,
            0,
        )
        .target()
    };
    let start = Instant::now();
    let mut poll = tokio::time::interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let job_fut = job.run(CancellationToken::new());
    tokio::pin!(job_fut);

    let mut ttfb: Option<Duration> = None;
    let mut first_progress_workers = None;
    let mut peak_workers = 0_u16;
    let mut transitions: Vec<(u32, u16)> = Vec::new();
    let mut last_workers = 0_u16;

    let timeout_sleep = tokio::time::sleep(timeout);
    tokio::pin!(timeout_sleep);

    let completed = loop {
        tokio::select! {
            _ = &mut timeout_sleep => {
                panic!("baseline run timed out after {timeout:?}");
            }
            _ = poll.tick() => {
                let (downloaded, workers) = live_progress.snapshot();
                let elapsed = start.elapsed();
                if ttfb.is_none() && downloaded.get() > 0 {
                    ttfb = Some(elapsed);
                    first_progress_workers = Some(workers);
                }
                if workers != last_workers {
                    transitions.push((elapsed.as_millis() as u32, workers));
                    last_workers = workers;
                }
                if workers > peak_workers {
                    peak_workers = workers;
                }
            }
            res = &mut job_fut => {
                break res?;
            }
        }
    };

    let total_duration = start.elapsed();
    let downloaded_bytes = tokio::fs::read(&folder.path().join("file.bin")).await?;
    assert_eq!(downloaded_bytes, payload, "downloaded payload mismatch");
    assert_eq!(completed.status, DownloadStatus::Completed);

    let (final_bytes, _) = live_progress.snapshot();
    assert_eq!(final_bytes.get(), payload.len() as u64);
    if last_workers != 0 {
        transitions.push((total_duration.as_millis() as u32, 0));
    }
    let ttfb_upper = ttfb.unwrap_or(total_duration);

    let throughput_bps = if total_duration.as_secs_f64() > 0.0 {
        payload.len() as f64 / total_duration.as_secs_f64()
    } else {
        0.0
    };

    let transitions_str = transitions
        .iter()
        .map(|(ms, w)| format!("{ms}ms:{w}"))
        .collect::<Vec<_>>()
        .join("->");

    println!(
        "{scenario_id},{},{},{:.2},{:.2},{:.4},{},{},{peak_workers},\"{transitions_str}\",{initial_target},{}",
        run_index,
        strategy.name(),
        ttfb_upper.as_secs_f64() * 1000.0,
        total_duration.as_secs_f64() * 1000.0,
        throughput_bps / (1024.0 * 1024.0),
        server.probe_requests(),
        server.range_requests(),
        first_progress_workers
            .map(|workers| workers.to_string())
            .unwrap_or_default(),
    );

    Ok(())
}

async fn run_scenario(
    scenario_id: &str,
    payload_len: usize,
    throttle: ServerThrottle,
    strategies: &[Strategy],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    assert!(!strategies.is_empty());
    let payload = (0..=255).cycle().take(payload_len).collect::<Vec<u8>>();

    for rep in 0..3 {
        for offset in 0..strategies.len() {
            let strategy = strategies[(offset + rep) % strategies.len()];
            run_single_baseline(
                scenario_id,
                rep + 1,
                strategy,
                &payload,
                &throttle,
                RUN_TIMEOUT,
            )
            .await?;
        }
    }
    Ok(())
}

async fn run_long_scenario(
    scenario_id: &str,
    payload_len: usize,
    throttle: ServerThrottle,
    timeout: Duration,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let payload = (0..=255).cycle().take(payload_len).collect::<Vec<u8>>();
    let strategies = [Strategy::Adaptive, Strategy::Fixed8];

    for rep in 0..3 {
        for offset in 0..strategies.len() {
            let strategy = strategies[(offset + rep) % strategies.len()];
            run_single_baseline(scenario_id, rep + 1, strategy, &payload, &throttle, timeout)
                .await?;
        }
    }
    Ok(())
}

fn floor_startup_connections(config: &HttpDownloadJobConfig, remaining: u64) -> u16 {
    let capacity = (remaining / config.min_part_size.get().max(1)).max(1);
    config
        .default_connections
        .max(1)
        .min(u16::try_from(capacity).unwrap_or(u16::MAX))
}

#[test]
fn floor_startup_candidate_should_only_reduce_requested_default_at_size_boundaries() {
    let config = HttpDownloadJobConfig::default();
    let minimum = config.min_part_size.get();
    for (remaining, expected) in [
        (0, 1),
        (minimum - 1, 1),
        (minimum, 1),
        (minimum + 1, 1),
        (minimum * 4 - 1, 3),
        (minimum * 4, 4),
        (minimum * 4 + 1, 4),
        (
            minimum * u64::from(config.default_connections) + 1,
            config.default_connections,
        ),
        (u64::MAX, config.default_connections),
    ] {
        assert_eq!(floor_startup_connections(&config, remaining), expected);
    }
    let zero_minimum = HttpDownloadJobConfig {
        min_part_size: Bytes::ZERO,
        ..config
    };
    assert_eq!(
        floor_startup_connections(&zero_minimum, u64::MAX),
        zero_minimum.default_connections
    );
}

#[tokio::test]
#[ignore = "manual startup performance baseline"]
async fn run_startup_baseline() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_startup_matrix(BASELINE_STRATEGIES).await
}

#[tokio::test]
#[ignore = "manual startup candidate comparison"]
async fn run_startup_candidate() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    run_startup_matrix(&[Strategy::Adaptive, Strategy::AdaptiveFloor]).await
}

async fn run_startup_matrix(
    strategies: &[Strategy],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("{CSV_HEADER}");
    let config = HttpDownloadJobConfig::default();
    let minimum = usize::try_from(config.min_part_size.get())?;
    let default_target = usize::from(config.default_connections);
    assert!(minimum > 1 && default_target >= 2);
    let middle = minimum
        .checked_mul(default_target / 2)
        .ok_or("startup fixture size overflow")?;
    let full = minimum
        .checked_mul(default_target)
        .ok_or("startup fixture size overflow")?;
    let sizes = [
        ("minimum_below", minimum - 1),
        (
            "minimum_above",
            minimum
                .checked_add(1)
                .ok_or("startup fixture size overflow")?,
        ),
        ("mid_target_below", middle - 1),
        (
            "mid_target_above",
            middle
                .checked_add(1)
                .ok_or("startup fixture size overflow")?,
        ),
        (
            "default_target_above",
            full.checked_add(1).ok_or("startup fixture size overflow")?,
        ),
    ];
    for (label, size) in sizes {
        for (profile, throttle) in [
            ("uncapped", ServerThrottle::Uncapped),
            (
                "shared",
                ServerThrottle::Shared(BytesPerSecond::new(10 * 1024 * 1024)),
            ),
            (
                "per_connection",
                ServerThrottle::PerConnection(BytesPerSecond::new(4 * 1024 * 1024)),
            ),
        ] {
            run_scenario(
                &format!("startup_{label}_{profile}"),
                size,
                throttle,
                strategies,
            )
            .await?;
        }
    }
    Ok(())
}

#[tokio::test]
#[ignore = "manual performance baseline"]
async fn run_performance_baseline() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("{CSV_HEADER}");

    run_scenario(
        "small_file_startup",
        2 * 1024 * 1024,
        ServerThrottle::Uncapped,
        BASELINE_STRATEGIES,
    )
    .await?;

    run_scenario(
        "shared_bandwidth_contention",
        40 * 1024 * 1024,
        ServerThrottle::Shared(BytesPerSecond::new(10 * 1024 * 1024)),
        BASELINE_STRATEGIES,
    )
    .await?;

    run_scenario(
        "per_connection_throttling",
        40 * 1024 * 1024,
        ServerThrottle::PerConnection(BytesPerSecond::new(4 * 1024 * 1024)),
        BASELINE_STRATEGIES,
    )
    .await?;

    Ok(())
}

#[tokio::test]
#[ignore = "manual performance baseline"]
async fn run_long_ramp_up_baseline() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    println!("{CSV_HEADER}");

    run_long_scenario(
        "long_per_connection_throttling",
        128 * 1024 * 1024,
        ServerThrottle::PerConnection(BytesPerSecond::new(512 * 1024)),
        Duration::from_secs(120),
    )
    .await?;

    run_long_scenario(
        "long_shared_bandwidth_contention",
        128 * 1024 * 1024,
        ServerThrottle::Shared(BytesPerSecond::new(10 * 1024 * 1024)),
        Duration::from_secs(120),
    )
    .await?;

    Ok(())
}

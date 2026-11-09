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
use crate::download::{DownloadId, DownloadKind};

const SEGMENT_PACKETS: usize = 4;

struct TestDb {
    _dir: TempDir,
    download_repo: DownloadRepository,
}

async fn test_db() -> HlsDownloadJobResult<TestDb> {
    let dir = tempfile::tempdir()
        .map_err(|error| HlsDownloadJobError::Io(std::io::Error::other(error)))?;
    let database_url = format!("sqlite://{}", dir.path().join("hls.sqlite").display());
    let pool = db::bootstrap(&database_url).await?;
    Ok(TestDb {
        _dir: dir,
        download_repo: DownloadRepository::new(pool),
    })
}

fn ts_segment(fill: u8) -> Vec<u8> {
    let mut body = Vec::with_capacity(SEGMENT_PACKETS * 188);
    for _ in 0..SEGMENT_PACKETS {
        body.push(TS_SYNC_BYTE);
        body.extend(std::iter::repeat_n(fill, 187));
    }
    body
}

#[derive(Debug, Default)]
struct RouteState {
    hits: AtomicUsize,
    transient_failures: AtomicUsize,
}

#[derive(Debug)]
struct TestServer {
    addr: SocketAddr,
    state: Arc<RouteState>,
    task: JoinHandle<()>,
}

#[derive(Debug, Clone)]
enum Reply {
    Body {
        body: Vec<u8>,
        content_type: &'static str,
        etag: Option<&'static str>,
        truncate: bool,
    },
    Status(u16),
    Redirect(String),
    /// Sends headers immediately, then stalls so cancellation lands mid-body.
    Stalled(Vec<u8>),
    /// Body with no `Content-Length` and no validators, so identity cannot be proven.
    UnknownIdentity(Vec<u8>),
}

impl TestServer {
    async fn spawn(routes: Vec<(&'static str, Reply)>) -> HlsDownloadJobResult<Self> {
        Self::spawn_inner(routes, 0, Vec::new()).await
    }

    /// Routes can be replaced after the server started to simulate a changed remote plan.
    async fn spawn_mutable(
        routes: Vec<(&'static str, Reply)>,
    ) -> HlsDownloadJobResult<(
        Self,
        tokio::sync::mpsc::UnboundedSender<Vec<(&'static str, Reply)>>,
    )> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(std::io::Error::other)?;
        let addr = listener.local_addr().map_err(std::io::Error::other)?;
        let state = Arc::new(RouteState::default());
        let routes = Arc::new(tokio::sync::RwLock::new(to_routes(routes)));
        let task = {
            let routes = Arc::clone(&routes);
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        update = rx.recv() => {
                            let Some(update) = update else { break };
                            *routes.write().await = to_routes(update);
                        }
                        accepted = listener.accept() => {
                            let Ok((stream, _)) = accepted else { break };
                            let routes = Arc::clone(&routes);
                            let state = Arc::clone(&state);
                            tokio::spawn(async move {
                                let snapshot = routes.read().await.clone();
                                let _ = handle_connection(stream, snapshot, state).await;
                            });
                        }
                    }
                }
            })
        };
        Ok((Self { addr, state, task }, tx))
    }

    async fn spawn_inner(
        routes: Vec<(&'static str, Reply)>,
        transient_failures: usize,
        _unused: Vec<()>,
    ) -> HlsDownloadJobResult<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(std::io::Error::other)?;
        let addr = listener.local_addr().map_err(std::io::Error::other)?;
        let routes = to_routes(routes);
        let state = Arc::new(RouteState {
            hits: AtomicUsize::new(0),
            transient_failures: AtomicUsize::new(transient_failures),
        });
        let task = {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let routes = Arc::clone(&routes);
                    let state = Arc::clone(&state);
                    tokio::spawn(async move {
                        let _ = handle_connection(stream, routes, state).await;
                    });
                }
            })
        };
        Ok(Self { addr, state, task })
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    fn hits(&self) -> usize {
        self.state.hits.load(AtomicOrdering::Relaxed)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn to_routes(routes: Vec<(&'static str, Reply)>) -> Arc<BTreeMap<String, Reply>> {
    Arc::new(
        routes
            .into_iter()
            .map(|(path, reply)| (path.to_owned(), reply))
            .collect(),
    )
}

async fn handle_connection(
    mut stream: TcpStream,
    routes: Arc<BTreeMap<String, Reply>>,
    state: Arc<RouteState>,
) -> std::io::Result<()> {
    let mut buffer = vec![0_u8; 8192];
    let read = stream.read(&mut buffer).await?;
    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_owned();
    state.hits.fetch_add(1, AtomicOrdering::Relaxed);

    let reply = routes.get(&path).cloned().unwrap_or(Reply::Status(404));
    match reply {
        Reply::Status(status) => {
            let headers = format!(
                "HTTP/1.1 {status} Error\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
            );
            stream.write_all(headers.as_bytes()).await?;
        }
        Reply::Redirect(location) => {
            let headers = format!(
                "HTTP/1.1 302 Found\r\nConnection: close\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n"
            );
            stream.write_all(headers.as_bytes()).await?;
        }
        Reply::UnknownIdentity(body) => {
            let headers =
                "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: video/mp2t\r\n\r\n";
            stream.write_all(headers.as_bytes()).await?;
            stream.write_all(&body).await?;
            return stream.shutdown().await;
        }
        Reply::Stalled(body) => {
            let headers = format!(
                "HTTP/1.1 200 OK
Connection: close
Content-Length: {}
Content-Type: video/mp2t

",
                body.len()
            );
            stream.write_all(headers.as_bytes()).await?;
            stream.write_all(&body[..188]).await?;
            stream.flush().await?;
            tokio::time::sleep(Duration::from_secs(30)).await;
            return stream.shutdown().await;
        }
        Reply::Body {
            body,
            content_type,
            etag,
            truncate,
        } => {
            let total = body.len();
            if let Some((start, end)) = requested_range(&request)
                && start < total
            {
                let end = end.min(total - 1);
                let mut headers = format!(
                    "HTTP/1.1 206 Partial Content
Connection: close
Content-Length: {}
Content-Range: bytes {start}-{end}/{total}
Content-Type: {content_type}
",
                    end.saturating_sub(start) + 1
                );
                if let Some(etag) = etag {
                    headers.push_str(&format!(
                        "ETag: {etag}
"
                    ));
                }
                headers.push('\n');
                stream.write_all(headers.as_bytes()).await?;
                stream.write_all(&body[start..=end]).await?;
                return stream.shutdown().await;
            }
            if path.ends_with(".ts")
                && state
                    .transient_failures
                    .try_update(
                        AtomicOrdering::Relaxed,
                        AtomicOrdering::Relaxed,
                        |current| current.checked_sub(1),
                    )
                    .is_ok()
            {
                let headers =
                    "HTTP/1.1 503 Unavailable\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";
                stream.write_all(headers.as_bytes()).await?;
                return stream.shutdown().await;
            }
            let mut headers = format!(
                "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: {}\r\nContent-Type: {content_type}\r\n",
                body.len()
            );
            if let Some(etag) = etag {
                headers.push_str(&format!("ETag: {etag}\r\n"));
            }
            headers.push_str("\r\n");
            stream.write_all(headers.as_bytes()).await?;
            if truncate {
                stream.write_all(&body[..body.len() / 2]).await?;
            } else {
                stream.write_all(&body).await?;
            }
        }
    }
    stream.shutdown().await
}

/// Parses a `Range: bytes=start-end` request header.
fn requested_range(request: &str) -> Option<(usize, usize)> {
    request.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if !name.eq_ignore_ascii_case("range") {
            return None;
        }
        let (start, end) = value.trim().strip_prefix("bytes=")?.split_once('-')?;
        Some((start.parse().ok()?, end.parse().unwrap_or(usize::MAX)))
    })
}

fn media_playlist(paths: &[&str]) -> Vec<u8> {
    let mut body = String::from(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:5\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n",
    );
    for path in paths {
        body.push_str("#EXTINF:5.0,\n");
        body.push_str(path);
        body.push('\n');
    }
    body.push_str("#EXT-X-ENDLIST\n");
    body.into_bytes()
}

fn media_reply(paths: &[&str]) -> Reply {
    Reply::Body {
        body: media_playlist(paths),
        content_type: "application/vnd.apple.mpegurl",
        etag: None,
        truncate: false,
    }
}

fn ts_reply(fill: u8) -> Reply {
    Reply::Body {
        body: ts_segment(fill),
        content_type: "video/mp2t",
        etag: Some("\"segment-etag\""),
        truncate: false,
    }
}

fn sample_item(url: String, folder: &TempDir) -> DownloadItem {
    DownloadItem {
        id: DownloadId::new(1),
        kind: DownloadKind::Hls,
        url,
        download_page: None,
        headers: BTreeMap::new(),
        file_name: "video.ts".to_owned(),
        folder: folder.path().to_path_buf(),
        category: None,
        status: DownloadStatus::Added,
        total_bytes: None,
        downloaded_bytes: Bytes::ZERO,
        etag: None,
        last_modified: None,
        preferred_connections: None,
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
) -> HlsDownloadJobResult<(ButtonRun, DownloadItem)> {
    if db.download_repo.get(item.id).await?.is_none() {
        db.download_repo.add(&item).await?;
    }
    let job = HlsDownloadJob::new(db.download_repo.clone(), item.clone());
    let result = job.run(cancellation).await;
    let stored = db
        .download_repo
        .get(item.id)
        .await?
        .expect("download must still exist");
    Ok((ButtonRun::from(result), stored))
}

struct ButtonRun {
    result: HlsDownloadJobResult<DownloadItem>,
}

impl ButtonRun {
    fn expect_completed(self) -> DownloadItem {
        match self.result {
            Ok(item) => {
                assert_eq!(item.status, DownloadStatus::Completed);
                item
            }
            Err(error) => panic!("job failed: {error}"),
        }
    }
}

impl From<HlsDownloadJobResult<DownloadItem>> for ButtonRun {
    fn from(result: HlsDownloadJobResult<DownloadItem>) -> Self {
        Self { result }
    }
}

#[tokio::test]
async fn job_should_download_vod_media_playlist_and_assemble_in_order() -> HlsDownloadJobResult<()>
{
    let server = TestServer::spawn(vec![
        ("/index.m3u8", media_reply(&["/seg-0.ts", "/seg-1.ts"])),
        ("/seg-0.ts", ts_reply(0x11)),
        ("/seg-1.ts", ts_reply(0x22)),
    ])
    .await?;
    let db = test_db().await?;
    let folder = tempfile::tempdir()?;
    let item = sample_item(server.url("/index.m3u8"), &folder);

    let (job, _) = run_job(&db, item.clone(), CancellationToken::new()).await?;
    let item = job.expect_completed();

    let output = fs::read(folder.path().join("video.ts")).await?;
    let mut expected = ts_segment(0x11);
    expected.extend(ts_segment(0x22));
    assert_eq!(output, expected);
    let total =
        u64::try_from(expected.len()).map_err(|_| HlsDownloadJobError::ByteCountOverflow)?;
    assert_eq!(item.downloaded_bytes, Bytes::new(total));
    assert_eq!(item.total_bytes, Some(Bytes::new(total)));

    let plan = db
        .download_repo
        .hls_repository()
        .load(item.id)
        .await?
        .expect("plan must be persisted");
    assert_eq!(plan.segments.len(), 2);
    assert!(plan.segments.iter().all(|segment| segment.length.is_some()));
    assert_eq!(
        plan.segments[1].etag.as_deref(),
        Some("\"segment-etag\""),
        "remote validators must be recorded per segment"
    );
    assert!(
        !fs::try_exists(segment_directory(&item)).await?,
        "segment scratch directory is disposable after publication"
    );
    assert!(
        !fs::try_exists(folder.path().join("video.ts.raijin-part")).await?,
        "incomplete output must be removed after publication"
    );
    Ok(())
}

#[tokio::test]
async fn job_should_select_highest_variant_and_follow_redirects() -> HlsDownloadJobResult<()> {
    let server = TestServer::spawn(vec![
        (
            "/master.m3u8",
            Reply::Redirect("/real/master.m3u8".to_owned()),
        ),
        (
            "/real/master.m3u8",
            Reply::Body {
                body: concat!(
                    "#EXTM3U\n",
                    "#EXT-X-STREAM-INF:BANDWIDTH=800000\n",
                    "low.m3u8\n",
                    "#EXT-X-STREAM-INF:BANDWIDTH=2400000\n",
                    "high.m3u8\n"
                )
                .as_bytes()
                .to_vec(),
                content_type: "application/vnd.apple.mpegurl",
                etag: None,
                truncate: false,
            },
        ),
        ("/real/high.m3u8", media_reply(&["/real/high-0.ts"])),
        ("/real/high-0.ts", ts_reply(0x33)),
    ])
    .await?;
    let db = test_db().await?;
    let folder = tempfile::tempdir()?;
    let item = sample_item(server.url("/master.m3u8"), &folder);

    let (job, _) = run_job(&db, item, CancellationToken::new()).await?;
    job.expect_completed();

    let plan = db
        .download_repo
        .hls_repository()
        .load(DownloadId::new(1))
        .await?
        .expect("plan must be persisted");
    assert_eq!(plan.playlist_url, server.url("/real/high.m3u8"));
    assert_eq!(plan.segments[0].url, server.url("/real/high-0.ts"));
    Ok(())
}

#[tokio::test]
async fn unsupported_variant_should_fall_back_to_the_next_supported_rendition()
-> HlsDownloadJobResult<()> {
    let server = TestServer::spawn(vec![
        (
            "/master.m3u8",
            Reply::Body {
                body: concat!(
                    "#EXTM3U
",
                    "#EXT-X-STREAM-INF:BANDWIDTH=4000000
",
                    "fmp4.m3u8
",
                    "#EXT-X-STREAM-INF:BANDWIDTH=2000000
",
                    "ts.m3u8
"
                )
                .as_bytes()
                .to_vec(),
                content_type: "application/vnd.apple.mpegurl",
                etag: None,
                truncate: false,
            },
        ),
        (
            "/fmp4.m3u8",
            Reply::Body {
                body: concat!(
                    "#EXTM3U
",
                    "#EXT-X-TARGETDURATION:5
",
                    "#EXT-X-MAP:URI=\"init.mp4\"
",
                    "#EXTINF:5.0,
seg.m4s
",
                    "#EXT-X-ENDLIST
"
                )
                .as_bytes()
                .to_vec(),
                content_type: "application/vnd.apple.mpegurl",
                etag: None,
                truncate: false,
            },
        ),
        ("/ts.m3u8", media_reply(&["/ts-0.ts"])),
        ("/ts-0.ts", ts_reply(0x0a)),
    ])
    .await?;
    let db = test_db().await?;
    let folder = tempfile::tempdir()?;
    let item = sample_item(server.url("/master.m3u8"), &folder);

    let (job, _) = run_job(&db, item, CancellationToken::new()).await?;
    job.expect_completed();
    let plan = db
        .download_repo
        .hls_repository()
        .load(DownloadId::new(1))
        .await?
        .expect("plan must be persisted");
    assert_eq!(plan.playlist_url, server.url("/ts.m3u8"));
    assert_eq!(
        fs::read(folder.path().join("video.ts")).await?,
        ts_segment(0x0a)
    );
    Ok(())
}

#[tokio::test]
async fn variant_fallback_should_stay_bounded_on_a_mislabeled_master() -> HlsDownloadJobResult<()> {
    // A master that points every variant at the master itself would otherwise ping-pong
    // forever; loop detection plus the bounded playlist depth must stop it.
    let server = TestServer::spawn(vec![(
        "/master.m3u8",
        Reply::Body {
            body: concat!(
                "#EXTM3U
",
                "#EXT-X-STREAM-INF:BANDWIDTH=4000000
",
                "master.m3u8
",
                "#EXT-X-STREAM-INF:BANDWIDTH=2000000
",
                "master.m3u8
"
            )
            .as_bytes()
            .to_vec(),
            content_type: "application/vnd.apple.mpegurl",
            etag: None,
            truncate: false,
        },
    )])
    .await?;
    let db = test_db().await?;
    let folder = tempfile::tempdir()?;
    let item = sample_item(server.url("/master.m3u8"), &folder);

    let (job, stored) = run_job(&db, item, CancellationToken::new()).await?;
    let item = job
        .result
        .expect("resolution failures pause for validation");
    assert_eq!(item.status, DownloadStatus::Paused);
    assert_eq!(
        stored.failure.as_ref().map(|failure| failure.kind),
        Some(FailureKind::Validation)
    );
    assert!(
        server.hits() <= 3,
        "bounded fallback must not loop on the same playlist (hits: {})",
        server.hits()
    );
    Ok(())
}

#[tokio::test]
async fn variant_network_error_should_not_be_masked_by_another_rendition()
-> HlsDownloadJobResult<()> {
    let server = TestServer::spawn(vec![
        (
            "/master.m3u8",
            Reply::Body {
                body: concat!(
                    "#EXTM3U
",
                    "#EXT-X-STREAM-INF:BANDWIDTH=4000000
",
                    "broken.m3u8
",
                    "#EXT-X-STREAM-INF:BANDWIDTH=2000000
",
                    "ts.m3u8
"
                )
                .as_bytes()
                .to_vec(),
                content_type: "application/vnd.apple.mpegurl",
                etag: None,
                truncate: false,
            },
        ),
        ("/broken.m3u8", Reply::Status(500)),
        ("/ts.m3u8", media_reply(&["/ts-0.ts"])),
        ("/ts-0.ts", ts_reply(0x0b)),
    ])
    .await?;
    let db = test_db().await?;
    let folder = tempfile::tempdir()?;
    let item = sample_item(server.url("/master.m3u8"), &folder);

    let (job, stored) = run_job(&db, item, CancellationToken::new()).await?;
    job.result
        .expect_err("a server error must not silently change the selection");
    assert_eq!(stored.status, DownloadStatus::Error);
    assert_eq!(
        stored.failure.as_ref().map(|failure| failure.kind),
        Some(FailureKind::Network)
    );
    assert!(!fs::try_exists(folder.path().join("video.ts")).await?);
    Ok(())
}

#[tokio::test]
async fn unsupported_manifest_should_pause_without_any_output() -> HlsDownloadJobResult<()> {
    let server = TestServer::spawn(vec![(
        "/live.m3u8",
        Reply::Body {
            body: b"#EXTM3U\n#EXT-X-TARGETDURATION:5\n#EXTINF:5.0,\nseg.ts\n".to_vec(),
            content_type: "application/vnd.apple.mpegurl",
            etag: None,
            truncate: false,
        },
    )])
    .await?;
    let db = test_db().await?;
    let folder = tempfile::tempdir()?;
    let item = sample_item(server.url("/live.m3u8"), &folder);

    let (job, stored) = run_job(&db, item, CancellationToken::new()).await?;
    let item = job
        .result
        .expect("manual-action failures pause instead of failing");
    assert_eq!(item.status, DownloadStatus::Paused);
    assert_eq!(stored.status, DownloadStatus::Paused);
    assert_eq!(
        stored.failure.as_ref().map(|failure| failure.kind),
        Some(FailureKind::Validation)
    );
    assert!(!fs::try_exists(folder.path().join("video.ts")).await?);
    assert!(!fs::try_exists(folder.path().join("video.ts.raijin-part")).await?);
    assert!(
        db.download_repo
            .hls_repository()
            .load(stored.id)
            .await?
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn non_transport_stream_payload_should_not_produce_output() -> HlsDownloadJobResult<()> {
    let server = TestServer::spawn(vec![
        ("/index.m3u8", media_reply(&["/seg-0.ts"])),
        (
            "/seg-0.ts",
            Reply::Body {
                body: b"<!DOCTYPE html><html><body>blocked</body></html>".to_vec(),
                content_type: "text/html",
                etag: None,
                truncate: false,
            },
        ),
    ])
    .await?;
    let db = test_db().await?;
    let folder = tempfile::tempdir()?;
    let item = sample_item(server.url("/index.m3u8"), &folder);

    let (job, stored) = run_job(&db, item, CancellationToken::new()).await?;
    let item = job.result.expect("HTML is a manual-action failure");
    assert_eq!(item.status, DownloadStatus::Paused);
    assert_eq!(stored.status, DownloadStatus::Paused);
    assert_eq!(
        stored.failure.as_ref().map(|failure| failure.kind),
        Some(FailureKind::Validation)
    );
    assert!(!fs::try_exists(folder.path().join("video.ts")).await?);
    Ok(())
}

#[tokio::test]
async fn completed_segments_should_be_reused_on_resume() -> HlsDownloadJobResult<()> {
    let (server, update) = TestServer::spawn_mutable(vec![
        ("/index.m3u8", media_reply(&["/seg-0.ts", "/seg-1.ts"])),
        ("/seg-0.ts", ts_reply(0x44)),
        ("/seg-1.ts", Reply::Status(500)),
    ])
    .await?;
    let db = test_db().await?;
    let folder = tempfile::tempdir()?;
    let item = sample_item(server.url("/index.m3u8"), &folder);

    let (first, stored) = run_job(&db, item.clone(), CancellationToken::new()).await?;
    first
        .result
        .expect_err("failing segment must fail the job after retries");
    assert_eq!(stored.status, DownloadStatus::Error);
    let first_hits = server.hits();

    let plan = db
        .download_repo
        .hls_repository()
        .load(item.id)
        .await?
        .expect("plan must be persisted");
    assert_eq!(plan.segments[0].length, Some(ts_segment(0x44).len() as u64));
    assert_eq!(plan.segments[1].length, None);
    assert!(
        fs::try_exists(segment_path(&segment_directory(&stored), 0)).await?,
        "completed segment payload must survive a failed run"
    );

    update
        .send(vec![
            ("/index.m3u8", media_reply(&["/seg-0.ts", "/seg-1.ts"])),
            ("/seg-0.ts", ts_reply(0x44)),
            ("/seg-1.ts", ts_reply(0x55)),
        ])
        .map_err(|_| std::io::Error::other("server update channel closed"))?;
    let (second, _) = run_job(&db, item.clone(), CancellationToken::new()).await?;
    second.expect_completed();
    // Resume refetches the playlist, identity-probes the retained segment, then
    // downloads only the pending one; no segment is downloaded twice.
    let second_hits = server.hits() - first_hits;
    assert!(
        (3..=4).contains(&second_hits),
        "resume must not redownload verified segments (hits: {second_hits})"
    );

    let output = fs::read(folder.path().join("video.ts")).await?;
    let mut expected = ts_segment(0x44);
    expected.extend(ts_segment(0x55));
    assert_eq!(output, expected);
    Ok(())
}

#[tokio::test]
async fn changed_remote_plan_should_pause_for_validation() -> HlsDownloadJobResult<()> {
    let (server, update) = TestServer::spawn_mutable(vec![
        ("/index.m3u8", media_reply(&["/seg-0.ts", "/seg-1.ts"])),
        ("/seg-0.ts", ts_reply(0x66)),
        ("/seg-1.ts", Reply::Status(500)),
    ])
    .await?;
    let db = test_db().await?;
    let folder = tempfile::tempdir()?;
    let item = sample_item(server.url("/index.m3u8"), &folder);
    let (first, _) = run_job(&db, item.clone(), CancellationToken::new()).await?;
    first.result.expect_err("first run must fail");

    update
        .send(vec![
            ("/index.m3u8", media_reply(&["/seg-0.ts", "/seg-2.ts"])),
            ("/seg-0.ts", ts_reply(0x66)),
            ("/seg-2.ts", ts_reply(0x77)),
        ])
        .map_err(|_| std::io::Error::other("server update channel closed"))?;
    let (second, stored) = run_job(&db, item, CancellationToken::new()).await?;
    let item = second
        .result
        .expect("a changed plan pauses instead of failing the job");
    assert_eq!(item.status, DownloadStatus::Paused);
    assert_eq!(stored.status, DownloadStatus::Paused);
    let failure = stored.failure.expect("paused download keeps a failure");
    assert_eq!(failure.kind, FailureKind::Validation);
    assert!(failure.message.contains("manual validation"));
    assert!(!fs::try_exists(folder.path().join("video.ts")).await?);
    Ok(())
}

#[tokio::test]
async fn assembly_retry_should_work_when_every_segment_verifies_and_source_expired()
-> HlsDownloadJobResult<()> {
    let (server, update) = TestServer::spawn_mutable(vec![
        ("/index.m3u8", media_reply(&["/seg-0.ts", "/seg-1.ts"])),
        ("/seg-0.ts", ts_reply(0x01)),
        ("/seg-1.ts", ts_reply(0x02)),
    ])
    .await?;
    let db = test_db().await?;
    let folder = tempfile::tempdir()?;
    let item = sample_item(server.url("/index.m3u8"), &folder);
    let (first, stored) = run_job(&db, item.clone(), CancellationToken::new()).await?;
    let completed = first.expect_completed();

    // Reconstruct a crash between the last segment and publication: the output is
    // gone, the segment payloads are on disk exactly as persisted, and the plan
    // still records their length and checksum.
    fs::remove_file(folder.path().join("video.ts")).await?;
    let directory = segment_directory(&completed);
    fs::create_dir_all(&directory).await?;
    for (index, fill) in [(0_u32, 0x01_u8), (1, 0x02)] {
        fs::write(segment_path(&directory, index), ts_segment(fill)).await?;
    }
    let mut stalled = stored.clone();
    stalled.status = DownloadStatus::PreparingFile;
    stalled.total_bytes = None;
    stalled.completed_at = None;
    db.download_repo.update(&stalled).await?;

    // Source expires: every route disappears.
    update
        .send(Vec::new())
        .map_err(|_| std::io::Error::other("server update channel closed"))?;
    let hits_before = server.hits();

    let (second, stored) = run_job(&db, stalled, CancellationToken::new()).await?;
    let item = second.expect_completed();
    assert_eq!(stored.status, DownloadStatus::Completed);
    assert_eq!(
        server.hits(),
        hits_before,
        "verified local segments must not require the expired source"
    );

    let mut expected = ts_segment(0x01);
    expected.extend(ts_segment(0x02));
    assert_eq!(fs::read(folder.path().join("video.ts")).await?, expected);
    assert_eq!(item.downloaded_bytes, Bytes::new(expected.len() as u64));
    Ok(())
}

#[tokio::test]
async fn retained_segment_without_remote_identity_should_pause_for_validation()
-> HlsDownloadJobResult<()> {
    // Segment 0 downloads from a server that exposes neither validators nor a
    // length, so on resume its retained bytes cannot be proven to match what the
    // server would serve today. Mixing them with fresh bytes is refused.
    let server = TestServer::spawn(vec![
        ("/index.m3u8", media_reply(&["/seg-0.ts", "/seg-1.ts"])),
        ("/seg-0.ts", Reply::UnknownIdentity(ts_segment(0x03))),
        ("/seg-1.ts", Reply::Status(500)),
    ])
    .await?;
    let db = test_db().await?;
    let folder = tempfile::tempdir()?;
    let item = sample_item(server.url("/index.m3u8"), &folder);
    let (first, stored) = run_job(&db, item.clone(), CancellationToken::new()).await?;
    first.result.expect_err("pending segment must fail");

    let plan = db
        .download_repo
        .hls_repository()
        .load(stored.id)
        .await?
        .expect("plan must be persisted");
    assert_eq!(plan.segments[0].length, Some(ts_segment(0x03).len() as u64));
    assert!(plan.segments[0].etag.is_none());

    let (second, stored) = run_job(&db, item, CancellationToken::new()).await?;
    let item = second
        .result
        .expect("unverifiable retained bytes pause instead of mixing versions");
    assert_eq!(item.status, DownloadStatus::Paused);
    assert_eq!(
        stored.failure.as_ref().map(|failure| failure.kind),
        Some(FailureKind::Validation)
    );
    assert!(!fs::try_exists(folder.path().join("video.ts")).await?);
    Ok(())
}

#[tokio::test]
async fn crash_after_publish_should_preserve_existing_output_and_disambiguate()
-> HlsDownloadJobResult<()> {
    let server = TestServer::spawn(vec![
        ("/index.m3u8", media_reply(&["/seg-0.ts"])),
        ("/seg-0.ts", ts_reply(0xdd)),
    ])
    .await?;
    let db = test_db().await?;
    let folder = tempfile::tempdir()?;
    let item = sample_item(server.url("/index.m3u8"), &folder);
    let (first, _) = run_job(&db, item.clone(), CancellationToken::new()).await?;
    let item = first.expect_completed();
    let published = fs::read(folder.path().join("video.ts")).await?;

    // Simulate a crash between publication and lifecycle persistence: the output
    // exists but the download is not marked complete and no segments remain.
    let mut stalled = item.clone();
    stalled.status = DownloadStatus::PreparingFile;
    stalled.total_bytes = None;
    stalled.completed_at = None;
    db.download_repo.update(&stalled).await?;

    let (second, stored) = run_job(&db, stalled, CancellationToken::new()).await?;
    let item = second.expect_completed();
    assert_eq!(stored.status, DownloadStatus::Completed);
    assert_eq!(item.file_name, "video(1).ts");
    assert_eq!(fs::read(folder.path().join("video.ts")).await?, published);
    assert_eq!(
        fs::read(folder.path().join("video(1).ts")).await?,
        published
    );
    assert!(!fs::try_exists(folder.path().join("video.ts.raijin-part")).await?);
    Ok(())
}

#[tokio::test]
async fn transient_segment_failures_should_be_retried() -> HlsDownloadJobResult<()> {
    let server = TestServer::spawn_inner(
        vec![
            ("/index.m3u8", media_reply(&["/seg-0.ts"])),
            ("/seg-0.ts", ts_reply(0x88)),
        ],
        1,
        Vec::new(),
    )
    .await?;
    let db = test_db().await?;
    let folder = tempfile::tempdir()?;
    let item = sample_item(server.url("/index.m3u8"), &folder);

    let (job, _) = run_job(&db, item, CancellationToken::new()).await?;
    job.expect_completed();
    assert_eq!(
        fs::read(folder.path().join("video.ts")).await?,
        ts_segment(0x88)
    );
    Ok(())
}

#[tokio::test]
async fn truncated_segment_body_should_exhaust_retries_without_partial_segment()
-> HlsDownloadJobResult<()> {
    let truncated = Reply::Body {
        body: ts_segment(0x99),
        content_type: "video/mp2t",
        etag: None,
        truncate: true,
    };
    let server = TestServer::spawn(vec![
        ("/index.m3u8", media_reply(&["/seg-0.ts"])),
        ("/seg-0.ts", truncated),
    ])
    .await?;
    let db = test_db().await?;
    let folder = tempfile::tempdir()?;
    let item = sample_item(server.url("/index.m3u8"), &folder);

    let (job, stored) = run_job(&db, item.clone(), CancellationToken::new()).await?;
    job.result.expect_err("truncated segment must fail");
    assert_eq!(stored.status, DownloadStatus::Error);
    assert_eq!(
        stored.failure.as_ref().map(|failure| failure.kind),
        Some(FailureKind::TooManyRetries)
    );
    let directory = segment_directory(&stored);
    assert!(!fs::try_exists(segment_path(&directory, 0)).await?);
    assert!(
        !fs::try_exists(segment_temp_path(&directory, 0)).await?,
        "partial segment payload must not be left behind"
    );
    assert!(!fs::try_exists(folder.path().join("video.ts")).await?);
    Ok(())
}

#[tokio::test]
async fn cancellation_should_leave_paused_state_without_output() -> HlsDownloadJobResult<()> {
    let server = TestServer::spawn(vec![
        ("/index.m3u8", media_reply(&["/seg-0.ts"])),
        ("/seg-0.ts", ts_reply(0xaa)),
    ])
    .await?;
    let db = test_db().await?;
    let folder = tempfile::tempdir()?;
    let item = sample_item(server.url("/index.m3u8"), &folder);
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let (job, stored) = run_job(&db, item, cancellation).await?;
    let item = job
        .result
        .expect("cancellation is reported as a paused item, not an error");
    assert_eq!(item.status, DownloadStatus::Paused);
    assert_eq!(stored.status, DownloadStatus::Paused);
    assert_eq!(stored.downloaded_bytes, Bytes::ZERO);
    assert!(stored.failure.is_none());
    assert!(!fs::try_exists(folder.path().join("video.ts")).await?);
    assert!(!fs::try_exists(folder.path().join("video.ts.raijin-part")).await?);
    Ok(())
}

#[tokio::test]
async fn mid_stream_cancellation_should_stop_promptly_and_drain_workers() -> HlsDownloadJobResult<()>
{
    let server = TestServer::spawn(vec![
        ("/index.m3u8", media_reply(&["/seg-0.ts"])),
        ("/seg-0.ts", Reply::Stalled(ts_segment(0xcc))),
    ])
    .await?;
    let db = test_db().await?;
    let folder = tempfile::tempdir()?;
    let item = sample_item(server.url("/index.m3u8"), &folder);
    let cancellation = CancellationToken::new();

    let live = Arc::new(LiveDownloadProgress::default());
    let running = {
        let live = Arc::clone(&live);
        let cancellation = cancellation.clone();
        let db_repo = db.download_repo.clone();
        tokio::spawn(async move {
            if db_repo.get(item.id).await?.is_none() {
                db_repo.add(&item).await?;
            }
            let job = HlsDownloadJob::new(db_repo, item).with_live_progress(live);
            job.run(cancellation).await
        })
    };

    tokio::time::timeout(Duration::from_secs(5), async {
        while live.snapshot().0 == Bytes::ZERO {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| std::io::Error::other("segment never reported live progress"))?;
    assert_eq!(live.snapshot().1, 1);
    cancellation.cancel();

    let outcome = tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .map_err(|_| std::io::Error::other("cancellation did not stop the job"))?;
    let item = outcome.map_err(|_| std::io::Error::other("job task panicked"))??;
    assert_eq!(item.status, DownloadStatus::Paused);
    assert_eq!(item.downloaded_bytes, Bytes::ZERO);
    assert_eq!(live.snapshot(), (Bytes::ZERO, 0));

    let directory = segment_directory(&item);
    assert!(
        !fs::try_exists(segment_temp_path(&directory, 0)).await?,
        "canceled workers must not leave a partial segment file behind"
    );
    assert!(!fs::try_exists(segment_path(&directory, 0)).await?);
    assert!(!fs::try_exists(folder.path().join("video.ts")).await?);

    let plan = db
        .download_repo
        .hls_repository()
        .load(item.id)
        .await?
        .expect("plan must survive cancellation");
    assert_eq!(plan.segments[0].length, None);
    Ok(())
}

#[tokio::test]
async fn existing_output_should_never_be_overwritten() -> HlsDownloadJobResult<()> {
    let server = TestServer::spawn(vec![
        ("/index.m3u8", media_reply(&["/seg-0.ts"])),
        ("/seg-0.ts", ts_reply(0xbb)),
    ])
    .await?;
    let db = test_db().await?;
    let folder = tempfile::tempdir()?;
    fs::write(folder.path().join("video.ts"), b"existing").await?;
    fs::hard_link(
        folder.path().join("video.ts"),
        folder.path().join("video.ts.raijin-part"),
    )
    .await?;
    let item = sample_item(server.url("/index.m3u8"), &folder);

    let (job, _) = run_job(&db, item, CancellationToken::new()).await?;
    let item = job.expect_completed();
    assert_eq!(
        fs::read(folder.path().join("video.ts")).await?,
        b"existing".to_vec()
    );
    assert_eq!(item.file_name, "video(1).ts");
    assert_eq!(
        fs::read(folder.path().join("video(1).ts")).await?,
        ts_segment(0xbb)
    );
    Ok(())
}

#[tokio::test]
async fn segment_directory_should_derive_only_from_download_id() {
    let folder = tempfile::tempdir().expect("temp dir");
    let mut item = sample_item("https://cdn.example.com/a.m3u8".to_owned(), &folder);
    item.file_name = "../../escape.ts".to_owned();

    let directory = segment_directory(&item);
    assert_eq!(directory, folder.path().join(".raijin-hls-1"));
    assert_eq!(
        segment_path(&directory, 7),
        folder.path().join(".raijin-hls-1").join("00000007.ts")
    );
}

#[test]
fn ts_sync_should_validate_every_packet_boundary() {
    let body = ts_segment(0x01);
    assert!(ts_sync_ok(0, &body));
    assert!(ts_sync_ok(0, &body[..188]));
    assert!(ts_sync_ok(188, &body[188..]));

    let mut broken = body;
    broken[2 * 188] = b'x';
    assert!(!ts_sync_ok(0, &broken));

    let mut plain = vec![b'a'; 376];
    plain[0] = TS_SYNC_BYTE;
    assert!(!ts_sync_ok(0, &plain));
}

#[test]
fn captured_headers_should_only_reach_the_original_origin() {
    let origin_url = Url::parse("https://cdn.example.com/a.m3u8").expect("url");
    let origin = Origin::of(&origin_url).expect("origin");
    let mut captured = BTreeMap::new();
    captured.insert("authorization".to_owned(), "Bearer secret".to_owned());
    captured.insert("referer".to_owned(), "https://site.example.com".to_owned());
    captured.insert("Range".to_owned(), "bytes=0-".to_owned());
    captured.insert("Host".to_owned(), "evil.example.com".to_owned());

    let same = header_map_for(&origin_url, &captured, &origin).expect("headers");
    assert_eq!(
        same.get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer secret")
    );
    assert_eq!(
        same.get("referer").and_then(|value| value.to_str().ok()),
        Some("https://site.example.com")
    );
    assert!(same.get("range").is_none());
    assert_eq!(
        same.get(header::ACCEPT_ENCODING)
            .and_then(|value| value.to_str().ok()),
        Some(crate::download::http::ACCEPT_ENCODING_IDENTITY)
    );

    let other = Url::parse("https://other.example.com/seg.ts").expect("url");
    let stripped = header_map_for(&other, &captured, &origin).expect("headers");
    assert!(stripped.get("authorization").is_none());
    assert!(stripped.get("referer").is_none());

    let downgrade = Url::parse("http://cdn.example.com/seg.ts").expect("url");
    let downgraded = header_map_for(&downgrade, &captured, &origin).expect("headers");
    assert!(
        downgraded.get("authorization").is_none(),
        "scheme downgrade counts as a different origin"
    );
}

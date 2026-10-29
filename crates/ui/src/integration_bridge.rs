//! Bridges the localhost integration server into the GPUI app.
//!
//! The server runs on the engine runtime and never touches an `Entity`. Each
//! HTTP request becomes an [`IntegrationCommand`]; this module drains them on a
//! GPUI foreground task and dispatches only through handles the app supplies,
//! so engine and window state stay owned by the UI thread.

use std::path::Path;

use raijin_core::download::{DownloadManagerHandle, QueueId};
use raijin_core::integration::{
    AddDownloadRequest, DEFAULT_COMMAND_BUFFER, IntegrationCommand, IntegrationHandle,
    IntegrationReply, IntegrationResponder, PreparedItem, QueueSummary, prepare_request,
    summarize_item,
};
use raijin_core::queue::QueueManagerHandle;
use tokio::sync::mpsc;

/// Engine-side dependencies needed to answer requests.
#[derive(Debug, Clone)]
pub struct IntegrationEngine {
    /// Actor handle used to list and add downloads.
    pub manager: DownloadManagerHandle,
    /// Queue actor: admits extension downloads and answers `GET /queues`.
    pub queues: QueueManagerHandle,
    /// Runtime used to run engine work off the UI thread.
    pub runtime: tokio::runtime::Handle,
    /// Destination folder for extension-originated downloads.
    pub default_folder: std::path::PathBuf,
}

/// Server handle plus the receiving end the app must drain.
#[derive(Debug)]
pub struct IntegrationBridge {
    /// Cloned into the axum router.
    pub handle: IntegrationHandle,
    /// Drained by [`spawn_dispatcher`].
    pub receiver: mpsc::Receiver<IntegrationCommand>,
}

/// Creates the bridge pair.
#[must_use]
pub fn bridge() -> IntegrationBridge {
    let (handle, receiver) = IntegrationHandle::channel(DEFAULT_COMMAND_BUFFER);
    IntegrationBridge { handle, receiver }
}

/// Validates a confirmation request and hands the items to the caller.
///
/// The extension gets `202 Accepted` with no ids: the user still has to confirm
/// in the dialog, so claiming a download id here would be a lie.
#[must_use]
pub fn validate_confirmation(
    request: AddDownloadRequest,
    folder: &Path,
    responder: IntegrationResponder,
) -> (
    IntegrationResponder,
    IntegrationReply,
    Option<Vec<PreparedItem>>,
) {
    match prepare_request(&request, folder) {
        Ok(items) => (
            responder,
            IntegrationReply::Accepted { ids: Vec::new() },
            Some(items),
        ),
        Err(failure) => (
            responder,
            IntegrationReply::Failed(failure.message().to_owned()),
            None,
        ),
    }
}

/// Adds items without a dialog and hands each one to the Main queue.
///
/// Leaving a download out of a queue would strand it: admission is the only
/// path that starts a job, so the membership write is part of the request.
///
/// # Errors
///
/// Returns the engine message when validation or persistence fails.
pub async fn add_prepared(
    engine: &IntegrationEngine,
    prepared: Vec<PreparedItem>,
    autostart: bool,
) -> Result<IntegrationReply, String> {
    let mut ids = Vec::with_capacity(prepared.len());
    for item in prepared {
        let added = engine
            .manager
            .add(item.request)
            .await
            .map_err(|error| error.to_string())?;
        ids.push(added.id.get());
        if !autostart {
            engine
                .manager
                .pause(added.id)
                .await
                .map_err(|error| error.to_string())?;
        }
        if let Err(error) = engine.queues.enqueue(QueueId::MAIN, [added.id]).await {
            // The download row exists; failing the whole request would discard
            // ids the extension can still act on.
            tracing::warn!(
                ?error,
                id = added.id.get(),
                "queue enqueue after add failed"
            );
        }
    }
    Ok(IntegrationReply::Accepted { ids })
}

/// Builds the downloads snapshot for `GET /downloads`.
///
/// # Errors
///
/// Returns the engine message when the manager stops.
pub async fn list_downloads(engine: &IntegrationEngine) -> Result<IntegrationReply, String> {
    let items = engine
        .manager
        .list()
        .await
        .map_err(|error| error.to_string())?;
    Ok(IntegrationReply::Downloads(
        items.iter().map(summarize_item).collect(),
    ))
}

/// Builds the queue snapshot for `GET /queues`.
///
/// The queue actor answers from its own snapshot, so the payload stays in step
/// with admission instead of reading membership rows behind its back.
///
/// # Errors
///
/// Returns the queue message when the actor has stopped.
pub async fn list_queues(engine: &IntegrationEngine) -> Result<IntegrationReply, String> {
    let snapshots = engine
        .queues
        .list()
        .await
        .map_err(|error| error.to_string())?;
    let summaries = snapshots
        .iter()
        .map(|snapshot| QueueSummary {
            id: snapshot.queue.id.get(),
            name: snapshot.queue.name.clone(),
            max_concurrent: snapshot.queue.max_concurrent.get(),
            item_count: snapshot.items.len(),
        })
        .collect();
    Ok(IntegrationReply::Queues(summaries))
}

fn confirmation_reply(result: Result<(), String>, accepted: IntegrationReply) -> IntegrationReply {
    match result {
        Ok(()) => accepted,
        Err(error) => IntegrationReply::Failed(error),
    }
}

/// Sends a reply, logging when the requester already gave up.
pub fn reply(responder: IntegrationResponder, value: IntegrationReply) {
    if responder.send(value).is_err() {
        tracing::debug!("integration requester dropped before reply");
    }
}

/// Runs a future on the engine runtime and forwards its reply.
///
/// The spawned task owns the responder so the HTTP request stays pending until
/// the engine answers; failures become `Failed` instead of a dropped channel.
pub fn reply_async<F>(runtime: &tokio::runtime::Handle, responder: IntegrationResponder, future: F)
where
    F: std::future::Future<Output = Result<IntegrationReply, String>> + Send + 'static,
{
    runtime.spawn(async move {
        let value = match future.await {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!(%error, "integration command failed");
                IntegrationReply::Failed(error)
            }
        };
        reply(responder, value);
    });
}

/// Shared responder/receiver pair used by tests.
#[cfg(test)]
type TestReply = tokio::sync::oneshot::Receiver<IntegrationReply>;

/// Creates a detached responder for tests.
#[cfg(test)]
fn test_responder() -> (IntegrationResponder, TestReply) {
    tokio::sync::oneshot::channel()
}

/// Everything the app needs to start serving integration requests.
#[derive(Debug)]
pub struct Started {
    /// Drained on the GPUI foreground thread.
    pub receiver: mpsc::Receiver<IntegrationCommand>,
    /// Bound port, surfaced read-only in the Settings window.
    pub port: u16,
    /// Engine dependencies used to answer commands.
    pub engine: IntegrationEngine,
}

/// Opens the confirmation dialog for validated integration items.
///
/// Successful window creation transfers ownership before `202 Accepted` is sent.
/// The user still has to approve the download.
pub type DialogOpener = Box<
    dyn FnMut(
        &mut gpui_kit::App,
        Vec<PreparedItem>,
        Option<std::sync::Arc<tokio::sync::OwnedSemaphorePermit>>,
    ) -> Result<(), String>,
>;

/// Drains integration commands on the GPUI foreground thread.
///
/// Non-silent adds are validated here and handed to `open_dialog`; everything
/// else is answered on the engine runtime so the UI thread never awaits I/O.
pub fn spawn_dispatcher(cx: &mut gpui_kit::App, started: Started, mut open_dialog: DialogOpener) {
    let Started {
        mut receiver,
        engine,
        // The port is only shown in Settings; the dispatcher answers commands.
        port: _port,
    } = started;
    let runtime = engine.runtime.clone();
    cx.spawn(async move |cx| {
        while let Some(command) = receiver.recv().await {
            match command {
                IntegrationCommand::ConfirmAdd {
                    items,
                    options,
                    pending,
                    reply: responder,
                } => {
                    let request = AddDownloadRequest { items, options };
                    let (responder, value, prepared) =
                        validate_confirmation(request, &engine.default_folder, responder);
                    match prepared {
                        Some(prepared) => {
                            let result = cx.update(|cx| open_dialog(cx, prepared, pending));
                            reply(responder, confirmation_reply(result, value));
                        }
                        None => reply(responder, value),
                    }
                }
                IntegrationCommand::HeadlessAdd {
                    items,
                    options,
                    reply: responder,
                } => {
                    let request = AddDownloadRequest { items, options };
                    match prepare_request(&request, &engine.default_folder) {
                        Ok(prepared) => {
                            let engine = engine.clone();
                            reply_async(&runtime, responder, async move {
                                add_prepared(&engine, prepared, options.silent_start).await
                            });
                        }
                        Err(failure) => reply(
                            responder,
                            IntegrationReply::Failed(failure.message().to_owned()),
                        ),
                    }
                }
                IntegrationCommand::ListDownloads { reply: responder } => {
                    let engine = engine.clone();
                    reply_async(
                        &runtime,
                        responder,
                        async move { list_downloads(&engine).await },
                    );
                }
                IntegrationCommand::ListQueues { reply: responder } => {
                    let engine = engine.clone();
                    reply_async(
                        &runtime,
                        responder,
                        async move { list_queues(&engine).await },
                    );
                }
            }
        }
    })
    .detach();
}

#[cfg(test)]
mod tests {
    // Explicit imports: a `use super::*` glob would shadow the built-in
    // `#[test]` attribute with GPUI's `test` macro.
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use raijin_core::integration::{
        AddDownloadRequest, DownloadRequestItem, DownloadRequestOptions, DownloadType,
        IntegrationReply,
    };

    use super::{IntegrationBridge, bridge, reply, test_responder, validate_confirmation};

    #[test]
    fn confirmation_should_reject_failed_window_creation() {
        assert!(matches!(
            super::confirmation_reply(
                Err("window creation failed".to_owned()),
                IntegrationReply::Accepted { ids: Vec::new() },
            ),
            IntegrationReply::Failed(message) if message == "window creation failed"
        ));
        assert!(matches!(
            super::confirmation_reply(Ok(()), IntegrationReply::Accepted { ids: Vec::new() }),
            IntegrationReply::Accepted { .. }
        ));
    }

    fn item(link: &str) -> DownloadRequestItem {
        DownloadRequestItem {
            link: link.to_owned(),
            download_page: None,
            headers: None,
            description: None,
            suggested_name: None,
            total_bytes: None,
            r#type: DownloadType::Http,
        }
    }

    fn request(items: Vec<DownloadRequestItem>) -> AddDownloadRequest {
        AddDownloadRequest {
            items,
            options: DownloadRequestOptions::default(),
        }
    }

    #[tokio::test]
    async fn silent_add_should_keep_membership_and_only_start_when_requested() {
        use raijin_core::db::{DownloadRepository, PartRepository, QueueRepository};
        use raijin_core::download::{
            DownloadManagerHandle, DownloadManagerOptions, DownloadStatus, QueueId,
        };
        use raijin_core::queue::{QueueManagerHandle, QueueManagerOptions};

        let pool = raijin_core::db::bootstrap("sqlite::memory:")
            .await
            .expect("database");
        let downloads = DownloadRepository::new(pool.clone());
        let (manager, manager_task, _) = DownloadManagerHandle::spawn_with_events(
            downloads.clone(),
            PartRepository::new(pool.clone()),
            DownloadManagerOptions::default(),
        );
        let (queues, queue_task) = QueueManagerHandle::spawn(
            downloads,
            QueueRepository::new(pool.clone()),
            manager.clone(),
            manager.subscribe(),
            QueueManagerOptions::default(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let url = format!(
            "http://{}/file.zip",
            listener.local_addr().expect("address")
        );
        let engine = super::IntegrationEngine {
            manager,
            queues,
            runtime: tokio::runtime::Handle::current(),
            default_folder: std::env::temp_dir(),
        };
        for start in [false, true] {
            let prepared = raijin_core::integration::prepare_request(
                &request(vec![item(&url)]),
                &engine.default_folder,
            )
            .expect("prepared request");
            let IntegrationReply::Accepted { ids } = super::add_prepared(&engine, prepared, start)
                .await
                .expect("added")
            else {
                panic!("unexpected reply");
            };
            let downloads = engine.manager.list().await.expect("downloads");
            let added = downloads
                .iter()
                .find(|item| item.id.get() == ids[0])
                .expect("added item");
            assert_eq!(added.status == DownloadStatus::Paused, !start);
            let queues = engine.queues.list().await.expect("queues");
            assert!(queues.iter().any(|queue| queue.queue.id == QueueId::MAIN && queue.items.contains(&added.id)));
        }
        engine.queues.shutdown().await.expect("queue shutdown");
        queue_task.await.expect("queue task").expect("queue result");
        engine.manager.shutdown().await.expect("manager shutdown");
        manager_task
            .await
            .expect("manager task")
            .expect("manager result");
        pool.close().await;
    }

    #[test]
    fn bridge_should_expose_a_handle_and_a_receiver() {
        let IntegrationBridge {
            handle,
            mut receiver,
        } = bridge();
        assert!(receiver.try_recv().is_err());
        drop(handle);
        // Dropping the last handle closes the channel; draining returns `None`.
        assert!(receiver.blocking_recv().is_none());
    }

    #[test]
    fn confirmation_should_accept_valid_items_without_claiming_ids() {
        let folder = PathBuf::from("C:/Downloads");
        let (responder, replies) = test_responder();
        let (responder, value, items) = validate_confirmation(
            request(vec![item("https://cdn.example.com/clip.mp4")]),
            &folder,
            responder,
        );
        reply(responder, value);

        let items = items.expect("valid request yields dialog items");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].request.file_name, "clip.mp4");
        match replies.blocking_recv() {
            Ok(IntegrationReply::Accepted { ids }) => assert!(ids.is_empty()),
            other => panic!("expected Accepted with no ids, got {other:?}"),
        }
    }

    #[test]
    fn confirmation_should_reject_invalid_items_without_opening_a_dialog() {
        let folder = PathBuf::from("C:/Downloads");
        let (responder, replies) = test_responder();
        let (responder, value, items) = validate_confirmation(
            request(vec![item("ftp://example.com/a.zip")]),
            &folder,
            responder,
        );
        reply(responder, value);

        assert!(items.is_none());
        assert!(matches!(
            replies.blocking_recv(),
            Ok(IntegrationReply::Failed(_))
        ));
    }

    #[test]
    fn confirmation_should_reject_empty_payloads() {
        let folder = PathBuf::from("C:/Downloads");
        let (responder, replies) = test_responder();
        let (responder, value, items) =
            validate_confirmation(request(Vec::new()), &folder, responder);
        reply(responder, value);

        assert!(items.is_none());
        assert!(matches!(
            replies.blocking_recv(),
            Ok(IntegrationReply::Failed(_))
        ));
    }

    #[test]
    fn confirmation_should_keep_headers_for_authenticated_downloads() {
        let folder = PathBuf::from("C:/Downloads");
        let mut item = item("https://cdn.example.com/clip.mp4");
        let mut headers = BTreeMap::new();
        headers.insert("Cookie".to_owned(), "session=abc".to_owned());
        item.headers = Some(headers);

        let (responder, _replies) = test_responder();
        let (_responder, _value, items) =
            validate_confirmation(request(vec![item]), &folder, responder);
        let items = items.expect("valid request");
        assert_eq!(
            items[0].request.headers.get("Cookie"),
            Some(&"session=abc".to_owned())
        );
    }
}

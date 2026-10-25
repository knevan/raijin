use std::io;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

const MAX_COMMAND_BYTES: usize = 16 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(2);

/// Local launch requests never authorize silent downloads or credential access.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DesktopCommand {
    Activate,
    BrowserIntegration,
    Add(String),
    Background,
}

impl DesktopCommand {
    fn validate(&self) -> io::Result<()> {
        if let Self::Add(url) = self {
            let valid = url.len() <= 8192
                && reqwest::Url::parse(url).is_ok_and(|url| {
                    matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
                });
            if !valid {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid launch download URL",
                ));
            }
        }
        Ok(())
    }
}

/// Holds the exclusive pipe alive until the desktop process exits.
pub struct PrimaryInstance {
    receiver: Option<mpsc::Receiver<DesktopCommand>>,
    worker: JoinHandle<()>,
}

impl PrimaryInstance {
    pub fn take_receiver(&mut self) -> Option<mpsc::Receiver<DesktopCommand>> {
        self.receiver.take()
    }
}

impl Drop for PrimaryInstance {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

/// Claims the per-data-directory instance or forwards to its existing owner.
/// `None` means the request was accepted by the existing process.
pub async fn claim(
    data_dir: &Path,
    command: &DesktopCommand,
) -> io::Result<Option<PrimaryInstance>> {
    let data_dir = data_dir.canonicalize()?;
    claim_named(&pipe_name(&data_dir), command).await
}

fn pipe_name(data_dir: &Path) -> String {
    use std::os::windows::ffi::OsStrExt;
    let hash = data_dir
        .as_os_str()
        .encode_wide()
        .fold(0xcbf29ce484222325u64, |hash, unit| {
            (hash ^ u64::from(unit)).wrapping_mul(0x100000001b3)
        });
    format!(r"\\.\pipe\raijin-desktop-v1-{hash:016x}")
}

async fn claim_named(name: &str, command: &DesktopCommand) -> io::Result<Option<PrimaryInstance>> {
    command.validate()?;
    match ServerOptions::new()
        .first_pipe_instance(true)
        .reject_remote_clients(true)
        .create(name)
    {
        Ok(server) => {
            let (sender, receiver) = mpsc::channel(8);
            let worker = tokio::spawn(serve(server, name.to_owned(), sender));
            Ok(Some(PrimaryInstance {
                receiver: Some(receiver),
                worker,
            }))
        }
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            timeout(Duration::from_secs(10), forward(name, command))
                .await
                .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error))??;
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

async fn connect(name: &str, length: u32) -> io::Result<NamedPipeClient> {
    timeout(Duration::from_secs(10), async {
        loop {
            match ClientOptions::new().open(name) {
                Ok(mut client) => match client.write_u32(length).await {
                    Ok(()) => return Ok(client),
                    Err(error) if matches!(error.raw_os_error(), Some(109 | 232 | 233)) => {}
                    Err(error) => return Err(error),
                },
                Err(error) if matches!(error.raw_os_error(), Some(109 | 231 | 232 | 233)) => {}
                Err(error) => return Err(error),
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error))?
}

async fn forward(name: &str, command: &DesktopCommand) -> io::Result<()> {
    let bytes = serde_json::to_vec(command).map_err(io::Error::other)?;
    let length = u32::try_from(bytes.len()).map_err(io::Error::other)?;
    let mut client = connect(name, length).await?;
    client.write_all(&bytes).await?;
    match client.read_u8().await? {
        1 => client.write_u8(1).await,
        _ => Err(io::Error::other("desktop rejected the launch request")),
    }
}

async fn serve(mut server: NamedPipeServer, name: String, sender: mpsc::Sender<DesktopCommand>) {
    loop {
        if let Err(error) = server.connect().await {
            tracing::error!(%error, "desktop instance pipe stopped");
            break;
        }
        let next = match ServerOptions::new()
            .reject_remote_clients(true)
            .create(&name)
        {
            Ok(next) => next,
            Err(error) => {
                tracing::error!(%error, "could not prepare the next desktop pipe");
                break;
            }
        };
        let result = timeout(IO_TIMEOUT, receive(&mut server, &sender)).await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "desktop launch request rejected"),
            Err(error) => tracing::warn!(%error, "desktop launch request timed out"),
        }
        if let Err(error) = server.disconnect() {
            tracing::warn!(%error, "desktop instance pipe disconnect failed");
        }
        server = next;
    }
    std::future::pending::<()>().await;
    drop(server);
}

async fn receive(
    server: &mut NamedPipeServer,
    sender: &mpsc::Sender<DesktopCommand>,
) -> io::Result<()> {
    let length = usize::try_from(server.read_u32().await?).map_err(io::Error::other)?;
    if length == 0 || length > MAX_COMMAND_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid launch request size",
        ));
    }
    let mut bytes = vec![0; length];
    server.read_exact(&mut bytes).await?;
    let command: DesktopCommand = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    command.validate()?;
    sender.try_send(command).map_err(io::Error::other)?;
    server.write_u8(1).await?;
    server.read_u8().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn second_instance_should_forward_and_release_ownership_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let name = pipe_name(dir.path());
        let mut primary = claim_named(&name, &DesktopCommand::Activate)
            .await
            .unwrap()
            .unwrap();
        let mut receiver = primary.take_receiver().unwrap();
        for command in [
            DesktopCommand::BrowserIntegration,
            DesktopCommand::Activate,
            DesktopCommand::Add("https://example.com/a.zip".into()),
            DesktopCommand::Background,
        ] {
            assert!(claim_named(&name, &command).await.unwrap().is_none());
            assert_eq!(receiver.recv().await.unwrap(), command);
        }
        let worker = primary.worker.abort_handle();
        drop(primary);
        timeout(IO_TIMEOUT, async {
            while !worker.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            claim_named(&name, &DesktopCommand::Activate)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn malformed_or_oversized_requests_should_not_kill_the_server() {
        let dir = tempfile::tempdir().unwrap();
        let name = pipe_name(dir.path());
        let mut primary = claim_named(&name, &DesktopCommand::Activate)
            .await
            .unwrap()
            .unwrap();
        let mut receiver = primary.take_receiver().unwrap();
        for bytes in [
            b"\"Unknown\"".to_vec(),
            br#"{"Add":"file:///C:/secret"}"#.to_vec(),
            vec![b'x'; MAX_COMMAND_BYTES + 1],
        ] {
            let mut client = connect(&name, u32::try_from(bytes.len()).unwrap())
                .await
                .unwrap();
            let _ = client.write_all(&bytes).await;
            assert!(
                timeout(IO_TIMEOUT, client.read_u8())
                    .await
                    .unwrap()
                    .is_err()
            );
        }
        assert!(receiver.try_recv().is_err());
        forward(&name, &DesktopCommand::BrowserIntegration)
            .await
            .unwrap();
        assert_eq!(
            receiver.recv().await.unwrap(),
            DesktopCommand::BrowserIntegration
        );
    }

    #[tokio::test]
    async fn concurrent_launches_should_share_one_primary() {
        let dir = tempfile::tempdir().unwrap();
        let mut primary = claim(dir.path(), &DesktopCommand::Activate)
            .await
            .unwrap()
            .unwrap();
        let mut receiver = primary.take_receiver().unwrap();
        let mut launches = tokio::task::JoinSet::new();
        for _ in 0..6 {
            let path = dir.path().join(".");
            launches.spawn(async move {
                assert!(
                    claim(&path, &DesktopCommand::BrowserIntegration)
                        .await
                        .unwrap()
                        .is_none()
                );
            });
        }
        timeout(Duration::from_secs(10), async {
            while let Some(result) = launches.join_next().await {
                result.unwrap();
            }
        })
        .await
        .unwrap();
        for _ in 0..6 {
            assert_eq!(
                receiver.try_recv().unwrap(),
                DesktopCommand::BrowserIntegration
            );
        }
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn incomplete_client_should_time_out_without_blocking_future_launches() {
        let dir = tempfile::tempdir().unwrap();
        let name = pipe_name(dir.path());
        let mut primary = claim_named(&name, &DesktopCommand::Activate)
            .await
            .unwrap()
            .unwrap();
        let mut receiver = primary.take_receiver().unwrap();
        let _stalled = connect(&name, 1024).await.unwrap();
        timeout(
            Duration::from_secs(5),
            forward(&name, &DesktopCommand::BrowserIntegration),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            receiver.try_recv().unwrap(),
            DesktopCommand::BrowserIntegration
        );
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn full_queue_should_reject_instead_of_opening_unbounded_windows() {
        let dir = tempfile::tempdir().unwrap();
        let name = pipe_name(dir.path());
        let _primary = claim_named(&name, &DesktopCommand::Activate)
            .await
            .unwrap()
            .unwrap();
        for _ in 0..8 {
            forward(&name, &DesktopCommand::BrowserIntegration)
                .await
                .unwrap();
        }
        assert!(
            forward(&name, &DesktopCommand::BrowserIntegration)
                .await
                .is_err()
        );
    }
}

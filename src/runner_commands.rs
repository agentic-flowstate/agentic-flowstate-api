use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::handlers::chat_client_manager::ChatClientManager;

pub const RUNNER_COMMAND_SOCKET_ENV: &str = "AGENTIC_RUNNER_COMMAND_SOCKET";

const DEFAULT_RUNNER_COMMAND_TIMEOUT_MS: u64 = 750;
const STALE_SOCKET_CONNECT_TIMEOUT_MS: u64 = 100;
const SERVER_READ_TIMEOUT_SECONDS: u64 = 5;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "command", rename_all = "snake_case")]
enum RunnerCommand {
    CancelConversation { conversation_id: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RunnerCancelCommandResult {
    pub conversation_id: String,
    pub marker_set: bool,
    pub interrupted: bool,
    pub error: Option<String>,
}

pub struct RunnerCommandServer {
    path: PathBuf,
    shutdown: CancellationToken,
    task: JoinHandle<()>,
}

impl RunnerCommandServer {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn shutdown(self) {
        self.shutdown.cancel();
        if let Err(e) = self.task.await {
            tracing::warn!("Runner command server task failed to join: {}", e);
        }
    }
}

pub fn runner_command_socket_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os(RUNNER_COMMAND_SOCKET_ENV) {
        let path = PathBuf::from(path);
        if path.as_os_str().is_empty() {
            bail!("{RUNNER_COMMAND_SOCKET_ENV} is set but empty");
        }
        return Ok(path);
    }

    dirs::home_dir()
        .map(|home| {
            home.join(".agentic-flowstate")
                .join("agent-runner-command.sock")
        })
        .ok_or_else(|| {
            anyhow::anyhow!("Failed to resolve home directory for runner command socket")
        })
}

#[cfg(unix)]
pub async fn start_runner_command_server(
    manager: Arc<ChatClientManager>,
    shutdown: CancellationToken,
) -> Result<RunnerCommandServer> {
    let path = runner_command_socket_path()?;
    start_runner_command_server_at_path(path, manager, shutdown).await
}

#[cfg(not(unix))]
pub async fn start_runner_command_server(
    _manager: Arc<ChatClientManager>,
    _shutdown: CancellationToken,
) -> Result<RunnerCommandServer> {
    bail!("runner command server requires Unix domain socket support");
}

#[cfg(unix)]
pub async fn send_cancel_conversation_command(
    conversation_id: &str,
) -> Result<RunnerCancelCommandResult> {
    let path = runner_command_socket_path()?;
    send_cancel_conversation_command_to_path(
        &path,
        conversation_id,
        Duration::from_millis(DEFAULT_RUNNER_COMMAND_TIMEOUT_MS),
    )
    .await
}

#[cfg(not(unix))]
pub async fn send_cancel_conversation_command(
    _conversation_id: &str,
) -> Result<RunnerCancelCommandResult> {
    bail!("runner cancel command requires Unix domain socket support");
}

#[cfg(unix)]
async fn start_runner_command_server_at_path(
    path: PathBuf,
    manager: Arc<ChatClientManager>,
    shutdown: CancellationToken,
) -> Result<RunnerCommandServer> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    prepare_socket_path(&path).await?;
    let listener = UnixListener::bind(&path)
        .with_context(|| format!("Failed to bind runner command socket {}", path.display()))?;
    tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .await
        .with_context(|| {
            format!(
                "Failed to restrict runner command socket permissions at {}",
                path.display()
            )
        })?;

    tracing::info!("Runner command server listening at {}", path.display());

    let server_shutdown = shutdown.child_token();
    let task_shutdown = server_shutdown.clone();
    let task_path = path.clone();
    let task = tokio::spawn(async move {
        run_command_accept_loop(listener, manager, task_shutdown, task_path).await;
    });

    Ok(RunnerCommandServer {
        path,
        shutdown: server_shutdown,
        task,
    })
}

#[cfg(unix)]
async fn run_command_accept_loop(
    listener: tokio::net::UnixListener,
    manager: Arc<ChatClientManager>,
    shutdown: CancellationToken,
    path: PathBuf,
) {
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let manager = manager.clone();
                        tokio::spawn(async move {
                            if let Err(e) = handle_runner_command_connection(stream, manager).await {
                                tracing::warn!("Runner command connection failed: {}", e);
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("Runner command socket accept failed: {}", e);
                    }
                }
            }
        }
    }

    cleanup_socket_path(&path).await;
    tracing::info!("Runner command server stopped at {}", path.display());
}

#[cfg(unix)]
async fn handle_runner_command_connection(
    stream: tokio::net::UnixStream,
    manager: Arc<ChatClientManager>,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let bytes_read = tokio::time::timeout(
        Duration::from_secs(SERVER_READ_TIMEOUT_SECONDS),
        reader.read_line(&mut line),
    )
    .await
    .context("Timed out reading runner command")?
    .context("Failed reading runner command")?;
    if bytes_read == 0 {
        bail!("runner command connection closed before sending a command");
    }

    let command: RunnerCommand =
        serde_json::from_str(line.trim_end()).context("Failed to decode runner command")?;
    let result = execute_runner_command(command, manager).await;

    let mut stream = reader.into_inner();
    let mut response =
        serde_json::to_vec(&result).context("Failed to encode runner command response")?;
    response.push(b'\n');
    stream
        .write_all(&response)
        .await
        .context("Failed writing runner command response")?;
    stream
        .flush()
        .await
        .context("Failed flushing runner command response")?;

    Ok(())
}

#[cfg(unix)]
async fn execute_runner_command(
    command: RunnerCommand,
    manager: Arc<ChatClientManager>,
) -> RunnerCancelCommandResult {
    match command {
        RunnerCommand::CancelConversation { conversation_id } => {
            let started = chrono::Utc::now().timestamp_millis();
            manager.mark_cancelled_turn(&conversation_id).await;
            match manager.interrupt(&conversation_id).await {
                Ok(interrupted) => {
                    tracing::info!(
                        "[CANCEL] Runner command applied for conversation {} interrupted={} command_elapsed_ms={}",
                        conversation_id,
                        interrupted,
                        chrono::Utc::now().timestamp_millis().saturating_sub(started)
                    );
                    RunnerCancelCommandResult {
                        conversation_id,
                        marker_set: true,
                        interrupted,
                        error: None,
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "[CANCEL] Runner command marked cancellation but failed to interrupt conversation {} after {}ms: {}",
                        conversation_id,
                        chrono::Utc::now().timestamp_millis().saturating_sub(started),
                        e
                    );
                    RunnerCancelCommandResult {
                        conversation_id,
                        marker_set: true,
                        interrupted: false,
                        error: Some(e),
                    }
                }
            }
        }
    }
}

#[cfg(unix)]
async fn send_cancel_conversation_command_to_path(
    path: &Path,
    conversation_id: &str,
    timeout_duration: Duration,
) -> Result<RunnerCancelCommandResult> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    let mut stream = tokio::time::timeout(timeout_duration, UnixStream::connect(path))
        .await
        .with_context(|| {
            format!(
                "Timed out connecting to runner command socket {}",
                path.display()
            )
        })?
        .with_context(|| {
            format!(
                "Failed to connect to runner command socket {}",
                path.display()
            )
        })?;

    let mut command = serde_json::to_vec(&RunnerCommand::CancelConversation {
        conversation_id: conversation_id.to_string(),
    })
    .context("Failed to encode runner cancel command")?;
    command.push(b'\n');
    tokio::time::timeout(timeout_duration, stream.write_all(&command))
        .await
        .context("Timed out writing runner cancel command")?
        .context("Failed writing runner cancel command")?;
    tokio::time::timeout(timeout_duration, stream.flush())
        .await
        .context("Timed out flushing runner cancel command")?
        .context("Failed flushing runner cancel command")?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let bytes_read = tokio::time::timeout(timeout_duration, reader.read_line(&mut line))
        .await
        .context("Timed out waiting for runner cancel acknowledgement")?
        .context("Failed reading runner cancel acknowledgement")?;
    if bytes_read == 0 {
        bail!("runner command socket closed without acknowledging cancellation");
    }

    serde_json::from_str(line.trim_end()).context("Failed to decode runner cancel acknowledgement")
}

#[cfg(unix)]
async fn prepare_socket_path(path: &Path) -> Result<()> {
    use std::io::ErrorKind;
    use std::os::unix::fs::FileTypeExt;
    use tokio::net::UnixStream;

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.with_context(|| {
            format!(
                "Failed to create runner command socket directory {}",
                parent.display()
            )
        })?;
    }

    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => {
            if !metadata.file_type().is_socket() {
                bail!(
                    "Runner command socket path exists but is not a socket: {}",
                    path.display()
                );
            }

            match tokio::time::timeout(
                Duration::from_millis(STALE_SOCKET_CONNECT_TIMEOUT_MS),
                UnixStream::connect(path),
            )
            .await
            {
                Ok(Ok(_stream)) => {
                    bail!(
                        "Runner command socket already has a live listener at {}",
                        path.display()
                    );
                }
                Ok(Err(_)) => {
                    tokio::fs::remove_file(path).await.with_context(|| {
                        format!(
                            "Failed to remove stale runner command socket {}",
                            path.display()
                        )
                    })?;
                }
                Err(_) => {
                    bail!(
                        "Timed out checking existing runner command socket at {}",
                        path.display()
                    );
                }
            }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "Failed to inspect runner command socket path {}",
                    path.display()
                )
            });
        }
    }

    Ok(())
}

#[cfg(unix)]
async fn cleanup_socket_path(path: &Path) {
    use std::io::ErrorKind;
    use std::os::unix::fs::FileTypeExt;

    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_socket() => {
            if let Err(e) = tokio::fs::remove_file(path).await {
                tracing::warn!(
                    "Failed to remove runner command socket {}: {}",
                    path.display(),
                    e
                );
            }
        }
        Ok(_) => {
            tracing::warn!(
                "Not removing runner command path because it is no longer a socket: {}",
                path.display()
            );
        }
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(
                "Failed to inspect runner command socket during cleanup {}: {}",
                path.display(),
                e
            );
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{
        prepare_socket_path, send_cancel_conversation_command_to_path,
        start_runner_command_server_at_path,
    };
    use crate::handlers::chat_client_manager::ChatClientManager;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::UnixListener;
    use tokio_util::sync::CancellationToken;

    fn temp_socket_path(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(format!("/tmp/af-{}-{}.sock", name, uuid::Uuid::new_v4()))
    }

    #[tokio::test]
    async fn cancel_command_marks_runner_manager() {
        let path = temp_socket_path("cancel");
        let manager = Arc::new(ChatClientManager::new());
        let shutdown = CancellationToken::new();
        let server =
            start_runner_command_server_at_path(path.clone(), manager.clone(), shutdown).await;
        let server = server.expect("start command server");

        let result =
            send_cancel_conversation_command_to_path(&path, "conv-1", Duration::from_secs(1))
                .await
                .expect("send cancel command");

        assert_eq!(result.conversation_id, "conv-1");
        assert!(result.marker_set);
        assert!(!result.interrupted);
        assert_eq!(result.error, None);
        assert!(manager.is_turn_cancelled("conv-1").await);

        server.shutdown().await;
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn live_socket_path_is_not_replaced() {
        let path = temp_socket_path("live");
        let _listener = UnixListener::bind(&path).expect("bind live socket");

        let err = prepare_socket_path(&path)
            .await
            .expect_err("live socket should be rejected");

        assert!(
            err.to_string().contains("already has a live listener"),
            "unexpected error: {err:#}"
        );
        assert!(path.exists());
        let _ = std::fs::remove_file(&path);
    }
}

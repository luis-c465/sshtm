//! Unix socket server and coordinated process lifecycle.
pub mod ready;
pub mod sshcmd;
pub mod supervisor;
pub mod table;

use crate::{
    ipc::{self, Command, Response, ResponseResult},
    paths::RuntimePaths,
};
use anyhow::{Context, Result, ensure};
use nix::unistd::getuid;
use serde_json::json;
use std::{fs, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};
use tokio::{
    net::{UnixListener, UnixStream},
    signal::unix::{SignalKind, signal},
    sync::watch,
    task::JoinSet,
    time::interval,
};

#[derive(Clone, Debug)]
pub struct Settings {
    pub idle_exit: Duration,
    pub supervisor: supervisor::Settings,
    /// Inherited close-only startup lock, shared with the autostarting client.
    pub startup_lock: Option<Arc<fs::File>>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            idle_exit: Duration::from_secs(30),
            supervisor: supervisor::Settings::default(),
            startup_lock: None,
        }
    }
}
struct SocketGuard(std::path::PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub async fn run(paths: RuntimePaths, settings: Settings) -> Result<()> {
    // Install handlers before publishing readiness to the starter.
    let mut term = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let lock = if let Some(lock) = settings.startup_lock {
        crate::client::validate_startup_lock(&lock, &paths)?;
        lock
    } else {
        tokio::select! {
            biased;
            _ = term.recv() => return Ok(()),
            _ = interrupt.recv() => return Ok(()),
            lock = crate::client::startup_lock(&paths) => Arc::new(lock?),
        }
    };
    match UnixStream::connect(&paths.socket).await {
        Ok(_) => anyhow::bail!("daemon is already running"),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) => {}
        Err(e) => return Err(e).context("check existing daemon socket"),
    }
    match fs::remove_file(&paths.socket) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(&paths.socket).context("bind daemon socket")?;
    let _socket = SocketGuard(paths.socket.clone());
    fs::set_permissions(&paths.socket, fs::Permissions::from_mode(0o600))?;
    drop(lock);
    let table = Arc::new(table::Table::default());
    let (stop, mut stopping) = watch::channel(false);
    let mut clients = JoinSet::new();
    let mut timer = interval(Duration::from_millis(20));
    let serving: Result<()> = loop {
        tokio::select! {
            biased;
            _ = term.recv() => break Ok(()),
            _ = interrupt.recv() => break Ok(()),
            _ = stopping.changed() => break Ok(()),
            _ = timer.tick() => {
                if table.idle_for().is_some_and(|elapsed| elapsed >= settings.idle_exit) {
                    break Ok(());
                }
            }
            accepted = listener.accept() => {
                let (stream, _) = match accepted { Ok(value) => value, Err(e) => break Err(e.into()) };
                let table = table.clone();
                let settings = settings.supervisor.clone();
                let stop = stop.clone();
                clients.spawn(async move { serve(stream, table, settings, stop).await });
            }
            Some(result) = clients.join_next(), if !clients.is_empty() => {
                // Malformed/disconnected local clients must not stop the daemon.
                if let Err(error) = result { eprintln!("client task: {error}"); }
            }
        }
    };
    // A stalled client must not delay signal/shutdown cleanup. Open reservations
    // live in the table, not request futures, so aborting them cannot leak children.
    clients.abort_all();
    while clients.join_next().await.is_some() {}
    table.shutdown().await?;
    drop(listener);
    serving
}
async fn serve(
    mut stream: UnixStream,
    table: Arc<table::Table>,
    settings: supervisor::Settings,
    stop: watch::Sender<bool>,
) -> Result<()> {
    ensure!(
        stream.peer_cred()?.uid() == getuid().as_raw(),
        "peer uid does not match daemon uid"
    );
    let mut shutdown = false;
    let result: Result<serde_json::Value> = async {
        let request = ipc::read_request(&mut stream).await?;
        ensure!(
            request.v == ipc::VERSION,
            "daemon speaks protocol {}; run 'sshtm shutdown' (request version {})",
            ipc::VERSION,
            request.v
        );
        match request.command {
            Command::Open {
                host,
                forward,
                auth_sock,
            } => Ok(serde_json::to_value(
                table.open(host, forward, auth_sock, settings).await?,
            )?),
            Command::List => Ok(serde_json::to_value(table.list())?),
            Command::Kill { indices, all } => {
                let missing = table.kill(indices, all).await?;
                ensure!(
                    missing.is_empty(),
                    "{}",
                    missing
                        .iter()
                        .map(|i| format!("no tunnel {i}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                Ok(json!(null))
            }
            Command::Logs { index } => Ok(json!(table.logs(index).await?)),
            Command::Ping => Ok(json!({"pid": std::process::id()})),
            Command::Shutdown => {
                table.shutdown().await?;
                shutdown = true;
                Ok(json!(null))
            }
        }
    }
    .await;
    let response = Response {
        v: ipc::VERSION,
        result: match result {
            Ok(data) => ResponseResult::Ok { data },
            Err(error) => ResponseResult::Error {
                message: format!("{error:#}"),
            },
        },
    };
    let written = ipc::write_response(&mut stream, &response).await;
    if shutdown {
        let _ = stop.send(true);
    }
    written
}

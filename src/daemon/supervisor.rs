//! One SSH connection, supervised independently of the daemon and index table.
use std::{
    collections::VecDeque,
    ffi::OsString,
    net::TcpListener,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::{Pid, getppid},
};
use tokio::{
    io::{AsyncReadExt, BufReader},
    process::{Child, Command},
    sync::{oneshot, watch},
    task::JoinHandle,
    time::{Instant, sleep, timeout},
};

use crate::{
    daemon::{ready, sshcmd},
    spec::Forward,
};

pub const LOG_LINES: usize = 200;
// Bound individual lines too: an SSH/ProxyCommand diagnostic need not contain '\n'.
pub const LOG_LINE_BYTES: usize = 4096;
pub const AUTO_PORT_ATTEMPTS: usize = 3;

#[derive(Clone, Debug)]
pub struct Settings {
    pub ready_timeout: Duration,
    pub ready_poll: Duration,
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    pub backoff_reset: Duration,
    pub kill_grace: Duration,
    /// Per-command PATH override, useful for embedding and isolated fixtures.
    pub path: Option<OsString>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            ready_timeout: Duration::from_secs(20),
            ready_poll: Duration::from_millis(20),
            backoff_min: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            backoff_reset: Duration::from_secs(60),
            kill_grace: Duration::from_secs(2),
            path: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Starting,
    Up,
    Reconnecting,
    Stopped,
    Failed(String),
}
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub state: State,
    pub local: u16,
    pub pid: Option<u32>,
    pub attempt: u32,
    pub next_retry: Option<Instant>,
    pub up_since: Option<Instant>,
}
type Logs = Arc<Mutex<VecDeque<String>>>;
pub(crate) type PortAllocator = Arc<dyn Fn() -> Result<u16> + Send + Sync>;

/// Dropping a handle requests the same cleanup as `kill`, but does not wait.
/// Call `kill` when port release must be confirmed before returning.
pub struct Tunnel {
    state: watch::Receiver<Snapshot>,
    logs: Logs,
    cancel: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}
impl Tunnel {
    pub async fn open(
        host: String,
        forward: Forward,
        auth_sock: Option<OsString>,
        settings: Settings,
    ) -> Result<Self> {
        let mut tunnel = Self::start(host, forward, auth_sock, settings)?;
        if let Err(error) = tunnel.wait_ready().await {
            tunnel.kill().await?;
            return Err(error);
        }
        Ok(tunnel)
    }

    /// Start supervision without waiting for readiness, so the daemon can reserve
    /// an index and cancel a starting connection while retaining its join handle.
    pub fn start(
        host: String,
        forward: Forward,
        auth_sock: Option<OsString>,
        settings: Settings,
    ) -> Result<Self> {
        let local = forward.local.unwrap_or(0);
        Self::start_at(host, forward, auth_sock, settings, local, None)
    }

    pub(crate) fn start_at(
        host: String,
        forward: Forward,
        auth_sock: Option<OsString>,
        settings: Settings,
        local: u16,
        port_allocator: Option<PortAllocator>,
    ) -> Result<Self> {
        sshcmd::validate_host(&host)?;
        // Also validate programmatically constructed Forward values before any spawn.
        sshcmd::forward_args(&host, &forward, forward.local.unwrap_or(1), false)?;
        anyhow::ensure!(
            !settings.ready_poll.is_zero(),
            "readiness poll must be positive"
        );
        anyhow::ensure!(
            !settings.backoff_min.is_zero() && settings.backoff_max >= settings.backoff_min,
            "invalid backoff settings"
        );
        let initial = Snapshot {
            state: State::Starting,
            local,
            pid: None,
            attempt: 0,
            next_retry: None,
            up_since: None,
        };
        let (tx, state) = watch::channel(initial);
        let (cancel, stop) = oneshot::channel();
        let logs = Arc::new(Mutex::new(VecDeque::new()));
        let worker_logs = logs.clone();
        let task = tokio::spawn(async move {
            let mut worker = Worker {
                host,
                forward,
                auth_sock,
                settings,
                port_allocator,
                tx,
                logs: worker_logs,
                stop,
            };
            worker.run().await;
        });
        Ok(Self {
            state,
            logs,
            cancel: Some(cancel),
            task: Some(task),
        })
    }
    async fn wait_ready(&mut self) -> Result<()> {
        loop {
            match self.snapshot().state {
                State::Up => return Ok(()),
                State::Failed(error) => bail!("{error}"),
                State::Stopped => bail!("tunnel stopped before readiness"),
                _ => self.state.changed().await.context("supervisor stopped")?,
            }
        }
    }
    pub fn snapshot(&self) -> Snapshot {
        self.state.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<Snapshot> {
        self.state.clone()
    }
    pub fn logs(&self) -> Vec<String> {
        self.logs.lock().unwrap().iter().cloned().collect()
    }
    pub async fn kill(&mut self) -> Result<()> {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
        if let Some(task) = self.task.as_mut() {
            let result = task.await.context("supervisor task panicked");
            self.task.take();
            result?;
        }
        Ok(())
    }
}
impl Drop for Tunnel {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

struct Worker {
    host: String,
    forward: Forward,
    auth_sock: Option<OsString>,
    settings: Settings,
    port_allocator: Option<PortAllocator>,
    tx: watch::Sender<Snapshot>,
    logs: Logs,
    stop: oneshot::Receiver<()>,
}
impl Worker {
    async fn run(&mut self) {
        let mut established = false;
        let mut delay = self.settings.backoff_min;
        let mut auto_attempt = 0;
        loop {
            if self.tx.borrow().local == 0 {
                let port = match &self.port_allocator {
                    Some(allocate) => allocate(),
                    None => TcpListener::bind(("127.0.0.1", 0))
                        .and_then(|s| s.local_addr())
                        .map(|a| a.port())
                        .map_err(Into::into),
                };
                match port {
                    Ok(port) => self.tx.send_modify(|s| s.local = port),
                    Err(error) => {
                        self.fail(format!("allocate local port: {error}"));
                        return;
                    }
                }
            }
            let result = tokio::select! {
                biased;
                _ = &mut self.stop => break,
                result = sshcmd::probe(&self.host, self.auth_sock.as_deref(), self.settings.path.as_deref()) => result,
            };
            let result = match result {
                Ok(keepalive) => self.connection(keepalive).await,
                Err(error) => Err(error),
            };
            match result {
                Ok(ConnectionEnd::Stopped) => break,
                Ok(ConnectionEnd::Exited(uptime)) => {
                    established = true;
                    if uptime >= self.settings.backoff_reset {
                        delay = self.settings.backoff_min;
                        self.tx.send_modify(|s| s.attempt = 0);
                    }
                }
                Err(error) if !established => {
                    auto_attempt += 1;
                    // Only the current attempt can qualify for a bind retry.
                    let text = format!("{error:#}");
                    if self.forward.local.is_none()
                        && auto_attempt < AUTO_PORT_ATTEMPTS
                        && text.to_ascii_lowercase().contains("address already in use")
                    {
                        self.tx.send_modify(|s| s.local = 0);
                        continue;
                    }
                    self.fail(format!(
                        "{text}\nrun 'ssh {}' once interactively",
                        self.host
                    ));
                    return;
                }
                Err(error) => push_log(&self.logs, format!("sshtm: {error:#}")),
            }
            self.tx.send_modify(|s| {
                s.state = State::Reconnecting;
                s.pid = None;
                s.up_since = None;
                s.attempt = s.attempt.saturating_add(1);
                s.next_retry = Some(Instant::now() + delay);
            });
            tokio::select! { biased; _ = &mut self.stop => break, _ = sleep(delay) => {} }
            delay = next_backoff(delay, self.settings.backoff_max);
        }
        self.tx.send_modify(|s| {
            s.state = State::Stopped;
            s.pid = None;
            s.next_retry = None;
            s.up_since = None;
        });
    }
    fn fail(&self, error: String) {
        self.tx.send_modify(|s| {
            s.state = State::Failed(error);
            s.pid = None;
            s.next_retry = None;
            s.up_since = None;
        });
    }
    async fn connection(&mut self, keepalive: bool) -> Result<ConnectionEnd> {
        let args =
            sshcmd::forward_args(&self.host, &self.forward, self.tx.borrow().local, keepalive)?;
        let mut command = Command::new("ssh");
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        if let Some(path) = &self.settings.path {
            command.env("PATH", path);
        }
        match &self.auth_sock {
            Some(sock) => {
                command.env("SSH_AUTH_SOCK", sock);
            }
            None => {
                command.env_remove("SSH_AUTH_SOCK");
            }
        }
        let parent = nix::unistd::getpid();
        // SAFETY: only async-signal-safe syscalls run between fork and exec.
        unsafe {
            command.pre_exec(move || {
                nix::sys::prctl::set_pdeathsig(Signal::SIGTERM).map_err(std::io::Error::from)?;
                if getppid() != parent {
                    return Err(std::io::Error::other("supervisor parent exited"));
                }
                Ok(())
            });
        }
        let child = command.spawn().context("spawn ssh")?;
        let pid = child.id().context("ssh has no pid")?;
        let mut child = ChildGuard { child, pid };
        let stderr = child.child.stderr.take().unwrap();
        let logs = self.logs.clone();
        let attempt_logs = Arc::new(Mutex::new(VecDeque::new()));
        let diagnostics = attempt_logs.clone();
        let reader = tokio::spawn(async move { read_logs(stderr, logs, diagnostics).await });
        self.tx.send_modify(|s| {
            s.pid = Some(pid);
            s.next_retry = None;
        });
        let started = Instant::now();
        let mut up = None;
        let outcome = loop {
            tokio::select! {
                biased;
                _ = &mut self.stop => break Ok(ConnectionEnd::Stopped),
                result = child.child.wait() => {
                    break match (result, up) {
                        (Ok(_), Some(since)) => Ok(ConnectionEnd::Exited(Instant::now() - since)),
                        (Ok(status), None) => Err(anyhow::anyhow!("ssh exited before readiness: {status}")),
                        (Err(error), _) => Err(error.into()),
                    };
                }
                _ = sleep(self.settings.ready_poll) => {
                    if up.is_none() {
                        if ready::listening(pid, self.tx.borrow().local) {
                            let since = Instant::now();
                            up = Some(since);
                            self.tx.send_modify(|s| { s.state = State::Up; s.up_since = Some(since); });
                        } else if started.elapsed() >= self.settings.ready_timeout {
                            break Err(anyhow::anyhow!("ssh readiness timed out after {:?}", self.settings.ready_timeout));
                        }
                    }
                }
            }
        };
        child.terminate(self.settings.kill_grace).await;
        // Killing the process group closes inherited stderr pipes as well.
        let mut reader = reader;
        if timeout(Duration::from_millis(200), &mut reader)
            .await
            .is_err()
        {
            reader.abort();
            let _ = reader.await;
        }
        outcome.map_err(|error| {
            let tail = attempt_logs
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            error.context(tail)
        })
    }
}

enum ConnectionEnd {
    Stopped,
    Exited(Duration),
}
struct ChildGuard {
    child: Child,
    pid: u32,
}
impl ChildGuard {
    async fn terminate(&mut self, grace: Duration) {
        let group = Pid::from_raw(self.pid as i32);
        let _ = killpg(group, Signal::SIGTERM);
        if timeout(grace, self.child.wait()).await.is_err() {
            let _ = killpg(group, Signal::SIGKILL);
            let _ = self.child.wait().await;
        }
        // Clean up ProxyCommand children even if the group leader exited first.
        let _ = killpg(group, Signal::SIGKILL);
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = killpg(Pid::from_raw(self.pid as i32), Signal::SIGKILL);
    }
}

pub fn next_backoff(current: Duration, maximum: Duration) -> Duration {
    current.saturating_mul(2).min(maximum)
}
fn push_log(logs: &Logs, line: String) {
    let mut logs = logs.lock().unwrap();
    if logs.len() == LOG_LINES {
        logs.pop_front();
    }
    logs.push_back(line);
}
fn push_diagnostic(logs: &Logs, attempt: &Logs, line: &[u8]) {
    let line = String::from_utf8_lossy(line).into_owned();
    push_log(logs, line.clone());
    push_log(attempt, line);
}
async fn read_logs(stderr: tokio::process::ChildStderr, logs: Logs, attempt: Logs) {
    let mut reader = BufReader::new(stderr);
    let mut buffer = [0; 1024];
    let mut line = Vec::new();
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) | Err(_) => {
                if !line.is_empty() {
                    push_diagnostic(&logs, &attempt, &line);
                }
                break;
            }
            Ok(n) => {
                for &byte in &buffer[..n] {
                    if byte == b'\n' {
                        push_diagnostic(&logs, &attempt, &line);
                        line.clear();
                    } else {
                        line.push(byte);
                        if line.len() == LOG_LINE_BYTES {
                            push_diagnostic(&logs, &attempt, &line);
                            line.clear();
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capped_backoff_and_ring() {
        let max = Duration::from_secs(60);
        let mut delay = Duration::from_secs(1);
        let mut seen = Vec::new();
        for _ in 0..9 {
            seen.push(delay.as_secs());
            delay = next_backoff(delay, max);
        }
        assert_eq!(seen, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
        let logs = Arc::new(Mutex::new(VecDeque::new()));
        for n in 0..201 {
            push_log(&logs, n.to_string());
        }
        let logs = logs.lock().unwrap();
        assert_eq!(logs.len(), LOG_LINES);
        assert_eq!(logs.front().unwrap(), "1");
        assert_eq!(logs.back().unwrap(), "200");
    }
}

//! Daemon connections, startup locking, and detached autostart.
use std::{
    fs::File,
    io,
    os::{
        fd::AsRawFd,
        unix::{fs::MetadataExt, process::CommandExt},
    },
    path::PathBuf,
    process::{Command as ProcessCommand, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use nix::{
    fcntl::{FcntlArg, FdFlag, fcntl},
    libc,
};
use tokio::net::UnixStream;

use crate::{
    ipc::{self, Command, Request, Response},
    paths::{RuntimePaths, private_file},
};

pub const AUTOSTART_WAIT: Duration = Duration::from_secs(2);

fn try_lock(file: &File) -> io::Result<()> {
    // flock is nonblocking and the descriptor remains owned by the caller.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Close-only lock ownership: an inherited duplicate keeps the shared flock
/// alive after the starter exits. Never explicitly LOCK_UN this descriptor.
pub async fn startup_lock(paths: &RuntimePaths) -> Result<File> {
    let file = private_file(&paths.lock, false)?;
    loop {
        match try_lock(&file) {
            Ok(()) => return Ok(file),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Validate an inherited lock against the secure runtime path, restore CLOEXEC
/// before any SSH child can be spawned, and ensure exclusive flock ownership.
pub fn validate_startup_lock(file: &File, paths: &RuntimePaths) -> Result<()> {
    fcntl(file, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))?;
    let expected = private_file(&paths.lock, false)?.metadata()?;
    let actual = file.metadata()?;
    ensure!(
        actual.is_file()
            && actual.uid() == nix::unistd::Uid::current().as_raw()
            && actual.mode() & 0o077 == 0
            && actual.nlink() == 1
            && actual.dev() == expected.dev()
            && actual.ino() == expected.ino(),
        "inherited startup lock does not match {}",
        paths.lock.display()
    );
    try_lock(file).context("inherited startup lock is not exclusively held")
}

#[derive(Debug, Clone)]
pub struct Client {
    pub paths: RuntimePaths,
    executable: Option<PathBuf>,
}

impl Client {
    pub fn new(paths: RuntimePaths) -> Self {
        Self {
            paths,
            executable: None,
        }
    }

    /// Override the daemon executable when embedding the client in another binary.
    pub fn with_executable(paths: RuntimePaths, executable: PathBuf) -> Self {
        Self {
            paths,
            executable: Some(executable),
        }
    }

    pub async fn connect(&self) -> Result<Option<UnixStream>> {
        match UnixStream::connect(&self.paths.socket).await {
            Ok(stream) => Ok(Some(stream)),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error).context("connect to daemon"),
        }
    }

    pub async fn request(&self, command: Command) -> Result<Option<Response>> {
        let mut stream = match self.connect().await? {
            Some(stream) => stream,
            None if matches!(command, Command::Open { .. }) => self.autostart().await?,
            None => return Ok(None),
        };
        ipc::write_request(&mut stream, &Request::new(command)).await?;
        Ok(Some(ipc::read_response(&mut stream).await?))
    }

    async fn autostart(&self) -> Result<UnixStream> {
        let client = self.clone();
        // Dropping the requester must not release the lock while a late daemon
        // can still bind. This task owns the entire startup and cleanup transaction.
        tokio::spawn(async move { client.startup_transaction().await }).await?
    }

    async fn startup_transaction(&self) -> Result<UnixStream> {
        let guard = startup_lock(&self.paths).await?;
        if let Some(stream) = self.connect().await? {
            return Ok(stream);
        }
        let log = private_file(&self.paths.log, true)?;
        let executable = match &self.executable {
            Some(path) => path.clone(),
            None => std::env::current_exe()?,
        };
        let lock_fd = guard.as_raw_fd();
        let mut command = ProcessCommand::new(executable);
        command
            .arg("daemon")
            .arg("--startup-lock-fd")
            .arg(lock_fd.to_string())
            .arg("--runtime-dir")
            .arg(&self.paths.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log));
        // setsid is async-signal-safe; no allocations or locks in the fork child.
        unsafe {
            command.pre_exec(move || {
                nix::unistd::setsid().map_err(io::Error::from)?;
                // Only the fork child clears CLOEXEC. The parent's copy stays
                // private, and close-only ownership preserves the child's lock.
                if libc::fcntl(lock_fd, libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().context("start daemon")?;
        let deadline = tokio::time::Instant::now() + AUTOSTART_WAIT;
        let outcome = async {
            loop {
                if let Some(stream) = self.connect().await? {
                    return Ok(stream);
                }
                if let Some(status) = child.try_wait()? {
                    bail!(
                        "daemon exited with {status}; see {}",
                        self.paths.log.display()
                    );
                }
                if tokio::time::Instant::now() >= deadline {
                    bail!(
                        "timed out starting daemon; see {}",
                        self.paths.log.display()
                    );
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        .await;
        match outcome {
            Ok(stream) => {
                // Reap when the detached daemon eventually exits without tying up a worker.
                tokio::spawn(async move {
                    while let Ok(None) = child.try_wait() {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                });
                Ok(stream)
            }
            Err(error) => {
                // Every failure path reaps the starter before releasing the lock.
                tokio::task::spawn_blocking(move || {
                    let _ = child.kill();
                    child.wait()
                })
                .await
                .context("join daemon startup cleanup")?
                .context("reap failed daemon starter")?;
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn absent_read_only_requests_do_not_start_daemon() {
        let root = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::at(root.path().join("runtime")).unwrap();
        let client = Client::new(paths);
        assert!(client.request(Command::List).await.unwrap().is_none());
        assert!(!client.paths.lock.exists());
        assert!(!client.paths.log.exists());
    }

    fn mock_starter(root: &std::path::Path, fail_connect: bool) -> Client {
        use std::os::unix::fs::PermissionsExt;
        let paths = RuntimePaths::at(root.join("runtime")).unwrap();
        let script = root.join("starter");
        // The hidden daemon arguments put the runtime directory in $5.
        let fail = if fail_connect {
            "ln -s sock \"$5/sock\"\n"
        } else {
            ""
        };
        std::fs::write(
            &script,
            format!("#!/bin/sh\nprintf '%s' \"$$\" > \"$5/pid\"\n{fail}exec sleep 30\n"),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        Client::with_executable(paths, script)
    }

    async fn starter_pid(paths: &RuntimePaths) -> nix::unistd::Pid {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(value) = std::fs::read_to_string(paths.dir.join("pid"))
                    && let Ok(pid) = value.parse::<i32>()
                {
                    return nix::unistd::Pid::from_raw(pid);
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn cancelled_startup_retains_lock_until_child_cleanup() {
        let root = tempfile::tempdir().unwrap();
        let client = mock_starter(root.path(), false);
        let paths = client.paths.clone();
        let caller = tokio::spawn(async move { client.autostart().await });
        let pid = starter_pid(&paths).await;
        caller.abort();
        let _ = caller.await;
        let file = private_file(&paths.lock, false).unwrap();
        assert!(try_lock(&file).is_err());
        let guard = tokio::time::timeout(Duration::from_secs(4), startup_lock(&paths))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            nix::sys::signal::kill(pid, None),
            Err(nix::errno::Errno::ESRCH)
        );
        drop(guard);
    }

    #[tokio::test]
    async fn connect_failure_reaps_starter_before_unlocking() {
        let root = tempfile::tempdir().unwrap();
        let client = mock_starter(root.path(), true);
        let paths = client.paths.clone();
        let caller = tokio::spawn(async move { client.autostart().await });
        let pid = starter_pid(&paths).await;
        assert!(caller.await.unwrap().is_err());
        let guard = startup_lock(&paths).await.unwrap();
        assert_eq!(
            nix::sys::signal::kill(pid, None),
            Err(nix::errno::Errno::ESRCH)
        );
        drop(guard);
    }

    #[tokio::test]
    async fn inherited_lock_is_close_only_and_validated() {
        let root = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::at(root.path().join("runtime")).unwrap();
        let lock = startup_lock(&paths).await.unwrap();
        let inherited = lock.try_clone().unwrap();
        drop(lock);
        let contender = private_file(&paths.lock, false).unwrap();
        assert!(try_lock(&contender).is_err());
        validate_startup_lock(&inherited, &paths).unwrap();
        let flags = FdFlag::from_bits_retain(fcntl(&inherited, FcntlArg::F_GETFD).unwrap());
        assert!(flags.contains(FdFlag::FD_CLOEXEC));
        let unrelated = private_file(&paths.log, false).unwrap();
        assert!(validate_startup_lock(&unrelated, &paths).is_err());
        drop(inherited);
        try_lock(&contender).unwrap();
    }

    #[tokio::test]
    async fn startup_lock_serializes_callers() {
        let root = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::at(root.path().join("runtime")).unwrap();
        let guard = startup_lock(&paths).await.unwrap();
        let waiter = tokio::spawn(async move { startup_lock(&paths).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        drop(guard);
        drop(waiter.await.unwrap());
    }
}

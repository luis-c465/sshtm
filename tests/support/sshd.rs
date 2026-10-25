//! Opt-in, unprivileged OpenSSH fixture. All configuration lives in one tempdir.
//! `PermitUserRC no` disables ~/.ssh/rc. This is not a sandbox: sshd still uses
//! the account's real login shell (including its noninteractive startup hooks),
//! and OpenSSH may run system hooks such as /etc/ssh/sshrc. Use a test account
//! whose shell and system hooks are trusted not to mutate external state.
#![allow(dead_code)] // Shared with later forwarding integration tests.

use anyhow::{Context, Result, bail, ensure};
use nix::sys::signal::{Signal, kill, killpg};
use nix::unistd::{Pid, Uid, User};
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tempfile::TempDir;

pub fn enabled() -> bool {
    std::env::var_os("SSHTM_E2E_SSHD").is_some_and(|value| value == "1")
}

pub struct Sshd {
    // The process guard must be dropped before the directory holding its files.
    server: Option<Server>,
    echo: Option<Echo>,
    dir: TempDir,
    path: OsString,
}

impl Sshd {
    pub fn start() -> Result<Self> {
        ensure!(enabled(), "set SSHTM_E2E_SSHD=1 to enable real SSH tests");
        ensure!(
            !Uid::effective().is_root(),
            "the SSH fixture must run without root"
        );
        let dir = tempfile::Builder::new().prefix("sshtm-e2e-").tempdir()?;
        let mut fixture = Self {
            server: None,
            echo: None,
            dir,
            path: OsString::new(),
        };
        let root = fixture.root();
        for name in ["host", "client"] {
            let output = Command::new("/usr/bin/ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(root.join(name))
                .output()?;
            ensure!(
                output.status.success(),
                "ssh-keygen: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        fs::copy(root.join("client.pub"), root.join("authorized_keys"))?;
        let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        let user = User::from_uid(Uid::current())?
            .context("current user has no passwd entry")?
            .name;
        let host_key = fs::read_to_string(root.join("host.pub"))?;
        fs::write(
            root.join("known_hosts"),
            format!("[127.0.0.1]:{port} {host_key}"),
        )?;
        fs::write(
            root.join("sshd_config"),
            format!(
                "ListenAddress 127.0.0.1\nPort {port}\nHostKey {}\nPidFile {}\nAuthorizedKeysFile {}\nStrictModes no\nUsePAM no\nPermitUserRC no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nPubkeyAuthentication yes\nAllowUsers {user}\nAllowTcpForwarding yes\nLogLevel ERROR\n",
                config_path(&root.join("host")),
                config_path(&root.join("sshd.pid")),
                config_path(&root.join("authorized_keys"))
            ),
        )?;
        fs::write(
            root.join("ssh_config"),
            format!(
                "Host *\n HostName 127.0.0.1\n Port {port}\n User {user}\n IdentityFile {}\n IdentitiesOnly yes\n IdentityAgent none\n UserKnownHostsFile {}\n GlobalKnownHostsFile /dev/null\n StrictHostKeyChecking yes\n BatchMode yes\n ConnectTimeout 2\n ControlMaster no\n ControlPath none\n ForkAfterAuthentication no\n LogLevel ERROR\n",
                config_path(&root.join("client")),
                config_path(&root.join("known_hosts"))
            ),
        )?;
        let bin = root.join("bin");
        fs::create_dir(&bin)?;
        fs::write(
            bin.join("ssh"),
            format!(
                "#!/bin/sh\nexec /usr/bin/ssh -F {} \"$@\"\n",
                shell_quote(&root.join("ssh_config"))
            ),
        )?;
        fs::set_permissions(bin.join("ssh"), fs::Permissions::from_mode(0o700))?;
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        fixture.path = std::env::join_paths(paths)?;
        let log = fs::File::create(root.join("sshd.log"))?;
        // A separate subreaper avoids changing the test runner's process-wide
        // child adoption policy, and can reap killed shells' orphaned children.
        let child = Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "support::sshd::e2e_sshd_reaper",
                "--ignored",
                "--nocapture",
            ])
            .env("SSHTM_E2E_REAPER_CONFIG", root.join("sshd_config"))
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()?;
        fixture.server = Some(Server(child));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            if fixture.server.as_mut().unwrap().0.try_wait()?.is_some()
                || Instant::now() >= deadline
            {
                bail!(
                    "sshd did not start: {}",
                    fs::read_to_string(root.join("sshd.log"))?
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
        fixture.echo = Some(Echo::start()?);
        Ok(fixture)
    }

    pub fn root(&self) -> PathBuf {
        self.dir.path().to_owned()
    }
    pub fn process_group(&self) -> i32 {
        self.server.as_ref().unwrap().0.id() as i32
    }
    pub fn echo_addr(&self) -> SocketAddr {
        self.echo.as_ref().unwrap().addr
    }
    pub fn configure(&self, command: &mut Command) {
        command.env("PATH", &self.path);
    }
    pub fn ssh(&self) -> Command {
        // Command resolves the executable using its command-local PATH.
        let mut command = Command::new("ssh");
        self.configure(&mut command);
        command
    }
}

fn config_path(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
}
fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

// sshd session children can call setsid(), escaping the listener's process group.
// Inventory descendants as well; the reaper below freezes the tree before killing.
pub fn descendants(parent: i32) -> Vec<i32> {
    let processes: Vec<(i32, i32)> = fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse().ok()?;
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            let fields: Vec<_> = stat.rsplit_once(')')?.1.split_whitespace().collect();
            Some((pid, fields.get(1)?.parse().ok()?))
        })
        .collect();
    let mut found = vec![parent];
    let mut index = 0;
    while index < found.len() {
        let pid = found[index];
        for &(child, ppid) in &processes {
            if ppid == pid && !found.contains(&child) {
                found.push(child);
            }
        }
        index += 1;
    }
    found.remove(0);
    found
}

pub fn process_exists(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}
pub fn group_exists(group: i32) -> bool {
    killpg(Pid::from_raw(group), None).is_ok()
}

struct Server(Child);
impl Drop for Server {
    fn drop(&mut self) {
        // Resume only the reaper if a test stopped the server group. Session
        // shells must stay stopped until the reaper kills them.
        let _ = kill(Pid::from_raw(self.0.id() as i32), Signal::SIGCONT);
        // EOF asks the dedicated reaper to freeze, kill and reap its entire tree.
        drop(self.0.stdin.take());
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "internal subprocess entry point for the SSH fixture"]
fn e2e_sshd_reaper() {
    let Some(config) = std::env::var_os("SSHTM_E2E_REAPER_CONFIG") else {
        return;
    };
    nix::sys::prctl::set_child_subreaper(true).expect("enable isolated child reaper");
    let mut sshd = Command::new("/usr/bin/sshd")
        .args(["-D", "-e", "-f"])
        .arg(config)
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn sshd");
    let mut input = Vec::new();
    let _ = std::io::stdin().read_to_end(&mut input);
    let parent = std::process::id() as i32;
    // Stop ancestors before descendants, then rescan: an in-flight fork can
    // appear after a /proc snapshot. Only a stable, fully stopped tree is safe.
    let mut previous = Vec::new();
    loop {
        let mut tree = descendants(parent);
        for &pid in &tree {
            let _ = kill(Pid::from_raw(pid), Signal::SIGSTOP);
        }
        let stopped = tree.iter().all(|pid| {
            fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|stat| {
                    stat.rsplit_once(')')
                        .map(|(_, rest)| rest.trim_start().starts_with(['T', 't', 'Z']))
                })
                .unwrap_or(true)
        });
        tree.sort_unstable();
        if stopped && tree == previous {
            break;
        }
        previous = tree;
        thread::sleep(Duration::from_millis(10));
    }
    // Never resume shells: even terminating their current command can cause a
    // shell to execute its next command or a trap and fork new background work.
    for pid in previous.into_iter().rev() {
        let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
    }
    let _ = sshd.wait();
    // Subreaper adoption keeps orphaned grandchildren here, not at host PID 1.
    loop {
        match nix::sys::wait::waitpid(Pid::from_raw(-1), None) {
            Ok(_) | Err(nix::errno::Errno::EINTR) => continue,
            Err(nix::errno::Errno::ECHILD) => break,
            Err(error) => panic!("reap SSH fixture descendants: {error}"),
        }
    }
}

struct Echo {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Echo {
    fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = thread::Builder::new()
            .name("e2e-echo".into())
            .spawn(move || {
                while !stopping.load(Ordering::Relaxed) {
                    if let Ok((mut stream, _)) = listener.accept() {
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
                        let _ = stream.set_write_timeout(Some(Duration::from_millis(50)));
                        let mut bytes = [0; 8192];
                        while !stopping.load(Ordering::Relaxed) {
                            match stream.read(&mut bytes) {
                                Ok(0) => break,
                                Ok(n) => {
                                    if stream.write_all(&bytes[..n]).is_err() {
                                        break;
                                    }
                                }
                                Err(e)
                                    if matches!(
                                        e.kind(),
                                        std::io::ErrorKind::WouldBlock
                                            | std::io::ErrorKind::TimedOut
                                    ) => {}
                                Err(_) => break,
                            }
                        }
                    } else {
                        thread::sleep(Duration::from_millis(10));
                    }
                }
            })?;
        Ok(Self {
            addr,
            stop,
            worker: Some(worker),
        })
    }
}
impl Drop for Echo {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

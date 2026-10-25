//! OpenSSH host validation, config probing, and arguments.
use std::{ffi::OsStr, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail, ensure};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::{Pid, getppid},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
};

const PROBE_STDOUT_BYTES: usize = 1024 * 1024;
const PROBE_STDERR_BYTES: usize = 16 * 1024;

// Keep the group alive until its pipes have drained, even if the leader exits first.
struct ProbeChild(Child, Pid);

impl Drop for ProbeChild {
    fn drop(&mut self) {
        let _ = killpg(self.1, Signal::SIGKILL);
        // Tokio's kill_on_drop also arranges for the leader to be reaped.
    }
}

async fn read_stdout(mut stream: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        let count = stream
            .read(&mut chunk)
            .await
            .context("read ssh -G stdout")?;
        if count == 0 {
            return Ok(output);
        }
        ensure!(
            output.len() + count <= PROBE_STDOUT_BYTES,
            "ssh -G stdout exceeds {PROBE_STDOUT_BYTES} bytes"
        );
        output.extend_from_slice(&chunk[..count]);
    }
}

async fn read_stderr(mut stream: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut tail = Vec::with_capacity(PROBE_STDERR_BYTES);
    let mut chunk = [0; 8192];
    loop {
        let count = stream
            .read(&mut chunk)
            .await
            .context("read ssh -G stderr")?;
        if count == 0 {
            return Ok(tail);
        }
        let excess = (tail.len() + count).saturating_sub(PROBE_STDERR_BYTES);
        tail.drain(..excess);
        tail.extend_from_slice(&chunk[..count]);
    }
}

async fn run_probe(mut command: Command, timeout: Duration) -> Result<bool> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let parent = nix::unistd::getpid();
    // SAFETY: only async-signal-safe syscalls run between fork and exec.
    unsafe {
        command.pre_exec(move || {
            nix::sys::prctl::set_pdeathsig(Signal::SIGTERM).map_err(std::io::Error::from)?;
            if getppid() != parent {
                return Err(std::io::Error::other("probe parent exited"));
            }
            Ok(())
        });
    }
    let child = command.spawn().context("run ssh -G")?;
    let pid = Pid::from_raw(child.id().context("ssh has no pid")? as i32);
    let mut child = ProbeChild(child, pid);
    let stdout = child.0.stdout.take().unwrap();
    let stderr = child.0.stderr.take().unwrap();
    let (status, stdout, stderr) = tokio::time::timeout(timeout, async {
        tokio::try_join!(
            async { child.0.wait().await.context("wait for ssh -G") },
            read_stdout(stdout),
            read_stderr(stderr),
        )
    })
    .await
    .with_context(|| format!("ssh -G timed out after {} seconds", timeout.as_secs_f64()))??;
    ensure!(
        status.success(),
        "ssh -G failed: {}",
        String::from_utf8_lossy(&stderr).trim()
    );
    needs_keepalive(std::str::from_utf8(&stdout).context("ssh -G output is not UTF-8")?)
}

use crate::spec::Forward;

pub fn validate_host(host: &str) -> Result<()> {
    ensure!(
        !host.is_empty()
            && !host.starts_with('-')
            && !host.chars().any(|c| c.is_whitespace() || c.is_control()),
        "invalid SSH host: {host:?}"
    );
    Ok(())
}

pub fn needs_keepalive(config: &str) -> Result<bool> {
    for line in config.lines() {
        let mut fields = line.split_whitespace();
        if fields
            .next()
            .is_some_and(|key| key.eq_ignore_ascii_case("serveraliveinterval"))
        {
            let value = fields.next().context("missing serveraliveinterval value")?;
            ensure!(
                fields.next().is_none() && value.bytes().all(|b| b.is_ascii_digit()),
                "invalid serveraliveinterval"
            );
            return Ok(value
                .parse::<u64>()
                .context("invalid serveraliveinterval")?
                == 0);
        }
    }
    bail!("ssh -G output has no serveraliveinterval")
}

pub fn forward_args(
    host: &str,
    forward: &Forward,
    local: u16,
    default_keepalive: bool,
) -> Result<Vec<String>> {
    validate_host(host)?;
    ensure!(
        local != 0,
        "local port must be allocated before spawning ssh"
    );
    // Revalidate public fields before embedding them in OpenSSH's -L grammar.
    let remote = if forward.rhost.contains(':') {
        format!("[{}]", forward.rhost)
    } else {
        forward.rhost.clone()
    };
    let spec = format!("{local}:{remote}:{}", forward.rport);
    let _: Forward = spec.parse()?;
    let mut args = vec!["-N".into(), "-T".into()];
    for option in [
        "BatchMode=yes",
        "ExitOnForwardFailure=yes",
        "ControlMaster=no",
        "ControlPath=none",
        "ForkAfterAuthentication=no",
    ] {
        args.extend(["-o".into(), option.into()]);
    }
    if default_keepalive {
        for option in ["ServerAliveInterval=15", "ServerAliveCountMax=3"] {
            args.extend(["-o".into(), option.into()]);
        }
    }
    args.extend([
        "-L".into(),
        format!("127.0.0.1:{spec}"),
        "--".into(),
        host.into(),
    ]);
    Ok(args)
}

pub async fn probe(host: &str, auth_sock: Option<&OsStr>, path: Option<&OsStr>) -> Result<bool> {
    validate_host(host)?;
    let mut command = Command::new("ssh");
    command
        .args(["-G", "--", host])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    if let Some(sock) = auth_sock {
        command.env("SSH_AUTH_SOCK", sock);
    } else {
        command.env_remove("SSH_AUTH_SOCK");
    }
    if let Some(path) = path {
        command.env("PATH", path);
    }
    run_probe(command, Duration::from_secs(20)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[tokio::test]
    async fn probe_output_bounds() {
        let error = run_probe(
            shell(
                "(while :; do printf '%8192s' x >&2; done) &\nwhile :; do printf '%8192s' x; done",
            ),
            Duration::from_secs(5),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("stdout exceeds"), "{error:#}");

        let error = run_probe(shell(
            "n=0; while [ \"$n\" -lt 128 ]; do printf '%8192s' x >&2; n=$((n+1)); done; echo end-marker >&2; exit 1"
        ), Duration::from_secs(5)).await.unwrap_err();
        let message = error.to_string();
        assert!(message.ends_with("end-marker"));
        // Lossy UTF-8 diagnostics can use at most three bytes per retained byte.
        assert!(message.len() <= 3 * PROBE_STDERR_BYTES + 32);

        assert!(
            run_probe(
                shell("echo serveraliveinterval 0; while :; do printf '%8192s' x >&2; done"),
                Duration::from_millis(100)
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("timed out")
        );
    }

    struct CleanupGroup(Pid);
    impl Drop for CleanupGroup {
        fn drop(&mut self) {
            let _ = killpg(self.0, Signal::SIGKILL);
        }
    }

    async fn descendants_stopped(cancel: bool) {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pids");
        let mut command = shell("sleep 60 & echo $$ $! > \"$PIDFILE\"; wait");
        command.env("PIDFILE", &pidfile);
        let task = tokio::spawn(run_probe(command, Duration::from_millis(500)));
        let pids = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&pidfile) {
                    let pids: Vec<i32> = text
                        .split_whitespace()
                        .filter_map(|p| p.parse().ok())
                        .collect();
                    if pids.len() == 2 {
                        break pids;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("probe did not start");
        let _cleanup = CleanupGroup(Pid::from_raw(pids[0]));
        if cancel {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            assert!(
                tokio::time::timeout(Duration::from_secs(3), task)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap_err()
                    .to_string()
                    .contains("timed out")
            );
        }
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                // An orphan may remain a zombie until the host init reaps it.
                let running = pids.iter().any(|pid| {
                    std::fs::read_to_string(format!("/proc/{pid}/stat"))
                        .is_ok_and(|stat| !stat.rsplit_once(") ").unwrap().1.starts_with('Z'))
                });
                if !running {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("probe descendants survived cleanup");
    }

    #[tokio::test]
    async fn probe_cancellation_kills_descendants() {
        descendants_stopped(true).await;
    }

    #[tokio::test]
    async fn probe_timeout_kills_descendants() {
        descendants_stopped(false).await;
    }

    #[test]
    fn hosts() {
        for host in ["", "-oProxyCommand=x", "a b", "a\n", "a\0", "a\u{a0}b"] {
            assert!(validate_host(host).is_err());
        }
        for host in ["db", "user@host", "::1", "user@[::1]"] {
            validate_host(host).unwrap();
        }
    }
    #[test]
    fn config() {
        assert!(
            needs_keepalive("host db\nserveraliveinterval 0\nserveralivecountmax 3\n").unwrap()
        );
        assert!(!needs_keepalive("ServerAliveInterval\t30\n").unwrap());
        for value in [
            "",
            "serveraliveinterval",
            "serveraliveinterval -1",
            "serveraliveinterval nope",
            "serveraliveinterval 0 extra",
        ] {
            assert!(needs_keepalive(value).is_err());
        }
    }
    #[test]
    fn arguments() {
        let forward: Forward = "[::1]:5432".parse().unwrap();
        let args = forward_args("user@db", &forward, 1234, true).unwrap();
        assert_eq!(
            args,
            [
                "-N",
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ControlMaster=no",
                "-o",
                "ControlPath=none",
                "-o",
                "ForkAfterAuthentication=no",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-L",
                "127.0.0.1:1234:[::1]:5432",
                "--",
                "user@db"
            ]
        );
        assert!(
            !forward_args("db", &forward, 1234, false)
                .unwrap()
                .iter()
                .any(|s| s.starts_with("ServerAlive"))
        );
        assert!(forward_args("db", &forward, 0, false).is_err());
        assert!(forward_args("-bad", &forward, 1, false).is_err());
    }
    #[tokio::test]
    async fn invalid_host_before_spawn() {
        assert!(
            probe("-bad", None, Some(OsStr::new("/nonexistent")))
                .await
                .unwrap_err()
                .to_string()
                .contains("invalid SSH host")
        );
    }
    #[tokio::test]
    async fn probe_environment_and_errors() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("ssh");
        std::fs::write(&script, "#!/bin/sh\n[ \"$1 $2\" = '-G --' ] || exit 2\ncase $3 in\n agent) [ \"$SSH_AUTH_SOCK\" = /test/agent ] || exit 3;;\n absent) [ \"${SSH_AUTH_SOCK+x}\" != x ] || exit 4;;\n fail) echo deliberate >&2; exit 255;;\n malformed) echo rubbish; exit 0;;\nesac\necho serveraliveinterval 0\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = Some(dir.path().as_os_str());
        assert!(
            probe("agent", Some(OsStr::new("/test/agent")), path)
                .await
                .unwrap()
        );
        assert!(probe("absent", None, path).await.unwrap());
        assert!(
            probe("fail", None, path)
                .await
                .unwrap_err()
                .to_string()
                .contains("deliberate")
        );
        assert!(probe("malformed", None, path).await.is_err());
    }
}

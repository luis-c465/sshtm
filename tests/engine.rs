mod support;

use sshtm::{
    daemon::supervisor::{Settings, State, Tunnel},
    spec::Forward,
};
use std::{ffi::OsString, net::TcpListener, time::Duration};
use tokio::time::{Instant, timeout};

fn settings() -> Settings {
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("ssh");
    assert!(
        binary.exists(),
        "workspace fake-ssh binary missing: {}",
        binary.display()
    );
    Settings {
        path: Some(binary.parent().unwrap().as_os_str().to_owned()),
        ..Settings::default()
    }
}
fn auto() -> Forward {
    "0:localhost:1".parse().unwrap()
}
fn free(port: u16) {
    assert!(
        TcpListener::bind(("127.0.0.1", port)).is_ok(),
        "port {port} still bound"
    );
}
fn dead(pid: u32) {
    assert!(
        !std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "child {pid} survived"
    );
}
async fn state(tunnel: &Tunnel, wanted: State, within: Duration) {
    let mut receiver = tunnel.subscribe();
    timeout(within, async {
        loop {
            if receiver.borrow().state == wanted {
                break;
            }
            receiver.changed().await.unwrap();
        }
    })
    .await
    .unwrap_or_else(|_| panic!("expected {wanted:?}, got {:?}", tunnel.snapshot()));
}

#[tokio::test]
async fn opens_logs_and_kills() {
    for host in ["ok", "keepalive"] {
        let started = Instant::now();
        let mut tunnel = Tunnel::open(host.into(), auto(), None, settings())
            .await
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        let snapshot = tunnel.snapshot();
        assert_eq!(snapshot.state, State::Up);
        assert!(snapshot.local > 0);
        assert!(
            tunnel
                .logs()
                .iter()
                .any(|line| line == &format!("fake-ssh: {host}"))
        );
        tunnel.kill().await.unwrap();
        tunnel.kill().await.unwrap();
        free(snapshot.local);
        dead(snapshot.pid.unwrap());
    }
}

#[tokio::test]
async fn initial_failure_and_timeout_cleanup() {
    use std::os::unix::fs::PermissionsExt;
    for host in ["fail", "hang"] {
        let fixture = tempfile::tempdir().unwrap();
        let pid_file = fixture.path().join("child-pid");
        let script = fixture.path().join("ssh");
        let fake = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("ssh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nif [ \"$1\" != -G ]; then echo $$ > '{}'; fi\nexec '{}' \"$@\"\n",
                pid_file.display(),
                fake.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let port = TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let forward = format!("{port}:localhost:1").parse().unwrap();
        let mut config = settings();
        config.path = Some(fixture.path().as_os_str().to_owned());
        config.ready_timeout = Duration::from_millis(150);
        let started = Instant::now();
        let error = Tunnel::open(host.into(), forward, None, config)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.contains(if host == "fail" {
                "Permission denied"
            } else {
                "timed out"
            }),
            "{error}"
        );
        assert!(error.contains("once interactively"));
        if host == "hang" {
            assert!(started.elapsed() >= Duration::from_millis(150));
        }
        let pid = std::fs::read_to_string(pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        dead(pid);
        free(port);
    }
}

#[tokio::test]
async fn invalid_host_never_executes_ssh() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("executed");
    let script = dir.path().join("ssh");
    std::fs::write(
        &script,
        format!("#!/bin/sh\n: > '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    for host in ["-oProxyCommand=x", "a b", "", "a\nb"] {
        let config = Settings {
            path: Some(dir.path().as_os_str().to_owned()),
            ..Settings::default()
        };
        assert!(
            Tunnel::open(host.into(), auto(), None, config)
                .await
                .is_err()
        );
    }
    assert!(!marker.exists());
}

#[tokio::test]
async fn stubborn_escalates_and_drop_cancels() {
    let config = settings();
    let grace = config.kill_grace;
    let mut tunnel = Tunnel::open("stubborn".into(), auto(), None, config)
        .await
        .unwrap();
    let snapshot = tunnel.snapshot();
    let start = Instant::now();
    tunnel.kill().await.unwrap();
    assert!(start.elapsed() >= grace);
    assert!(start.elapsed() < grace + Duration::from_secs(1));
    free(snapshot.local);
    dead(snapshot.pid.unwrap());

    let tunnel = Tunnel::open("ok".into(), auto(), None, settings())
        .await
        .unwrap();
    let snapshot = tunnel.snapshot();
    let mut receiver = tunnel.subscribe();
    drop(tunnel);
    timeout(Duration::from_secs(1), async {
        loop {
            if receiver.borrow().state == State::Stopped {
                break;
            }
            receiver.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    free(snapshot.local);
    dead(snapshot.pid.unwrap());
}

#[tokio::test]
async fn flap_reconnects_with_exponential_delays_and_can_cancel_backoff() {
    let mut tunnel = Tunnel::open("flap".into(), auto(), None, settings())
        .await
        .unwrap();
    let port = tunnel.snapshot().local;
    for (attempt, delay) in [(1, 1), (2, 2), (3, 4)] {
        state(&tunnel, State::Reconnecting, Duration::from_secs(3)).await;
        let snapshot = tunnel.snapshot();
        assert_eq!(snapshot.attempt, attempt);
        assert_eq!(snapshot.local, port);
        let remaining = snapshot
            .next_retry
            .unwrap()
            .saturating_duration_since(Instant::now());
        assert!(remaining <= Duration::from_secs(delay));
        assert!(remaining > Duration::from_secs(delay) - Duration::from_millis(200));
        if attempt < 3 {
            state(&tunnel, State::Up, Duration::from_secs(delay + 2)).await;
        }
    }
    let started = Instant::now();
    tunnel.kill().await.unwrap();
    assert!(started.elapsed() < Duration::from_millis(300));
    free(port);
}

#[tokio::test]
async fn agent_environment_is_per_tunnel_and_absence_is_removed() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let binary = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("ssh");
    let script = dir.path().join("ssh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\necho \"agent=${{SSH_AUTH_SOCK-unset}}\" >&2\nexec '{}' \"$@\"\n",
            binary.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    for sock in [Some(OsString::from("/test/agent")), None] {
        let config = Settings {
            path: Some(dir.path().as_os_str().to_owned()),
            ..Settings::default()
        };
        let expected = if sock.is_some() {
            "agent=/test/agent"
        } else {
            "agent=unset"
        };
        let mut tunnel = Tunnel::open("ok".into(), auto(), sock, config)
            .await
            .unwrap();
        assert!(tunnel.logs().iter().any(|s| s == expected));
        tunnel.kill().await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires SSHTM_E2E_SSHD=1 and unprivileged OpenSSH"]
async fn e2e_engine_forward_and_reconnect() {
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };
    use std::io::{Read, Write};
    use support::sshd::{Sshd, descendants, enabled};
    if !enabled() {
        return;
    }
    let fixture = Sshd::start().unwrap();
    // Override keepalives in the fixture's own config, not the product defaults.
    let config_path = fixture.root().join("ssh_config");
    let config = std::fs::read_to_string(&config_path).unwrap();
    std::fs::write(
        config_path,
        format!("Host *\n ServerAliveInterval 2\n ServerAliveCountMax 2\n{config}"),
    )
    .unwrap();
    let command = fixture.ssh();
    let path = command
        .get_envs()
        .find(|(key, _)| *key == "PATH")
        .unwrap()
        .1
        .unwrap()
        .to_owned();
    let settings = Settings {
        path: Some(path),
        ..Settings::default()
    };
    let forward = format!("0:localhost:{}", fixture.echo_addr().port())
        .parse()
        .unwrap();
    let mut tunnel = Tunnel::open("fixture".into(), forward, None, settings)
        .await
        .unwrap();
    let snapshot = tunnel.snapshot();
    let round_trip = |port| {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream.write_all(b"ping").unwrap();
        let mut reply = [0; 4];
        stream.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"ping");
    };
    round_trip(snapshot.local);
    // Session processes can leave the listener's process group; stop the whole tree.
    let stopped = descendants(fixture.process_group());
    for &pid in stopped.iter().rev() {
        let _ = kill(Pid::from_raw(pid), Signal::SIGSTOP);
    }
    state(&tunnel, State::Reconnecting, Duration::from_secs(15)).await;
    for pid in stopped {
        let _ = kill(Pid::from_raw(pid), Signal::SIGCONT);
    }
    state(&tunnel, State::Up, Duration::from_secs(25)).await;
    round_trip(snapshot.local);
    let pid = tunnel.snapshot().pid.unwrap();
    tunnel.kill().await.unwrap();
    free(snapshot.local);
    dead(pid);
}

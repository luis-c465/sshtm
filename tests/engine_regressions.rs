use sshtm::{
    daemon::supervisor::{Settings, Tunnel},
    spec::Forward,
};
use std::{net::TcpListener, os::unix::fs::PermissionsExt, time::Duration};

fn fake() -> std::path::PathBuf {
    std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("ssh")
}
fn auto() -> Forward {
    "0:localhost:1".parse().unwrap()
}

#[tokio::test]
async fn cancelled_kill_remains_joinable() {
    let config = Settings {
        path: Some(fake().parent().unwrap().as_os_str().to_owned()),
        kill_grace: Duration::from_millis(200),
        ..Settings::default()
    };
    let mut tunnel = Tunnel::open("stubborn".into(), auto(), None, config)
        .await
        .unwrap();
    let snapshot = tunnel.snapshot();
    assert!(
        tokio::time::timeout(Duration::from_millis(10), tunnel.kill())
            .await
            .is_err()
    );
    tunnel.kill().await.unwrap();
    assert!(!std::path::Path::new(&format!("/proc/{}", snapshot.pid.unwrap())).exists());
    assert!(TcpListener::bind(("127.0.0.1", snapshot.local)).is_ok());
}

#[tokio::test]
async fn auto_port_retries_only_current_bind_failures_and_caps_attempts() {
    for (failures, auth_failure, expected) in [(1, true, 2), (3, false, 3), (2, false, 3)] {
        let dir = tempfile::tempdir().unwrap();
        let counter = dir.path().join("attempts");
        let script = dir.path().join("ssh");
        std::fs::write(&script, format!(
            "#!/bin/sh\nif [ \"$1\" = -G ]; then echo 'serveraliveinterval 0'; exit 0; fi\nn=0\nif [ -f '{counter}' ]; then read -r n < '{counter}'; fi\nn=$((n+1))\necho \"$n\" > '{counter}'\nif [ \"$n\" -le {failures} ]; then echo 'Address already in use' >&2; exit 255; fi\n{last}\n",
            counter = counter.display(),
            last = if auth_failure { "echo 'Permission denied' >&2; exit 255".into() } else { format!("exec '{}' \"$@\"", fake().display()) }
        )).unwrap();
        std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = Settings {
            path: Some(dir.path().as_os_str().to_owned()),
            ..Settings::default()
        };
        let result = Tunnel::open("ok".into(), auto(), None, config).await;
        if failures == 2 && !auth_failure {
            let mut tunnel = result.unwrap();
            tunnel.kill().await.unwrap();
        } else {
            let error = result.err().unwrap().to_string();
            assert!(
                error.contains(if auth_failure {
                    "Permission denied"
                } else {
                    "Address already in use"
                }),
                "{error}"
            );
        }
        assert_eq!(
            std::fs::read_to_string(counter)
                .unwrap()
                .trim()
                .parse::<u32>()
                .unwrap(),
            expected
        );
    }
}

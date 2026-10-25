//! Each helper is a separate client process; the product CLI remains R3 work.
use sshtm::{
    client::Client,
    ipc::{Command as RequestCommand, ResponseResult},
    paths::RuntimePaths,
};
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[test]
fn autostart_process_helper() {
    let Some(dir) = std::env::var_os("SSHTM_TEST_CLIENT_RUNTIME") else {
        return;
    };
    let output = PathBuf::from(std::env::var_os("SSHTM_TEST_CLIENT_OUTPUT").unwrap());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let client = Client::with_executable(
            RuntimePaths::at(PathBuf::from(dir)).unwrap(),
            PathBuf::from(env!("CARGO_BIN_EXE_sshtm")),
        );
        let response = client
            .request(RequestCommand::Open {
                host: "ok".into(),
                forward: "0:1".parse().unwrap(),
                auth_sock: None,
            })
            .await
            .unwrap()
            .unwrap();
        match response.result {
            ResponseResult::Ok { data } => {
                std::fs::write(output, serde_json::to_vec(&data).unwrap()).unwrap()
            }
            ResponseResult::Error { message } => panic!("{message}"),
        }
    });
}
struct Cleanup {
    dir: PathBuf,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            if let Ok(paths) = RuntimePaths::at(self.dir.clone()) {
                let client = Client::new(paths);
                let _ = tokio::time::timeout(
                    Duration::from_secs(4),
                    client.request(RequestCommand::Shutdown),
                )
                .await;
            }
        });
    }
}
#[test]
fn ten_processes_start_one_daemon() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("runtime");
    let _cleanup = Cleanup { dir: dir.clone() };
    let fake = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_owned();
    let children: Vec<_> = (0..10)
        .map(|i| {
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "autostart_process_helper", "--nocapture"])
                .env("SSHTM_TEST_CLIENT_RUNTIME", &dir)
                .env(
                    "SSHTM_TEST_CLIENT_OUTPUT",
                    root.path().join(format!("row-{i}")),
                )
                .env("PATH", &fake)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let mut indices = Vec::new();
    for i in 0..10 {
        let row: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.path().join(format!("row-{i}"))).unwrap())
                .unwrap();
        indices.push(row["idx"].as_u64().unwrap());
    }
    indices.sort();
    assert_eq!(indices, (1..=10).collect::<Vec<_>>());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let client = Client::new(RuntimePaths::at(dir.clone()).unwrap());
        let response = client.request(RequestCommand::Ping).await.unwrap().unwrap();
        let pid = match response.result {
            ResponseResult::Ok { data } => data["pid"].as_u64().unwrap(),
            _ => panic!("ping failed"),
        };
        let mut owners = Vec::new();
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
                continue;
            };
            let args: Vec<_> = cmdline.split(|b| *b == 0).collect();
            if args
                .iter()
                .any(|a| *a == dir.as_os_str().as_encoded_bytes())
                && args.iter().any(|a| *a == b"daemon")
            {
                owners.push(entry.file_name().to_string_lossy().parse::<u64>().unwrap());
            }
        }
        assert_eq!(owners, vec![pid]);
        let response = client
            .request(RequestCommand::Shutdown)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(response.result, ResponseResult::Ok { .. }));
        let deadline = Instant::now() + Duration::from_secs(3);
        while dir.join("sock").exists() {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
}
#[test]
fn insecure_runtime_is_refused_without_starting() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("runtime");
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_sshtm"))
        .args(["daemon", "--runtime-dir"])
        .arg(&dir)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains(dir.to_str().unwrap()));
    assert!(!dir.join("sock").exists());
}

/// Subprocess entry point: runtime shutdown must not wait for a flock waiter,
/// and runtime/process death must not unlock a stopped daemon starter.
#[test]
fn startup_lifetime_helper() {
    let Some(mode) = std::env::var_os("SSHTM_TEST_STARTUP_MODE") else {
        return;
    };
    let dir = PathBuf::from(std::env::var_os("SSHTM_TEST_STARTUP_DIR").unwrap());
    let executable = PathBuf::from(std::env::var_os("SSHTM_TEST_STARTUP_EXE").unwrap());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let client = Client::with_executable(RuntimePaths::at(dir.clone()).unwrap(), executable);
        let request = tokio::spawn(async move {
            client
                .request(RequestCommand::Open {
                    host: "ok".into(),
                    forward: "0:1".parse().unwrap(),
                    auth_sock: None,
                })
                .await
        });
        if mode == "lock_wait" {
            tokio::time::sleep(Duration::from_millis(50)).await;
        } else {
            tokio::time::timeout(Duration::from_secs(3), async {
                while !dir.join("starter-pid").exists() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
        }
        request.abort();
        let _ = request.await;
    });
    if mode == "process_death" {
        // Keep both runtime and startup transaction alive until SIGKILL.
        std::fs::write(dir.join("runtime-dropped"), "runtime still alive").unwrap();
        loop {
            std::thread::sleep(Duration::from_secs(30));
        }
    }
    drop(runtime);
    std::fs::write(dir.join("runtime-dropped"), "done").unwrap();
}

struct ChildGuard(std::process::Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn lifetime_helper(dir: &std::path::Path, executable: &std::path::Path, mode: &str) -> ChildGuard {
    ChildGuard(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "startup_lifetime_helper", "--nocapture"])
            .env("SSHTM_TEST_STARTUP_MODE", mode)
            .env("SSHTM_TEST_STARTUP_DIR", dir)
            .env("SSHTM_TEST_STARTUP_EXE", executable)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    )
}

fn wait_for_file(path: &std::path::Path) {
    let deadline = Instant::now() + Duration::from_secs(4);
    while !path.exists() {
        assert!(Instant::now() < deadline, "missing {}", path.display());
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn wait_for_exit(child: &mut std::process::Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(Instant::now() < deadline, "subprocess did not exit");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn runtime_shutdown_does_not_wait_for_external_startup_lock() {
    use nix::fcntl::{Flock, FlockArg};
    let root = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::at(root.path().join("runtime")).unwrap();
    let file = sshtm::paths::private_file(&paths.lock, false).unwrap();
    let _external_lock = Flock::lock(file, FlockArg::LockExclusive).unwrap();
    let mut helper = lifetime_helper(
        &paths.dir,
        std::path::Path::new(env!("CARGO_BIN_EXE_sshtm")),
        "lock_wait",
    );
    assert!(wait_for_exit(&mut helper.0).success());
    assert!(paths.dir.join("runtime-dropped").exists());
    // The external holder deliberately keeps its lock until after the helper exits.
}

struct StarterCleanup(nix::unistd::Pid);
impl Drop for StarterCleanup {
    fn drop(&mut self) {
        let _ = nix::sys::signal::kill(self.0, nix::sys::signal::Signal::SIGKILL);
    }
}

fn inherited_lock_survives_starter_owner_death(mode: &str) {
    use nix::{
        fcntl::{Flock, FlockArg},
        sys::signal::{Signal, kill},
        unistd::Pid,
    };
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::at(root.path().join("runtime")).unwrap();
    let _cleanup = Cleanup {
        dir: paths.dir.clone(),
    };
    let wrapper = root.path().join("starter");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf '%s' \"$$\" > \"$5/starter-pid\"\nkill -STOP $$\nexec '{}' \"$@\"\n",
            env!("CARGO_BIN_EXE_sshtm")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut helper = lifetime_helper(&paths.dir, &wrapper, mode);
    wait_for_file(&paths.dir.join("runtime-dropped"));
    let pid = Pid::from_raw(
        std::fs::read_to_string(paths.dir.join("starter-pid"))
            .unwrap()
            .parse()
            .unwrap(),
    );
    let _starter_cleanup = StarterCleanup(pid);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let status = std::fs::read_to_string(format!("/proc/{}/status", pid.as_raw())).unwrap();
        if status
            .lines()
            .any(|line| line.starts_with("State:") && line.contains('T'))
        {
            break;
        }
        assert!(Instant::now() < deadline, "starter did not stop");
        std::thread::sleep(Duration::from_millis(5));
    }
    if mode == "process_death" {
        helper.0.kill().unwrap();
        helper.0.wait().unwrap();
    } else {
        assert!(wait_for_exit(&mut helper.0).success());
    }
    let file = sshtm::paths::private_file(&paths.lock, false).unwrap();
    assert!(
        Flock::lock(file, FlockArg::LockExclusiveNonblock).is_err(),
        "starter lost inherited lock after {mode}"
    );
    let mut contender = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_sshtm"))
            .args(["daemon", "--runtime-dir"])
            .arg(&paths.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    std::thread::sleep(Duration::from_millis(50));
    assert!(contender.0.try_wait().unwrap().is_none());
    kill(pid, Signal::SIGCONT).unwrap();
    // The resumed inherited owner must publish first. The manual contender
    // obtains the lock afterwards and refuses to replace its live socket.
    assert!(!wait_for_exit(&mut contender.0).success());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let client = Client::new(paths.clone());
        let response = client.request(RequestCommand::Ping).await.unwrap().unwrap();
        assert!(matches!(response.result, ResponseResult::Ok { data } if data["pid"].as_u64() == Some(pid.as_raw() as u64)));
        client.request(RequestCommand::Shutdown).await.unwrap().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while paths.socket.exists() {
            assert!(Instant::now() < deadline);
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
}

#[test]
fn inherited_lock_survives_runtime_shutdown() {
    inherited_lock_survives_starter_owner_death("runtime_death");
}

#[test]
fn inherited_lock_survives_parent_process_death() {
    inherited_lock_survives_starter_owner_death("process_death");
}

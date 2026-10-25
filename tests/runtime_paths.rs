//! The real fallback is exercised only after mounting a private /tmp.
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    process::{Command, Output},
};

use sshtm::paths::RuntimePaths;

fn cli() -> Command {
    let executable = std::env::var_os("SSHTM_ISOLATED_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_sshtm").into());
    let mut command = Command::new(executable);
    command.env_remove("XDG_RUNTIME_DIR").env_remove("COMPLETE");
    command
}

fn refused(output: Output) {
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unsafe runtime directory"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn xdg_symlink_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    symlink(&target, root.path().join("sshtm")).unwrap();
    refused(
        cli()
            .env("XDG_RUNTIME_DIR", root.path())
            .args(["daemon", "--idle-exit", "1"])
            .output()
            .unwrap(),
    );
    assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
}

#[test]
fn foreign_private_directory_is_refused() {
    // procfs supplies a foreign-owned, private directory without chown or root.
    let path = std::path::Path::new("/proc/1/map_files");
    let Ok(metadata) = fs::symlink_metadata(path) else {
        eprintln!("SKIP foreign-owner fixture: /proc/1/map_files unavailable");
        return;
    };
    if metadata.uid() == nix::unistd::Uid::current().as_raw()
        || metadata.mode() & 0o077 != 0
        || !metadata.is_dir()
    {
        eprintln!("SKIP foreign-owner fixture: procfs directory is not private and foreign-owned");
        return;
    }
    assert!(RuntimePaths::at(path.to_path_buf()).is_err());
    refused(
        cli()
            .args(["daemon", "--runtime-dir"])
            .arg(path)
            .output()
            .unwrap(),
    );
}

#[test]
fn isolated_fallback_cli() {
    // Probe separately: a failed test after successful setup must never be skipped.
    let setup = "mount --make-rprivate / && mount -t tmpfs -o mode=1777 tmpfs /tmp";
    let probe = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "sh", "-c", setup])
        .output();
    match probe {
        Ok(output) if output.status.success() => (),
        result => {
            eprintln!(
                "SKIP isolated fallback CLI: user/mount namespaces unavailable: {result:?}; pure fallback resolution and tempfile validation remain covered by unit tests"
            );
            let root = tempfile::tempdir().unwrap();
            let paths = RuntimePaths::at(root.path().join("runtime")).unwrap();
            assert_eq!(fs::metadata(paths.dir).unwrap().mode() & 0o777, 0o700);
            return;
        }
    }
    // Open both executables before hiding /tmp. The deliberately inherited
    // descriptors keep builds under /tmp executable through procfs as well.
    let script = format!(
        "exec 8<\"$1\" 9<\"$2\" || exit 91; shift 2; {setup} || exit 90; exec /proc/self/fd/9 \"$@\""
    );
    let output = Command::new("unshare")
        .args([
            "--user",
            "--map-root-user",
            "--mount",
            "sh",
            "-c",
            &script,
            "sh",
        ])
        .arg(env!("CARGO_BIN_EXE_sshtm"))
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", "fallback_namespace_helper", "--nocapture"])
        .env("SSHTM_ISOLATED_TMP", "1")
        .env("SSHTM_ISOLATED_BINARY", "/proc/self/fd/8")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn fallback_namespace_helper() {
    if std::env::var_os("SSHTM_ISOLATED_TMP").is_none() {
        return;
    }
    // The parent enters a fresh user+mount namespace and overlays /tmp before
    // invoking this helper; no fallback path in the host mount is accessed.
    let fallback = std::path::PathBuf::from(format!("/tmp/sshtm-{}", nix::unistd::Uid::current()));
    assert!(!fallback.exists());
    let output = cli().args(["daemon", "--idle-exit", "1"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata = fs::symlink_metadata(&fallback).unwrap();
    assert!(metadata.is_dir());
    assert_eq!(metadata.uid(), nix::unistd::Uid::current().as_raw());
    assert_eq!(metadata.mode() & 0o777, 0o700);
    fs::remove_dir_all(&fallback).unwrap();

    let target = tempfile::tempdir().unwrap();
    symlink(target.path(), &fallback).unwrap();
    refused(cli().args(["daemon", "--idle-exit", "1"]).output().unwrap());
    assert_eq!(fs::read_dir(target.path()).unwrap().count(), 0);
    fs::remove_file(&fallback).unwrap();

    // uid 0 outside this user namespace is unmapped here; bind its private
    // procfs directory at the fallback to isolate ownership from permissions.
    let foreign = std::path::Path::new("/proc/1/map_files");
    let Ok(metadata) = fs::metadata(foreign) else {
        eprintln!("SKIP fallback foreign-owner fixture: procfs directory unavailable");
        return;
    };
    if metadata.uid() == nix::unistd::Uid::current().as_raw() || metadata.mode() & 0o077 != 0 {
        eprintln!(
            "SKIP fallback foreign-owner fixture: procfs directory is not private and foreign-owned"
        );
        return;
    }
    fs::create_dir(&fallback).unwrap();
    assert!(
        Command::new("mount")
            .arg("--bind")
            .arg(foreign)
            .arg(&fallback)
            .status()
            .unwrap()
            .success()
    );
    refused(cli().args(["daemon", "--idle-exit", "1"]).output().unwrap());
}

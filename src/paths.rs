//! Runtime directory resolution and permission checks.
use std::{
    fs::{self, DirBuilder, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use nix::unistd::Uid;

#[derive(Debug, Clone)]
pub struct RuntimePaths {
    pub dir: PathBuf,
    pub socket: PathBuf,
    pub lock: PathBuf,
    pub log: PathBuf,
}

impl RuntimePaths {
    pub fn resolve() -> Result<Self> {
        let dir = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(base) => PathBuf::from(base).join("sshtm"),
            None => fallback_dir(Uid::current().as_raw()),
        };
        Self::at(dir)
    }

    pub fn at(dir: PathBuf) -> Result<Self> {
        match DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error).with_context(|| format!("create {}", dir.display())),
        }
        let metadata =
            fs::symlink_metadata(&dir).with_context(|| format!("inspect {}", dir.display()))?;
        ensure!(
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && metadata.uid() == Uid::current().as_raw()
                && metadata.mode() & 0o077 == 0,
            "unsafe runtime directory {}: must be an owned private directory (0700)",
            dir.display()
        );
        Ok(Self {
            socket: dir.join("sock"),
            lock: dir.join("lock"),
            log: dir.join("daemon.log"),
            dir,
        })
    }
}

fn fallback_dir(uid: u32) -> PathBuf {
    PathBuf::from(format!("/tmp/sshtm-{uid}"))
}

/// Open without following symlinks or truncating an unvalidated file.
pub fn private_file(path: &Path, truncate: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == Uid::current().as_raw()
            && metadata.mode() & 0o077 == 0
            && metadata.nlink() == 1,
        "unsafe runtime file {}",
        path.display()
    );
    if truncate {
        file.set_len(0)?;
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt, symlink};

    #[test]
    fn private_directory_and_files() {
        let root = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::at(root.path().join("runtime")).unwrap();
        assert_eq!(fs::metadata(&paths.dir).unwrap().mode() & 0o777, 0o700);
        private_file(&paths.lock, false).unwrap();
        fs::set_permissions(&paths.dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(RuntimePaths::at(paths.dir).is_err());
    }

    #[test]
    fn fallback_resolution_is_pure() {
        assert_eq!(fallback_dir(12345), PathBuf::from("/tmp/sshtm-12345"));
        assert_eq!(fallback_dir(0), PathBuf::from("/tmp/sshtm-0"));
    }

    #[test]
    fn unsafe_files_are_not_truncated() {
        use nix::{sys::stat::Mode, unistd::mkfifo};

        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        let bytes = b"must survive rejected truncation";
        fs::write(&target, bytes).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let link = root.path().join("symlink");
        symlink(&target, &link).unwrap();
        assert!(private_file(&link, true).is_err());
        assert_eq!(fs::read(&target).unwrap(), bytes);

        let hardlink = root.path().join("hardlink");
        fs::hard_link(&target, &hardlink).unwrap();
        for path in [&target, &hardlink] {
            assert!(private_file(path, true).is_err());
            assert_eq!(fs::read(path).unwrap(), bytes);
        }
        fs::remove_file(hardlink).unwrap();
        for mode in [0o640, 0o602, 0o666] {
            fs::set_permissions(&target, fs::Permissions::from_mode(mode)).unwrap();
            assert!(private_file(&target, true).is_err());
            assert_eq!(fs::read(&target).unwrap(), bytes);
        }
        let fifo = root.path().join("fifo");
        mkfifo(&fifo, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        assert!(private_file(&fifo, true).is_err());
        assert!(fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo());
        assert!(private_file(root.path(), true).is_err());

        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        private_file(&target, false).unwrap();
        assert_eq!(fs::read(&target).unwrap(), bytes);
        private_file(&target, true).unwrap();
        assert!(fs::read(&target).unwrap().is_empty());
    }

    #[test]
    fn refuses_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let link = root.path().join("link");
        symlink(root.path(), &link).unwrap();
        assert!(RuntimePaths::at(link.clone()).is_err());
        assert!(private_file(&link, true).is_err());
    }
}

use std::{fs::File, os::fd::FromRawFd, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use clap::{Parser, Subcommand};
use sshtm::{daemon, paths::RuntimePaths};

#[derive(Parser)]
#[command(name = "sshtm", version)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    #[command(hide = true)]
    Daemon {
        #[arg(long)]
        idle_exit: Option<u64>,
        #[arg(long, hide = true)]
        startup_lock_fd: Option<i32>,
        #[arg(long, hide = true)]
        runtime_dir: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("sshtm: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    match Args::parse().command {
        Some(Command::Daemon {
            idle_exit,
            startup_lock_fd,
            runtime_dir,
        }) => {
            let paths = match runtime_dir {
                Some(dir) => RuntimePaths::at(dir)?,
                None => RuntimePaths::resolve()?,
            };
            let startup_lock = if let Some(fd) = startup_lock_fd {
                anyhow::ensure!(fd >= 3, "invalid startup lock descriptor");
                // Duplicate with CLOEXEC before taking ownership. A bad CLI fd
                // fails normally rather than constructing an invalid File.
                let duplicate = unsafe { nix::libc::fcntl(fd, nix::libc::F_DUPFD_CLOEXEC, 3) };
                if duplicate == -1 {
                    return Err(std::io::Error::last_os_error().into());
                }
                // SAFETY: fcntl returned a fresh descriptor owned by this process.
                let lock = unsafe { File::from_raw_fd(duplicate) };
                // This hidden argument transfers ownership of the original fd.
                unsafe {
                    nix::libc::close(fd);
                }
                sshtm::client::validate_startup_lock(&lock, &paths)?;
                Some(Arc::new(lock))
            } else {
                None
            };
            let mut settings = daemon::Settings {
                startup_lock,
                ..Default::default()
            };
            if let Some(seconds) = idle_exit {
                settings.idle_exit = Duration::from_secs(seconds);
            }
            daemon::run(paths, settings).await
        }
        None => anyhow::bail!("not implemented yet"),
    }
}

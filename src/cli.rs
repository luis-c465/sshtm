//! Command-line arguments, validation, and stable user-facing output.
use std::{collections::BTreeSet, ffi::OsStr, path::PathBuf, time::Duration};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use clap_complete::{ArgValueCompleter, CompletionCandidate, env::EnvCompleter};

use crate::{
    client::Client,
    daemon::{sshcmd::validate_host, table::Row},
    ipc::{Command as Request, Response, ResponseResult},
    paths::RuntimePaths,
    spec::Forward,
};

// Stay comfortably inside the 64 KiB IPC request cap even for ten-digit indices.
pub const MAX_KILL_INDICES: usize = 4096;

#[derive(Debug, Parser)]
#[command(name = "sshtm", version, about = "Supervise local SSH port forwards")]
pub struct Args {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Open one tunnel per spec, using the system SSH configuration
    Open {
        #[arg(value_parser = host_argument, add = ArgValueCompleter::new(complete_hosts))]
        host: String,
        /// PORT, LPORT:PORT, RHOST:PORT, or LPORT:RHOST:PORT (local 0 picks a port)
        #[arg(required = true, num_args = 1..)]
        specs: Vec<Forward>,
    },
    /// List tunnels (also the default command)
    Ls {
        /// Print a stable JSON array instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Kill tunnels by index or inclusive range
    Kill {
        #[arg(required_unless_present = "all", conflicts_with = "all", value_parser = index_range,
            add = ArgValueCompleter::new(complete_indices))]
        indices: Vec<IndexRange>,
        #[arg(long)]
        all: bool,
    },
    /// Show the tunnel's recent SSH stderr lines
    Logs {
        #[arg(value_parser = clap::value_parser!(u32).range(1..),
            add = ArgValueCompleter::new(complete_indices))]
        index: u32,
    },
    /// List concrete aliases from ~/.ssh/config, including Include files
    Hosts,
    /// Print dynamic shell registration (source it again after upgrades)
    Completions {
        #[arg(value_enum)]
        shell: CompletionShell,
    },
    /// Kill all tunnels and stop the daemon
    Shutdown,
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

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
}

impl CompletionShell {
    fn write(self) -> Result<()> {
        let shell: &dyn EnvCompleter = match self {
            Self::Bash => &clap_complete::env::Bash,
            Self::Zsh => &clap_complete::env::Zsh,
            Self::Fish => &clap_complete::env::Fish,
        };
        // Use the same registration backend as CompleteEnv without mutating the
        // process environment (the CLI runtime already has worker threads).
        shell.write_registration(
            "COMPLETE",
            "sshtm",
            "sshtm",
            "sshtm",
            &mut std::io::stdout(),
        )?;
        Ok(())
    }
}

fn host_candidates(current: &OsStr, aliases: Vec<String>) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    aliases
        .into_iter()
        .filter(|s| s.starts_with(prefix))
        .map(CompletionCandidate::new)
        .collect()
}

fn complete_hosts(current: &OsStr) -> Vec<CompletionCandidate> {
    host_candidates(current, crate::hosts::list().unwrap_or_default())
}

fn complete_indices(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    let result = (|| -> Result<Vec<Row>> {
        let paths = RuntimePaths::resolve()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        runtime.block_on(async {
            // Completion must not hang on an unresponsive daemon or start one.
            let response = tokio::time::timeout(
                Duration::from_millis(300),
                Client::new(paths).request(Request::List),
            )
            .await??;
            match response {
                Some(response) => Ok(serde_json::from_value(response_data(response)?)?),
                None => Ok(Vec::new()),
            }
        })
    })();
    result
        .unwrap_or_default()
        .into_iter()
        .filter(|row| row.idx.to_string().starts_with(prefix))
        .map(|row| CompletionCandidate::new(row.idx.to_string()).help(Some(row.host.into())))
        .collect()
}

#[derive(Debug, Clone)]
pub struct IndexRange {
    first: u32,
    last: u32,
}

fn host_argument(value: &str) -> std::result::Result<String, String> {
    validate_host(value).map_err(|error| error.to_string())?;
    Ok(value.to_owned())
}

fn index_range(value: &str) -> std::result::Result<IndexRange, String> {
    fn index(value: &str) -> std::result::Result<u32, String> {
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
            return Err("expected a positive index or inclusive range (for example 2-4)".into());
        }
        value
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| "index must be between 1 and 4294967295".into())
    }
    let (first, last) = match value.split_once('-') {
        Some((first, last)) => (index(first)?, index(last)?),
        None => {
            let n = index(value)?;
            (n, n)
        }
    };
    if last < first {
        return Err("range end must be at least its start".into());
    }
    if u64::from(last) - u64::from(first) + 1 > MAX_KILL_INDICES as u64 {
        return Err(format!(
            "at most {MAX_KILL_INDICES} indices per kill request"
        ));
    }
    Ok(IndexRange { first, last })
}

/// Validate aggregate range size before resolving runtime paths or contacting a daemon.
pub fn kill_indices(ranges: &[IndexRange]) -> std::result::Result<Vec<u32>, clap::Error> {
    let mut indices = BTreeSet::new();
    for range in ranges {
        for index in range.first..=range.last {
            indices.insert(index);
            if indices.len() > MAX_KILL_INDICES {
                return Err(clap::Error::raw(
                    clap::error::ErrorKind::ValueValidation,
                    format!("at most {MAX_KILL_INDICES} indices per kill request\n"),
                ));
            }
        }
    }
    Ok(indices.into_iter().collect())
}

fn response_data(response: Response) -> Result<serde_json::Value> {
    match response.result {
        ResponseResult::Ok { data } => Ok(data),
        ResponseResult::Error { message } => bail!("{message}"),
    }
}

pub fn table(rows: &[Row]) -> String {
    if rows.is_empty() {
        return "no tunnels\n".into();
    }
    let mut cells = vec![vec![
        "IDX".into(),
        "HOST".into(),
        "LOCAL".into(),
        "REMOTE".into(),
        "STATE".into(),
        "UPTIME".into(),
    ]];
    cells.extend(rows.iter().map(|row| {
        let state = if row.state == "reconnecting" {
            match row.next_retry_ms {
                Some(ms) => format!(
                    "reconnecting (attempt {}, in {:.1}s)",
                    row.attempt,
                    ms as f64 / 1000.0
                ),
                None => format!("reconnecting (attempt {})", row.attempt),
            }
        } else {
            row.state.clone()
        };
        vec![
            row.idx.to_string(),
            row.host.clone(),
            row.local.clone(),
            row.remote.clone(),
            state,
            format!("{}s", row.uptime),
        ]
    }));
    let widths: Vec<_> = (0..6)
        .map(|column| {
            cells
                .iter()
                .map(|row| row[column].chars().count())
                .max()
                .unwrap()
        })
        .collect();
    let mut output = String::new();
    for row in cells {
        for (column, value) in row.iter().enumerate() {
            output.push_str(value);
            if column < 5 {
                output.push_str(&" ".repeat(widths[column] - value.chars().count() + 2));
            }
        }
        output.push('\n');
    }
    output
}

/// Dispatch public commands. Daemon lifecycle remains in the binary entry point.
pub async fn run(command: Command) -> Result<()> {
    // Clap validates individual ranges; check their union before any side effects.
    let indices = match &command {
        Command::Kill { indices, .. } => kill_indices(indices).unwrap_or_else(|error| error.exit()),
        _ => Vec::new(),
    };
    // Config listing and registration are independent of daemon paths/permissions.
    match &command {
        Command::Hosts => {
            for host in crate::hosts::list()? {
                println!("{host}");
            }
            return Ok(());
        }
        Command::Completions { shell } => return shell.write(),
        _ => {}
    }
    let client = Client::new(RuntimePaths::resolve()?);
    match command {
        Command::Open { host, specs } => {
            let mut failed = false;
            for forward in specs {
                let result = async {
                    let response = client
                        .request(Request::Open {
                            host: host.clone(),
                            forward,
                            auth_sock: std::env::var_os("SSH_AUTH_SOCK"),
                        })
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("daemon unavailable"))?;
                    let row: Row = serde_json::from_value(response_data(response)?)?;
                    println!("{} {} -> {}", row.idx, row.local, row.remote);
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                if let Err(error) = result {
                    eprintln!("sshtm: {error:#}\nhint: run 'ssh {host}' once interactively");
                    failed = true;
                }
            }
            if failed {
                bail!("one or more tunnels failed to open");
            }
        }
        Command::Ls { json } => {
            let rows: Vec<Row> = match client.request(Request::List).await? {
                Some(response) => serde_json::from_value(response_data(response)?)?,
                None => Vec::new(),
            };
            if json {
                println!("{}", serde_json::to_string(&rows)?);
            } else {
                print!("{}", table(&rows));
            }
        }
        Command::Kill { all, .. } => {
            match client
                .request(Request::Kill {
                    indices: indices.clone(),
                    all,
                })
                .await?
            {
                Some(response) => {
                    response_data(response)?;
                }
                None if all => println!("no tunnels"),
                None => bail!(
                    "{}",
                    indices
                        .iter()
                        .map(|i| format!("no tunnel {i}"))
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            }
        }
        Command::Logs { index } => match client.request(Request::Logs { index }).await? {
            Some(response) => {
                let lines: Vec<String> = serde_json::from_value(response_data(response)?)?;
                for line in lines {
                    println!("{line}");
                }
            }
            None => println!("no tunnels"),
        },
        Command::Shutdown => {
            if let Some(response) = client.request(Request::Shutdown).await? {
                response_data(response)?;
            }
        }
        Command::Hosts | Command::Completions { .. } => unreachable!("handled without daemon"),
        Command::Daemon { .. } => unreachable!("daemon is dispatched by the binary"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fish_host_completion_uses_fixture_aliases() {
        use clap::CommandFactory;
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".ssh")).unwrap();
        let config = home.path().join(".ssh/config");
        std::fs::write(&config, "Host beta alpha *.wild !neg\nInclude extra\n").unwrap();
        std::fs::write(home.path().join(".ssh/extra"), "Host gamma alpha\n").unwrap();
        let aliases = crate::hosts::list_from(&config, home.path()).unwrap();
        let mut cmd = Args::command();
        cmd.build();
        let mut cmd = cmd.mut_subcommand("open", |cmd| {
            cmd.mut_arg("host", |arg| {
                arg.add(ArgValueCompleter::new(move |current: &OsStr| {
                    host_candidates(current, aliases.clone())
                }))
            })
        });
        cmd.build();
        for (prefix, expected) in [("", "alpha\nbeta\ngamma\n"), ("b", "beta\n"), ("z", "")] {
            let mut output = Vec::new();
            clap_complete::env::Fish
                .write_complete(
                    &mut cmd,
                    ["sshtm", "open", prefix]
                        .into_iter()
                        .map(Into::into)
                        .collect(),
                    None,
                    &mut output,
                )
                .unwrap();
            let output = String::from_utf8(output).unwrap();
            // The engine also offers --help at an empty positional word.
            let aliases = output
                .lines()
                .filter(|line| !line.starts_with('-'))
                .map(|line| format!("{line}\n"))
                .collect::<String>();
            assert_eq!(aliases, expected);
        }
    }

    #[test]
    fn range_validation_and_bounds() {
        for invalid in [
            "0",
            "-1",
            "3-2",
            "1-",
            "1-2-3",
            "4294967296",
            "1-4294967295",
            "+1",
        ] {
            assert!(index_range(invalid).is_err(), "{invalid}");
        }
        assert_eq!(
            kill_indices(&[index_range("2-4").unwrap(), index_range("3").unwrap()]).unwrap(),
            vec![2, 3, 4]
        );
        assert_eq!(
            kill_indices(&[index_range("4294967295").unwrap()]).unwrap(),
            vec![u32::MAX]
        );
        assert!(
            kill_indices(&[index_range("1-4096").unwrap(), index_range("4097").unwrap()]).is_err()
        );
    }

    #[test]
    fn reconnect_table_shows_attempt_and_delay() {
        let row = Row {
            idx: 1,
            host: "host".into(),
            local: "127.0.0.1:5432".into(),
            remote: "localhost:5432".into(),
            state: "reconnecting".into(),
            uptime: 0,
            pid: None,
            attempt: 3,
            next_retry_ms: Some(1500),
        };
        let rendered = table(&[row]);
        assert!(rendered.contains("reconnecting (attempt 3, in 1.5s)"));
        assert_eq!(rendered.lines().count(), 2);
    }
}

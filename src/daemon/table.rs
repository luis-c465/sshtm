//! Stable, lowest-free indices and atomic local-port reservations.
use std::{
    collections::BTreeMap,
    net::TcpListener,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU16, Ordering},
    },
    time::Duration,
};

use anyhow::{Result, bail, ensure};
use serde::Serialize;
use tokio::sync::{Mutex as AsyncMutex, watch};
use tokio::time::Instant;

use super::supervisor::{self, Snapshot, State, Tunnel};
use crate::spec::Forward;

#[derive(Debug, Clone, Serialize)]
pub struct Row {
    pub idx: u32,
    pub host: String,
    pub local: String,
    pub remote: String,
    pub state: String,
    pub uptime: u64,
    pub pid: Option<u32>,
    pub attempt: u32,
    pub next_retry_ms: Option<u64>,
}
struct Entry {
    host: String,
    forward: Forward,
    snapshot: watch::Receiver<Snapshot>,
    tunnel: AsyncMutex<Tunnel>,
    reserved: Arc<AtomicU16>,
}
struct Inner {
    entries: BTreeMap<u32, Arc<Entry>>,
    closing: bool,
    empty_since: Instant,
}
impl Default for Inner {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            closing: false,
            empty_since: Instant::now(),
        }
    }
}
type PortPicker = Arc<dyn Fn() -> std::io::Result<TcpListener> + Send + Sync>;
pub struct Table {
    inner: Arc<Mutex<Inner>>,
    pick_port: PortPicker,
}
impl Default for Table {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            pick_port: Arc::new(|| TcpListener::bind(("127.0.0.1", 0))),
        }
    }
}
impl Table {
    pub fn idle_for(&self) -> Option<Duration> {
        let inner = self.inner.lock().unwrap();
        inner
            .entries
            .is_empty()
            .then(|| inner.empty_since.elapsed())
    }

    pub async fn open(
        &self,
        host: String,
        forward: Forward,
        auth_sock: Option<std::ffi::OsString>,
        settings: supervisor::Settings,
    ) -> Result<Row> {
        let (index, entry) = {
            let mut inner = self.inner.lock().unwrap();
            ensure!(!inner.closing, "daemon is shutting down");
            if let Some(port) = forward.local {
                for (index, entry) in &inner.entries {
                    if entry.reserved.load(Ordering::Relaxed) == port {
                        bail!("local port {port} is held by tunnel {index}");
                    }
                }
            }
            let index = lowest_free(inner.entries.keys().copied())?;
            let local = match forward.local {
                Some(port) => port,
                None => allocate_port(&inner, &self.pick_port)?,
            };
            let reserved = Arc::new(AtomicU16::new(local));
            // Every retry checks and publishes its replacement reservation under
            // this lock, just like initial allocation and explicit opens.
            let weak_inner = Arc::downgrade(&self.inner);
            let token = reserved.clone();
            let picker = self.pick_port.clone();
            let allocator: supervisor::PortAllocator = Arc::new(move || {
                let inner = weak_inner
                    .upgrade()
                    .ok_or_else(|| anyhow::anyhow!("table stopped"))?;
                let inner = inner.lock().unwrap();
                ensure!(
                    !inner.closing
                        && inner
                            .entries
                            .get(&index)
                            .is_some_and(|e| Arc::ptr_eq(&e.reserved, &token)),
                    "tunnel stopped"
                );
                let port = allocate_port(&inner, &picker)?;
                token.store(port, Ordering::Relaxed);
                Ok(port)
            });
            let tunnel = Tunnel::start_at(
                host.clone(),
                forward.clone(),
                auth_sock,
                settings,
                local,
                Some(allocator),
            )?;
            let entry = Arc::new(Entry {
                host,
                forward,
                snapshot: tunnel.subscribe(),
                tunnel: AsyncMutex::new(tunnel),
                reserved,
            });
            inner.entries.insert(index, entry.clone());
            (index, entry)
        };
        let mut snapshot = entry.snapshot.clone();
        let result = loop {
            let state = snapshot.borrow().state.clone();
            match state {
                State::Up => break Ok(row(index, &entry)),
                State::Failed(error) => break Err(anyhow::anyhow!(error)),
                State::Stopped => {
                    break Err(anyhow::anyhow!("tunnel {index} stopped before readiness"));
                }
                _ => {
                    if snapshot.changed().await.is_err() {
                        break Err(anyhow::anyhow!("supervisor stopped"));
                    }
                }
            }
        };
        if result.is_err() {
            self.remove(index, &entry).await?;
        }
        result
    }
    pub fn list(&self) -> Vec<Row> {
        self.inner
            .lock()
            .unwrap()
            .entries
            .iter()
            .map(|(&i, e)| row(i, e))
            .collect()
    }
    pub async fn logs(&self, index: u32) -> Result<Vec<String>> {
        let entry = self.inner.lock().unwrap().entries.get(&index).cloned();
        let entry = entry.ok_or_else(|| anyhow::anyhow!("no tunnel {index}"))?;
        let logs = entry.tunnel.lock().await.logs();
        Ok(logs)
    }
    async fn remove(&self, index: u32, entry: &Arc<Entry>) -> Result<()> {
        entry.tunnel.lock().await.kill().await?;
        let mut inner = self.inner.lock().unwrap();
        // A simultaneous kill may have removed it and a new open reused its index.
        if inner
            .entries
            .get(&index)
            .is_some_and(|e| Arc::ptr_eq(e, entry))
        {
            inner.entries.remove(&index);
            if inner.entries.is_empty() {
                inner.empty_since = Instant::now();
            }
        }
        Ok(())
    }
    pub async fn kill(&self, indices: Vec<u32>, all: bool) -> Result<Vec<u32>> {
        let (entries, missing) = {
            let inner = self.inner.lock().unwrap();
            let indices: std::collections::BTreeSet<_> = if all {
                inner.entries.keys().copied().collect()
            } else {
                indices.into_iter().collect()
            };
            let mut entries = Vec::new();
            let mut missing = Vec::new();
            for index in indices {
                match inner.entries.get(&index) {
                    Some(e) => entries.push((index, e.clone())),
                    None => missing.push(index),
                }
            }
            (entries, missing)
        };
        let mut tasks = tokio::task::JoinSet::new();
        // Kill concurrently, so shutdown is bounded by one grace period, not N.
        for (_, entry) in &entries {
            let entry = entry.clone();
            tasks.spawn(async move { entry.tunnel.lock().await.kill().await });
        }
        while let Some(result) = tasks.join_next().await {
            result??;
        }
        let mut inner = self.inner.lock().unwrap();
        let was_empty = inner.entries.is_empty();
        for (index, entry) in entries {
            if inner
                .entries
                .get(&index)
                .is_some_and(|e| Arc::ptr_eq(e, &entry))
            {
                inner.entries.remove(&index);
            }
        }
        if !was_empty && inner.entries.is_empty() {
            inner.empty_since = Instant::now();
        }
        Ok(missing)
    }
    pub async fn shutdown(&self) -> Result<()> {
        self.inner.lock().unwrap().closing = true;
        self.kill(Vec::new(), true).await?;
        Ok(())
    }
}
fn allocate_port(inner: &Inner, pick: &PortPicker) -> Result<u16> {
    // Keep rejected candidates bound so the kernel cannot repeatedly return a
    // reserved but not-yet-listening port. The caller holds the table lock.
    let mut excluded = Vec::new();
    loop {
        let listener = pick()?;
        let port = listener.local_addr()?.port();
        if !inner
            .entries
            .values()
            .any(|e| e.reserved.load(Ordering::Relaxed) == port)
        {
            return Ok(port);
        }
        excluded.push(listener);
    }
}
fn lowest_free(indices: impl Iterator<Item = u32>) -> Result<u32> {
    let mut next = 1u32;
    for index in indices {
        if index == next {
            next = next
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("tunnel index exhausted"))?;
        } else if index > next {
            break;
        }
    }
    Ok(next)
}
fn row(index: u32, entry: &Entry) -> Row {
    let s = entry.snapshot.borrow();
    let state = match s.state {
        State::Starting => "starting",
        State::Up => "up",
        State::Reconnecting => "reconnecting",
        State::Stopped => "stopped",
        State::Failed(_) => "failed",
    };
    let host = if entry.forward.rhost.contains(':') {
        format!("[{}]", entry.forward.rhost)
    } else {
        entry.forward.rhost.clone()
    };
    Row {
        idx: index,
        host: entry.host.clone(),
        local: format!("127.0.0.1:{}", entry.reserved.load(Ordering::Relaxed)),
        remote: format!("{host}:{}", entry.forward.rport),
        state: state.into(),
        uptime: s.up_since.map(|t| t.elapsed().as_secs()).unwrap_or(0),
        pid: s.pid,
        attempt: s.attempt,
        next_retry_ms: s.next_retry.map(|t| {
            t.saturating_duration_since(Instant::now())
                .as_millis()
                .min(u64::MAX as u128) as u64
        }),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn automatic_retry_cannot_steal_a_starting_reservation() {
        use std::{os::unix::fs::PermissionsExt, sync::atomic::AtomicUsize};
        let fixture = tempfile::tempdir().unwrap();
        let marker = fixture.path().join("attempted");
        let fake = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("ssh");
        let script = fixture.path().join("ssh");
        std::fs::write(&script, format!(
            "#!/bin/sh\nfor arg do last=$arg; done\nif [ \"$1\" != -G ] && [ \"$last\" = ok ] && [ ! -f '{}' ]; then\n : > '{}'\n echo 'Address already in use' >&2\n exit 255\nfi\nexec '{}' \"$@\"\n",
            marker.display(), marker.display(), fake.display()
        )).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let reserved = TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let calls = Arc::new(AtomicUsize::new(0));
        let picker_calls = calls.clone();
        let table = Arc::new(Table {
            inner: Arc::new(Mutex::new(Inner::default())),
            pick_port: Arc::new(move || {
                // First auto allocation is free; its forced bind failure retries
                // onto the explicit starting reservation, then a free candidate.
                let call = picker_calls.fetch_add(1, Ordering::Relaxed);
                TcpListener::bind(("127.0.0.1", if call == 1 { reserved } else { 0 }))
            }),
        });
        let settings = supervisor::Settings {
            path: Some(fixture.path().as_os_str().to_owned()),
            ready_timeout: Duration::from_secs(5),
            ..Default::default()
        };
        let starting_table = table.clone();
        let starting_settings = settings.clone();
        let opening = tokio::spawn(async move {
            starting_table
                .open(
                    "hang".into(),
                    format!("{reserved}:1").parse().unwrap(),
                    None,
                    starting_settings,
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !table
                .list()
                .iter()
                .any(|row| row.idx == 1 && row.pid.is_some())
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let row = table
            .open("ok".into(), "0:1".parse().unwrap(), None, settings)
            .await
            .unwrap();
        assert_eq!(row.idx, 2);
        assert_ne!(row.local, format!("127.0.0.1:{reserved}"));
        assert_eq!(calls.load(Ordering::Relaxed), 3);
        assert_eq!(table.list()[0].local, format!("127.0.0.1:{reserved}"));
        table.shutdown().await.unwrap();
        assert!(opening.await.unwrap().is_err());
        assert!(TcpListener::bind(("127.0.0.1", reserved)).is_ok());
    }

    #[test]
    fn holes_are_reused() {
        assert_eq!(lowest_free([].into_iter()).unwrap(), 1);
        assert_eq!(lowest_free([1, 3].into_iter()).unwrap(), 2);
        assert_eq!(lowest_free([3].into_iter()).unwrap(), 1);
    }
}

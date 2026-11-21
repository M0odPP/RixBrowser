use crate::error::{IpcError, Result};
use crate::protocol::{BrowserToContent, ContentToBrowser, PipelineId};
#[cfg(unix)]
use crate::transport::{IpcListener, UnixIpcReceiver, UnixIpcSender};
use rix_core::{RixScheme, RixUrl};
use std::collections::HashMap;
use std::net::IpAddr;
#[cfg(unix)]
use std::os::fd::{BorrowedFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;
#[cfg(unix)]
use tokio::process::Command;
#[cfg(unix)]
use tokio::sync::{mpsc, oneshot};
#[cfg(unix)]
use tokio::task::JoinHandle;

#[cfg(unix)]
const CHILD_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(unix)]
const CHILD_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessRole {
    Browser,
    Network,
    Content { site_id: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildLaunchArgs {
    pub role: ProcessRole,
    pub ipc_socket: PathBuf,
}

pub fn parse_process_args(args: &[String]) -> Result<Option<ChildLaunchArgs>> {
    let mut role_str: Option<String> = None;
    let mut site_id: Option<u64> = None;
    let mut ipc_socket: Option<PathBuf> = None;

    let mut idx = 0;
    while idx < args.len() {
        let arg = &args[idx];
        if let Some(val) = arg.strip_prefix("--process=") {
            role_str = Some(val.to_string());
        } else if arg == "--process" {
            idx += 1;
            let val = args.get(idx).ok_or_else(|| {
                IpcError::InvalidEndpoint("--process requires a role value".to_string())
            })?;
            role_str = Some(val.clone());
        } else if let Some(val) = arg.strip_prefix("--site-id=") {
            let parsed = val.parse::<u64>().map_err(|_| {
                IpcError::InvalidEndpoint(format!("invalid --site-id value: {val}"))
            })?;
            site_id = Some(parsed);
        } else if arg == "--site-id" {
            idx += 1;
            let val = args.get(idx).ok_or_else(|| {
                IpcError::InvalidEndpoint("--site-id requires an integer value".to_string())
            })?;
            let parsed = val.parse::<u64>().map_err(|_| {
                IpcError::InvalidEndpoint(format!("invalid --site-id value: {val}"))
            })?;
            site_id = Some(parsed);
        } else if let Some(val) = arg.strip_prefix("--ipc-socket=") {
            ipc_socket = Some(PathBuf::from(val));
        } else if arg == "--ipc-socket" {
            idx += 1;
            let val = args.get(idx).ok_or_else(|| {
                IpcError::InvalidEndpoint("--ipc-socket requires a path".to_string())
            })?;
            ipc_socket = Some(PathBuf::from(val));
        }
        idx += 1;
    }

    let Some(role_name) = role_str else {
        return Ok(None);
    };

    let socket = ipc_socket
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| {
            IpcError::InvalidEndpoint("--ipc-socket is required for child processes".to_string())
        })?;

    let role = match role_name.as_str() {
        "content" => {
            let site_id = site_id.ok_or_else(|| {
                IpcError::InvalidEndpoint("--site-id is required for --process=content".to_string())
            })?;
            ProcessRole::Content { site_id }
        }
        "network" => ProcessRole::Network,
        other => {
            return Err(IpcError::InvalidEndpoint(format!(
                "unsupported child process role: {other}"
            )))
        }
    };

    Ok(Some(ChildLaunchArgs {
        role,
        ipc_socket: socket,
    }))
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SiteKey {
    Origin {
        scheme: String,
        registrable_domain: String,
    },
    Opaque(u64),
}

impl SiteKey {
    pub fn from_url(url: &RixUrl, opaque_id: u64) -> Self {
        match url.scheme() {
            RixScheme::Http | RixScheme::Https => {
                let Some(raw_host) = url.host_str().filter(|h| !h.is_empty()) else {
                    return Self::Opaque(opaque_id);
                };
                let normalized = raw_host.trim_end_matches('.').to_ascii_lowercase();
                let unbracketed = normalized.trim_start_matches('[').trim_end_matches(']');
                let registrable_domain =
                    if normalized == "localhost" || unbracketed.parse::<IpAddr>().is_ok() {
                        normalized
                    } else {
                        psl::domain_str(&normalized)
                            .unwrap_or(&normalized)
                            .to_string()
                    };
                Self::Origin {
                    scheme: url.scheme().to_string(),
                    registrable_domain,
                }
            }
            _ => Self::Opaque(opaque_id),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineHealth {
    Starting,
    Running,
    Crashed {
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorEvent {
    ProcessCrashed {
        site_id: u64,
        site: SiteKey,
        affected_pipelines: Vec<PipelineId>,
        exit_code: Option<i32>,
        signal: Option<i32>,
    },
}

#[cfg(unix)]
struct ChildExitNotice {
    site_id: u64,
    exit_code: Option<i32>,
    signal: Option<i32>,
}

#[cfg(unix)]
struct ManagedContentProcess {
    site: SiteKey,
    pid: Option<u32>,
    sender: UnixIpcSender<BrowserToContent>,
    receiver: UnixIpcReceiver<ContentToBrowser>,
    pipelines: Vec<PipelineId>,
    expected_shutdown: Arc<AtomicBool>,
    force_kill_tx: Option<oneshot::Sender<()>>,
    watcher: Option<JoinHandle<()>>,
}

#[cfg(unix)]
impl Drop for ManagedContentProcess {
    fn drop(&mut self) {
        if let Some(watcher) = self.watcher.take() {
            watcher.abort();
        }
    }
}

#[cfg(unix)]
pub struct ProcessSupervisor {
    executable: PathBuf,
    runtime_dir: PathBuf,
    next_site_id: u64,
    nonce: AtomicU64,
    processes: HashMap<u64, ManagedContentProcess>,
    site_by_key: HashMap<SiteKey, u64>,
    pipeline_sites: HashMap<PipelineId, u64>,
    pipeline_health: HashMap<PipelineId, PipelineHealth>,
    exit_tx: mpsc::UnboundedSender<ChildExitNotice>,
    exit_rx: mpsc::UnboundedReceiver<ChildExitNotice>,
}

#[cfg(unix)]
impl ProcessSupervisor {
    pub fn new(executable: impl Into<PathBuf>, runtime_dir: impl Into<PathBuf>) -> Self {
        let (exit_tx, exit_rx) = mpsc::unbounded_channel();
        Self {
            executable: executable.into(),
            runtime_dir: runtime_dir.into(),
            next_site_id: 1,
            nonce: AtomicU64::new(1),
            processes: HashMap::new(),
            site_by_key: HashMap::new(),
            pipeline_sites: HashMap::new(),
            pipeline_health: HashMap::new(),
            exit_tx,
            exit_rx,
        }
    }

    pub fn current_exe(runtime_dir: impl Into<PathBuf>) -> Result<Self> {
        let exe = std::env::current_exe()?;
        Ok(Self::new(exe, runtime_dir))
    }

    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }

    fn apply_exit_notice(&mut self, notice: ChildExitNotice) -> Option<SupervisorEvent> {
        let proc_entry = self.processes.remove(&notice.site_id)?;
        self.site_by_key.remove(&proc_entry.site);
        for &pipeline in &proc_entry.pipelines {
            self.pipeline_health.insert(
                pipeline,
                PipelineHealth::Crashed {
                    exit_code: notice.exit_code,
                    signal: notice.signal,
                },
            );
        }
        Some(SupervisorEvent::ProcessCrashed {
            site_id: notice.site_id,
            site: proc_entry.site.clone(),
            affected_pipelines: proc_entry.pipelines.clone(),
            exit_code: notice.exit_code,
            signal: notice.signal,
        })
    }

    pub fn poll_events(&mut self) -> Vec<SupervisorEvent> {
        let mut events = Vec::new();
        while let Ok(notice) = self.exit_rx.try_recv() {
            if let Some(evt) = self.apply_exit_notice(notice) {
                events.push(evt);
            }
        }
        events
    }

    pub async fn next_event(&mut self) -> Option<SupervisorEvent> {
        loop {
            let notice = self.exit_rx.recv().await?;
            if let Some(evt) = self.apply_exit_notice(notice) {
                return Some(evt);
            }
        }
    }

    pub async fn ensure_content_process(&mut self, site: SiteKey) -> Result<u64> {
        // Drain pending crash notices first so dead entries are never reused.
        let _ = self.poll_events();

        if let Some(&existing_id) = self.site_by_key.get(&site) {
            if self.processes.contains_key(&existing_id) {
                return Ok(existing_id);
            }
            self.site_by_key.remove(&site);
        }

        let site_id = self.next_site_id;
        self.next_site_id = self.next_site_id.wrapping_add(1);
        let seq = self.nonce.fetch_add(1, Ordering::Relaxed);

        std::fs::create_dir_all(&self.runtime_dir)?;
        let sock_path = self.runtime_dir.join(format!("c-{site_id}-{seq}.sock"));
        let listener = IpcListener::bind(&sock_path)?;

        let mut cmd = Command::new(&self.executable);
        cmd.arg("--process=content")
            .arg(format!("--site-id={site_id}"))
            .arg(format!("--ipc-socket={}", sock_path.display()))
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|err| {
            IpcError::Process(format!(
                "failed to spawn content process {}: {err}",
                self.executable.display()
            ))
        })?;
        let pid = child.id();

        let channel = tokio::time::timeout(
            CHILD_CONNECT_TIMEOUT,
            listener.accept::<BrowserToContent, ContentToBrowser>(),
        )
        .await
        .map_err(|_| IpcError::ProcessTimeout("content child UDS handshake"))??;
        drop(listener);

        let (sender, receiver) = channel.into_split();
        let expected_shutdown = Arc::new(AtomicBool::new(false));
        let expected_flag = Arc::clone(&expected_shutdown);
        let exit_tx = self.exit_tx.clone();
        let (force_kill_tx, mut force_kill_rx) = oneshot::channel();

        let watcher = tokio::spawn(async move {
            let status = tokio::select! {
                res = child.wait() => Some(res),
                _ = &mut force_kill_rx => {
                    let _ = child.kill().await;
                    Some(child.wait().await)
                }
            };
            if let Some(res) = status {
                if !expected_flag.load(Ordering::Acquire) {
                    let (exit_code, signal) = match res {
                        Ok(st) => (st.code(), st.signal()),
                        Err(_) => (None, None),
                    };
                    let _ = exit_tx.send(ChildExitNotice {
                        site_id,
                        exit_code,
                        signal,
                    });
                }
            }
        });

        self.site_by_key.insert(site.clone(), site_id);
        self.processes.insert(
            site_id,
            ManagedContentProcess {
                site,
                pid,
                sender,
                receiver,
                pipelines: Vec::new(),
                expected_shutdown,
                force_kill_tx: Some(force_kill_tx),
                watcher: Some(watcher),
            },
        );

        Ok(site_id)
    }

    pub fn register_pipeline(&mut self, site_id: u64, pipeline: PipelineId) -> Result<()> {
        let entry = self
            .processes
            .get_mut(&site_id)
            .ok_or_else(|| IpcError::Process(format!("unknown content site_id {site_id}")))?;
        if !entry.pipelines.contains(&pipeline) {
            entry.pipelines.push(pipeline);
        }
        self.pipeline_sites.insert(pipeline, site_id);
        self.pipeline_health
            .insert(pipeline, PipelineHealth::Starting);
        Ok(())
    }

    pub fn mark_pipeline_running(&mut self, pipeline: PipelineId) {
        self.pipeline_health
            .insert(pipeline, PipelineHealth::Running);
    }

    pub fn pipeline_health(&self, pipeline: PipelineId) -> Option<PipelineHealth> {
        self.pipeline_health.get(&pipeline).copied()
    }

    pub fn child_pid(&self, site_id: u64) -> Option<u32> {
        self.processes.get(&site_id).and_then(|p| p.pid)
    }

    pub fn active_site_count(&self) -> usize {
        self.processes.len()
    }

    pub async fn send_to_site(&mut self, site_id: u64, msg: &BrowserToContent) -> Result<usize> {
        let entry = self
            .processes
            .get_mut(&site_id)
            .ok_or_else(|| IpcError::Process(format!("unknown content site_id {site_id}")))?;
        entry.sender.send(msg).await
    }

    pub async fn send_with_fds_to_site(
        &mut self,
        site_id: u64,
        msg: &BrowserToContent,
        fds: &[BorrowedFd<'_>],
    ) -> Result<usize> {
        let entry = self
            .processes
            .get_mut(&site_id)
            .ok_or_else(|| IpcError::Process(format!("unknown content site_id {site_id}")))?;
        entry.sender.send_with_fds(msg, fds).await
    }

    pub async fn recv_from_site(&mut self, site_id: u64) -> Result<Option<ContentToBrowser>> {
        let entry = self
            .processes
            .get_mut(&site_id)
            .ok_or_else(|| IpcError::Process(format!("unknown content site_id {site_id}")))?;
        entry.receiver.recv().await
    }

    pub async fn recv_with_fds_from_site(
        &mut self,
        site_id: u64,
    ) -> Result<Option<(ContentToBrowser, Vec<OwnedFd>)>> {
        let entry = self
            .processes
            .get_mut(&site_id)
            .ok_or_else(|| IpcError::Process(format!("unknown content site_id {site_id}")))?;
        entry.receiver.recv_with_fds().await
    }

    pub async fn shutdown_site(&mut self, site_id: u64) -> Result<()> {
        let Some(mut entry) = self.processes.remove(&site_id) else {
            return Ok(());
        };
        self.site_by_key.remove(&entry.site);
        for &pipeline in &entry.pipelines {
            self.pipeline_sites.remove(&pipeline);
        }

        entry.expected_shutdown.store(true, Ordering::Release);
        let _ = entry.sender.send(&BrowserToContent::Shutdown).await;
        let _ = entry.sender.shutdown().await;

        if let Some(mut watcher) = entry.watcher.take() {
            if tokio::time::timeout(CHILD_SHUTDOWN_TIMEOUT, &mut watcher)
                .await
                .is_err()
            {
                if let Some(tx) = entry.force_kill_tx.take() {
                    let _ = tx.send(());
                }
                let _ = tokio::time::timeout(CHILD_SHUTDOWN_TIMEOUT, watcher).await;
            }
        }
        Ok(())
    }

    pub async fn shutdown_all(&mut self) {
        let ids: Vec<u64> = self.processes.keys().copied().collect();
        for site_id in ids {
            let _ = self.shutdown_site(site_id).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_key_groups_same_etld_plus_one_and_isolates_different_sites() {
        let a = RixUrl::parse("https://a.example.org/page").expect("url");
        let sub = RixUrl::parse("https://sub.a.example.org/other").expect("url");
        let other_scheme = RixUrl::parse("http://a.example.org/").expect("url");
        let b = RixUrl::parse("https://b.example.com/").expect("url");
        let ip = RixUrl::parse("http://127.0.0.1:8080/").expect("url");
        let local = RixUrl::parse("http://localhost:3000/").expect("url");
        let internal = RixUrl::parse("rix://chrome/browser.html").expect("url");

        assert_eq!(SiteKey::from_url(&a, 1), SiteKey::from_url(&sub, 2));
        assert_ne!(
            SiteKey::from_url(&a, 1),
            SiteKey::from_url(&other_scheme, 2)
        );
        assert_ne!(SiteKey::from_url(&a, 1), SiteKey::from_url(&b, 2));
        assert_eq!(
            SiteKey::from_url(&ip, 1),
            SiteKey::Origin {
                scheme: "http".into(),
                registrable_domain: "127.0.0.1".into(),
            }
        );
        assert_eq!(
            SiteKey::from_url(&local, 1),
            SiteKey::Origin {
                scheme: "http".into(),
                registrable_domain: "localhost".into(),
            }
        );
        assert_eq!(SiteKey::from_url(&internal, 42), SiteKey::Opaque(42));
    }

    #[test]
    fn parse_process_args_handles_browser_and_child_modes() {
        let browser = vec!["https://example.org".to_string()];
        assert_eq!(parse_process_args(&browser).expect("browser"), None);

        let content = vec![
            "--process=content".to_string(),
            "--site-id=17".to_string(),
            "--ipc-socket=/tmp/rix.sock".to_string(),
        ];
        assert_eq!(
            parse_process_args(&content).expect("content"),
            Some(ChildLaunchArgs {
                role: ProcessRole::Content { site_id: 17 },
                ipc_socket: PathBuf::from("/tmp/rix.sock"),
            })
        );

        let network = vec![
            "--process".to_string(),
            "network".to_string(),
            "--ipc-socket".to_string(),
            "/tmp/net.sock".to_string(),
        ];
        assert_eq!(
            parse_process_args(&network).expect("network"),
            Some(ChildLaunchArgs {
                role: ProcessRole::Network,
                ipc_socket: PathBuf::from("/tmp/net.sock"),
            })
        );

        let missing_site = vec![
            "--process=content".to_string(),
            "--ipc-socket=/tmp/rix.sock".to_string(),
        ];
        assert!(matches!(
            parse_process_args(&missing_site),
            Err(IpcError::InvalidEndpoint(_))
        ));
    }
}

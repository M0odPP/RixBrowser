use rix_ipc::{
    apply_sandbox, connect_unix, BrowserToContent, ContentToBrowser, ContentToNetwork,
    NetworkToContent, Result, SandboxConfig, SealedShmemBlob,
};
use std::path::Path;
use tracing::{debug, info};

fn build_content_sandbox_config() -> SandboxConfig {
    let mut cfg = SandboxConfig::for_content();
    if std::env::var_os("RIX_DISABLE_SANDBOX_RLIMIT").is_some() {
        cfg.address_space_limit_bytes = None;
    }
    cfg
}

pub async fn run_content_process(site_id: u64, ipc_socket: &Path) -> Result<()> {
    let mut channel = connect_unix::<ContentToBrowser, BrowserToContent>(ipc_socket).await?;
    let sandbox_status = apply_sandbox(&build_content_sandbox_config())?;
    debug!(
        site_id,
        no_new_privs = sandbox_status.no_new_privs,
        rlimit_applied = sandbox_status.rlimit_applied,
        landlock_abi = ?sandbox_status.landlock_abi_version,
        "content process initialized"
    );

    while let Some((msg, fds)) = channel.recv_with_fds().await? {
        for fd in fds {
            let blob = SealedShmemBlob::from_owned_fd(fd)?;
            debug!(
                site_id,
                blob_bytes = blob.len(),
                "validated sealed shmem FD"
            );
        }

        match msg {
            BrowserToContent::CreatePipeline { pipeline, .. } => {
                channel
                    .send(&ContentToBrowser::PipelineReady { pipeline })
                    .await?;
            }
            BrowserToContent::Ping { seq } => {
                channel.send(&ContentToBrowser::Pong { seq }).await?;
            }
            BrowserToContent::DestroyPipeline { .. }
            | BrowserToContent::CommitNavigation { .. }
            | BrowserToContent::ViewportChanged { .. }
            | BrowserToContent::InputEvent { .. }
            | BrowserToContent::ScrollOffsetSync { .. }
            | BrowserToContent::BeginFrame { .. }
            | BrowserToContent::ThemeChanged { .. } => {}
            BrowserToContent::Shutdown => {
                let _ = channel.shutdown().await;
                return Ok(());
            }
        }
    }

    info!(site_id, "content process exiting cleanly on EOF");
    Ok(())
}

pub async fn run_network_process(ipc_socket: &Path) -> Result<()> {
    let mut channel = connect_unix::<NetworkToContent, ContentToNetwork>(ipc_socket).await?;
    let sandbox_status = apply_sandbox(&SandboxConfig::for_network())?;
    debug!(
        no_new_privs = sandbox_status.no_new_privs,
        rlimit_applied = sandbox_status.rlimit_applied,
        "network process initialized"
    );

    while let Some(msg) = channel.recv().await? {
        match msg {
            ContentToNetwork::FetchStart { .. }
            | ContentToNetwork::FetchCancel { .. }
            | ContentToNetwork::WebSocketOpen { .. }
            | ContentToNetwork::WebSocketSend { .. }
            | ContentToNetwork::WebSocketClose { .. } => {}
        }
    }

    info!("network process exiting cleanly on EOF");
    Ok(())
}

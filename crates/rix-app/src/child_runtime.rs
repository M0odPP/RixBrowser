use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use rix_content::{drain_ready_tasks, ContentRuntime};
use rix_dom::Document;
use rix_ipc::{
    apply_sandbox, connect_unix, BrowserToContent, ContentToBrowser, ContentToNetwork, LoadState,
    NetworkToContent, PipelineId, Result, SandboxConfig, SealedShmemBlob,
};
use rix_text::FontDatabase;
use tracing::{debug, info};

fn build_content_sandbox_config() -> SandboxConfig {
    let mut cfg = SandboxConfig::for_content();
    if std::env::var_os("RIX_DISABLE_SANDBOX_RLIMIT").is_some() {
        cfg.address_space_limit_bytes = None;
    }
    cfg
}

async fn flush_outbound_ipc(
    channel: &mut rix_ipc::UnixIpcChannel<ContentToBrowser, BrowserToContent>,
    rt: &mut ContentRuntime,
) -> Result<()> {
    for out_msg in rt.drain_outbound_ipc() {
        channel.send(&out_msg).await?;
    }
    Ok(())
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

    let font_db = Arc::new(FontDatabase::new());
    let mut pipelines: HashMap<PipelineId, ContentRuntime> = HashMap::new();

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
            BrowserToContent::CreatePipeline {
                pipeline, viewport, ..
            } => {
                let doc = Rc::new(RefCell::new(Document::new()));
                let mut rt = ContentRuntime::new(doc, viewport.logical_size, Arc::clone(&font_db));
                rt.set_pipeline_id(pipeline);
                pipelines.insert(pipeline, rt);
                channel
                    .send(&ContentToBrowser::PipelineReady { pipeline })
                    .await?;
            }
            BrowserToContent::DestroyPipeline { pipeline } => {
                pipelines.remove(&pipeline);
            }
            BrowserToContent::LoadDocument {
                pipeline,
                url,
                html,
                sandbox,
            } => {
                if let Some(rt) = pipelines.get_mut(&pipeline) {
                    rt.set_document_url(&url);
                    rt.set_sandbox_attr(sandbox.as_deref());
                    let load_res = rt.load_html_with_scripts(&html);
                    drain_ready_tasks(&rt.event_loop, &mut rt.vm);
                    flush_outbound_ipc(&mut channel, rt).await?;
                    let state = match load_res {
                        Ok(()) => LoadState::Complete,
                        Err(err) => LoadState::Failed {
                            message: err.to_string(),
                        },
                    };
                    channel
                        .send(&ContentToBrowser::LoadStateChanged { pipeline, state })
                        .await?;
                }
            }
            BrowserToContent::EvalScript {
                pipeline,
                seq,
                source,
            } => {
                if let Some(rt) = pipelines.get_mut(&pipeline) {
                    let (ok, value) = match rt.eval_script(&source) {
                        Ok(val) => {
                            drain_ready_tasks(&rt.event_loop, &mut rt.vm);
                            (true, rt.vm.value_to_rust_string(val))
                        }
                        Err(err) => (false, err.to_string()),
                    };
                    flush_outbound_ipc(&mut channel, rt).await?;
                    channel
                        .send(&ContentToBrowser::ScriptResult {
                            pipeline,
                            seq,
                            ok,
                            value,
                        })
                        .await?;
                }
            }
            BrowserToContent::ChildFrameBound {
                parent_pipeline,
                frame_node_id,
                child_pipeline,
                origin,
                ..
            } => {
                if let Some(rt) = pipelines.get_mut(&parent_pipeline) {
                    rt.bind_child_frame(frame_node_id, child_pipeline, origin);
                }
            }
            BrowserToContent::ChildFrameCrashed {
                parent_pipeline,
                child_pipeline,
                ..
            } => {
                if let Some(rt) = pipelines.get_mut(&parent_pipeline) {
                    let _ = rt.mark_child_frame_crashed(child_pipeline);
                }
            }
            BrowserToContent::DeliverPostMessage {
                target_pipeline,
                source_pipeline,
                data_json,
                origin,
                transfer_ports,
            } => {
                if let Some(rt) = pipelines.get_mut(&target_pipeline) {
                    rt.deliver_ipc_post_message(
                        source_pipeline,
                        &data_json,
                        &origin,
                        &transfer_ports,
                    );
                    drain_ready_tasks(&rt.event_loop, &mut rt.vm);
                    flush_outbound_ipc(&mut channel, rt).await?;
                }
            }
            BrowserToContent::DeliverPortMessage {
                target_pipeline,
                port_id,
                data_json,
                transfer_ports,
            } => {
                if let Some(rt) = pipelines.get_mut(&target_pipeline) {
                    rt.deliver_ipc_port_message(
                        target_pipeline,
                        port_id,
                        &data_json,
                        &transfer_ports,
                    );
                    drain_ready_tasks(&rt.event_loop, &mut rt.vm);
                    flush_outbound_ipc(&mut channel, rt).await?;
                }
            }
            BrowserToContent::RequestDisplayList { pipeline, epoch } => {
                if let Some(rt) = pipelines.get_mut(&pipeline) {
                    let dl = rt.build_display_list(pipeline);
                    let bytes = dl.to_bytes().unwrap_or_default();
                    channel
                        .send(&ContentToBrowser::DisplayListReady {
                            pipeline,
                            epoch,
                            bytes,
                        })
                        .await?;
                }
            }
            BrowserToContent::ViewportChanged { pipeline, viewport } => {
                if let Some(rt) = pipelines.get_mut(&pipeline) {
                    rt.host.borrow_mut().viewport = viewport.logical_size;
                }
            }
            BrowserToContent::Ping { seq } => {
                channel.send(&ContentToBrowser::Pong { seq }).await?;
            }
            BrowserToContent::CommitNavigation { .. }
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

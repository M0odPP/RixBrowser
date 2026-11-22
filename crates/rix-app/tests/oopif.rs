//! Real OS multi-process Out-of-Process Iframe (OOPIF) isolation, cross-process
//! `postMessage` & `MessageChannel`/`MessagePort` transfer, headless GPU subframe
//! compositing, and `SIGKILL` child crash recovery tests.

#![cfg(unix)]

use std::sync::Mutex;
use std::time::Duration;

use rix_core::{RixUrl, Size};
use rix_ipc::{
    BrowserToContent, ContentToBrowser, LoadState, PipelineHealth, PipelineId, ProcessSupervisor,
    SiteKey, SupervisorEvent, SurfaceId, TabId, ViewportMetrics,
};
use rix_render::{Color, Compositor, DisplayList, LayerRole, VelloRenderer};
use tempfile::TempDir;

const RIX_APP_BIN: &str = env!("CARGO_BIN_EXE_rix-app");

fn pixel_at(rgba: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let idx = ((y * width + x) * 4) as usize;
    [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]]
}

fn render_rgba(compositor: &mut Compositor, width: u32, height: u32) -> Vec<u8> {
    static GPU_LOCK: Mutex<()> = Mutex::new(());
    let _guard = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let scene = compositor.compose().expect("compositor produces scene");
    let mut renderer = VelloRenderer::new();
    renderer
        .render_to_rgba_blocking(scene, width, height, Color::BLACK)
        .expect("headless GPU render")
}

#[tokio::test]
async fn cross_origin_oopif_postmessage_gpu_compositing_and_sigkill_inline_crash() {
    let runtime_dir = TempDir::new().expect("tempdir");
    let mut supervisor = ProcessSupervisor::new(RIX_APP_BIN, runtime_dir.path());

    // 1. Spawn TWO isolated OS Content Processes for distinct sites.
    let parent_site = SiteKey::from_url(
        &RixUrl::parse("https://parent.example.org/").expect("url"),
        1,
    );
    let child_site = SiteKey::from_url(
        &RixUrl::parse("https://child.example.com/widget").expect("url"),
        1,
    );
    let parent_site_id = supervisor
        .ensure_content_process(parent_site)
        .await
        .expect("spawn parent process");
    let child_site_id = supervisor
        .ensure_content_process(child_site)
        .await
        .expect("spawn child OOPIF process");
    assert_ne!(parent_site_id, child_site_id);

    let parent_pid = supervisor.child_pid(parent_site_id).expect("parent pid");
    let child_pid = supervisor.child_pid(child_site_id).expect("child pid");
    assert_ne!(parent_pid, child_pid);

    let parent_pipe = PipelineId(10);
    let child_pipe = PipelineId(20);
    supervisor
        .register_pipeline(parent_site_id, parent_pipe)
        .expect("reg parent");
    supervisor
        .register_pipeline(child_site_id, child_pipe)
        .expect("reg child");

    // 2. Create parent pipeline & load HTML containing a cross-origin <iframe>.
    supervisor
        .send_to_site(
            parent_site_id,
            &BrowserToContent::CreatePipeline {
                pipeline: parent_pipe,
                tab: TabId(1),
                viewport: ViewportMetrics {
                    logical_size: Size::new(240.0, 160.0),
                    device_pixel_ratio: 1.0,
                },
            },
        )
        .await
        .expect("create parent pipeline");
    assert_eq!(
        supervisor.recv_from_site(parent_site_id).await.unwrap(),
        Some(ContentToBrowser::PipelineReady {
            pipeline: parent_pipe
        })
    );
    supervisor.mark_pipeline_running(parent_pipe);

    let parent_html = r#"<!doctype html>
    <html>
    <head>
        <title>ParentAlive</title>
        <style>
            html, body { margin: 0; padding: 0; width: 240px; height: 160px; background: #143cc8; }
            iframe { display: block; margin-left: 40px; margin-top: 30px; border: 0; }
        </style>
    </head>
    <body>
        <iframe id="widget" src="https://child.example.com/widget" width="160" height="100"></iframe>
        <script>
            window.replies = [];
            window.addEventListener("message", function(e) {
                window.replies.push("win:" + e.origin + ":" + e.data);
            });
        </script>
    </body>
    </html>"#;

    supervisor
        .send_to_site(
            parent_site_id,
            &BrowserToContent::LoadDocument {
                pipeline: parent_pipe,
                url: "https://parent.example.org/".to_string(),
                html: parent_html.to_string(),
                sandbox: None,
            },
        )
        .await
        .expect("load parent html");

    // Parent OS process MUST emit CreateChildFrame for the cross-origin iframe, then LoadStateChanged(Complete).
    let (frame_node_id, iframe_surface_id, child_url) = match supervisor
        .recv_from_site(parent_site_id)
        .await
        .expect("recv CreateChildFrame")
        .expect("some")
    {
        ContentToBrowser::CreateChildFrame {
            parent_pipeline,
            frame_node_id,
            surface_id,
            url,
            ..
        } => {
            assert_eq!(parent_pipeline, parent_pipe);
            (frame_node_id, surface_id, url)
        }
        other => panic!("expected CreateChildFrame, got {other:?}"),
    };
    assert_eq!(child_url, "https://child.example.com/widget");
    assert_eq!(
        supervisor.recv_from_site(parent_site_id).await.unwrap(),
        Some(ContentToBrowser::LoadStateChanged {
            pipeline: parent_pipe,
            state: LoadState::Complete
        })
    );

    // 3. Create child OOPIF pipeline in Process B and bind it to parent's iframe!
    supervisor
        .send_to_site(
            child_site_id,
            &BrowserToContent::CreatePipeline {
                pipeline: child_pipe,
                tab: TabId(1),
                viewport: ViewportMetrics {
                    logical_size: Size::new(160.0, 100.0),
                    device_pixel_ratio: 1.0,
                },
            },
        )
        .await
        .expect("create child pipeline");
    assert_eq!(
        supervisor.recv_from_site(child_site_id).await.unwrap(),
        Some(ContentToBrowser::PipelineReady {
            pipeline: child_pipe
        })
    );
    supervisor.mark_pipeline_running(child_pipe);

    let child_html = r#"<!doctype html>
    <html>
    <head>
        <style>html, body { margin: 0; padding: 0; width: 160px; height: 100px; background: #10c85a; }</style>
    </head>
    <body>
        <script>
            window.addEventListener("message", function(e) {
                e.source.postMessage("echo:" + e.data.cmd, e.origin);
                if (e.ports && e.ports.length === 1) {
                    var p = e.ports[0];
                    p.onmessage = function(pe) {
                        p.postMessage("port-reply:" + pe.data);
                    };
                }
            });
        </script>
    </body>
    </html>"#;

    supervisor
        .send_to_site(
            child_site_id,
            &BrowserToContent::LoadDocument {
                pipeline: child_pipe,
                url: child_url.clone(),
                html: child_html.to_string(),
                sandbox: None,
            },
        )
        .await
        .expect("load child html");
    assert_eq!(
        supervisor.recv_from_site(child_site_id).await.unwrap(),
        Some(ContentToBrowser::LoadStateChanged {
            pipeline: child_pipe,
            state: LoadState::Complete
        })
    );

    supervisor
        .send_to_site(
            parent_site_id,
            &BrowserToContent::ChildFrameBound {
                parent_pipeline: parent_pipe,
                frame_node_id,
                child_pipeline: child_pipe,
                surface_id: iframe_surface_id,
                origin: "https://child.example.com".to_string(),
                is_cross_origin: true,
            },
        )
        .await
        .expect("bind child frame");

    // 4. Exercise cross-process `window.postMessage` + transferred `MessageChannel` `MessagePort`!
    supervisor
        .send_to_site(
            parent_site_id,
            &BrowserToContent::EvalScript {
                pipeline: parent_pipe,
                seq: 1,
                source: r#"
                    window.ch = new MessageChannel();
                    window.ch.port1.onmessage = function(e) {
                        window.replies.push("port:" + e.data);
                    };
                    var f = document.getElementById("widget");
                    f.contentWindow.postMessage({ cmd: "hello-oopif" }, "https://child.example.com", [window.ch.port2]);
                    window.ch.port1.postMessage("ping-over-ipc");
                    "sent"
                "#
                .to_string(),
            },
        )
        .await
        .expect("eval postMessage in parent");

    // Route Parent -> Child PostMessageToFrame and MessagePortSend over Browser IPC!
    let mut forwarded_msgs = Vec::new();
    loop {
        let msg = supervisor
            .recv_from_site(parent_site_id)
            .await
            .expect("recv parent")
            .expect("some");
        match msg {
            ContentToBrowser::ScriptResult {
                seq: 1, ok, value, ..
            } => {
                assert!(ok && value == "sent", "parent script failed: {value}");
                break;
            }
            other => forwarded_msgs.push(other),
        }
    }
    assert_eq!(forwarded_msgs.len(), 2);

    for m in forwarded_msgs {
        match m {
            ContentToBrowser::PostMessageToFrame {
                source_pipeline,
                target_pipeline,
                data_json,
                source_origin,
                transfer_ports,
                ..
            } => {
                supervisor
                    .send_to_site(
                        child_site_id,
                        &BrowserToContent::DeliverPostMessage {
                            target_pipeline,
                            source_pipeline,
                            data_json,
                            origin: source_origin,
                            transfer_ports,
                        },
                    )
                    .await
                    .expect("deliver postMessage to child");
            }
            ContentToBrowser::MessagePortSend {
                port_id,
                data_json,
                transfer_ports,
                ..
            } => {
                supervisor
                    .send_to_site(
                        child_site_id,
                        &BrowserToContent::DeliverPortMessage {
                            target_pipeline: child_pipe,
                            port_id,
                            data_json,
                            transfer_ports,
                        },
                    )
                    .await
                    .expect("deliver port msg to child");
            }
            other => panic!("unexpected parent IPC: {other:?}"),
        }
    }

    // Receive Child -> Parent replies (1 window.postMessage echo + 1 MessagePort reply)!
    let reply1 = supervisor
        .recv_from_site(child_site_id)
        .await
        .expect("recv child reply1")
        .expect("some");
    let reply2 = supervisor
        .recv_from_site(child_site_id)
        .await
        .expect("recv child reply2")
        .expect("some");

    for reply in [reply1, reply2] {
        match reply {
            ContentToBrowser::PostMessageToFrame {
                source_pipeline,
                target_pipeline,
                data_json,
                source_origin,
                transfer_ports,
                ..
            } => {
                supervisor
                    .send_to_site(
                        parent_site_id,
                        &BrowserToContent::DeliverPostMessage {
                            target_pipeline,
                            source_pipeline,
                            data_json,
                            origin: source_origin,
                            transfer_ports,
                        },
                    )
                    .await
                    .expect("deliver child postMessage to parent");
            }
            ContentToBrowser::MessagePortSend {
                port_id,
                data_json,
                transfer_ports,
                ..
            } => {
                supervisor
                    .send_to_site(
                        parent_site_id,
                        &BrowserToContent::DeliverPortMessage {
                            target_pipeline: parent_pipe,
                            port_id,
                            data_json,
                            transfer_ports,
                        },
                    )
                    .await
                    .expect("deliver child port msg to parent");
            }
            other => panic!("unexpected child reply: {other:?}"),
        }
    }

    // Verify Parent OS process received BOTH cross-process replies!
    supervisor
        .send_to_site(
            parent_site_id,
            &BrowserToContent::EvalScript {
                pipeline: parent_pipe,
                seq: 2,
                source: "window.replies.join('|')".to_string(),
            },
        )
        .await
        .expect("read replies");
    match supervisor
        .recv_from_site(parent_site_id)
        .await
        .expect("recv replies")
        .expect("some")
    {
        ContentToBrowser::ScriptResult {
            seq: 2, ok, value, ..
        } => {
            assert!(ok);
            assert!(
                value.contains("win:https://child.example.com:echo:hello-oopif")
                    && value.contains("port:port-reply:ping-over-ipc"),
                "unexpected cross-process replies: {value}"
            );
        }
        other => panic!("expected ScriptResult(2), got {other:?}"),
    }

    // 5. Request real DisplayLists from BOTH OS processes and composite via ExternalSurface!
    supervisor
        .send_to_site(
            parent_site_id,
            &BrowserToContent::RequestDisplayList {
                pipeline: parent_pipe,
                epoch: 1,
            },
        )
        .await
        .expect("req parent dl");
    supervisor
        .send_to_site(
            child_site_id,
            &BrowserToContent::RequestDisplayList {
                pipeline: child_pipe,
                epoch: 1,
            },
        )
        .await
        .expect("req child dl");

    let parent_dl = match supervisor
        .recv_from_site(parent_site_id)
        .await
        .unwrap()
        .unwrap()
    {
        ContentToBrowser::DisplayListReady { bytes, .. } => {
            DisplayList::from_bytes(&bytes).expect("decode parent dl")
        }
        other => panic!("expected parent DisplayListReady, got {other:?}"),
    };
    let child_dl = match supervisor
        .recv_from_site(child_site_id)
        .await
        .unwrap()
        .unwrap()
    {
        ContentToBrowser::DisplayListReady { bytes, .. } => {
            DisplayList::from_bytes(&bytes).expect("decode child dl")
        }
        other => panic!("expected child DisplayListReady, got {other:?}"),
    };

    let mut compositor = Compositor::new(Size::new(240.0, 160.0), 1.0);
    compositor.submit_display_list(parent_pipe, LayerRole::WebSurface, 1, parent_dl);
    compositor.submit_display_list(child_pipe, LayerRole::Subframe, 1, child_dl);
    compositor.bind_external_surface_pipeline(iframe_surface_id, child_pipe, 160, 100);

    let live_rgba = render_rgba(&mut compositor, 240, 160);
    let parent_px = pixel_at(&live_rgba, 240, 10, 10);
    let child_px = pixel_at(&live_rgba, 240, 120, 80);
    assert!(
        parent_px[2] > 180 && parent_px[1] < 90,
        "parent blue pixel expected: {parent_px:?}"
    );
    assert!(
        child_px[1] > 180 && child_px[0] < 40,
        "child OOPIF emerald pixel expected inside iframe: {child_px:?}"
    );

    // 6. SIGKILL the child OOPIF OS process! Verify inline crashed-frame UI & parent survival!
    let kill_status = std::process::Command::new("kill")
        .args(["-9", &child_pid.to_string()])
        .status()
        .expect("invoke kill -9 on child OOPIF");
    assert!(kill_status.success());

    let crash_event = tokio::time::timeout(Duration::from_secs(5), supervisor.next_event())
        .await
        .expect("crash notice timeout")
        .expect("crash event");
    match crash_event {
        SupervisorEvent::ProcessCrashed {
            site_id,
            affected_pipelines,
            signal,
            ..
        } => {
            assert_eq!(site_id, child_site_id);
            assert_eq!(affected_pipelines, vec![child_pipe]);
            assert_eq!(signal, Some(9));
        }
    }
    assert!(matches!(
        supervisor.pipeline_health(child_pipe),
        Some(PipelineHealth::Crashed {
            signal: Some(9),
            ..
        })
    ));
    assert_eq!(
        supervisor.pipeline_health(parent_pipe),
        Some(PipelineHealth::Running),
        "parent pipeline must remain completely healthy after child OOPIF SIGKILL"
    );

    // Notify parent & compositor of child OOPIF crash -> inline crashed frame rendered!
    let affected_surfaces: Vec<SurfaceId> =
        compositor.mark_pipeline_crashed(child_pipe, "Subframe crashed");
    assert_eq!(affected_surfaces, vec![iframe_surface_id]);
    assert!(compositor.external_surface_is_crashed(iframe_surface_id));

    let crashed_rgba = render_rgba(&mut compositor, 240, 160);
    let parent_after = pixel_at(&crashed_rgba, 240, 10, 10);
    let crash_badge = pixel_at(&crashed_rgba, 240, 120, 80);
    assert!(
        parent_after[2] > 180 && parent_after[1] < 90,
        "parent document pixels must remain intact: {parent_after:?}"
    );
    assert!(
        crash_badge[0] > 180,
        "inline crashed-frame red alert badge must render inside dead iframe: {crash_badge:?}"
    );

    // Confirm parent OS process is STILL alive, executes JS, and responds to IPC Ping/Pong!
    supervisor
        .send_to_site(
            parent_site_id,
            &BrowserToContent::EvalScript {
                pipeline: parent_pipe,
                seq: 3,
                source: "document.title + ':' + window.replies.length".to_string(),
            },
        )
        .await
        .expect("eval in surviving parent");
    match supervisor
        .recv_from_site(parent_site_id)
        .await
        .expect("recv surviving parent")
        .expect("some")
    {
        ContentToBrowser::ScriptResult {
            seq: 3, ok, value, ..
        } => {
            assert!(ok);
            assert_eq!(value, "ParentAlive:2");
        }
        other => panic!("expected ScriptResult(3), got {other:?}"),
    }

    supervisor.shutdown_all().await;
    assert_eq!(supervisor.active_site_count(), 0);
}

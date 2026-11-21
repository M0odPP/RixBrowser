#![cfg(unix)]

use rix_core::{RixUrl, Size};
use rix_ipc::{
    BrowserToContent, ContentToBrowser, PipelineHealth, PipelineId, ProcessSupervisor,
    SealedShmemBlob, SiteKey, SupervisorEvent, TabId, ViewportMetrics,
};
use std::os::fd::{AsFd, AsRawFd};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const RIX_APP_BIN: &str = env!("CARGO_BIN_EXE_rix-app");

fn open_fd_count() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .expect("read /proc/self/fd")
        .count()
}

#[tokio::test]
async fn content_process_1000_frames_and_sealed_shmem_latency() {
    let runtime_dir = TempDir::new().expect("tempdir");
    let mut supervisor = ProcessSupervisor::new(RIX_APP_BIN, runtime_dir.path());

    let site = SiteKey::from_url(&RixUrl::parse("https://example.org/app").expect("url"), 1);
    let site_id = supervisor
        .ensure_content_process(site.clone())
        .await
        .expect("spawn content");
    assert_eq!(
        supervisor
            .ensure_content_process(site)
            .await
            .expect("reuse"),
        site_id
    );

    let pipeline = PipelineId(1);
    supervisor
        .register_pipeline(site_id, pipeline)
        .expect("register");
    supervisor
        .send_to_site(
            site_id,
            &BrowserToContent::CreatePipeline {
                pipeline,
                tab: TabId(1),
                viewport: ViewportMetrics {
                    logical_size: Size::new(1280.0, 720.0),
                    device_pixel_ratio: 1.0,
                },
            },
        )
        .await
        .expect("create pipeline");

    let ready = supervisor
        .recv_from_site(site_id)
        .await
        .expect("recv")
        .expect("some");
    assert_eq!(ready, ContentToBrowser::PipelineReady { pipeline });
    supervisor.mark_pipeline_running(pipeline);
    assert_eq!(
        supervisor.pipeline_health(pipeline),
        Some(PipelineHealth::Running)
    );

    // Send a sealed shared-memory blob over SCM_RIGHTS attached to a Ping message.
    let blob_payload = vec![0xCDu8; 32 * 1024];
    let blob = SealedShmemBlob::from_bytes(&blob_payload).expect("blob");
    let blob_fd = blob.try_clone_fd().expect("clone fd");
    supervisor
        .send_with_fds_to_site(
            site_id,
            &BrowserToContent::Ping { seq: 0 },
            &[blob_fd.as_fd()],
        )
        .await
        .expect("send_with_fds");
    let pong0 = supervisor
        .recv_from_site(site_id)
        .await
        .expect("recv")
        .expect("pong");
    assert_eq!(pong0, ContentToBrowser::Pong { seq: 0 });
    drop(blob_fd);

    // Measure 1,000 binary round-trip frames against the real child process.
    let mut latencies_us = Vec::with_capacity(1000);
    for seq in 1..=1000u64 {
        let start = Instant::now();
        supervisor
            .send_to_site(site_id, &BrowserToContent::Ping { seq })
            .await
            .expect("send ping");
        let reply = supervisor
            .recv_from_site(site_id)
            .await
            .expect("recv pong")
            .expect("some");
        assert_eq!(reply, ContentToBrowser::Pong { seq });
        latencies_us.push(start.elapsed().as_secs_f64() * 1_000_000.0);
    }

    latencies_us.sort_by(|a, b| a.total_cmp(b));
    let p50_us = latencies_us[500];
    let p99_us = latencies_us[990];
    eprintln!("rix-app IPC 1000-frame latency: p50={p50_us:.1}us p99={p99_us:.1}us");
    assert!(
        p99_us < 5_000.0,
        "p99 IPC round-trip exceeded 5ms: {p99_us:.1}us"
    );

    supervisor.shutdown_all().await;
    assert_eq!(supervisor.active_site_count(), 0);
}

#[tokio::test]
async fn site_isolation_and_sigkill_crash_recovery() {
    let runtime_dir = TempDir::new().expect("tempdir");
    let mut supervisor = ProcessSupervisor::new(RIX_APP_BIN, runtime_dir.path());

    let site_a = SiteKey::from_url(&RixUrl::parse("https://a.example.org/").expect("url"), 1);
    let site_a_sub = SiteKey::from_url(&RixUrl::parse("https://sub.example.org/").expect("url"), 2);
    let site_b = SiteKey::from_url(&RixUrl::parse("https://b.example.com/").expect("url"), 3);

    let id_a = supervisor
        .ensure_content_process(site_a.clone())
        .await
        .expect("spawn A");
    let id_a_sub = supervisor
        .ensure_content_process(site_a_sub)
        .await
        .expect("reuse A");
    let id_b = supervisor
        .ensure_content_process(site_b.clone())
        .await
        .expect("spawn B");

    assert_eq!(id_a, id_a_sub);
    assert_ne!(id_a, id_b);
    assert_eq!(supervisor.active_site_count(), 2);

    let pid_a = supervisor.child_pid(id_a).expect("pid A");
    let pid_b = supervisor.child_pid(id_b).expect("pid B");
    assert_ne!(pid_a, pid_b);

    let pipe_a = PipelineId(101);
    let pipe_b = PipelineId(202);
    supervisor.register_pipeline(id_a, pipe_a).expect("reg A");
    supervisor.register_pipeline(id_b, pipe_b).expect("reg B");
    supervisor.mark_pipeline_running(pipe_a);
    supervisor.mark_pipeline_running(pipe_b);

    // Crash site A with SIGKILL; sibling site B must remain unaffected.
    let kill_status = std::process::Command::new("kill")
        .args(["-9", &pid_a.to_string()])
        .status()
        .expect("invoke kill -9");
    assert!(kill_status.success());

    let event = tokio::time::timeout(Duration::from_secs(5), supervisor.next_event())
        .await
        .expect("timeout waiting for crash event")
        .expect("crash event");
    assert_eq!(
        event,
        SupervisorEvent::ProcessCrashed {
            site_id: id_a,
            site: site_a.clone(),
            affected_pipelines: vec![pipe_a],
            exit_code: None,
            signal: Some(9),
        }
    );
    assert_eq!(
        supervisor.pipeline_health(pipe_a),
        Some(PipelineHealth::Crashed {
            exit_code: None,
            signal: Some(9),
        })
    );
    assert_eq!(
        supervisor.pipeline_health(pipe_b),
        Some(PipelineHealth::Running)
    );

    // Verify site B is still responsive.
    supervisor
        .send_to_site(id_b, &BrowserToContent::Ping { seq: 77 })
        .await
        .expect("send B");
    assert_eq!(
        supervisor
            .recv_from_site(id_b)
            .await
            .expect("recv B")
            .expect("some"),
        ContentToBrowser::Pong { seq: 77 }
    );

    // Reload site A cleanly into a new Content Process.
    let recovered_id_a = supervisor
        .ensure_content_process(site_a)
        .await
        .expect("respawn A");
    assert_ne!(recovered_id_a, id_a);
    supervisor
        .send_to_site(recovered_id_a, &BrowserToContent::Ping { seq: 88 })
        .await
        .expect("send recovered A");
    assert_eq!(
        supervisor
            .recv_from_site(recovered_id_a)
            .await
            .expect("recv recovered A")
            .expect("some"),
        ContentToBrowser::Pong { seq: 88 }
    );

    supervisor.shutdown_all().await;
}

#[tokio::test]
async fn spawn_and_shutdown_20_children_no_fd_leaks() {
    let runtime_dir = TempDir::new().expect("tempdir");
    let mut supervisor = ProcessSupervisor::new(RIX_APP_BIN, runtime_dir.path());

    // Warm up one cycle so Tokio/stdio lazy descriptors settle before baseline check.
    let warmup_site = SiteKey::Opaque(0);
    let warmup_id = supervisor
        .ensure_content_process(warmup_site)
        .await
        .expect("warmup");
    supervisor
        .shutdown_site(warmup_id)
        .await
        .expect("shutdown warmup");

    let before = open_fd_count();
    let probe = SealedShmemBlob::from_bytes(b"fd-baseline").expect("probe");
    let baseline_fd = probe.as_fd().as_raw_fd();
    drop(probe);

    for i in 1..=20u64 {
        let id = supervisor
            .ensure_content_process(SiteKey::Opaque(i))
            .await
            .expect("spawn child");
        supervisor
            .send_to_site(id, &BrowserToContent::Ping { seq: i })
            .await
            .expect("ping");
        assert_eq!(
            supervisor.recv_from_site(id).await.expect("pong"),
            Some(ContentToBrowser::Pong { seq: i })
        );
        supervisor.shutdown_site(id).await.expect("shutdown child");
    }

    let probe_after = SealedShmemBlob::from_bytes(b"fd-after").expect("probe");
    let after_fd = probe_after.as_fd().as_raw_fd();
    drop(probe_after);
    let after = open_fd_count();

    assert!(
        after_fd - baseline_fd < 10,
        "lowest available FD drifted across 20 spawn/shutdown cycles: {baseline_fd} -> {after_fd}"
    );
    assert!(
        after <= before + 4,
        "FD count grew across 20 spawn/shutdown cycles: before={before}, after={after}"
    );
}

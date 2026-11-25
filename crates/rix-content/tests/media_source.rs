use rix_content::ContentRuntime;
use rix_media::demuxer::mp4::{mux_fmp4_init_segment, mux_fmp4_media_segment, Fmp4SampleInput};
use rix_media::demuxer::webm::{mux_webm_cluster, mux_webm_init_segment, WebmBlockInput};
use rix_media::{TrackInfo, TrackKind, VideoCodec, VideoTrackConfig};

fn js_byte_array_literal(bytes: &[u8]) -> String {
    let mut s = String::from("new Uint8Array([");
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        use std::fmt::Write;
        let _ = write!(&mut s, "{b}");
    }
    s.push_str("])");
    s
}

#[test]
fn mse_seamless_change_type_mp4_to_webm_and_gc_safety() {
    let mut rt = ContentRuntime::with_empty_document();

    let mp4_track = TrackInfo {
        id: 1,
        kind: TrackKind::Video(VideoTrackConfig {
            codec: VideoCodec::H264,
            width: 640,
            height: 360,
        }),
        timescale: 1_000,
        duration_us: None,
        language: None,
        extra_data: vec![0x01, 0x64, 0x00, 0x1e, 0xff, 0xe1, 0x00, 0x02, 0x67, 0x64],
    };
    let mp4_init = mux_fmp4_init_segment(&[mp4_track], 1_000);
    let mp4_seg = mux_fmp4_media_segment(
        1,
        1,
        0,
        &[Fmp4SampleInput {
            data: &[0, 0, 0, 2, 0x65, 0x88],
            duration_ticks: 100,
            composition_offset_ticks: 0,
            is_keyframe: true,
        }],
    );

    let webm_track = TrackInfo {
        id: 1,
        kind: TrackKind::Video(VideoTrackConfig {
            codec: VideoCodec::Vp9,
            width: 1280,
            height: 720,
        }),
        timescale: 1_000,
        duration_us: None,
        language: None,
        extra_data: Vec::new(),
    };
    let webm_init = mux_webm_init_segment(&[webm_track], Some(200.0));
    let webm_cluster = mux_webm_cluster(
        100,
        &[WebmBlockInput {
            track_id: 1,
            rel_timestamp_ms: 0,
            is_keyframe: true,
            data: &[0x82, 0x49, 0x83, 0x42, 0x00, 0x01],
        }],
    );
    let webm_combined = [webm_init, webm_cluster].concat();

    let script = format!(
        r#"
        var mp4Init = {};
        var mp4Seg = {};
        var webmChunk = {};
        var ms = new MediaSource();
        var video = document.createElement("video");
        var url = URL.createObjectURL(ms);
        var done = "";
        var sb = null;
        ms.addEventListener("sourceopen", function() {{
            sb = ms.addSourceBuffer('video/mp4; codecs="avc1.640028"');
            var step = 0;
            sb.addEventListener("updateend", function() {{
                step++;
                if (step === 1) {{
                    sb.appendBuffer(mp4Seg);
                }} else if (step === 2) {{
                    sb.changeType('video/webm; codecs="vp9"');
                    sb.appendBuffer(webmChunk);
                }} else if (step === 3) {{
                    ms.endOfStream();
                    done = sb.buffered.length + "|" + sb.buffered.start(0).toFixed(2) + "|" + sb.buffered.end(0).toFixed(2) + "|" + ms.readyState;
                }}
            }});
            sb.appendBuffer(mp4Init);
        }});
        video.src = url;
        "#,
        js_byte_array_literal(&mp4_init),
        js_byte_array_literal(&mp4_seg),
        js_byte_array_literal(&webm_combined),
    );

    rt.eval_script(&script).expect("eval MSE script");
    // Run GC mid-flight to verify MSE roots survive collection.
    let _ = rt.collect_garbage();
    for _ in 0..12 {
        rt.step_once();
    }
    let _ = rt.collect_garbage();

    let done = rt.eval_script("done").expect("read done");
    assert_eq!(rt.vm.value_to_rust_string(done), "1|0.00|0.13|ended");

    // Verify codec_generation switched seamlessly from 1 (H.264 fMP4) to 2 (VP9 WebM).
    let ms_sys = rt.media_source.borrow();
    let entry = ms_sys.media_sources.values().next().expect("MediaSource");
    let sb_eng = &entry.engine.source_buffers[0];
    assert_eq!(sb_eng.frames.len(), 2);
    assert_eq!(sb_eng.frames[0].codec_generation, 1);
    assert_eq!(sb_eng.frames[1].codec_generation, 2);
}

#[test]
fn mse_quota_eviction_and_quota_exceeded_error() {
    let mut rt = ContentRuntime::with_empty_document();

    let mp4_track = TrackInfo {
        id: 1,
        kind: TrackKind::Video(VideoTrackConfig {
            codec: VideoCodec::H264,
            width: 320,
            height: 240,
        }),
        timescale: 1_000,
        duration_us: None,
        language: None,
        extra_data: Vec::new(),
    };
    let init = mux_fmp4_init_segment(&[mp4_track], 1_000);
    let payload_300 = vec![0x65u8; 300];
    let seg1 = mux_fmp4_media_segment(
        1,
        1,
        0,
        &[Fmp4SampleInput {
            data: &payload_300,
            duration_ticks: 100,
            composition_offset_ticks: 0,
            is_keyframe: true,
        }],
    );
    let seg2 = mux_fmp4_media_segment(
        2,
        1,
        100,
        &[Fmp4SampleInput {
            data: &payload_300,
            duration_ticks: 100,
            composition_offset_ticks: 0,
            is_keyframe: true,
        }],
    );
    let seg3 = mux_fmp4_media_segment(
        3,
        1,
        200,
        &[Fmp4SampleInput {
            data: &payload_300,
            duration_ticks: 100,
            composition_offset_ticks: 0,
            is_keyframe: true,
        }],
    );

    // 1. Open MediaSource and create SourceBuffer.
    rt.eval_script(
        r#"
        var ms = new MediaSource();
        var video = document.createElement("video");
        var sb = null;
        ms.onsourceopen = function() {
            sb = ms.addSourceBuffer('video/mp4; codecs="avc1.640028"');
        };
        video.srcObject = ms;
        "#,
    )
    .expect("setup");
    for _ in 0..4 {
        rt.step_once();
    }

    // Configure a 700-byte quota so seg1+seg2 fit (600 bytes), seg3 triggers eviction of seg1,
    // and an oversized 900-byte single append throws QuotaExceededError synchronously.
    let sb_val = rt.eval_script("sb").expect("get sb");
    let sb_h = sb_val.as_object().expect("sb object");
    assert!(rt
        .media_source
        .borrow_mut()
        .set_source_buffer_quota(sb_h, 700));

    let append_script = format!(
        r#"
        var initBuf = {};
        var seg1 = {};
        var seg2 = {};
        var seg3 = {};
        var quotaErr = "";
        try {{ sb.appendBuffer(new Uint8Array(900)); }} catch (e) {{ quotaErr = e.name; }}
        var step = 0;
        var summary = "";
        sb.onupdateend = function() {{
            step++;
            if (step === 1) sb.appendBuffer(seg1);
            else if (step === 2) sb.appendBuffer(seg2);
            else if (step === 3) {{ video.currentTime = 0.15; sb.appendBuffer(seg3); }}
            else if (step === 4) {{
                summary = quotaErr + "|" + sb.buffered.length + "|" + sb.buffered.start(0).toFixed(2) + "|" + sb.buffered.end(0).toFixed(2);
            }}
        }};
        sb.appendBuffer(initBuf);
        "#,
        js_byte_array_literal(&init),
        js_byte_array_literal(&seg1),
        js_byte_array_literal(&seg2),
        js_byte_array_literal(&seg3),
    );
    rt.eval_script(&append_script).expect("run quota test");
    for _ in 0..16 {
        rt.step_once();
    }

    let summary = rt.eval_script("summary").expect("read summary");
    // seg1 [0.00, 0.10] was evicted so remaining range starts at 0.10 or 0.20 and ends at 0.30!
    let s = rt.vm.value_to_rust_string(summary);
    assert!(
        s.starts_with("QuotaExceededError|1|") && s.ends_with("|0.30") && !s.contains("|0.00|"),
        "expected QuotaExceededError and evicted oldest frame, got `{s}`"
    );
}

use std::f32::consts::PI;
use std::sync::Mutex;

use rix_chrome::DevHudState;
use rix_content::ContentRuntime;
use rix_core::Size;
use rix_ipc::PipelineId;
use rix_media::demuxer::mp4::{mux_fmp4_init_segment, mux_fmp4_media_segment, Fmp4SampleInput};
use rix_media::{AudioCodec, AudioTrackConfig, TrackInfo, TrackKind, VideoCodec, VideoTrackConfig};
use rix_render::{Color, Compositor, VelloRenderer};

static GPU_LOCK: Mutex<()> = Mutex::new(());

fn js_u8_array(bytes: &[u8]) -> String {
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

fn make_solid_yuv420_bt709(
    width: usize,
    height: usize,
    r: u8,
    g: u8,
    b: u8,
) -> rusty_h264::YuvFrame {
    let rf = f64::from(r) / 255.0;
    let gf = f64::from(g) / 255.0;
    let bf = f64::from(b) / 255.0;
    let y = 0.2126 * rf + 0.7152 * gf + 0.0722 * bf;
    let cb = 0.5 * (bf - y) / (1.0 - 0.0722);
    let cr = 0.5 * (rf - y) / (1.0 - 0.2126);
    let y8 = (16.0 + 219.0 * y).round().clamp(0.0, 255.0) as u8;
    let u8_val = (128.0 + 224.0 * cb).round().clamp(0.0, 255.0) as u8;
    let v8_val = (128.0 + 224.0 * cr).round().clamp(0.0, 255.0) as u8;
    let cw = width / 2;
    let ch = height / 2;
    rusty_h264::YuvFrame {
        width,
        height,
        y: vec![y8; width * height],
        u: vec![u8_val; cw * ch],
        v: vec![v8_val; cw * ch],
    }
}

fn build_wav_440hz(sample_rate: u32, channels: u16, frames: usize) -> Vec<u8> {
    let data_bytes = frames * usize::from(channels) * 2;
    let mut out = Vec::with_capacity(44 + data_bytes);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data_bytes) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * u32::from(channels) * 2).to_le_bytes());
    out.extend_from_slice(&(channels * 2).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_bytes as u32).to_le_bytes());
    for i in 0..frames {
        let s = (2.0 * PI * 440.0 * i as f32 / sample_rate as f32).sin() * 0.5;
        let q = (s * 32_767.0).round() as i16;
        for _ in 0..channels {
            out.extend_from_slice(&q.to_le_bytes());
        }
    }
    out
}

#[test]
fn html5_video_mse_h264_zero_copy_gpu_sync_and_hud_telemetry() {
    let _guard = GPU_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // 1. Encode a real 6-frame H.264 stream (frames 0..3 red, frames 3..6 blue) at 60 FPS!
    let mut cfg = rusty_h264::EncoderConfig::baseline(96, 64);
    cfg.qp = 18;
    let enc = rusty_h264::Encoder::new(cfg).expect("H.264 encoder");
    let source_yuv: Vec<rusty_h264::YuvFrame> = (0..6)
        .map(|i| {
            if i < 3 {
                make_solid_yuv420_bt709(96, 64, 225, 40, 45)
            } else {
                make_solid_yuv420_bt709(96, 64, 40, 80, 230)
            }
        })
        .collect();
    let aus = enc.encode_all(&source_yuv).expect("encode H.264 stream");
    assert_eq!(aus.len(), 6);

    // Mux into real ISO BMFF fMP4 init + 60 FPS video segment + AAC-LC audio segment.
    let v_track = TrackInfo {
        id: 1,
        kind: TrackKind::Video(VideoTrackConfig {
            codec: VideoCodec::H264,
            width: 96,
            height: 64,
        }),
        timescale: 60_000,
        duration_us: None,
        language: None,
        extra_data: Vec::new(),
    };
    let a_track = TrackInfo {
        id: 2,
        kind: TrackKind::Audio(AudioTrackConfig {
            codec: AudioCodec::Aac,
            sample_rate: 48_000,
            channels: 2,
            pcm_format: None,
        }),
        timescale: 48_000,
        duration_us: None,
        language: None,
        extra_data: vec![0x11, 0x90],
    };
    let init = mux_fmp4_init_segment(&[v_track, a_track], 1_000);
    let v_samples: Vec<Fmp4SampleInput<'_>> = aus
        .iter()
        .enumerate()
        .map(|(i, au)| Fmp4SampleInput {
            data: au,
            duration_ticks: 1_000, // 1000 / 60000 = 16.667 ms (60 FPS!)
            composition_offset_ticks: 0,
            is_keyframe: i == 0,
        })
        .collect();
    let v_seg = mux_fmp4_media_segment(1, 1, 0, &v_samples);
    let aac_silent = [0x21, 0x10, 0x04, 0x60, 0x8c, 0x1c];
    let a_seg = mux_fmp4_media_segment(
        2,
        2,
        0,
        &[Fmp4SampleInput {
            data: &aac_silent,
            duration_ticks: 1_024,
            composition_offset_ticks: 0,
            is_keyframe: true,
        }],
    );

    // 2. Initialize ContentRuntime + headless VelloRenderer + Compositor sharing the same wgpu::Device.
    let mut renderer = VelloRenderer::new();
    let shared = renderer
        .ensure_shared_gpu_device_blocking()
        .expect("headless wgpu device");
    let mut compositor = Compositor::new(Size::new(160.0, 120.0), 1.0);
    let mut rt = ContentRuntime::with_empty_document();
    rt.attach_shared_gpu(shared);

    let html = r#"<!doctype html><html><body style="margin:0;background:#000;"><video id="v" width="96" height="64"></video></body></html>"#;
    rt.load_html_with_scripts(html).expect("load HTML");

    // Initial DisplayList submit (contains `<video>` ExternalSurface).
    let (dl_rebuilt0, _) =
        rt.sync_frame_to_compositor(PipelineId(1), 1, &mut compositor, Some(&mut renderer));
    assert!(dl_rebuilt0, "initial layout should build DisplayList once");

    // 3. Drive MSE + `<video>.play()` from JavaScript.
    let script = format!(
        r#"
        var initBytes = {};
        var vSeg = {};
        var aSeg = {};
        var v = document.getElementById("v");
        var ms = new MediaSource();
        var playEvents = [];
        v.addEventListener("play", function() {{ playEvents.push("play"); }});
        ms.addEventListener("sourceopen", function() {{
            var sb = ms.addSourceBuffer('video/mp4; codecs="avc1.640028, mp4a.40.2"');
            var step = 0;
            sb.addEventListener("updateend", function() {{
                step++;
                if (step === 1) sb.appendBuffer(vSeg);
                else if (step === 2) sb.appendBuffer(aSeg);
                else if (step === 3) {{
                    ms.endOfStream();
                    v.play();
                }}
            }});
            sb.appendBuffer(initBytes);
        }});
        v.src = URL.createObjectURL(ms);
        "#,
        js_u8_array(&init),
        js_u8_array(&v_seg),
        js_u8_array(&a_seg),
    );
    rt.eval_script(&script).expect("eval video MSE script");
    for _ in 0..16 {
        rt.step_once();
    }
    // Flush the one-time DOM attribute update from `v.src = URL.createObjectURL(ms)` before playback begins.
    let _ = rt.sync_frame_to_compositor(PipelineId(1), 2, &mut compositor, Some(&mut renderer));

    // 4. Step 60 FPS video playback across all 6 frames (800 audio samples @ 48 kHz = 16.667 ms each).
    let mut dl_rebuilds_during_video = 0u64;
    let mut first_frame_rgb = None;
    let mut last_frame_rgb = None;

    for frame_idx in 0..6u64 {
        let published = rt
            .media_elements
            .borrow_mut()
            .step_playback(&rt.media_source, 800);
        assert_eq!(
            published, 1,
            "frame {frame_idx} should be due and published to TripleBufferShmem"
        );

        let (dl_rebuilt, surfaces_presented) = rt.sync_frame_to_compositor(
            PipelineId(1),
            3 + frame_idx,
            &mut compositor,
            Some(&mut renderer),
        );
        if dl_rebuilt {
            dl_rebuilds_during_video += 1;
        }
        assert_eq!(
            surfaces_presented, 1,
            "frame {frame_idx} should present zero-copy onto GPU ExternalSurface"
        );

        if frame_idx == 0 || frame_idx == 5 {
            let scene = compositor.compose_force().clone();
            let rgba = renderer
                .render_to_rgba_blocking(&scene, 160, 120, Color::BLACK)
                .expect("composite video frame");
            let px = (32 * 160 + 48) * 4;
            let rgb = (rgba[px], rgba[px + 1], rgba[px + 2]);
            if frame_idx == 0 {
                first_frame_rgb = Some(rgb);
            } else {
                last_frame_rgb = Some(rgb);
            }
        }
    }

    assert_eq!(
        dl_rebuilds_during_video, 0,
        "video playback MUST cause ZERO DisplayList rebuilds"
    );

    // Frame 0 must be red (~225, 40, 45); frame 5 must be blue (~40, 80, 230).
    let (r0, g0, b0) = first_frame_rgb.unwrap();
    let (r5, g5, b5) = last_frame_rgb.unwrap();
    assert!(
        r0 > 180 && b0 < 90,
        "expected red first video frame, got ({r0},{g0},{b0})"
    );
    assert!(
        b5 > 180 && r5 < 90,
        "expected blue final video frame, got ({r5},{g5},{b5})"
    );

    // 5. Verify F12 Developer HUD telemetry: 60 FPS, 0 DL rebuilds, < 15 ms A/V jitter!
    let tel = rt.media_elements.borrow().telemetry();
    assert_eq!(tel.frames_presented, 6);
    assert_eq!(tel.frames_dropped, 0);
    assert!(
        tel.video_fps > 55.0,
        "expected ~60 FPS video rate, got {}",
        tel.video_fps
    );
    assert!(
        tel.av_sync_jitter_ms < 15.0,
        "expected A/V sync jitter < 15 ms, got {} ms",
        tel.av_sync_jitter_ms
    );

    let mut hud = DevHudState::new();
    hud.record_media_telemetry(
        tel.video_fps,
        tel.av_sync_jitter_ms,
        dl_rebuilds_during_video,
    );
    assert_eq!(hud.video_display_list_rebuilds, 0);
    assert!(hud.av_sync_jitter_ms < 15.0);

    // Verify JS video element state & GC safety.
    let vw = rt
        .eval_script("document.getElementById('v').videoWidth")
        .unwrap();
    let vh = rt
        .eval_script("document.getElementById('v').videoHeight")
        .unwrap();
    assert_eq!(vw.as_number(), Some(96.0));
    assert_eq!(vh.as_number(), Some(64.0));
    let _ = rt.collect_garbage();
}

#[test]
fn js_webaudio_offline_fft_biquad_gain_and_decode_audio_data() {
    let mut rt = ContentRuntime::with_empty_document();
    let wav = build_wav_440hz(44_100, 2, 1_024);

    let script = format!(
        r#"
        var wavBytes = {};
        var result = "";
        (async function() {{
            var offline = new OfflineAudioContext(2, 4096, 48000);
            var oscLow = offline.createOscillator();
            oscLow.type = "sine";
            oscLow.frequency.value = 440;

            var oscHigh = offline.createOscillator();
            oscHigh.type = "sine";
            oscHigh.frequency.value = 8000;

            var filter = offline.createBiquadFilter();
            filter.type = "lowpass";
            filter.frequency.value = 1000;
            filter.Q.value = 0;

            var gain = offline.createGain();
            gain.gain.value = 0.5;

            var analyser = offline.createAnalyser();
            analyser.fftSize = 2048;
            analyser.smoothingTimeConstant = 0;

            oscLow.connect(filter);
            oscHigh.connect(filter);
            filter.connect(gain);
            gain.connect(analyser);
            analyser.connect(offline.destination);

            oscLow.start(0);
            oscHigh.start(0);

            var rendered = await offline.startRendering();
            var freq = new Float32Array(analyser.frequencyBinCount);
            analyser.getFloatFrequencyData(freq);

            var peakBin = 0;
            var peakDb = -999;
            for (var i = 0; i < freq.length; i++) {{
                if (freq[i] > peakDb) {{
                    peakDb = freq[i];
                    peakBin = i;
                }}
            }}
            var binHz = 48000 / 2048;
            var expected440 = Math.round(440 / binHz);
            var bin8k = Math.round(8000 / binHz);
            var peakOk = Math.abs(peakBin - expected440) <= 1;
            var attenOk = (peakDb - freq[bin8k]) > 24.0;

            // Real-time AudioContext + decodeAudioData on a 44.1 kHz stereo WAV buffer
            var ctx = new AudioContext({{ sampleRate: 48000 }});
            var decoded = await ctx.decodeAudioData(wavBytes.buffer);
            var ch0 = decoded.getChannelData(0);
            var bs = ctx.createBufferSource();
            bs.buffer = decoded;
            bs.connect(ctx.destination);
            bs.start(0);
            await ctx.suspend();
            var suspState = ctx.state;
            await ctx.resume();
            var runState = ctx.state;

            result = rendered.length + "|" + rendered.numberOfChannels + "|" + peakOk + "|" + attenOk
                + "|" + decoded.sampleRate + "|" + decoded.length + "|" + (ch0.length === 1024)
                + "|" + suspState + "|" + runState;
        }})();
        "#,
        js_u8_array(&wav),
    );

    rt.eval_script(&script).expect("eval WebAudio script");
    for _ in 0..8 {
        rt.step_once();
    }
    rt.webaudio.borrow_mut().step_realtime_contexts(256);
    let _ = rt.collect_garbage();

    let out = rt.eval_script("result").expect("read result");
    assert_eq!(
        rt.vm.value_to_rust_string(out),
        "4096|2|true|true|44100|1024|true|suspended|running"
    );
}

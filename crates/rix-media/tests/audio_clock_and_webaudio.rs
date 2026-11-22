use std::f32::consts::PI;
use std::sync::Arc;

use rix_media::{
    AudioBuffer, AudioGraph, AudioNodeKind, AudioSampleQueue, AvSyncDecision, BiquadFilterType,
    MediaClock, OscillatorType, AV_SYNC_TOLERANCE_US,
};

#[test]
fn media_clock_master_sync_drift_and_jitter_under_15ms() {
    let clock = MediaClock::new(48_000);
    assert!(!clock.is_playing());
    assert_eq!(clock.current_time_us(), 0);

    // Paused clock must not advance even if callbacks occur.
    clock.advance_audio_frames(4_800);
    assert_eq!(clock.current_time_us(), 0);

    clock.play();
    // Simulate 60 FPS video frames (16.667 ms = 800 audio samples @ 48 kHz each) with <= 3 ms jitter.
    for frame in 0..60i64 {
        let video_pts_us = frame * 16_667 + if frame % 2 == 0 { 2_000 } else { -2_000 };
        match clock.evaluate_video_frame(video_pts_us) {
            AvSyncDecision::Present { drift_us } => {
                assert!(drift_us.abs() <= AV_SYNC_TOLERANCE_US);
            }
            other => panic!("frame {frame} expected Present, got {other:?}"),
        }
        clock.advance_audio_frames(800);
    }

    let (presented, dropped) = clock.frame_stats();
    assert_eq!(presented, 60);
    assert_eq!(dropped, 0);
    assert!(
        clock.jitter_ms() < 15.0,
        "expected smoothed A/V jitter < 15 ms, got {} ms",
        clock.jitter_ms()
    );
    assert!(
        clock.peak_jitter_ms() < 15.0,
        "expected peak A/V jitter < 15 ms, got {} ms",
        clock.peak_jitter_ms()
    );

    // Early frame (> +25 ms) returns Wait; late frame (< -25 ms) returns Drop.
    let now_us = clock.current_time_us();
    assert!(matches!(
        clock.evaluate_video_frame(now_us + 40_000),
        AvSyncDecision::Wait { .. }
    ));
    assert!(matches!(
        clock.evaluate_video_frame(now_us - 40_000),
        AvSyncDecision::Drop { .. }
    ));
    assert_eq!(clock.frame_stats().1, 1);

    // Verify 2.0x playbackRate doubles time progression and seek resets position accurately.
    clock.seek_seconds(10.0);
    assert_eq!(clock.current_time_us(), 10_000_000);
    clock.set_playback_rate(2.0);
    clock.advance_audio_frames(24_000); // 0.5 s real audio => +1.0 s media time at 2x
    assert_eq!(clock.current_time_us(), 11_000_000);
}

#[test]
fn webaudio_oscillator_gain_biquad_lowpass_and_analyser_fft() {
    let sr = 48_000u32;
    let mut graph = AudioGraph::new(sr, 2);

    // Two oscillators: 440 Hz (passband) and 8000 Hz (stopband above 1000 Hz lowpass cutoff).
    let osc_low = graph.create_oscillator();
    let osc_high = graph.create_oscillator();
    let filter = graph.create_biquad_filter();
    let gain = graph.create_gain();
    let analyser = graph.create_analyser();
    let dest = graph.destination();

    if let Some(AudioNodeKind::Oscillator {
        osc_type,
        frequency,
        start_time,
        ..
    }) = graph.node_mut(osc_low)
    {
        *osc_type = OscillatorType::Sine;
        frequency.set_value(440.0);
        *start_time = Some(0.0);
    }
    if let Some(AudioNodeKind::Oscillator {
        osc_type,
        frequency,
        start_time,
        ..
    }) = graph.node_mut(osc_high)
    {
        *osc_type = OscillatorType::Sine;
        frequency.set_value(8_000.0);
        *start_time = Some(0.0);
    }
    if let Some(AudioNodeKind::BiquadFilter(bf)) = graph.node_mut(filter) {
        bf.filter_type = BiquadFilterType::Lowpass;
        bf.frequency.set_value(1_000.0);
        bf.q.set_value(0.0);
    }
    if let Some(AudioNodeKind::Gain { gain: g }) = graph.node_mut(gain) {
        g.set_value(0.5);
    }
    if let Some(AudioNodeKind::Analyser(an)) = graph.node_mut(analyser) {
        an.set_fft_size(2048).unwrap();
        an.smoothing_time_constant = 0.0;
    }

    graph.connect(osc_low, filter);
    graph.connect(osc_high, filter);
    graph.connect(filter, gain);
    graph.connect(gain, analyser);
    graph.connect(analyser, dest);

    // Render 4096 frames so biquad settles and fills the 2048-point FFT window cleanly.
    let rendered = graph.render_offline(4_096);
    assert_eq!(rendered.length(), 4_096);
    assert_eq!(rendered.number_of_channels(), 2);

    // Inspect real FFT spectrum from AnalyserNode.
    let mut freq_db = vec![-160.0f32; 1_024];
    let mut byte_bins = vec![0u8; 1_024];
    if let Some(AudioNodeKind::Analyser(an)) = graph.node_mut(analyser) {
        an.get_float_frequency_data(&mut freq_db);
        an.get_byte_frequency_data(&mut byte_bins);
    }

    let bin_hz = sr as f32 / 2048.0; // ~23.4375 Hz per bin
    let bin_440 = (440.0 / bin_hz).round() as usize;
    let bin_8k = (8_000.0 / bin_hz).round() as usize;

    let (peak_bin, &peak_db) = freq_db
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.total_cmp(b.1))
        .unwrap();
    assert!(
        peak_bin.abs_diff(bin_440) <= 1,
        "expected FFT peak near 440 Hz bin {bin_440}, got bin {peak_bin} ({:.1} Hz)",
        peak_bin as f32 * bin_hz
    );

    let db_8k = freq_db[bin_8k];
    assert!(
        peak_db - db_8k > 24.0,
        "expected 1 kHz lowpass filter to attenuate 8 kHz by > 24 dB (440 Hz={peak_db:.1} dB, 8 kHz={db_8k:.1} dB)"
    );
    assert!(byte_bins[peak_bin] > 100);
}

#[test]
fn webaudio_buffer_source_param_ramp_media_element_queue_and_worklet() {
    let sr = 48_000u32;
    let mut graph = AudioGraph::new(sr, 2);

    // 1. AudioBufferSourceNode playing a constant 1.0 buffer into a linearRamp GainNode (0.0 -> 1.0 over 0.01 s).
    let mut src_buf = AudioBuffer::new(1, 1024, sr);
    src_buf.channels[0].fill(1.0);
    let bs = graph.create_buffer_source();
    if let Some(AudioNodeKind::BufferSource {
        buffer, start_time, ..
    }) = graph.node_mut(bs)
    {
        *buffer = Some(Arc::new(src_buf));
        *start_time = Some(0.0);
    }
    let gain = graph.create_gain();
    if let Some(AudioNodeKind::Gain { gain: g }) = graph.node_mut(gain) {
        g.set_value(0.0);
        g.set_value_at_time(0.0, 0.0);
        g.linear_ramp_to_value_at_time(1.0, 480.0 / sr as f64);
    }
    // 2. AudioWorkletNode doubling the signal.
    let worklet = graph.create_worklet(
        "doubler".to_string(),
        Box::new(|inputs, outputs, _frame, _t| {
            for (in_ch, out_ch) in inputs.iter().zip(outputs.iter_mut()) {
                for (dst, &src) in out_ch.iter_mut().zip(in_ch.iter()) {
                    *dst = src * 2.0;
                }
            }
        }),
    );

    graph.connect(bs, gain);
    graph.connect(gain, worklet);
    graph.connect(worklet, graph.destination());

    let out = graph.render_offline(512);
    // At sample 0: ramp = 0.0 => out = 0.0; at sample 240 (halfway): ramp = 0.5 => worklet out = 1.0; at sample 480: ramp = 1.0 => worklet out = 2.0!
    assert!(out.channels[0][0].abs() < 0.02);
    assert!((out.channels[0][240] - 1.0).abs() < 0.05);
    assert!((out.channels[0][480] - 2.0).abs() < 0.05);

    // 3. MediaElementAudioSourceNode pulling decoded interleaved PCM through AudioSampleQueue.
    let mut g2 = AudioGraph::new(sr, 2);
    let queue = Arc::new(AudioSampleQueue::new(4096));
    let mut sine = Vec::with_capacity(512 * 2);
    for i in 0..512 {
        let v = (2.0 * PI * 440.0 * i as f32 / sr as f32).sin() * 0.75;
        sine.push(v);
        sine.push(v);
    }
    queue.push_interleaved(&sine);
    let media_node = g2.create_media_element_source(Arc::clone(&queue), 2);
    g2.connect(media_node, g2.destination());

    let mixed = g2.render_offline(256);
    assert!((mixed.channels[0][10] - sine[20]).abs() < 1e-5);
    assert!((mixed.channels[1][10] - sine[21]).abs() < 1e-5);
}

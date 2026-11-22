use rix_media::demuxer::mp4::{mux_fmp4_init_segment, mux_fmp4_media_segment, Fmp4SampleInput};
use rix_media::demuxer::webm::{mux_webm_cluster, mux_webm_init_segment, WebmBlockInput};
use rix_media::{
    create_video_decoder, demux_bytes, ColorMatrix, ColorRange, EncodedPacket, PacketKind,
    PixelFormat, TrackInfo, TrackKind, VideoCodec, VideoFrame, VideoTrackConfig,
};

/// Generates a deterministic moving-square YUV420p sequence (`frame_count` frames).
/// Moving geometry guarantees inter-frame motion compensation (P/B frames) is exercised and
/// ensures frame order bugs or dropped inter-frames immediately spike luma MAE.
fn make_moving_yuv_sequence(width: u32, height: u32, frame_count: usize) -> Vec<VideoFrame> {
    let w = width as usize;
    let h = height as usize;
    let cw = w / 2;
    let ch = h / 2;

    let mut frames = Vec::with_capacity(frame_count);
    for idx in 0..frame_count {
        let mut y = vec![32u8; w * h];
        let mut u = vec![128u8; cw * ch];
        let mut v = vec![128u8; cw * ch];

        // Subtle horizontal gradient background.
        for row in 0..h {
            for col in 0..w {
                y[row * w + col] = (32 + ((col * 48) / w)) as u8;
            }
        }

        // Bright moving 24x24 block shifting by +8 px horizontally and +4 px vertically per frame.
        let bx = (8 + idx * 8).min(w.saturating_sub(24));
        let by = (6 + idx * 4).min(h.saturating_sub(24));
        for row in by..(by + 24).min(h) {
            for col in bx..(bx + 24).min(w) {
                y[row * w + col] = 220;
            }
        }
        for row in (by / 2)..((by + 24) / 2).min(ch) {
            for col in (bx / 2)..((bx + 24) / 2).min(cw) {
                u[row * cw + col] = 90;
                v[row * cw + col] = 196;
            }
        }

        frames.push(VideoFrame {
            width,
            height,
            pixel_format: PixelFormat::Yuv420p,
            color_matrix: ColorMatrix::Bt709,
            color_range: ColorRange::Limited,
            y_plane: y,
            u_plane: u,
            v_plane: v,
            y_stride: width,
            u_stride: cw as u32,
            v_stride: cw as u32,
            pts_us: (idx as i64) * 33_333,
            duration_us: 33_333,
            is_keyframe: idx == 0,
        });
    }
    frames
}

fn verify_decoded_sequence(
    codec_name: &str,
    expected: &[VideoFrame],
    decoded: &[VideoFrame],
    max_mae: f64,
) -> Vec<u64> {
    assert_eq!(
        decoded.len(),
        expected.len(),
        "{codec_name}: expected {} decoded frames, got {}",
        expected.len(),
        decoded.len()
    );
    let mut checksums = Vec::with_capacity(decoded.len());
    for (idx, (exp, got)) in expected.iter().zip(decoded.iter()).enumerate() {
        assert_eq!(got.width, exp.width, "{codec_name} frame {idx} width");
        assert_eq!(got.height, exp.height, "{codec_name} frame {idx} height");
        let mae = got.luma_mae(exp);
        assert!(
            mae <= max_mae,
            "{codec_name} frame {idx} luma MAE {mae:.2} exceeded threshold {max_mae:.2}"
        );
        // Also ensure frame `idx` matches its own source frame better than a distant frame,
        // proving display ordering and inter-frame motion updates actually took place!
        if idx > 0 {
            let mae_vs_f0 = got.luma_mae(&expected[0]);
            assert!(
                mae < mae_vs_f0,
                "{codec_name} frame {idx} looks closer to frame 0 ({mae_vs_f0:.2}) than frame {idx} ({mae:.2})"
            );
        }
        let csum = got.checksum_fnv1a64();
        assert_ne!(csum, 0);
        assert_eq!(got.checksum_sha256_hex().len(), 64);
        checksums.push(csum);
    }
    // Distinct moving frames must produce distinct pixel checksums.
    assert_ne!(
        checksums[0], checksums[1],
        "{codec_name}: P-frame checksum must differ from keyframe checksum"
    );
    checksums
}

#[test]
fn av1_rav1d_decodes_keyframe_and_inter_frames_with_deterministic_checksums() {
    let source = make_moving_yuv_sequence(96, 64, 6);

    let mut enc_cfg = rusty_av1e::EncoderConfig::with_speed_preset(8);
    enc_cfg.width = 96;
    enc_cfg.height = 64;
    enc_cfg.min_key_frame_interval = 30;
    enc_cfg.max_key_frame_interval = 30;
    enc_cfg.quantizer = 60;

    let cfg = rusty_av1e::Config::new().with_encoder_config(enc_cfg);
    let mut ctx: rusty_av1e::Context<u8> = cfg.new_context().expect("rav1e context");

    let mut encoded_packets = Vec::new();
    for src in &source {
        let mut frame = ctx.new_frame();
        frame.planes[0].copy_from_raw_u8(&src.y_plane, src.y_stride as usize, 1);
        frame.planes[1].copy_from_raw_u8(&src.u_plane, src.u_stride as usize, 1);
        frame.planes[2].copy_from_raw_u8(&src.v_plane, src.v_stride as usize, 1);
        ctx.send_frame(frame).expect("send AV1 frame");
    }
    ctx.flush();

    loop {
        match ctx.receive_packet() {
            Ok(pkt) => encoded_packets.push((pkt.data, pkt.input_frameno == 0)),
            Err(rusty_av1e::EncoderStatus::LimitReached) => break,
            Err(rusty_av1e::EncoderStatus::NeedMoreData) => break,
            Err(rusty_av1e::EncoderStatus::Encoded) => continue,
            Err(other) => panic!("unexpected rav1e status: {other:?}"),
        }
    }
    assert_eq!(encoded_packets.len(), source.len());
    assert!(encoded_packets[0].1);
    assert!(!encoded_packets[1].1);

    // Mux into fMP4 (`av01`), demux through `rix-media`, and decode twice to verify determinism.
    let track = TrackInfo {
        id: 1,
        kind: TrackKind::Video(VideoTrackConfig {
            codec: VideoCodec::Av1,
            width: 96,
            height: 64,
        }),
        timescale: 30_000,
        duration_us: None,
        language: None,
        extra_data: vec![0x81, 0x04, 0x0c, 0x00],
    };
    let init = mux_fmp4_init_segment(&[track], 1_000);
    let samples: Vec<Fmp4SampleInput<'_>> = encoded_packets
        .iter()
        .map(|(data, is_kf)| Fmp4SampleInput {
            data,
            duration_ticks: 1_000,
            composition_offset_ticks: 0,
            is_keyframe: *is_kf,
        })
        .collect();
    let media_seg = mux_fmp4_media_segment(1, 1, 0, &samples);
    let full = [init, media_seg].concat();
    let demuxed = demux_bytes(&full, None).expect("demux AV1 fMP4");

    let decode_once = || {
        let mut dec = create_video_decoder(&demuxed.tracks[0]).expect("AV1 decoder");
        let mut out = Vec::new();
        for pkt in &demuxed.packets {
            out.extend(dec.decode(pkt).expect("decode AV1 pkt"));
        }
        out.extend(dec.flush().expect("flush AV1"));
        out
    };

    let run1 = decode_once();
    let run2 = decode_once();
    let csums1 = verify_decoded_sequence("AV1", &source, &run1, 12.0);
    let csums2: Vec<u64> = run2.iter().map(VideoFrame::checksum_fnv1a64).collect();
    assert_eq!(
        csums1, csums2,
        "AV1 rav1d decoding must be bit-exact deterministic"
    );
}

#[test]
fn h264_decodes_bframe_dpb_reordering_and_mp4_avcc_streams() {
    let source = make_moving_yuv_sequence(96, 64, 6);

    // 1. Encode with real B-frames (`bframes = 2`, Main profile CAVLC) to exercise DPB reordering!
    let mut cfg = rusty_h264::EncoderConfig::new(96, 64);
    cfg.profile = rusty_h264::Profile::Main;
    cfg.bframes = 2;
    cfg.cabac = false;
    cfg.qp = 20;

    let enc = rusty_h264::Encoder::new(cfg).expect("H.264 encoder");
    let yuv_frames: Vec<rusty_h264::YuvFrame> = source
        .iter()
        .map(|src| rusty_h264::YuvFrame {
            width: src.width as usize,
            height: src.height as usize,
            y: src.y_plane.clone(),
            u: src.u_plane.clone(),
            v: src.v_plane.clone(),
        })
        .collect();
    let annex_b: Vec<u8> = enc
        .encode_all(&yuv_frames)
        .expect("H.264 B-frame encode_all")
        .concat();

    let aus = rix_media::video::split_h264_access_units(&annex_b);
    assert!(
        aus.len() >= source.len(),
        "expected at least 6 access units"
    );

    let track = TrackInfo {
        id: 1,
        kind: TrackKind::Video(VideoTrackConfig {
            codec: VideoCodec::H264,
            width: 96,
            height: 64,
        }),
        timescale: 30_000,
        duration_us: None,
        language: None,
        extra_data: Vec::new(),
    };

    let decode_annex_b = || {
        let mut dec = create_video_decoder(&track).expect("H.264 decoder");
        let mut out = Vec::new();
        for (idx, au) in aus.iter().enumerate() {
            let pkt = EncodedPacket {
                track_id: 1,
                kind: PacketKind::Video,
                data: au.to_vec(),
                pts_us: (idx as i64) * 33_333,
                dts_us: (idx as i64) * 33_333,
                duration_us: 33_333,
                is_keyframe: rix_media::video::h264_au_is_idr(au),
            };
            out.extend(dec.decode(&pkt).expect("decode H.264 AU"));
        }
        out.extend(dec.flush().expect("flush H.264 DPB"));
        out
    };

    let run1 = decode_annex_b();
    let run2 = decode_annex_b();
    let csums1 = verify_decoded_sequence("H.264 (I/P/B DPB)", &source, &run1, 10.0);
    let csums2: Vec<u64> = run2.iter().map(VideoFrame::checksum_fnv1a64).collect();
    assert_eq!(csums1, csums2, "H.264 DPB decode must be deterministic");

    // 2. Also verify ISO BMFF `avcC` + 4-byte length-prefixed MP4 sample conversion!
    let mut sps_nals: Vec<Vec<u8>> = Vec::new();
    let mut pps_nals: Vec<Vec<u8>> = Vec::new();
    let mut avcc_samples: Vec<(Vec<u8>, bool)> = Vec::new();
    for au in &aus {
        let mut sample = Vec::new();
        let mut has_vcl = false;
        let mut is_idr = false;
        for nal in split_annex_b_nals(au) {
            if nal.is_empty() {
                continue;
            }
            let nal_type = nal[0] & 0x1f;
            match nal_type {
                7 => sps_nals.push(nal.to_vec()),
                8 => pps_nals.push(nal.to_vec()),
                1 | 5 => {
                    has_vcl = true;
                    if nal_type == 5 {
                        is_idr = true;
                    }
                    sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    sample.extend_from_slice(nal);
                }
                _ => {}
            }
        }
        if has_vcl {
            avcc_samples.push((sample, is_idr));
        }
    }
    assert!(!sps_nals.is_empty() && !pps_nals.is_empty());

    let mut avcc = vec![
        1,
        sps_nals[0][1],
        sps_nals[0][2],
        sps_nals[0][3],
        0xff,
        0xe0 | (sps_nals.len() as u8),
    ];
    for sps in &sps_nals {
        avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(sps);
    }
    avcc.push(pps_nals.len() as u8);
    for pps in &pps_nals {
        avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(pps);
    }

    let mp4_track = TrackInfo {
        id: 1,
        kind: TrackKind::Video(VideoTrackConfig {
            codec: VideoCodec::H264,
            width: 96,
            height: 64,
        }),
        timescale: 30_000,
        duration_us: None,
        language: None,
        extra_data: avcc,
    };
    let init = mux_fmp4_init_segment(&[mp4_track], 1_000);
    let sample_inputs: Vec<Fmp4SampleInput<'_>> = avcc_samples
        .iter()
        .map(|(data, is_kf)| Fmp4SampleInput {
            data,
            duration_ticks: 1_000,
            composition_offset_ticks: 0,
            is_keyframe: *is_kf,
        })
        .collect();
    let media_seg = mux_fmp4_media_segment(1, 1, 0, &sample_inputs);
    let demuxed = demux_bytes(&[init, media_seg].concat(), None).expect("demux H.264 fMP4");

    let mut mp4_dec = create_video_decoder(&demuxed.tracks[0]).expect("H.264 avcC decoder");
    let mut mp4_frames = Vec::new();
    for pkt in &demuxed.packets {
        mp4_frames.extend(mp4_dec.decode(pkt).expect("decode H.264 avcC pkt"));
    }
    mp4_frames.extend(mp4_dec.flush().expect("flush H.264 avcC"));
    let mp4_csums: Vec<u64> = mp4_frames
        .iter()
        .map(VideoFrame::checksum_fnv1a64)
        .collect();
    assert_eq!(
        mp4_csums, csums1,
        "MP4 avcC stream must produce bit-exact checksums identical to Annex-B"
    );
}

fn split_annex_b_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0usize;
    while i + 3 <= data.len() {
        if i + 4 <= data.len() && data[i..i + 4] == [0, 0, 0, 1] {
            starts.push((i, i + 4));
            i += 4;
        } else if data[i..i + 3] == [0, 0, 1] {
            starts.push((i, i + 3));
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::with_capacity(starts.len());
    for idx in 0..starts.len() {
        let start = starts[idx].1;
        let end = starts.get(idx + 1).map(|s| s.0).unwrap_or(data.len());
        if start < end {
            nals.push(&data[start..end]);
        }
    }
    nals
}

#[test]
fn vp8_decodes_keyframe_and_inter_p_frames_from_webm() {
    let source = make_moving_yuv_sequence(96, 64, 6);

    let enc_cfg = vp8_encoder::EncoderConfig::new(96, 64, 30, 500_000).with_keyframe_interval(None);
    let mut enc = vp8_encoder::Encoder::new(enc_cfg).expect("VP8 encoder");

    let mut encoded = Vec::new();
    for (idx, src) in source.iter().enumerate() {
        let pic = vp8_encoder::Picture {
            width: src.width,
            height: src.height,
            y: &src.y_plane,
            u: &src.u_plane,
            v: &src.v_plane,
            y_stride: src.y_stride as usize,
            uv_stride: src.u_stride as usize,
        };
        let frame = enc
            .encode(&pic, idx == 0)
            .expect("encode VP8")
            .expect("VP8 frame output");
        if idx == 0 {
            assert!(frame.keyframe);
        } else {
            assert!(
                !frame.keyframe,
                "VP8 frame {idx} should be an inter P-frame"
            );
        }
        encoded.push(frame);
    }

    let track = TrackInfo {
        id: 1,
        kind: TrackKind::Video(VideoTrackConfig {
            codec: VideoCodec::Vp8,
            width: 96,
            height: 64,
        }),
        timescale: 1_000,
        duration_us: None,
        language: None,
        extra_data: Vec::new(),
    };
    let init = mux_webm_init_segment(&[track], Some(200.0));
    let blocks: Vec<WebmBlockInput<'_>> = encoded
        .iter()
        .enumerate()
        .map(|(idx, f)| WebmBlockInput {
            track_id: 1,
            rel_timestamp_ms: (idx as i16) * 33,
            is_keyframe: f.keyframe,
            data: &f.data,
        })
        .collect();
    let cluster = mux_webm_cluster(0, &blocks);
    let demuxed = demux_bytes(&[init, cluster].concat(), None).expect("demux VP8 WebM");

    let decode_once = || {
        let mut dec = create_video_decoder(&demuxed.tracks[0]).expect("VP8 decoder");
        let mut out = Vec::new();
        for pkt in &demuxed.packets {
            out.extend(dec.decode(pkt).expect("decode VP8 pkt"));
        }
        out.extend(dec.flush().expect("flush VP8"));
        out
    };

    let run1 = decode_once();
    let run2 = decode_once();
    let csums1 = verify_decoded_sequence("VP8", &source, &run1, 10.0);
    let csums2: Vec<u64> = run2.iter().map(VideoFrame::checksum_fnv1a64).collect();
    assert_eq!(
        csums1, csums2,
        "VP8 decoding must be bit-exact deterministic"
    );
}

#[test]
fn vp9_decodes_keyframe_and_inter_frames_from_webm() {
    let source = make_moving_yuv_sequence(96, 64, 6);

    let mut enc = rusty_vp9::Vp9Encoder::default();
    enc.configure(&rusty_vp9::Vp9EncoderConfig {
        crf: Some(16),
        speed: Some(4),
        lag: Some(0),
        ..Default::default()
    })
    .expect("configure VP9 encoder");

    let mut encoded = Vec::new();
    for src in &source {
        enc.push_frame(
            [&src.y_plane, &src.u_plane, &src.v_plane],
            [
                src.y_stride as usize,
                src.u_stride as usize,
                src.v_stride as usize,
            ],
            src.width,
            src.height,
        )
        .expect("push VP9 frame");
        loop {
            match enc.next_packet() {
                Ok(pkt) => encoded.push(pkt),
                Err(rusty_vp9::Error::Again) => break,
                Err(e) => panic!("VP9 encode error: {e}"),
            }
        }
    }
    enc.flush();
    loop {
        match enc.next_packet() {
            Ok(pkt) => encoded.push(pkt),
            Err(rusty_vp9::Error::Eof) | Err(rusty_vp9::Error::Again) => break,
            Err(e) => panic!("VP9 flush error: {e}"),
        }
    }
    assert_eq!(encoded.len(), source.len());
    assert!(encoded[0].keyframe);
    assert!(!encoded[1].keyframe, "VP9 frame 1 must be an inter-frame");

    let track = TrackInfo {
        id: 1,
        kind: TrackKind::Video(VideoTrackConfig {
            codec: VideoCodec::Vp9,
            width: 96,
            height: 64,
        }),
        timescale: 1_000,
        duration_us: None,
        language: None,
        extra_data: Vec::new(),
    };
    let init = mux_webm_init_segment(&[track], Some(200.0));
    let blocks: Vec<WebmBlockInput<'_>> = encoded
        .iter()
        .enumerate()
        .map(|(idx, p)| WebmBlockInput {
            track_id: 1,
            rel_timestamp_ms: (idx as i16) * 33,
            is_keyframe: p.keyframe,
            data: &p.data,
        })
        .collect();
    let cluster = mux_webm_cluster(0, &blocks);
    let demuxed = demux_bytes(&[init, cluster].concat(), None).expect("demux VP9 WebM");

    let decode_once = || {
        let mut dec = create_video_decoder(&demuxed.tracks[0]).expect("VP9 decoder");
        let mut out = Vec::new();
        for pkt in &demuxed.packets {
            out.extend(dec.decode(pkt).expect("decode VP9 pkt"));
        }
        out.extend(dec.flush().expect("flush VP9"));
        out
    };

    let run1 = decode_once();
    let run2 = decode_once();
    let csums1 = verify_decoded_sequence("VP9", &source, &run1, 10.0);
    let csums2: Vec<u64> = run2.iter().map(VideoFrame::checksum_fnv1a64).collect();
    assert_eq!(
        csums1, csums2,
        "VP9 decoding must be bit-exact deterministic"
    );
}

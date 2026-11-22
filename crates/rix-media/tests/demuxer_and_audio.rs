use std::f32::consts::PI;

use rix_media::demuxer::mp4::{
    mux_fmp4_init_segment, mux_fmp4_media_segment, parse_mp4_init_segment,
    parse_mp4_media_segments, Fmp4SampleInput,
};
use rix_media::demuxer::webm::{
    mux_webm_cluster, mux_webm_init_segment, parse_webm_init_segment, parse_webm_media_segments,
    WebmBlockInput,
};
use rix_media::{
    create_audio_decoder, decode_container_audio, demux_bytes, detect_container, AudioCodec,
    AudioTrackConfig, ContainerFormat, EncodedPacket, PacketKind, PcmSampleFormat, TrackInfo,
    TrackKind, VideoCodec, VideoTrackConfig,
};

fn build_wav_pcm16_sine(sample_rate: u32, channels: u16, frames: usize, freq_hz: f32) -> Vec<u8> {
    let data_bytes = frames * usize::from(channels) * 2;
    let mut out = Vec::with_capacity(44 + data_bytes);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data_bytes) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    let byte_rate = sample_rate * u32::from(channels) * 2;
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&(channels * 2).to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_bytes as u32).to_le_bytes());
    for i in 0..frames {
        let t = i as f32 / sample_rate as f32;
        let sample = (2.0 * PI * freq_hz * t).sin() * 0.5;
        let q = (sample * 32_767.0).round() as i16;
        for _ in 0..channels {
            out.extend_from_slice(&q.to_le_bytes());
        }
    }
    out
}

fn ogg_crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for &byte in data {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            if (crc & 0x8000_0000) != 0 {
                crc = (crc << 1) ^ 0x04c1_1db7;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

fn build_ogg_page(
    header_type: u8,
    granule_pos: u64,
    serial: u32,
    seq: u32,
    packet: &[u8],
) -> Vec<u8> {
    assert!(packet.len() < 255);
    let mut page = Vec::with_capacity(28 + packet.len());
    page.extend_from_slice(b"OggS");
    page.push(0); // stream_structure_version
    page.push(header_type);
    page.extend_from_slice(&granule_pos.to_le_bytes());
    page.extend_from_slice(&serial.to_le_bytes());
    page.extend_from_slice(&seq.to_le_bytes());
    page.extend_from_slice(&0u32.to_le_bytes()); // CRC placeholder
    page.push(1); // 1 segment
    page.push(packet.len() as u8);
    page.extend_from_slice(packet);
    let crc = ogg_crc32(&page);
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    page
}

fn build_ogg_opus_stream() -> Vec<u8> {
    // 19-byte RFC 7845 stereo 48 kHz OpusHead with pre-skip = 0 so all 960 frames are emitted.
    let opus_head: [u8; 19] = [
        b'O', b'p', b'u', b's', b'H', b'e', b'a', b'd', 1, 2, 0, 0, 0x80, 0xbb, 0x00, 0x00, 0, 0, 0,
    ];
    let mut opus_tags = Vec::new();
    opus_tags.extend_from_slice(b"OpusTags");
    opus_tags.extend_from_slice(&3u32.to_le_bytes());
    opus_tags.extend_from_slice(b"rix");
    opus_tags.extend_from_slice(&0u32.to_le_bytes());

    // Valid RFC 6716 20 ms fullband stereo CELT frame (`TOC = 0xFC`, silence payload).
    let opus_pkt = [0xfc, 0xff, 0xfe];

    let mut out = Vec::new();
    out.extend_from_slice(&build_ogg_page(0x02, 0, 1, 0, &opus_head));
    out.extend_from_slice(&build_ogg_page(0x00, 0, 1, 1, &opus_tags));
    out.extend_from_slice(&build_ogg_page(0x04, 960, 1, 2, &opus_pkt));
    out
}

fn flac_crc8(bytes: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in bytes {
        crc ^= b;
        for _ in 0..8 {
            if (crc & 0x80) != 0 {
                crc = (crc << 1) ^ 0x07;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

fn flac_crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in bytes {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            if (crc & 0x8000) != 0 {
                crc = (crc << 1) ^ 0x8005;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// Builds a valid single-frame 16-bit mono 44.1 kHz FLAC file (constant subframe value `1024`).
fn build_flac_constant_stream() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    // Metadata block header: last=1, type=0 (STREAMINFO), length=34
    out.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
    let mut si = [0u8; 34];
    // min_block_size = 192, max_block_size = 192
    si[0..2].copy_from_slice(&192u16.to_be_bytes());
    si[2..4].copy_from_slice(&192u16.to_be_bytes());
    // sample_rate=44100 (20 bits) | channels-1=0 (3 bits) | bps-1=15 (5 bits) | total_samples=192 (36 bits)
    let sr = 44_100u64;
    let ch_minus1 = 0u64;
    let bps_minus1 = 15u64;
    let total_samples = 192u64;
    let packed: u64 = (sr << 44) | (ch_minus1 << 41) | (bps_minus1 << 36) | total_samples;
    si[10..18].copy_from_slice(&packed.to_be_bytes());
    out.extend_from_slice(&si);

    // FLAC frame: sync=0xFFF8 (fixed blocksize), blocksize=0b0001 (192 samples), sample_rate=0b1001 (44.1 kHz),
    // channel_assignment=0b0000 (mono), sample_size=0b100 (16-bit), reserved=0, frame_number=0.
    let mut frame = vec![0xff, 0xf8, 0x19, 0x08, 0x00];
    let header_crc8 = flac_crc8(&frame);
    frame.push(header_crc8);
    // Subframe header: zero-bit(0) + type=000000 (CONSTANT) + wasted=0 => 0x00, then 16-bit big-endian sample `1024`.
    frame.push(0x00);
    frame.extend_from_slice(&1024i16.to_be_bytes());
    let footer_crc16 = flac_crc16(&frame);
    frame.extend_from_slice(&footer_crc16.to_be_bytes());
    out.extend_from_slice(&frame);
    out
}

/// Builds a valid MPEG-1 Layer III 128 kbps 44.1 kHz stereo silent stream (3 consecutive 417-byte frames).
fn build_mp3_silent_stream() -> Vec<u8> {
    let mut frame = vec![0u8; 417];
    // Sync (0xFFE) | MPEG1 (11) | Layer III (01) | no CRC (1) => 0xFF, 0xFB
    // Bitrate index 9 (128 kbps) | 44.1 kHz (00) | padding 0 | private 0 => 0x90
    // Joint stereo (01) | mode ext 00 | copy 0 | orig 1 | emphasis 00 => 0x64
    frame[0] = 0xff;
    frame[1] = 0xfb;
    frame[2] = 0x90;
    frame[3] = 0x64;
    let mut stream = Vec::with_capacity(frame.len() * 3);
    for _ in 0..3 {
        stream.extend_from_slice(&frame);
    }
    stream
}

#[test]
fn demux_fmp4_multi_track_with_bframe_cto_and_extra_data() {
    let video_track = TrackInfo {
        id: 1,
        kind: TrackKind::Video(VideoTrackConfig {
            codec: VideoCodec::H264,
            width: 1920,
            height: 1080,
        }),
        timescale: 90_000,
        duration_us: None,
        language: Some("eng".to_string()),
        extra_data: vec![0x01, 0x64, 0x00, 0x28, 0xff, 0xe1, 0x00, 0x02, 0x67, 0x64],
    };
    let audio_track = TrackInfo {
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
        // AAC-LC 48 kHz stereo AudioSpecificConfig
        extra_data: vec![0x11, 0x90],
    };

    let init = mux_fmp4_init_segment(&[video_track, audio_track], 1_000);
    assert_eq!(detect_container(&init), Some(ContainerFormat::Mp4));

    let seg_video = mux_fmp4_media_segment(
        1,
        1,
        0,
        &[
            Fmp4SampleInput {
                data: &[0x00, 0x00, 0x00, 0x02, 0x65, 0x88],
                duration_ticks: 3_000, // 33.333 ms
                composition_offset_ticks: 0,
                is_keyframe: true,
            },
            Fmp4SampleInput {
                data: &[0x00, 0x00, 0x00, 0x02, 0x41, 0x9a],
                duration_ticks: 3_000,
                composition_offset_ticks: 3_000, // P-frame displayed after B-frame
                is_keyframe: false,
            },
            Fmp4SampleInput {
                data: &[0x00, 0x00, 0x00, 0x02, 0x01, 0x9e],
                duration_ticks: 3_000,
                composition_offset_ticks: -3_000, // B-frame displayed before P-frame
                is_keyframe: false,
            },
        ],
    );

    // Valid silent raw AAC-LC stereo frame (48 kHz, 1024 samples).
    let aac_silent_frame = [0x21, 0x10, 0x04, 0x60, 0x8c, 0x1c];
    let seg_audio = mux_fmp4_media_segment(
        2,
        2,
        0,
        &[Fmp4SampleInput {
            data: &aac_silent_frame,
            duration_ticks: 1_024,
            composition_offset_ticks: 0,
            is_keyframe: true,
        }],
    );

    // 1. Verify full-buffer MP4 demuxing.
    let mut full = Vec::new();
    full.extend_from_slice(&init);
    full.extend_from_slice(&seg_video);
    full.extend_from_slice(&seg_audio);

    let demuxed = demux_bytes(&full, None).expect("fMP4 demux succeeds");
    assert_eq!(demuxed.container, ContainerFormat::Mp4);
    assert_eq!(demuxed.tracks.len(), 2);
    assert_eq!(
        demuxed.tracks[0].extra_data,
        vec![0x01, 0x64, 0x00, 0x28, 0xff, 0xe1, 0x00, 0x02, 0x67, 0x64]
    );
    assert_eq!(demuxed.tracks[1].extra_data, vec![0x11, 0x90]);

    let v_pkts: Vec<_> = demuxed.packets.iter().filter(|p| p.track_id == 1).collect();
    assert_eq!(v_pkts.len(), 3);
    assert!(v_pkts[0].is_keyframe);
    assert!(!v_pkts[1].is_keyframe);
    assert!(!v_pkts[2].is_keyframe);
    assert_eq!(v_pkts[0].dts_us, 0);
    assert_eq!(v_pkts[0].pts_us, 0);
    assert_eq!(v_pkts[1].dts_us, 33_333);
    assert_eq!(v_pkts[1].pts_us, 66_666);
    assert_eq!(v_pkts[2].dts_us, 66_666);
    assert_eq!(v_pkts[2].pts_us, 33_333);

    // 2. Verify incremental MSE fMP4 init + media segment parsing.
    let mut init_state = parse_mp4_init_segment(&init).expect("parse init");
    let inc_audio = parse_mp4_media_segments(&seg_audio, &mut init_state).expect("parse audio seg");
    assert_eq!(inc_audio.len(), 1);
    assert_eq!(inc_audio[0].data, aac_silent_frame);

    // 3. Verify pure-Rust AAC-LC decoding of the demuxed MP4 audio packet.
    let mut aac_dec = create_audio_decoder(&demuxed.tracks[1]).expect("create AAC decoder");
    let decoded = aac_dec.decode(&inc_audio[0]).expect("decode AAC packet");
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].sample_rate, 48_000);
    assert_eq!(decoded[0].channels, 2);
    assert_eq!(decoded[0].frame_count(), 1_024);
}

#[test]
fn demux_webm_av1_vp9_and_opus_incremental_clusters() {
    let tracks = vec![
        TrackInfo {
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
        },
        TrackInfo {
            id: 2,
            kind: TrackKind::Audio(AudioTrackConfig {
                codec: AudioCodec::Opus,
                sample_rate: 48_000,
                channels: 2,
                pcm_format: None,
            }),
            timescale: 1_000,
            duration_us: None,
            language: None,
            extra_data: Vec::new(),
        },
    ];

    let init = mux_webm_init_segment(&tracks, Some(1_000.0));
    assert_eq!(detect_container(&init), Some(ContainerFormat::WebM));

    let opus_pkt = [0xfc, 0xff, 0xfe];
    let cluster0 = mux_webm_cluster(
        0,
        &[
            WebmBlockInput {
                track_id: 1,
                rel_timestamp_ms: 0,
                is_keyframe: true,
                data: &[0x82, 0x49, 0x83, 0x42],
            },
            WebmBlockInput {
                track_id: 1,
                rel_timestamp_ms: 33,
                is_keyframe: false,
                data: &[0x86, 0x01, 0x02],
            },
            WebmBlockInput {
                track_id: 2,
                rel_timestamp_ms: 0,
                is_keyframe: true,
                data: &opus_pkt,
            },
        ],
    );
    let cluster1 = mux_webm_cluster(
        100,
        &[WebmBlockInput {
            track_id: 2,
            rel_timestamp_ms: 20,
            is_keyframe: true,
            data: &opus_pkt,
        }],
    );

    let mut state = parse_webm_init_segment(&init).expect("webm init");
    assert_eq!(state.duration_us, Some(1_000_000));
    let p0 = parse_webm_media_segments(&cluster0, &mut state).expect("cluster 0");
    let p1 = parse_webm_media_segments(&cluster1, &mut state).expect("cluster 1");
    assert_eq!(p0.len(), 3);
    assert!(p0[0].is_keyframe);
    assert!(!p0[1].is_keyframe);
    assert_eq!(p0[1].pts_us, 33_000);
    assert_eq!(p1.len(), 1);
    assert_eq!(p1[0].pts_us, 120_000);

    // Decode the WebM Opus packets via PureOpusDecoder.
    let audio_track = state.tracks.get(&2).expect("audio track");
    let mut opus_dec = create_audio_decoder(audio_track).expect("create Opus decoder");
    let decoded = opus_dec.decode(&p0[2]).expect("decode Opus packet");
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].sample_rate, 48_000);
    assert_eq!(decoded[0].channels, 2);
    assert_eq!(decoded[0].frame_count(), 960);
}

#[test]
fn decode_wav_ogg_opus_flac_mp3_and_raw_pcm() {
    // 1. WAV PCM16 440 Hz sine
    let wav = build_wav_pcm16_sine(44_100, 2, 1_024, 440.0);
    assert_eq!(detect_container(&wav), Some(ContainerFormat::Wav));
    let wav_frames = decode_container_audio(&wav, None).expect("decode WAV");
    let total_wav_frames: usize = wav_frames.iter().map(|f| f.frame_count()).sum();
    assert_eq!(total_wav_frames, 1_024);
    assert_eq!(wav_frames[0].sample_rate, 44_100);
    assert_eq!(wav_frames[0].channels, 2);
    let peak = wav_frames[0]
        .samples
        .iter()
        .copied()
        .map(f32::abs)
        .fold(0.0f32, f32::max);
    assert!((peak - 0.5).abs() < 0.02, "expected ~0.5 peak, got {peak}");

    // 2. Ogg/Opus via Symphonia + symphonia-opus
    let ogg_opus = build_ogg_opus_stream();
    assert_eq!(detect_container(&ogg_opus), Some(ContainerFormat::Ogg));
    let opus_frames = decode_container_audio(&ogg_opus, None).expect("decode Ogg/Opus");
    let total_opus: usize = opus_frames.iter().map(|f| f.frame_count()).sum();
    assert_eq!(total_opus, 960);
    assert_eq!(opus_frames[0].sample_rate, 48_000);
    assert_eq!(opus_frames[0].channels, 2);

    // 3. FLAC constant subframe (value 1024 / 32768 = 0.03125)
    let flac = build_flac_constant_stream();
    assert_eq!(detect_container(&flac), Some(ContainerFormat::Flac));
    let flac_frames = decode_container_audio(&flac, None).expect("decode FLAC");
    assert_eq!(flac_frames.len(), 1);
    assert_eq!(flac_frames[0].frame_count(), 192);
    assert!((flac_frames[0].samples[0] - (1024.0 / 32_768.0)).abs() < 1e-4);

    // 4. MP3 MPEG-1 Layer III stream (3 frames * 1152 samples)
    let mp3 = build_mp3_silent_stream();
    assert_eq!(detect_container(&mp3), Some(ContainerFormat::Mp3));
    let mp3_frames = decode_container_audio(&mp3, None).expect("decode MP3");
    assert!(!mp3_frames.is_empty());
    let total_mp3: usize = mp3_frames.iter().map(|f| f.frame_count()).sum();
    assert_eq!(total_mp3, 3 * 1_152);
    assert_eq!(mp3_frames[0].sample_rate, 44_100);
    assert_eq!(mp3_frames[0].channels, 2);

    // 5. Direct RawPcmDecoder F32LE check
    let pcm_track = TrackInfo {
        id: 1,
        kind: TrackKind::Audio(AudioTrackConfig {
            codec: AudioCodec::Pcm,
            sample_rate: 48_000,
            channels: 1,
            pcm_format: Some(PcmSampleFormat::F32Le),
        }),
        timescale: 48_000,
        duration_us: None,
        language: None,
        extra_data: Vec::new(),
    };
    let mut pcm_dec = create_audio_decoder(&pcm_track).expect("create PCM decoder");
    let mut bytes = Vec::new();
    for v in [0.25f32, -0.5, 0.75, -1.0] {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    let pkt = EncodedPacket {
        track_id: 1,
        kind: PacketKind::Audio,
        data: bytes,
        pts_us: 0,
        dts_us: 0,
        duration_us: 83,
        is_keyframe: true,
    };
    let pcm_out = pcm_dec.decode(&pkt).expect("decode raw PCM");
    assert_eq!(pcm_out[0].samples, vec![0.25, -0.5, 0.75, -1.0]);
}

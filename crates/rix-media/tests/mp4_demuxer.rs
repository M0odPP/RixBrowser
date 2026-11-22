use rix_media::demuxer::mp4::{
    mux_fmp4_init_segment, mux_fmp4_media_segment, parse_mp4_init_segment,
    parse_mp4_media_segments, Fmp4SampleInput,
};
use rix_media::{
    demux_bytes, detect_container, AudioCodec, AudioTrackConfig, ContainerFormat, TrackInfo,
    TrackKind, VideoCodec, VideoTrackConfig,
};

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
                duration_ticks: 3_000,
                composition_offset_ticks: 0,
                is_keyframe: true,
            },
            Fmp4SampleInput {
                data: &[0x00, 0x00, 0x00, 0x02, 0x41, 0x9a],
                duration_ticks: 3_000,
                composition_offset_ticks: 3_000,
                is_keyframe: false,
            },
            Fmp4SampleInput {
                data: &[0x00, 0x00, 0x00, 0x02, 0x01, 0x9e],
                duration_ticks: 3_000,
                composition_offset_ticks: -3_000,
                is_keyframe: false,
            },
        ],
    );

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

    let mut init_state = parse_mp4_init_segment(&init).expect("parse init");
    let inc_audio = parse_mp4_media_segments(&seg_audio, &mut init_state).expect("parse audio seg");
    assert_eq!(inc_audio.len(), 1);
    assert_eq!(inc_audio[0].data, aac_silent_frame);
}

#[test]
fn demux_standard_non_fragmented_mp4_stbl_tables() {
    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + payload.len());
        out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }
    fn fbx(kind: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
        let mut full = Vec::with_capacity(4 + payload.len());
        full.extend_from_slice(&((u32::from(version) << 24) | flags).to_be_bytes());
        full.extend_from_slice(payload);
        bx(kind, &full)
    }

    let ftyp = bx(b"ftyp", b"isom\0\0\x02\0isommp41");
    let sample0 = b"AV1_KEYFRAME_SAMPLE";
    let sample1 = b"AV1_INTER_P_SAMPLE!";
    let mdat_payload: Vec<u8> = [sample0.as_slice(), sample1.as_slice()].concat();
    let mdat = bx(b"mdat", &mdat_payload);
    let chunk_offset = (ftyp.len() + 8) as u32;

    let mut av01_body = vec![0u8; 78];
    av01_body[24..26].copy_from_slice(&640u16.to_be_bytes());
    av01_body[26..28].copy_from_slice(&360u16.to_be_bytes());
    av01_body.extend_from_slice(&bx(b"av1C", &[0x81, 0x04, 0x0c, 0x00]));
    let av01 = bx(b"av01", &av01_body);

    let mut stsd_p = 1u32.to_be_bytes().to_vec();
    stsd_p.extend_from_slice(&av01);
    let stsd = fbx(b"stsd", 0, 0, &stsd_p);

    let mut stts_p = Vec::new();
    stts_p.extend_from_slice(&1u32.to_be_bytes());
    stts_p.extend_from_slice(&2u32.to_be_bytes());
    stts_p.extend_from_slice(&1_000u32.to_be_bytes());
    let stts = fbx(b"stts", 0, 0, &stts_p);

    let mut stsc_p = Vec::new();
    stsc_p.extend_from_slice(&1u32.to_be_bytes());
    stsc_p.extend_from_slice(&1u32.to_be_bytes());
    stsc_p.extend_from_slice(&2u32.to_be_bytes());
    stsc_p.extend_from_slice(&1u32.to_be_bytes());
    let stsc = fbx(b"stsc", 0, 0, &stsc_p);

    let mut stsz_p = Vec::new();
    stsz_p.extend_from_slice(&0u32.to_be_bytes());
    stsz_p.extend_from_slice(&2u32.to_be_bytes());
    stsz_p.extend_from_slice(&(sample0.len() as u32).to_be_bytes());
    stsz_p.extend_from_slice(&(sample1.len() as u32).to_be_bytes());
    let stsz = fbx(b"stsz", 0, 0, &stsz_p);

    let mut stco_p = Vec::new();
    stco_p.extend_from_slice(&1u32.to_be_bytes());
    stco_p.extend_from_slice(&chunk_offset.to_be_bytes());
    let stco = fbx(b"stco", 0, 0, &stco_p);

    let mut stss_p = Vec::new();
    stss_p.extend_from_slice(&1u32.to_be_bytes());
    stss_p.extend_from_slice(&1u32.to_be_bytes());
    let stss = fbx(b"stss", 0, 0, &stss_p);

    let stbl = bx(b"stbl", &[stsd, stts, stsc, stsz, stco, stss].concat());
    let minf = bx(b"minf", &stbl);

    let mut mdhd_p = vec![0u8; 20];
    mdhd_p[8..12].copy_from_slice(&30_000u32.to_be_bytes());
    mdhd_p[12..16].copy_from_slice(&2_000u32.to_be_bytes());
    let mdhd = fbx(b"mdhd", 0, 0, &mdhd_p);
    let mdia = bx(b"mdia", &[mdhd, minf].concat());

    let mut tkhd_p = vec![0u8; 80];
    tkhd_p[8..12].copy_from_slice(&1u32.to_be_bytes());
    let tkhd = fbx(b"tkhd", 0, 3, &tkhd_p);
    let trak = bx(b"trak", &[tkhd, mdia].concat());

    let mut mvhd_p = vec![0u8; 92];
    mvhd_p[8..12].copy_from_slice(&1_000u32.to_be_bytes());
    mvhd_p[12..16].copy_from_slice(&67u32.to_be_bytes());
    let mvhd = fbx(b"mvhd", 0, 0, &mvhd_p);
    let moov = bx(b"moov", &[mvhd, trak].concat());

    let file = [ftyp, mdat, moov].concat();
    let demuxed = demux_bytes(&file, None).expect("standard MP4 demux");
    assert_eq!(demuxed.tracks.len(), 1);
    assert_eq!(
        demuxed.tracks[0].kind,
        TrackKind::Video(VideoTrackConfig {
            codec: VideoCodec::Av1,
            width: 640,
            height: 360,
        })
    );
    assert_eq!(demuxed.tracks[0].extra_data, vec![0x81, 0x04, 0x0c, 0x00]);
    assert_eq!(demuxed.packets.len(), 2);
    assert!(demuxed.packets[0].is_keyframe);
    assert!(!demuxed.packets[1].is_keyframe);
    assert_eq!(demuxed.packets[0].data, sample0);
    assert_eq!(demuxed.packets[1].data, sample1);
    assert_eq!(demuxed.packets[0].pts_us, 0);
    assert_eq!(demuxed.packets[1].pts_us, 33_333);
}

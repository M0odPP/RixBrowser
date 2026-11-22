//! Symphonia-backed demuxer for Ogg, WAV, MP3, FLAC, and general container fallback.

use std::collections::HashMap;
use std::io::Cursor;

use symphonia::core::codecs::audio::well_known as audio_wk;
use symphonia::core::codecs::video::well_known as video_wk;
use symphonia::core::codecs::CodecParameters;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, Track};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use crate::types::{
    AudioCodec, AudioTrackConfig, ContainerFormat, DemuxedMedia, EncodedPacket, MediaError,
    PacketKind, PcmSampleFormat, TrackInfo, TrackKind, VideoCodec, VideoTrackConfig,
};

/// Demuxes Ogg, WAV, MP3, FLAC, or any Symphonia-supported container into normalized packets.
pub fn demux_with_symphonia(
    bytes: &[u8],
    container: ContainerFormat,
    extension_hint: Option<&str>,
) -> Result<DemuxedMedia, MediaError> {
    let cursor = Cursor::new(bytes.to_vec());
    let mss = MediaSourceStream::new(Box::new(cursor), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = extension_hint {
        hint.with_extension(ext);
    } else {
        match container {
            ContainerFormat::Ogg => {
                hint.with_extension("ogg");
            }
            ContainerFormat::Wav => {
                hint.with_extension("wav");
            }
            ContainerFormat::Mp3 => {
                hint.with_extension("mp3");
            }
            ContainerFormat::Flac => {
                hint.with_extension("flac");
            }
            ContainerFormat::Mp4 => {
                hint.with_extension("mp4");
            }
            ContainerFormat::WebM => {
                hint.with_extension("webm");
            }
        }
    }

    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| MediaError::MalformedContainer(format!("symphonia probe failed: {e}")))?;

    let mut tracks = Vec::new();
    let mut track_meta: HashMap<u32, (PacketKind, Option<symphonia::core::units::TimeBase>)> =
        HashMap::new();
    let mut max_duration_us: Option<u64> = None;

    for track in format.tracks() {
        if let Some(info) = convert_symphonia_track(track) {
            if let Some(d) = info.duration_us {
                max_duration_us = Some(max_duration_us.map_or(d, |cur| cur.max(d)));
            }
            let pkind = match info.kind {
                TrackKind::Video(_) => PacketKind::Video,
                TrackKind::Audio(_) => PacketKind::Audio,
            };
            track_meta.insert(track.id, (pkind, track.time_base));
            tracks.push(info);
        }
    }

    let mut packets = Vec::new();
    loop {
        match format.next_packet() {
            Ok(Some(pkt)) => {
                let Some(&(pkind, tb_opt)) = track_meta.get(&pkt.track_id) else {
                    continue;
                };
                let (pts_us, dts_us, duration_us) = if let Some(tb) = tb_opt {
                    let pts = tb.calc_time_saturating(pkt.pts).as_micros() as i64;
                    let dts = tb.calc_time_saturating(pkt.dts).as_micros() as i64;
                    let dur = tb.calc_duration_saturating(pkt.dur).as_micros().max(0) as u64;
                    (pts, dts, dur)
                } else {
                    (pkt.pts.get(), pkt.dts.get(), pkt.dur.get())
                };
                packets.push(EncodedPacket {
                    track_id: pkt.track_id,
                    kind: pkind,
                    data: pkt.data.into_vec(),
                    pts_us,
                    dts_us,
                    duration_us,
                    is_keyframe: pkind == PacketKind::Audio,
                });
            }
            Ok(None) => break,
            Err(SymphoniaError::IoError(err))
                if err.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(SymphoniaError::ResetRequired) => continue,
            Err(err) => {
                return Err(MediaError::MalformedContainer(format!(
                    "symphonia demux failed: {err}"
                )));
            }
        }
    }

    Ok(DemuxedMedia {
        container,
        duration_us: max_duration_us,
        tracks,
        packets,
    })
}

fn convert_symphonia_track(track: &Track) -> Option<TrackInfo> {
    let params = track.codec_params.as_ref()?;
    let timescale = track
        .time_base
        .map(|tb| tb.denom.get())
        .unwrap_or(1_000_000);
    let duration_us = track.num_frames.and_then(|nf| {
        track.time_base.map(|tb| {
            tb.calc_duration_saturating(symphonia::core::units::Duration::from(nf))
                .as_micros()
                .max(0) as u64
        })
    });

    match params {
        CodecParameters::Audio(ap) => {
            let (codec, pcm_format) = map_audio_codec(ap.codec)?;
            let sample_rate = ap.sample_rate.unwrap_or(48_000);
            let channels = ap
                .channels
                .as_ref()
                .map(|c| c.count() as u16)
                .unwrap_or(2)
                .max(1);
            Some(TrackInfo {
                id: track.id,
                kind: TrackKind::Audio(AudioTrackConfig {
                    codec,
                    sample_rate,
                    channels,
                    pcm_format,
                }),
                timescale,
                duration_us,
                language: track.language.clone(),
                extra_data: ap
                    .extra_data
                    .as_ref()
                    .map(|b| b.to_vec())
                    .unwrap_or_default(),
            })
        }
        CodecParameters::Video(vp) => {
            let codec = map_video_codec(vp.codec)?;
            let extra_data = vp
                .extra_data
                .first()
                .map(|e| e.data.to_vec())
                .unwrap_or_default();
            Some(TrackInfo {
                id: track.id,
                kind: TrackKind::Video(VideoTrackConfig {
                    codec,
                    width: u32::from(vp.width.unwrap_or(0)),
                    height: u32::from(vp.height.unwrap_or(0)),
                }),
                timescale,
                duration_us,
                language: track.language.clone(),
                extra_data,
            })
        }
        _ => None,
    }
}

fn map_audio_codec(
    id: symphonia::core::codecs::audio::AudioCodecId,
) -> Option<(AudioCodec, Option<PcmSampleFormat>)> {
    match id {
        audio_wk::CODEC_ID_OPUS => Some((AudioCodec::Opus, None)),
        audio_wk::CODEC_ID_AAC => Some((AudioCodec::Aac, None)),
        audio_wk::CODEC_ID_VORBIS => Some((AudioCodec::Vorbis, None)),
        audio_wk::CODEC_ID_MP1 | audio_wk::CODEC_ID_MP2 | audio_wk::CODEC_ID_MP3 => {
            Some((AudioCodec::Mp3, None))
        }
        audio_wk::CODEC_ID_FLAC => Some((AudioCodec::Flac, None)),
        audio_wk::CODEC_ID_PCM_U8 => Some((AudioCodec::Pcm, Some(PcmSampleFormat::U8))),
        audio_wk::CODEC_ID_PCM_S16LE => Some((AudioCodec::Pcm, Some(PcmSampleFormat::I16Le))),
        audio_wk::CODEC_ID_PCM_S24LE => Some((AudioCodec::Pcm, Some(PcmSampleFormat::I24Le))),
        audio_wk::CODEC_ID_PCM_S32LE => Some((AudioCodec::Pcm, Some(PcmSampleFormat::I32Le))),
        audio_wk::CODEC_ID_PCM_F32LE => Some((AudioCodec::Pcm, Some(PcmSampleFormat::F32Le))),
        _ => None,
    }
}

fn map_video_codec(id: symphonia::core::codecs::video::VideoCodecId) -> Option<VideoCodec> {
    match id {
        video_wk::CODEC_ID_AV1 => Some(VideoCodec::Av1),
        video_wk::CODEC_ID_H264 => Some(VideoCodec::H264),
        video_wk::CODEC_ID_VP8 => Some(VideoCodec::Vp8),
        video_wk::CODEC_ID_VP9 => Some(VideoCodec::Vp9),
        _ => None,
    }
}

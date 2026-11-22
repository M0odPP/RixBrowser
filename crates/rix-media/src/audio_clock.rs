//! Master audio playback clock (`MediaClock`) driven by `cpal` audio output (with headless fallback),
//! A/V synchronization (`< 25 ms` drift tolerance), jitter telemetry, and lock-safe audio PCM queues.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Maximum permissible A/V drift before triggering frame drop or wait (`25 ms` per Spec 06 §4.2).
pub const AV_SYNC_TOLERANCE_US: i64 = 25_000;

/// Decision returned by [`MediaClock::evaluate_video_frame`] for a decoded video frame's PTS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvSyncDecision {
    /// Present this video frame immediately (within `< 25 ms` tolerance of the audio clock).
    Present { drift_us: i64 },
    /// Frame is early; wait `delay_us` microseconds before presenting.
    Wait { delay_us: i64 },
    /// Frame is late beyond tolerance; drop it to catch up with the master audio clock.
    Drop { late_us: i64 },
}

/// Lock-free master audio playback clock driven directly by rendered audio output samples.
pub struct MediaClock {
    sample_rate: u32,
    samples_rendered: AtomicU64,
    base_pts_us: AtomicI64,
    playing: AtomicBool,
    muted: AtomicBool,
    /// Playback rate scaled by 1000 (`1000` = 1.0x).
    rate_milli: AtomicU32,
    /// Linear volume scaled by 1000 (`1000` = 1.0).
    volume_milli: AtomicU32,
    /// Exponential moving average of `|drift_us|` for `F12` HUD A/V sync jitter telemetry.
    jitter_ema_us: AtomicU64,
    /// Peak observed `|drift_us|` across presented frames.
    jitter_peak_us: AtomicU64,
    frames_presented: AtomicU64,
    frames_dropped: AtomicU64,
}

impl MediaClock {
    /// Creates a new paused [`MediaClock`] running at `sample_rate` Hz (default `48_000`).
    #[must_use]
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate: sample_rate.max(8_000),
            samples_rendered: AtomicU64::new(0),
            base_pts_us: AtomicI64::new(0),
            playing: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            rate_milli: AtomicU32::new(1_000),
            volume_milli: AtomicU32::new(1_000),
            jitter_ema_us: AtomicU64::new(0),
            jitter_peak_us: AtomicU64::new(0),
            frames_presented: AtomicU64::new(0),
            frames_dropped: AtomicU64::new(0),
        }
    }

    /// Returns the hardware / graph sample rate in Hz.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Starts or resumes playback.
    pub fn play(&self) {
        self.playing.store(true, Ordering::Release);
    }

    /// Pauses playback while preserving the current timeline position.
    pub fn pause(&self) {
        self.playing.store(false, Ordering::Release);
    }

    /// Returns true if playback is active.
    #[must_use]
    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Acquire)
    }

    /// Seeks the master clock to `target_pts_us` microseconds.
    pub fn seek_us(&self, target_pts_us: i64) {
        self.base_pts_us
            .store(target_pts_us.max(0), Ordering::Release);
        self.samples_rendered.store(0, Ordering::Release);
    }

    /// Seeks the master clock to `seconds`.
    pub fn seek_seconds(&self, seconds: f64) {
        if seconds.is_finite() {
            self.seek_us((seconds.max(0.0) * 1_000_000.0).round() as i64);
        }
    }

    /// Sets playback rate (clamped to `0.0625..=16.0`), rebasing the clock seamlessly.
    pub fn set_playback_rate(&self, rate: f64) {
        if !rate.is_finite() || rate <= 0.0 {
            return;
        }
        let cur = self.current_time_us();
        self.base_pts_us.store(cur, Ordering::Release);
        self.samples_rendered.store(0, Ordering::Release);
        let clamped = (rate.clamp(0.0625, 16.0) * 1_000.0).round() as u32;
        self.rate_milli.store(clamped.max(1), Ordering::Release);
    }

    /// Returns the current playback rate multiplier (`1.0` = normal speed).
    #[must_use]
    pub fn playback_rate(&self) -> f64 {
        f64::from(self.rate_milli.load(Ordering::Acquire)) / 1_000.0
    }

    /// Sets linear volume in `[0.0, 1.0]`.
    pub fn set_volume(&self, volume: f64) {
        let clamped = (volume.clamp(0.0, 1.0) * 1_000.0).round() as u32;
        self.volume_milli.store(clamped, Ordering::Release);
    }

    /// Returns linear volume in `[0.0, 1.0]`.
    #[must_use]
    pub fn volume(&self) -> f64 {
        f64::from(self.volume_milli.load(Ordering::Acquire)) / 1_000.0
    }

    /// Sets muted state.
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Release);
    }

    /// Returns muted state.
    #[must_use]
    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Acquire)
    }

    /// Effective linear gain (`0.0` when muted).
    #[must_use]
    pub fn effective_gain(&self) -> f32 {
        if self.is_muted() {
            0.0
        } else {
            self.volume() as f32
        }
    }

    /// Advances the master audio clock by `frame_count` rendered audio frames (invoked by the
    /// `cpal` audio output callback or headless render quantum step).
    pub fn advance_audio_frames(&self, frame_count: u64) {
        if self.playing.load(Ordering::Acquire) {
            self.samples_rendered
                .fetch_add(frame_count, Ordering::AcqRel);
        }
    }

    /// Returns the master playback timestamp in microseconds.
    #[must_use]
    pub fn current_time_us(&self) -> i64 {
        let base = self.base_pts_us.load(Ordering::Acquire);
        let samples = self.samples_rendered.load(Ordering::Acquire);
        let rate_milli = u128::from(self.rate_milli.load(Ordering::Acquire));
        let delta_us = (u128::from(samples) * 1_000_000u128 * rate_milli)
            / (u128::from(self.sample_rate) * 1_000u128);
        base.saturating_add(delta_us as i64)
    }

    /// Returns the master playback timestamp in seconds.
    #[must_use]
    pub fn current_time_s(&self) -> f64 {
        self.current_time_us() as f64 / 1_000_000.0
    }

    /// Evaluates whether a video frame at `video_pts_us` should be presented now, delayed, or dropped
    /// to keep A/V sync strictly within `< 25 ms`, updating jitter telemetry.
    pub fn evaluate_video_frame(&self, video_pts_us: i64) -> AvSyncDecision {
        let audio_us = self.current_time_us();
        let drift_us = video_pts_us - audio_us;

        if drift_us > AV_SYNC_TOLERANCE_US {
            AvSyncDecision::Wait { delay_us: drift_us }
        } else if drift_us < -AV_SYNC_TOLERANCE_US {
            self.frames_dropped.fetch_add(1, Ordering::Relaxed);
            AvSyncDecision::Drop { late_us: -drift_us }
        } else {
            self.record_presented_drift(drift_us);
            AvSyncDecision::Present { drift_us }
        }
    }

    /// Records the absolute presentation drift for a presented video frame.
    pub fn record_presented_drift(&self, drift_us: i64) {
        let abs_us = drift_us.unsigned_abs();
        self.frames_presented.fetch_add(1, Ordering::Relaxed);
        let prev = self.jitter_ema_us.load(Ordering::Relaxed);
        let next = if prev == 0 {
            abs_us
        } else {
            (prev * 7 + abs_us) / 8
        };
        self.jitter_ema_us.store(next, Ordering::Relaxed);
        let _ = self.jitter_peak_us.fetch_max(abs_us, Ordering::Relaxed);
    }

    /// Returns smoothed A/V synchronization jitter in milliseconds for the `F12` HUD.
    #[must_use]
    pub fn jitter_ms(&self) -> f32 {
        (self.jitter_ema_us.load(Ordering::Relaxed) as f64 / 1_000.0) as f32
    }

    /// Returns peak observed A/V drift in milliseconds.
    #[must_use]
    pub fn peak_jitter_ms(&self) -> f32 {
        (self.jitter_peak_us.load(Ordering::Relaxed) as f64 / 1_000.0) as f32
    }

    /// Returns `(presented_frames, dropped_frames)`.
    #[must_use]
    pub fn frame_stats(&self) -> (u64, u64) {
        (
            self.frames_presented.load(Ordering::Relaxed),
            self.frames_dropped.load(Ordering::Relaxed),
        )
    }
}

/// Thread-safe interleaved `f32` PCM sample queue connecting decoded audio streams to `cpal`
/// output or a WebAudio `MediaElementAudioSourceNode`.
#[derive(Debug, Default)]
pub struct AudioSampleQueue {
    samples: Mutex<VecDeque<f32>>,
    max_samples: usize,
}

impl AudioSampleQueue {
    /// Creates a new queue capped at `max_samples` interleaved `f32` samples.
    #[must_use]
    pub fn new(max_samples: usize) -> Self {
        Self {
            samples: Mutex::new(VecDeque::with_capacity(max_samples.min(65_536))),
            max_samples: max_samples.max(1024),
        }
    }

    /// Enqueues interleaved `f32` samples from an audio decoder.
    pub fn push_interleaved(&self, pcm: &[f32]) {
        let mut q = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        let overflow = q
            .len()
            .saturating_add(pcm.len())
            .saturating_sub(self.max_samples);
        if overflow > 0 {
            let drop_n = overflow.min(q.len());
            q.drain(..drop_n);
        }
        q.extend(pcm.iter().copied());
    }

    /// Pulls up to `dst.len()` interleaved samples into `dst`, zero-filling any underrun tail.
    pub fn pop_into(&self, dst: &mut [f32]) -> usize {
        let mut q = self.samples.lock().unwrap_or_else(|e| e.into_inner());
        let mut read = 0usize;
        for slot in dst.iter_mut() {
            if let Some(s) = q.pop_front() {
                *slot = s;
                read += 1;
            } else {
                *slot = 0.0;
            }
        }
        read
    }

    /// Clears all queued samples (called on seek).
    pub fn clear(&self) {
        self.samples
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Returns the number of queued interleaved samples.
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Active audio output backend mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioOutputMode {
    /// Real OS audio device output via `cpal` (ALSA / PipeWire / PulseAudio / CoreAudio / WASAPI).
    HardwareCpal,
    /// Deterministic headless audio clock fallback when no OS output device is available.
    Headless,
}

/// Live audio output stream driven by `cpal` (with automatic headless fallback in CI environments).
pub struct AudioOutputDevice {
    _stream: Option<cpal::Stream>,
    pub mode: AudioOutputMode,
    pub sample_rate: u32,
    pub channels: u16,
    pub clock: Arc<MediaClock>,
}

impl AudioOutputDevice {
    /// Attempts to open the system's default `cpal` output stream driving `render_callback`
    /// and advancing `clock` on every rendered audio buffer. Falls back cleanly to
    /// [`AudioOutputMode::Headless`] when no hardware audio device exists.
    pub fn start_or_headless(
        requested_sample_rate: u32,
        requested_channels: u16,
        clock: Arc<MediaClock>,
        mut render_callback: impl FnMut(&mut [f32], u16) + Send + 'static,
    ) -> Self {
        let try_open = || -> Option<(cpal::Stream, u32, u16)> {
            let host = cpal::default_host();
            let device = host.default_output_device()?;
            let default_cfg = device.default_output_config().ok()?;
            let channels = default_cfg.channels().max(1);
            let sample_rate = default_cfg.sample_rate();
            let config = cpal::StreamConfig {
                channels,
                sample_rate,
                buffer_size: cpal::BufferSize::Default,
            };
            let clk = Arc::clone(&clock);
            let stream = device
                .build_output_stream(
                    config,
                    move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
                        let ch = channels.max(1);
                        if clk.is_playing() {
                            render_callback(out, ch);
                            let gain = clk.effective_gain();
                            if (gain - 1.0).abs() > 1e-4 {
                                for s in out.iter_mut() {
                                    *s *= gain;
                                }
                            }
                            let frames = (out.len() / usize::from(ch)) as u64;
                            clk.advance_audio_frames(frames);
                        } else {
                            out.fill(0.0);
                        }
                    },
                    |err| {
                        tracing::warn!("cpal audio stream error: {err}");
                    },
                    None,
                )
                .ok()?;
            stream.play().ok()?;
            Some((stream, sample_rate, channels))
        };

        if let Some((stream, sr, ch)) = try_open() {
            Self {
                _stream: Some(stream),
                mode: AudioOutputMode::HardwareCpal,
                sample_rate: sr,
                channels: ch,
                clock,
            }
        } else {
            Self {
                _stream: None,
                mode: AudioOutputMode::Headless,
                sample_rate: requested_sample_rate.max(8_000),
                channels: requested_channels.clamp(1, 2),
                clock,
            }
        }
    }

    /// Creates an explicit deterministic headless audio device (useful for offline tests and CI).
    #[must_use]
    pub fn headless(sample_rate: u32, channels: u16, clock: Arc<MediaClock>) -> Self {
        Self {
            _stream: None,
            mode: AudioOutputMode::Headless,
            sample_rate: sample_rate.max(8_000),
            channels: channels.clamp(1, 2),
            clock,
        }
    }
}

//! Pure-Rust W3C WebAudio DSP Graph engine.
//!
//! Implements 128-frame render-quantum graph evaluation with sample-accurate [`AudioParam`]
//! automation, [`AudioBuffer`], [`GainNode`], [`BiquadFilterNode`] (Audio EQ Cookbook, all 8
//! W3C filter types), [`AnalyserNode`] (real radix-2 Cooley-Tukey FFT + Blackman window + dBFS),
//! [`OscillatorNode`], [`AudioBufferSourceNode`], [`MediaElementAudioSourceNode`], and
//! [`AudioWorkletNode`].

use std::collections::{HashMap, VecDeque};
use std::f64::consts::PI;
use std::sync::Arc;

use crate::audio_clock::AudioSampleQueue;

/// Standard W3C WebAudio render quantum size (128 sample frames per block).
pub const RENDER_QUANTUM_FRAMES: usize = 128;

/// Unique identifier for a node inside an [`AudioGraph`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AudioNodeId(pub u32);

/// Multi-channel planar floating-point audio buffer (`AudioBuffer`).
#[derive(Debug, Clone)]
pub struct AudioBuffer {
    pub sample_rate: u32,
    pub channels: Vec<Vec<f32>>,
}

impl AudioBuffer {
    /// Creates a zero-initialized `AudioBuffer` with `channel_count` channels of `length` frames.
    #[must_use]
    pub fn new(channel_count: usize, length: usize, sample_rate: u32) -> Self {
        let ch = channel_count.max(1);
        Self {
            sample_rate: sample_rate.max(8_000),
            channels: (0..ch).map(|_| vec![0.0; length]).collect(),
        }
    }

    /// Creates an `AudioBuffer` by de-interleaving `interleaved` samples.
    #[must_use]
    pub fn from_interleaved(interleaved: &[f32], channel_count: usize, sample_rate: u32) -> Self {
        let ch = channel_count.max(1);
        let frames = interleaved.len() / ch;
        let mut channels = vec![vec![0.0f32; frames]; ch];
        for (frame_idx, chunk) in interleaved.chunks_exact(ch).enumerate() {
            for c in 0..ch {
                channels[c][frame_idx] = chunk[c];
            }
        }
        Self {
            sample_rate: sample_rate.max(8_000),
            channels,
        }
    }

    #[must_use]
    pub fn number_of_channels(&self) -> usize {
        self.channels.len()
    }

    #[must_use]
    pub fn length(&self) -> usize {
        self.channels.first().map_or(0, Vec::len)
    }

    #[must_use]
    pub fn duration(&self) -> f64 {
        self.length() as f64 / f64::from(self.sample_rate)
    }
}

/// Scheduled `AudioParam` automation event.
#[derive(Debug, Clone, Copy)]
pub enum ParamEvent {
    SetValueAtTime {
        value: f32,
        time: f64,
    },
    LinearRampToValueAtTime {
        value: f32,
        end_time: f64,
    },
    ExponentialRampToValueAtTime {
        value: f32,
        end_time: f64,
    },
    SetTargetAtTime {
        target: f32,
        start_time: f64,
        time_constant: f64,
    },
}

impl ParamEvent {
    fn time(&self) -> f64 {
        match *self {
            Self::SetValueAtTime { time, .. } => time,
            Self::LinearRampToValueAtTime { end_time, .. }
            | Self::ExponentialRampToValueAtTime { end_time, .. } => end_time,
            Self::SetTargetAtTime { start_time, .. } => start_time,
        }
    }
}

/// W3C `AudioParam` with base value, min/max clamping, and timeline automation curve evaluation.
#[derive(Debug, Clone)]
pub struct AudioParam {
    pub value: f32,
    pub default_value: f32,
    pub min_value: f32,
    pub max_value: f32,
    events: Vec<ParamEvent>,
}

impl AudioParam {
    #[must_use]
    pub fn new(default_value: f32, min_value: f32, max_value: f32) -> Self {
        Self {
            value: default_value,
            default_value,
            min_value,
            max_value,
            events: Vec::new(),
        }
    }

    pub fn set_value(&mut self, v: f32) {
        self.value = v.clamp(self.min_value, self.max_value);
    }

    pub fn set_value_at_time(&mut self, value: f32, start_time: f64) {
        self.insert_event(ParamEvent::SetValueAtTime {
            value: value.clamp(self.min_value, self.max_value),
            time: start_time.max(0.0),
        });
    }

    pub fn linear_ramp_to_value_at_time(&mut self, value: f32, end_time: f64) {
        self.insert_event(ParamEvent::LinearRampToValueAtTime {
            value: value.clamp(self.min_value, self.max_value),
            end_time: end_time.max(0.0),
        });
    }

    pub fn exponential_ramp_to_value_at_time(&mut self, value: f32, end_time: f64) {
        self.insert_event(ParamEvent::ExponentialRampToValueAtTime {
            value: value.clamp(self.min_value, self.max_value),
            end_time: end_time.max(0.0),
        });
    }

    pub fn set_target_at_time(&mut self, target: f32, start_time: f64, time_constant: f64) {
        self.insert_event(ParamEvent::SetTargetAtTime {
            target: target.clamp(self.min_value, self.max_value),
            start_time: start_time.max(0.0),
            time_constant: time_constant.max(1e-6),
        });
    }

    pub fn cancel_scheduled_values(&mut self, start_time: f64) {
        self.events.retain(|e| e.time() < start_time);
    }

    fn insert_event(&mut self, ev: ParamEvent) {
        self.events.push(ev);
        self.events.sort_by(|a, b| a.time().total_cmp(&b.time()));
    }

    /// Evaluates the automated parameter value at exact timeline instant `t` (in seconds).
    #[must_use]
    pub fn value_at(&self, t: f64) -> f32 {
        if self.events.is_empty() {
            return self.value;
        }
        let mut cur_val = self.value;
        let mut cur_time = 0.0f64;

        for &ev in &self.events {
            match ev {
                ParamEvent::SetValueAtTime { value, time } => {
                    if t < time {
                        return cur_val;
                    }
                    cur_val = value;
                    cur_time = time;
                }
                ParamEvent::LinearRampToValueAtTime { value, end_time } => {
                    if t <= end_time {
                        let span = (end_time - cur_time).max(1e-9);
                        let frac = ((t - cur_time) / span).clamp(0.0, 1.0) as f32;
                        return cur_val + (value - cur_val) * frac;
                    }
                    cur_val = value;
                    cur_time = end_time;
                }
                ParamEvent::ExponentialRampToValueAtTime { value, end_time } => {
                    if t <= end_time {
                        let span = (end_time - cur_time).max(1e-9);
                        let frac = ((t - cur_time) / span).clamp(0.0, 1.0);
                        if cur_val.abs() > 1e-6
                            && value.abs() > 1e-6
                            && cur_val.signum() == value.signum()
                        {
                            let ratio = f64::from(value / cur_val);
                            return (f64::from(cur_val) * ratio.powf(frac)) as f32;
                        }
                        return cur_val + (value - cur_val) * (frac as f32);
                    }
                    cur_val = value;
                    cur_time = end_time;
                }
                ParamEvent::SetTargetAtTime {
                    target,
                    start_time,
                    time_constant,
                } => {
                    if t < start_time {
                        return cur_val;
                    }
                    let exp = (-((t - start_time) / time_constant)).exp() as f32;
                    cur_val = target + (cur_val - target) * exp;
                    cur_time = t;
                }
            }
        }
        cur_val.clamp(self.min_value, self.max_value)
    }
}

/// W3C `OscillatorType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OscillatorType {
    Sine,
    Square,
    Sawtooth,
    Triangle,
}

impl OscillatorType {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sine" => Some(Self::Sine),
            "square" => Some(Self::Square),
            "sawtooth" => Some(Self::Sawtooth),
            "triangle" => Some(Self::Triangle),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sine => "sine",
            Self::Square => "square",
            Self::Sawtooth => "sawtooth",
            Self::Triangle => "triangle",
        }
    }
}

/// W3C `BiquadFilterType` (all 8 standard Audio EQ Cookbook filter types).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BiquadFilterType {
    Lowpass,
    Highpass,
    Bandpass,
    Lowshelf,
    Highshelf,
    Peaking,
    Notch,
    Allpass,
}

impl BiquadFilterType {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "lowpass" => Some(Self::Lowpass),
            "highpass" => Some(Self::Highpass),
            "bandpass" => Some(Self::Bandpass),
            "lowshelf" => Some(Self::Lowshelf),
            "highshelf" => Some(Self::Highshelf),
            "peaking" => Some(Self::Peaking),
            "notch" => Some(Self::Notch),
            "allpass" => Some(Self::Allpass),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lowpass => "lowpass",
            Self::Highpass => "highpass",
            Self::Bandpass => "bandpass",
            Self::Lowshelf => "lowshelf",
            Self::Highshelf => "highshelf",
            Self::Peaking => "peaking",
            Self::Notch => "notch",
            Self::Allpass => "allpass",
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct BiquadChannelState {
    z1: f64,
    z2: f64,
}

/// Direct Form II Transposed Biquad Filter node (`BiquadFilterNode`).
#[derive(Debug, Clone)]
pub struct BiquadFilterState {
    pub filter_type: BiquadFilterType,
    pub frequency: AudioParam,
    pub detune: AudioParam,
    pub q: AudioParam,
    pub gain: AudioParam,
    channels: [BiquadChannelState; 2],
}

impl BiquadFilterState {
    fn new(sample_rate: u32) -> Self {
        let nyquist = (sample_rate as f32) * 0.5;
        Self {
            filter_type: BiquadFilterType::Lowpass,
            frequency: AudioParam::new(350.0, 10.0, nyquist),
            detune: AudioParam::new(0.0, -153_600.0, 153_600.0),
            q: AudioParam::new(1.0, 0.0001, 1_000.0),
            gain: AudioParam::new(0.0, -40.0, 40.0),
            channels: [BiquadChannelState::default(); 2],
        }
    }

    /// Computes normalized Audio EQ Cookbook coefficients `(b0, b1, b2, a1, a2)` at instant `t`.
    fn coefficients(&self, sample_rate: f64, t: f64) -> (f64, f64, f64, f64, f64) {
        let base_freq = f64::from(self.frequency.value_at(t));
        let detune_cents = f64::from(self.detune.value_at(t));
        let computed_freq =
            (base_freq * 2.0f64.powf(detune_cents / 1200.0)).clamp(1.0, sample_rate * 0.499);
        let q = f64::from(self.q.value_at(t)).max(1e-4);
        let gain_db = f64::from(self.gain.value_at(t));

        let w0 = 2.0 * PI * computed_freq / sample_rate;
        let cos_w0 = w0.cos();
        let sin_w0 = w0.sin();
        let a_lin = 10.0f64.powf(gain_db / 40.0);

        let (b0, b1, b2, a0, a1, a2) = match self.filter_type {
            BiquadFilterType::Lowpass => {
                // W3C WebAudio: Q is in dB for lowpass/highpass (`alpha = sin(w0)/(2*10^(Q/20))`),
                // matching Chromium/Firefox/Audio EQ Cookbook resonance specification.
                let q_lin = 10.0f64.powf(q / 20.0);
                let alpha = sin_w0 / (2.0 * q_lin.max(1e-4));
                (
                    (1.0 - cos_w0) * 0.5,
                    1.0 - cos_w0,
                    (1.0 - cos_w0) * 0.5,
                    1.0 + alpha,
                    -2.0 * cos_w0,
                    1.0 - alpha,
                )
            }
            BiquadFilterType::Highpass => {
                let q_lin = 10.0f64.powf(q / 20.0);
                let alpha = sin_w0 / (2.0 * q_lin.max(1e-4));
                (
                    (1.0 + cos_w0) * 0.5,
                    -(1.0 + cos_w0),
                    (1.0 + cos_w0) * 0.5,
                    1.0 + alpha,
                    -2.0 * cos_w0,
                    1.0 - alpha,
                )
            }
            BiquadFilterType::Bandpass => {
                let alpha = sin_w0 / (2.0 * q);
                (alpha, 0.0, -alpha, 1.0 + alpha, -2.0 * cos_w0, 1.0 - alpha)
            }
            BiquadFilterType::Notch => {
                let alpha = sin_w0 / (2.0 * q);
                (
                    1.0,
                    -2.0 * cos_w0,
                    1.0,
                    1.0 + alpha,
                    -2.0 * cos_w0,
                    1.0 - alpha,
                )
            }
            BiquadFilterType::Allpass => {
                let alpha = sin_w0 / (2.0 * q);
                (
                    1.0 - alpha,
                    -2.0 * cos_w0,
                    1.0 + alpha,
                    1.0 + alpha,
                    -2.0 * cos_w0,
                    1.0 - alpha,
                )
            }
            BiquadFilterType::Peaking => {
                let alpha = sin_w0 / (2.0 * q);
                (
                    1.0 + alpha * a_lin,
                    -2.0 * cos_w0,
                    1.0 - alpha * a_lin,
                    1.0 + alpha / a_lin,
                    -2.0 * cos_w0,
                    1.0 - alpha / a_lin,
                )
            }
            BiquadFilterType::Lowshelf => {
                let alpha = sin_w0 * 0.5 * 2.0f64.sqrt();
                let two_sqrt_a_alpha = 2.0 * a_lin.sqrt() * alpha;
                (
                    a_lin * ((a_lin + 1.0) - (a_lin - 1.0) * cos_w0 + two_sqrt_a_alpha),
                    2.0 * a_lin * ((a_lin - 1.0) - (a_lin + 1.0) * cos_w0),
                    a_lin * ((a_lin + 1.0) - (a_lin - 1.0) * cos_w0 - two_sqrt_a_alpha),
                    (a_lin + 1.0) + (a_lin - 1.0) * cos_w0 + two_sqrt_a_alpha,
                    -2.0 * ((a_lin - 1.0) + (a_lin + 1.0) * cos_w0),
                    (a_lin + 1.0) + (a_lin - 1.0) * cos_w0 - two_sqrt_a_alpha,
                )
            }
            BiquadFilterType::Highshelf => {
                let alpha = sin_w0 * 0.5 * 2.0f64.sqrt();
                let two_sqrt_a_alpha = 2.0 * a_lin.sqrt() * alpha;
                (
                    a_lin * ((a_lin + 1.0) + (a_lin - 1.0) * cos_w0 + two_sqrt_a_alpha),
                    -2.0 * a_lin * ((a_lin - 1.0) + (a_lin + 1.0) * cos_w0),
                    a_lin * ((a_lin + 1.0) + (a_lin - 1.0) * cos_w0 - two_sqrt_a_alpha),
                    (a_lin + 1.0) - (a_lin - 1.0) * cos_w0 + two_sqrt_a_alpha,
                    2.0 * ((a_lin - 1.0) - (a_lin + 1.0) * cos_w0),
                    (a_lin + 1.0) - (a_lin - 1.0) * cos_w0 - two_sqrt_a_alpha,
                )
            }
        };

        let inv_a0 = 1.0 / a0;
        (
            b0 * inv_a0,
            b1 * inv_a0,
            b2 * inv_a0,
            a1 * inv_a0,
            a2 * inv_a0,
        )
    }

    fn process(&mut self, block: &mut [Vec<f32>], sample_rate: f64, block_start_t: f64) {
        let (b0, b1, b2, a1, a2) = self.coefficients(sample_rate, block_start_t);
        for (ch_idx, chan) in block.iter_mut().enumerate().take(2) {
            let st = &mut self.channels[ch_idx];
            for sample in chan.iter_mut() {
                let x = f64::from(*sample);
                let y = b0 * x + st.z1;
                st.z1 = b1 * x - a1 * y + st.z2;
                st.z2 = b2 * x - a2 * y;
                *sample = y as f32;
            }
        }
    }
}

/// Real-time FFT spectrum and time-domain waveform analyser (`AnalyserNode`).
#[derive(Debug, Clone)]
pub struct AnalyserState {
    pub fft_size: usize,
    pub min_decibels: f32,
    pub max_decibels: f32,
    pub smoothing_time_constant: f32,
    ring: VecDeque<f32>,
    smoothed_magnitudes: Vec<f32>,
}

impl AnalyserState {
    #[must_use]
    pub fn new() -> Self {
        let fft_size = 2048;
        Self {
            fft_size,
            min_decibels: -100.0,
            max_decibels: -30.0,
            smoothing_time_constant: 0.8,
            ring: VecDeque::from(vec![0.0f32; fft_size]),
            smoothed_magnitudes: vec![0.0; fft_size / 2],
        }
    }

    pub fn set_fft_size(&mut self, size: usize) -> Result<(), &'static str> {
        if !(32..=32_768).contains(&size) || !size.is_power_of_two() {
            return Err("fftSize must be a power of 2 in [32, 32768]");
        }
        self.fft_size = size;
        self.ring = VecDeque::from(vec![0.0f32; size]);
        self.smoothed_magnitudes = vec![0.0; size / 2];
        Ok(())
    }

    #[must_use]
    pub fn frequency_bin_count(&self) -> usize {
        self.fft_size / 2
    }

    fn ingest(&mut self, block: &[Vec<f32>]) {
        let frames = block.first().map_or(0, Vec::len);
        let ch_count = block.len().max(1) as f32;
        for i in 0..frames {
            let mut mono = 0.0f32;
            for ch in block {
                mono += ch.get(i).copied().unwrap_or(0.0);
            }
            mono /= ch_count;
            if self.ring.len() >= self.fft_size {
                self.ring.pop_front();
            }
            self.ring.push_back(mono);
        }
    }

    /// Computes latest time-domain waveform samples (`fft_size` floats in `[-1.0, 1.0]`).
    pub fn get_float_time_domain_data(&self, out: &mut [f32]) {
        let n = out.len().min(self.ring.len());
        let start = self.ring.len().saturating_sub(n);
        for (dst, &src) in out[..n].iter_mut().zip(self.ring.iter().skip(start)) {
            *dst = src;
        }
    }

    /// Computes latest time-domain waveform as unsigned bytes (`128` = 0.0).
    pub fn get_byte_time_domain_data(&self, out: &mut [u8]) {
        let mut tmp = vec![0.0f32; out.len()];
        self.get_float_time_domain_data(&mut tmp);
        for (dst, &s) in out.iter_mut().zip(&tmp) {
            let v = ((s + 1.0) * 128.0).floor().clamp(0.0, 255.0);
            *dst = v as u8;
        }
    }

    /// Computes FFT magnitude spectrum in dBFS (with W3C Blackman window & exponential smoothing).
    pub fn get_float_frequency_data(&mut self, out: &mut [f32]) {
        let n = self.fft_size;
        let bins = n / 2;
        let mut re = vec![0.0f64; n];
        let mut im = vec![0.0f64; n];

        // W3C WebAudio §1.17 Blackman window (`alpha = 0.16`).
        let a0 = 0.42;
        let a1 = 0.50;
        let a2 = 0.08;
        let denom = (n.saturating_sub(1)).max(1) as f64;
        let ring_start = self.ring.len().saturating_sub(n);
        for (i, &sample) in self.ring.iter().skip(ring_start).take(n).enumerate() {
            let phase = 2.0 * PI * (i as f64) / denom;
            let w = a0 - a1 * phase.cos() + a2 * (2.0 * phase).cos();
            re[i] = f64::from(sample) * w;
        }

        radix2_fft(&mut re, &mut im);

        let k = self.smoothing_time_constant.clamp(0.0, 0.9999);
        if self.smoothed_magnitudes.len() != bins {
            self.smoothed_magnitudes.resize(bins, 0.0);
        }
        let inv_n = 1.0 / (n as f64);
        for i in 0..bins {
            let mag = ((re[i] * re[i] + im[i] * im[i]).sqrt() * inv_n) as f32;
            let smoothed = k * self.smoothed_magnitudes[i] + (1.0 - k) * mag;
            self.smoothed_magnitudes[i] = smoothed;
            if i < out.len() {
                out[i] = if smoothed <= 1e-12 {
                    -160.0
                } else {
                    20.0 * smoothed.log10()
                };
            }
        }
    }

    /// Computes FFT magnitude spectrum mapped into `0..=255` over `[min_decibels, max_decibels]`.
    pub fn get_byte_frequency_data(&mut self, out: &mut [u8]) {
        let mut db = vec![-160.0f32; out.len()];
        self.get_float_frequency_data(&mut db);
        let span = (self.max_decibels - self.min_decibels).max(1e-4);
        for (dst, &val) in out.iter_mut().zip(&db) {
            let norm = ((val - self.min_decibels) / span).clamp(0.0, 1.0);
            *dst = (norm * 255.0).round() as u8;
        }
    }
}

impl Default for AnalyserState {
    fn default() -> Self {
        Self::new()
    }
}

/// Pure-Rust in-place iterative radix-2 Cooley-Tukey FFT (`n` must be a power of 2).
fn radix2_fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    debug_assert!(n.is_power_of_two());
    // Bit-reversal permutation.
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while (j & bit) != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    let mut len = 2usize;
    while len <= n {
        let half = len >> 1;
        let angle = -2.0 * PI / (len as f64);
        let wlen_re = angle.cos();
        let wlen_im = angle.sin();
        let mut start = 0usize;
        while start < n {
            let mut w_re = 1.0f64;
            let mut w_im = 0.0f64;
            for k in 0..half {
                let u_re = re[start + k];
                let u_im = im[start + k];
                let v_re = re[start + k + half] * w_re - im[start + k + half] * w_im;
                let v_im = re[start + k + half] * w_im + im[start + k + half] * w_re;
                re[start + k] = u_re + v_re;
                im[start + k] = u_im + v_im;
                re[start + k + half] = u_re - v_re;
                im[start + k + half] = u_im - v_im;
                let next_re = w_re * wlen_re - w_im * wlen_im;
                w_im = w_re * wlen_im + w_im * wlen_re;
                w_re = next_re;
            }
            start += len;
        }
        len <<= 1;
    }
}

/// Callback signature for [`AudioNodeKind::Worklet`] DSP processors.
pub type WorkletProcessorFn = Box<dyn FnMut(&[Vec<f32>], &mut [Vec<f32>], u64, f64) + Send>;

/// Concrete WebAudio node variant inside an [`AudioGraph`].
pub enum AudioNodeKind {
    Destination,
    Gain {
        gain: AudioParam,
    },
    BiquadFilter(BiquadFilterState),
    Analyser(AnalyserState),
    Oscillator {
        osc_type: OscillatorType,
        frequency: AudioParam,
        detune: AudioParam,
        phase: f64,
        start_time: Option<f64>,
        stop_time: Option<f64>,
    },
    BufferSource {
        buffer: Option<Arc<AudioBuffer>>,
        playback_rate: AudioParam,
        loop_enabled: bool,
        loop_start: f64,
        loop_end: f64,
        start_time: Option<f64>,
        stop_time: Option<f64>,
        cursor: f64,
        offset_s: f64,
        duration_s: Option<f64>,
        started_once: bool,
    },
    MediaElementSource {
        queue: Arc<AudioSampleQueue>,
        source_channels: u16,
    },
    Worklet {
        name: String,
        processor: WorkletProcessorFn,
    },
}

struct GraphNode {
    kind: AudioNodeKind,
    inputs: Vec<AudioNodeId>,
    outputs: Vec<AudioNodeId>,
}

/// Complete W3C WebAudio DSP graph supporting real-time rendering (`AudioContext`) and
/// deterministic offline rendering (`OfflineAudioContext`).
pub struct AudioGraph {
    sample_rate: u32,
    channels: usize,
    current_frame: u64,
    next_id: u32,
    destination_id: AudioNodeId,
    nodes: HashMap<AudioNodeId, GraphNode>,
}

impl AudioGraph {
    /// Creates a new WebAudio DSP graph at `sample_rate` Hz and `channels` output channels.
    #[must_use]
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        let dest = AudioNodeId(0);
        let mut nodes = HashMap::new();
        nodes.insert(
            dest,
            GraphNode {
                kind: AudioNodeKind::Destination,
                inputs: Vec::new(),
                outputs: Vec::new(),
            },
        );
        Self {
            sample_rate: sample_rate.max(8_000),
            channels: channels.clamp(1, 2),
            current_frame: 0,
            next_id: 1,
            destination_id: dest,
            nodes,
        }
    }

    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    #[must_use]
    pub fn channels(&self) -> usize {
        self.channels
    }

    #[must_use]
    pub fn current_time(&self) -> f64 {
        self.current_frame as f64 / f64::from(self.sample_rate)
    }

    #[must_use]
    pub fn destination(&self) -> AudioNodeId {
        self.destination_id
    }

    fn alloc_node(&mut self, kind: AudioNodeKind) -> AudioNodeId {
        let id = AudioNodeId(self.next_id);
        self.next_id += 1;
        self.nodes.insert(
            id,
            GraphNode {
                kind,
                inputs: Vec::new(),
                outputs: Vec::new(),
            },
        );
        id
    }

    pub fn create_gain(&mut self) -> AudioNodeId {
        self.alloc_node(AudioNodeKind::Gain {
            gain: AudioParam::new(1.0, -3.4e38, 3.4e38),
        })
    }

    pub fn create_biquad_filter(&mut self) -> AudioNodeId {
        self.alloc_node(AudioNodeKind::BiquadFilter(BiquadFilterState::new(
            self.sample_rate,
        )))
    }

    pub fn create_analyser(&mut self) -> AudioNodeId {
        self.alloc_node(AudioNodeKind::Analyser(AnalyserState::new()))
    }

    pub fn create_oscillator(&mut self) -> AudioNodeId {
        let nyq = self.sample_rate as f32 * 0.5;
        self.alloc_node(AudioNodeKind::Oscillator {
            osc_type: OscillatorType::Sine,
            frequency: AudioParam::new(440.0, -nyq, nyq),
            detune: AudioParam::new(0.0, -153_600.0, 153_600.0),
            phase: 0.0,
            start_time: None,
            stop_time: None,
        })
    }

    pub fn create_buffer_source(&mut self) -> AudioNodeId {
        self.alloc_node(AudioNodeKind::BufferSource {
            buffer: None,
            playback_rate: AudioParam::new(1.0, 0.001, 1024.0),
            loop_enabled: false,
            loop_start: 0.0,
            loop_end: 0.0,
            start_time: None,
            stop_time: None,
            cursor: 0.0,
            offset_s: 0.0,
            duration_s: None,
            started_once: false,
        })
    }

    pub fn create_media_element_source(
        &mut self,
        queue: Arc<AudioSampleQueue>,
        source_channels: u16,
    ) -> AudioNodeId {
        self.alloc_node(AudioNodeKind::MediaElementSource {
            queue,
            source_channels: source_channels.clamp(1, 2),
        })
    }

    pub fn create_worklet(&mut self, name: String, processor: WorkletProcessorFn) -> AudioNodeId {
        self.alloc_node(AudioNodeKind::Worklet { name, processor })
    }

    pub fn node(&self, id: AudioNodeId) -> Option<&AudioNodeKind> {
        self.nodes.get(&id).map(|n| &n.kind)
    }

    pub fn node_mut(&mut self, id: AudioNodeId) -> Option<&mut AudioNodeKind> {
        self.nodes.get_mut(&id).map(|n| &mut n.kind)
    }

    /// Connects output of `src` to input of `dst` (idempotent).
    pub fn connect(&mut self, src: AudioNodeId, dst: AudioNodeId) -> bool {
        if src == dst || !self.nodes.contains_key(&src) || !self.nodes.contains_key(&dst) {
            return false;
        }
        if let Some(s) = self.nodes.get_mut(&src) {
            if !s.outputs.contains(&dst) {
                s.outputs.push(dst);
            }
        }
        if let Some(d) = self.nodes.get_mut(&dst) {
            if !d.inputs.contains(&src) {
                d.inputs.push(src);
            }
        }
        true
    }

    /// Disconnects `src` from `dst`, or from all destinations when `dst` is `None`.
    pub fn disconnect(&mut self, src: AudioNodeId, dst: Option<AudioNodeId>) {
        let targets: Vec<AudioNodeId> = match dst {
            Some(d) => vec![d],
            None => self
                .nodes
                .get(&src)
                .map(|n| n.outputs.clone())
                .unwrap_or_default(),
        };
        if let Some(s) = self.nodes.get_mut(&src) {
            s.outputs.retain(|o| !targets.contains(o));
        }
        for t in targets {
            if let Some(d) = self.nodes.get_mut(&t) {
                d.inputs.retain(|&i| i != src);
            }
        }
    }

    /// Renders `frame_count` frames (in 128-frame quanta) into interleaved stereo/mono `out`.
    pub fn render_interleaved(&mut self, out: &mut [f32]) {
        let ch = self.channels;
        let total_frames = out.len() / ch;
        let mut rendered = 0usize;
        while rendered < total_frames {
            let block_frames = (total_frames - rendered).min(RENDER_QUANTUM_FRAMES);
            let planar = self.render_quantum(block_frames);
            for f in 0..block_frames {
                for c in 0..ch {
                    let s = planar
                        .get(c)
                        .and_then(|v| v.get(f))
                        .copied()
                        .unwrap_or(0.0)
                        .clamp(-1.0, 1.0);
                    out[(rendered + f) * ch + c] = s;
                }
            }
            rendered += block_frames;
        }
    }

    /// Deterministically renders `total_frames` into an [`AudioBuffer`] (backs `OfflineAudioContext`).
    #[must_use]
    pub fn render_offline(&mut self, total_frames: usize) -> AudioBuffer {
        let mut buf = AudioBuffer::new(self.channels, total_frames, self.sample_rate);
        let mut offset = 0usize;
        while offset < total_frames {
            let n = (total_frames - offset).min(RENDER_QUANTUM_FRAMES);
            let block = self.render_quantum(n);
            for c in 0..self.channels {
                buf.channels[c][offset..offset + n].copy_from_slice(&block[c][..n]);
            }
            offset += n;
        }
        buf
    }

    /// Evaluates one render quantum (`frames <= 128`) pulling from `destination_id` in cycle-safe
    /// reverse-dependency order.
    fn render_quantum(&mut self, frames: usize) -> Vec<Vec<f32>> {
        let mut cache: HashMap<AudioNodeId, Vec<Vec<f32>>> = HashMap::new();
        let mut visiting: Vec<AudioNodeId> = Vec::new();
        let out = self.eval_node(self.destination_id, frames, &mut cache, &mut visiting);
        self.current_frame = self.current_frame.saturating_add(frames as u64);
        out
    }

    fn eval_node(
        &mut self,
        id: AudioNodeId,
        frames: usize,
        cache: &mut HashMap<AudioNodeId, Vec<Vec<f32>>>,
        visiting: &mut Vec<AudioNodeId>,
    ) -> Vec<Vec<f32>> {
        if let Some(existing) = cache.get(&id) {
            return existing.clone();
        }
        if visiting.contains(&id) {
            // Cycle guard: break feedback cycles with silence for this quantum.
            return vec![vec![0.0; frames]; self.channels];
        }
        visiting.push(id);

        let inputs = self
            .nodes
            .get(&id)
            .map(|n| n.inputs.clone())
            .unwrap_or_default();

        let mut summed = vec![vec![0.0f32; frames]; self.channels];
        for src in inputs {
            let src_buf = self.eval_node(src, frames, cache, visiting);
            for (c, dst_ch) in summed.iter_mut().enumerate().take(self.channels) {
                let s_ch = src_buf.get(c).or_else(|| src_buf.first());
                if let Some(s) = s_ch {
                    for (dst, &sv) in dst_ch.iter_mut().zip(s.iter()).take(frames) {
                        *dst += sv;
                    }
                }
            }
        }

        let sr = f64::from(self.sample_rate);
        let t0 = self.current_frame as f64 / sr;
        let cur_frame = self.current_frame;
        let ch_count = self.channels;

        let output = if let Some(node) = self.nodes.get_mut(&id) {
            match &mut node.kind {
                AudioNodeKind::Destination => summed,
                AudioNodeKind::Gain { gain } => {
                    for i in 0..frames {
                        let g = gain.value_at(t0 + (i as f64) / sr);
                        for ch in &mut summed {
                            ch[i] *= g;
                        }
                    }
                    summed
                }
                AudioNodeKind::BiquadFilter(bf) => {
                    bf.process(&mut summed, sr, t0);
                    summed
                }
                AudioNodeKind::Analyser(an) => {
                    an.ingest(&summed);
                    summed
                }
                AudioNodeKind::Oscillator {
                    osc_type,
                    frequency,
                    detune,
                    phase,
                    start_time,
                    stop_time,
                } => {
                    let mut out = vec![vec![0.0f32; frames]; ch_count];
                    if let Some(st) = *start_time {
                        for i in 0..frames {
                            let t = t0 + (i as f64) / sr;
                            if t < st || stop_time.is_some_and(|et| t >= et) {
                                continue;
                            }
                            let f = f64::from(frequency.value_at(t));
                            let det = f64::from(detune.value_at(t));
                            let hz = f * 2.0f64.powf(det / 1200.0);
                            let sample = match *osc_type {
                                OscillatorType::Sine => (2.0 * PI * *phase).sin() as f32,
                                OscillatorType::Square => {
                                    if *phase < 0.5 {
                                        1.0
                                    } else {
                                        -1.0
                                    }
                                }
                                OscillatorType::Sawtooth => (2.0 * *phase - 1.0) as f32,
                                OscillatorType::Triangle => {
                                    (4.0 * (*phase - (*phase + 0.5).floor()).abs() - 1.0) as f32
                                }
                            };
                            *phase = (*phase + hz / sr).rem_euclid(1.0);
                            for ch in &mut out {
                                ch[i] = sample;
                            }
                        }
                    }
                    out
                }
                AudioNodeKind::BufferSource {
                    buffer,
                    playback_rate,
                    loop_enabled,
                    loop_start,
                    loop_end,
                    start_time,
                    stop_time,
                    cursor,
                    offset_s,
                    duration_s,
                    started_once,
                } => {
                    let mut out = vec![vec![0.0f32; frames]; ch_count];
                    if let (Some(buf), Some(st)) = (buffer.as_ref(), *start_time) {
                        let buf_len = buf.length();
                        let buf_sr = f64::from(buf.sample_rate);
                        if buf_len > 0 {
                            if !*started_once {
                                *cursor = (*offset_s * buf_sr).clamp(0.0, buf_len as f64);
                                *started_once = true;
                            }
                            let eff_stop = match (*stop_time, *duration_s) {
                                (Some(a), Some(d)) => Some(a.min(st + d)),
                                (Some(a), None) => Some(a),
                                (None, Some(d)) => Some(st + d),
                                (None, None) => None,
                            };
                            let l_start = (*loop_start * buf_sr).clamp(0.0, buf_len as f64);
                            let l_end = if *loop_end > *loop_start {
                                (*loop_end * buf_sr).clamp(l_start + 1.0, buf_len as f64)
                            } else {
                                buf_len as f64
                            };
                            for i in 0..frames {
                                let t = t0 + (i as f64) / sr;
                                if t < st || eff_stop.is_some_and(|et| t >= et) {
                                    continue;
                                }
                                if *cursor >= buf_len as f64 {
                                    if *loop_enabled && l_end > l_start {
                                        *cursor = l_start + (*cursor - l_start) % (l_end - l_start);
                                    } else {
                                        break;
                                    }
                                }
                                let idx0 = (*cursor).floor() as usize;
                                let idx1 = (idx0 + 1).min(buf_len - 1);
                                let frac = (*cursor - idx0 as f64) as f32;
                                for (c, dst_ch) in out.iter_mut().enumerate().take(ch_count) {
                                    let src_ch = &buf.channels[c.min(buf.channels.len() - 1)];
                                    dst_ch[i] = src_ch[idx0] * (1.0 - frac) + src_ch[idx1] * frac;
                                }
                                let rate = f64::from(playback_rate.value_at(t));
                                *cursor += rate * (buf_sr / sr);
                                if *loop_enabled && *cursor >= l_end && l_end > l_start {
                                    *cursor = l_start + (*cursor - l_start) % (l_end - l_start);
                                }
                            }
                        }
                    }
                    out
                }
                AudioNodeKind::MediaElementSource {
                    queue,
                    source_channels,
                } => {
                    let sch = usize::from((*source_channels).max(1));
                    let mut interleaved = vec![0.0f32; frames * sch];
                    queue.pop_into(&mut interleaved);
                    let mut out = vec![vec![0.0f32; frames]; ch_count];
                    for f in 0..frames {
                        for c in 0..ch_count {
                            out[c][f] = interleaved[f * sch + c.min(sch - 1)];
                        }
                    }
                    out
                }
                AudioNodeKind::Worklet { processor, .. } => {
                    let mut out = vec![vec![0.0f32; frames]; ch_count];
                    processor(&summed, &mut out, cur_frame, t0);
                    out
                }
            }
        } else {
            summed
        };

        visiting.pop();
        cache.insert(id, output.clone());
        output
    }
}

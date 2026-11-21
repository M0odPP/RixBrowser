//! Built-in `F12` Developer HUD telemetry state: live FPS, 120-frame rolling
//! stage breakdown (`Style | Layout | Paint | IPC | GPU ms`), Content PID & RSS MB,
//! idle-wakeup indicator (`IDLE 0 Hz`), Network waterfall, Console, and
//! deduplicated Missing-Feature Telemetry.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use rix_css::MissingCssFeature;

/// Rolling history capacity for the live frame-time graph.
pub const MAX_FRAME_SAMPLES: usize = 120;

/// Per-stage millisecond timing breakdown for one rendered frame.
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameStageSample {
    pub style_ms: f32,
    pub layout_ms: f32,
    pub paint_ms: f32,
    pub ipc_ms: f32,
    pub gpu_ms: f32,
    pub total_ms: f32,
}

/// Network request waterfall entry.
#[derive(Debug, Clone)]
pub struct NetworkEntry {
    pub url: String,
    pub method: String,
    pub protocol: String,
    pub status: u16,
    pub mime: String,
    pub bytes: usize,
    pub cache_hit: bool,
    pub ttfb_ms: f32,
    pub total_ms: f32,
}

/// Deduplicated missing CSS or Web API feature requested by the active page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingFeatureEntry {
    pub category: String,
    pub name: String,
    pub count: u32,
}

/// Console log/warning/error record displayed in the HUD Console tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsoleEntry {
    pub level: String,
    pub message: String,
    pub source: String,
}

/// Active sub-panel inside the `F12` Developer HUD.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HudTab {
    #[default]
    Performance,
    Network,
    MissingFeatures,
    Console,
}

/// Complete telemetry state backing the `F12` Developer HUD.
#[derive(Debug, Clone)]
pub struct DevHudState {
    pub active_tab: HudTab,
    pub samples: VecDeque<FrameStageSample>,
    pub frame_timestamps: VecDeque<Instant>,
    pub last_frame_at: Option<Instant>,
    pub content_pid: Option<u32>,
    pub content_rss_mb: f32,
    pub network: Vec<NetworkEntry>,
    pub missing_features: Vec<MissingFeatureEntry>,
    pub console: Vec<ConsoleEntry>,
}

impl Default for DevHudState {
    fn default() -> Self {
        Self::new()
    }
}

impl DevHudState {
    #[must_use]
    pub fn new() -> Self {
        Self {
            active_tab: HudTab::Performance,
            samples: VecDeque::with_capacity(MAX_FRAME_SAMPLES),
            frame_timestamps: VecDeque::with_capacity(MAX_FRAME_SAMPLES),
            last_frame_at: None,
            content_pid: Some(std::process::id()),
            content_rss_mb: read_process_rss_mb(std::process::id()).unwrap_or(0.0),
            network: Vec::new(),
            missing_features: Vec::new(),
            console: Vec::new(),
        }
    }

    /// Records a newly presented frame and refreshes FPS/RSS telemetry.
    pub fn record_frame(&mut self, sample: FrameStageSample) {
        self.record_frame_at(sample, Instant::now());
    }

    pub fn record_frame_at(&mut self, sample: FrameStageSample, now: Instant) {
        if self.samples.len() >= MAX_FRAME_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);

        self.frame_timestamps.push_back(now);
        while self
            .frame_timestamps
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) > Duration::from_secs(1))
        {
            self.frame_timestamps.pop_front();
        }
        self.last_frame_at = Some(now);
        if let Some(pid) = self.content_pid {
            if let Some(rss) = read_process_rss_mb(pid) {
                self.content_rss_mb = rss;
            }
        }
    }

    pub fn set_content_pid(&mut self, pid: Option<u32>) {
        self.content_pid = pid;
        self.content_rss_mb = pid.and_then(read_process_rss_mb).unwrap_or(0.0);
    }

    /// Returns `true` when no frame has been presented in the last 350ms (`IDLE 0 Hz`).
    #[must_use]
    pub fn is_idle(&self, now: Instant) -> bool {
        match self.last_frame_at {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= Duration::from_millis(350),
        }
    }

    /// Current rolling FPS (0 when idle).
    #[must_use]
    pub fn current_fps(&self, now: Instant) -> u32 {
        if self.is_idle(now) {
            return 0;
        }
        self.frame_timestamps
            .iter()
            .filter(|t| now.saturating_duration_since(**t) <= Duration::from_secs(1))
            .count() as u32
    }

    #[must_use]
    pub fn latest_sample(&self) -> FrameStageSample {
        self.samples.back().copied().unwrap_or_default()
    }

    pub fn record_network(&mut self, entry: NetworkEntry) {
        self.network.push(entry);
        if self.network.len() > 128 {
            self.network.remove(0);
        }
    }

    pub fn record_missing_feature(&mut self, category: &str, name: &str) {
        if let Some(existing) = self
            .missing_features
            .iter_mut()
            .find(|e| e.category == category && e.name == name)
        {
            existing.count = existing.count.saturating_add(1);
        } else {
            self.missing_features.push(MissingFeatureEntry {
                category: category.to_string(),
                name: name.to_string(),
                count: 1,
            });
        }
        self.missing_features
            .sort_by_key(|e| std::cmp::Reverse(e.count));
    }

    pub fn ingest_css_missing_features(&mut self, features: &[MissingCssFeature]) {
        for f in features {
            let cat = format!("CSS {:?}", f.kind);
            self.record_missing_feature(&cat, &f.name);
        }
    }

    pub fn record_console(
        &mut self,
        level: impl Into<String>,
        message: impl Into<String>,
        source: impl Into<String>,
    ) {
        self.console.push(ConsoleEntry {
            level: level.into(),
            message: message.into(),
            source: source.into(),
        });
        if self.console.len() > 200 {
            self.console.remove(0);
        }
    }

    pub fn clear_page_telemetry(&mut self) {
        self.missing_features.clear();
        self.console.clear();
    }
}

/// Reads resident memory (`VmRSS`) in MiB for `pid` on Linux without `unsafe`.
#[must_use]
pub fn read_process_rss_mb(pid: u32) -> Option<f32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: f32 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb / 1024.0);
        }
    }
    None
}

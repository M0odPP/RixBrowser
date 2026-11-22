//! GPU compositor-driven CSS `@keyframes` and `transition` animation engine.
//!
//! Runs `transform` and `opacity` animations directly inside [`crate::Compositor`]
//! at 60–120 Hz VSync with ZERO DOM mutations, ZERO CSS cascade passes, ZERO
//! layout wakes, and ZERO `DisplayList` rebuilds.
//!
//! Reference: Spec 02 §2, Spec 04 §2, Issue #23.

use rix_ipc::SpatialNodeId;
use serde::{Deserialize, Serialize};

use crate::display_list::AnimationId;

/// CSS easing curve evaluated directly inside the compositor.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub enum TimingCurve {
    Linear,
    #[default]
    Ease,
    EaseIn,
    EaseOut,
    EaseInOut,
    CubicBezier(f32, f32, f32, f32),
    Steps(u32, bool),
}

impl TimingCurve {
    /// Evaluates the easing function for normalized progress `t` in `[0.0, 1.0]`.
    #[must_use]
    pub fn evaluate(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Linear => t,
            Self::Ease => eval_cubic_bezier(0.25, 0.1, 0.25, 1.0, t),
            Self::EaseIn => eval_cubic_bezier(0.42, 0.0, 1.0, 1.0, t),
            Self::EaseOut => eval_cubic_bezier(0.0, 0.0, 0.58, 1.0, t),
            Self::EaseInOut => eval_cubic_bezier(0.42, 0.0, 0.58, 1.0, t),
            Self::CubicBezier(x1, y1, x2, y2) => eval_cubic_bezier(x1, y1, x2, y2, t),
            Self::Steps(count, jump_start) => {
                let n = count.max(1) as f32;
                if jump_start {
                    ((t * n).floor() + 1.0).min(n) / n
                } else {
                    (t * n).floor() / n
                }
            }
        }
    }
}

fn eval_cubic_bezier(x1: f32, y1: f32, x2: f32, y2: f32, x: f32) -> f32 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let sample_x = |u: f32| {
        let inv = 1.0 - u;
        3.0 * inv * inv * u * x1 + 3.0 * inv * u * u * x2 + u * u * u
    };
    let sample_dx = |u: f32| {
        let inv = 1.0 - u;
        3.0 * inv * inv * x1 + 6.0 * inv * u * (x2 - x1) + 3.0 * u * u * (1.0 - x2)
    };
    let sample_y = |u: f32| {
        let inv = 1.0 - u;
        3.0 * inv * inv * u * y1 + 3.0 * inv * u * u * y2 + u * u * u
    };
    let mut u = x;
    for _ in 0..6 {
        let err = sample_x(u) - x;
        if err.abs() < 1e-5 {
            break;
        }
        let d = sample_dx(u);
        if d.abs() < 1e-6 {
            break;
        }
        u = (u - err / d).clamp(0.0, 1.0);
    }
    sample_y(u)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AnimDirection {
    #[default]
    Normal,
    Reverse,
    Alternate,
    AlternateReverse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AnimFillMode {
    #[default]
    None,
    Forwards,
    Backwards,
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct TransformKeyframe {
    pub offset: f32,
    pub matrix: [f32; 6],
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OpacityKeyframe {
    pub offset: f32,
    pub opacity: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AnimationTarget {
    Transform {
        spatial: SpatialNodeId,
        keyframes: Vec<TransformKeyframe>,
    },
    Opacity {
        keyframes: Vec<OpacityKeyframe>,
    },
}

/// Sampled value produced by a [`CompositorAnimation`] at a VSync timestamp.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AnimationSample {
    Transform {
        spatial: SpatialNodeId,
        matrix: [f32; 6],
    },
    Opacity {
        animation_id: AnimationId,
        opacity: f32,
    },
}

/// Compositor-resident animation track (`@keyframes` or `transition`) attached to a [`crate::DisplayList`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompositorAnimation {
    pub id: AnimationId,
    pub start_time_ms: Option<f64>,
    pub duration_ms: f32,
    pub delay_ms: f32,
    /// `f32::INFINITY` represents CSS `infinite`.
    pub iteration_count: f32,
    pub direction: AnimDirection,
    pub fill_mode: AnimFillMode,
    pub paused: bool,
    pub timing: TimingCurve,
    pub target: AnimationTarget,
}

impl CompositorAnimation {
    /// Creates a one-shot compositor CSS `transition` track animating `spatial` from
    /// `from_matrix` to `to_matrix`.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn transform_transition(
        id: AnimationId,
        spatial: SpatialNodeId,
        from_matrix: [f32; 6],
        to_matrix: [f32; 6],
        duration_ms: f32,
        delay_ms: f32,
        timing: TimingCurve,
        start_time_ms: f64,
    ) -> Self {
        Self {
            id,
            start_time_ms: Some(start_time_ms),
            duration_ms: duration_ms.max(0.0),
            delay_ms,
            iteration_count: 1.0,
            direction: AnimDirection::Normal,
            fill_mode: AnimFillMode::Both,
            paused: false,
            timing,
            target: AnimationTarget::Transform {
                spatial,
                keyframes: vec![
                    TransformKeyframe {
                        offset: 0.0,
                        matrix: from_matrix,
                    },
                    TransformKeyframe {
                        offset: 1.0,
                        matrix: to_matrix,
                    },
                ],
            },
        }
    }

    /// Creates a one-shot compositor CSS `transition` track animating `opacity` from
    /// `from_opacity` to `to_opacity`.
    #[must_use]
    pub fn opacity_transition(
        id: AnimationId,
        from_opacity: f32,
        to_opacity: f32,
        duration_ms: f32,
        delay_ms: f32,
        timing: TimingCurve,
        start_time_ms: f64,
    ) -> Self {
        Self {
            id,
            start_time_ms: Some(start_time_ms),
            duration_ms: duration_ms.max(0.0),
            delay_ms,
            iteration_count: 1.0,
            direction: AnimDirection::Normal,
            fill_mode: AnimFillMode::Both,
            paused: false,
            timing,
            target: AnimationTarget::Opacity {
                keyframes: vec![
                    OpacityKeyframe {
                        offset: 0.0,
                        opacity: from_opacity.clamp(0.0, 1.0),
                    },
                    OpacityKeyframe {
                        offset: 1.0,
                        opacity: to_opacity.clamp(0.0, 1.0),
                    },
                ],
            },
        }
    }

    /// Samples this animation at `now_ms`. Anchors `start_time_ms` on the first tick.
    /// Returns `(sampled_value, still_active)`. When `still_active == false`, the
    /// compositor no longer needs to schedule VSync frames for this track.
    pub fn tick(&mut self, now_ms: f64) -> (Option<AnimationSample>, bool) {
        let start = *self.start_time_ms.get_or_insert(now_ms);
        if self.paused {
            return (None, false);
        }
        let elapsed = (now_ms - start) - f64::from(self.delay_ms);
        if elapsed < 0.0 {
            let sample = match self.fill_mode {
                AnimFillMode::Backwards | AnimFillMode::Both => {
                    let t = self.apply_direction(0.0, 0);
                    self.sample_at_progress(t)
                }
                AnimFillMode::None | AnimFillMode::Forwards => None,
            };
            return (sample, true);
        }

        let duration = f64::from(self.duration_ms);
        if duration <= 0.0 || self.iteration_count <= 0.0 {
            let sample = match self.fill_mode {
                AnimFillMode::Forwards | AnimFillMode::Both => {
                    let t = self.apply_direction(1.0, 0);
                    self.sample_at_progress(t)
                }
                AnimFillMode::None | AnimFillMode::Backwards => None,
            };
            return (sample, false);
        }

        let raw_iter = elapsed / duration;
        let finite_limit = if self.iteration_count.is_finite() {
            Some(f64::from(self.iteration_count))
        } else {
            None
        };

        if let Some(limit) = finite_limit {
            if raw_iter >= limit {
                let completed_idx = (limit.ceil() as u64).saturating_sub(1);
                let final_frac = (limit - completed_idx as f64).clamp(0.0, 1.0) as f32;
                let sample = match self.fill_mode {
                    AnimFillMode::Forwards | AnimFillMode::Both => {
                        let directed = self.apply_direction(final_frac, completed_idx);
                        let eased = self.timing.evaluate(directed);
                        self.sample_at_progress(eased)
                    }
                    AnimFillMode::None | AnimFillMode::Backwards => None,
                };
                return (sample, false);
            }
        }

        let iter_idx = raw_iter.floor() as u64;
        let local_t = (raw_iter - iter_idx as f64).clamp(0.0, 1.0) as f32;
        let directed = self.apply_direction(local_t, iter_idx);
        let eased = self.timing.evaluate(directed);
        (self.sample_at_progress(eased), true)
    }

    fn apply_direction(&self, t: f32, iter_idx: u64) -> f32 {
        let reverse = match self.direction {
            AnimDirection::Normal => false,
            AnimDirection::Reverse => true,
            AnimDirection::Alternate => iter_idx % 2 == 1,
            AnimDirection::AlternateReverse => iter_idx.is_multiple_of(2),
        };
        if reverse {
            1.0 - t
        } else {
            t
        }
    }

    fn sample_at_progress(&self, progress: f32) -> Option<AnimationSample> {
        let p = progress.clamp(0.0, 1.0);
        match &self.target {
            AnimationTarget::Transform { spatial, keyframes } => {
                if keyframes.is_empty() {
                    return None;
                }
                let matrix = sample_transform_keyframes(keyframes, p);
                Some(AnimationSample::Transform {
                    spatial: *spatial,
                    matrix,
                })
            }
            AnimationTarget::Opacity { keyframes } => {
                if keyframes.is_empty() {
                    return None;
                }
                let opacity = sample_opacity_keyframes(keyframes, p);
                Some(AnimationSample::Opacity {
                    animation_id: self.id,
                    opacity,
                })
            }
        }
    }
}

fn sample_transform_keyframes(kfs: &[TransformKeyframe], p: f32) -> [f32; 6] {
    if kfs.len() == 1 || p <= kfs[0].offset {
        return kfs[0].matrix;
    }
    let last = kfs[kfs.len() - 1];
    if p >= last.offset {
        return last.matrix;
    }
    for win in kfs.windows(2) {
        let a = win[0];
        let b = win[1];
        if p >= a.offset && p <= b.offset {
            let span = (b.offset - a.offset).max(1e-6);
            let frac = ((p - a.offset) / span).clamp(0.0, 1.0);
            let mut out = [0.0f32; 6];
            for i in 0..6 {
                out[i] = a.matrix[i] + (b.matrix[i] - a.matrix[i]) * frac;
            }
            return out;
        }
    }
    last.matrix
}

fn sample_opacity_keyframes(kfs: &[OpacityKeyframe], p: f32) -> f32 {
    if kfs.len() == 1 || p <= kfs[0].offset {
        return kfs[0].opacity.clamp(0.0, 1.0);
    }
    let last = kfs[kfs.len() - 1];
    if p >= last.offset {
        return last.opacity.clamp(0.0, 1.0);
    }
    for win in kfs.windows(2) {
        let a = win[0];
        let b = win[1];
        if p >= a.offset && p <= b.offset {
            let span = (b.offset - a.offset).max(1e-6);
            let frac = ((p - a.offset) / span).clamp(0.0, 1.0);
            return (a.opacity + (b.opacity - a.opacity) * frac).clamp(0.0, 1.0);
        }
    }
    last.opacity.clamp(0.0, 1.0)
}

//! W3C CSS Animations Level 1/2, CSS Transitions Level 1/2, and Web Animations Level 1
//! synchronous interpolation and composition engine (including CSS Values L5 `calc-size()`
//! and `interpolate-size`).

use std::collections::HashMap;
use std::sync::Arc;

use rix_core::InternedStr;
use rix_dom::NodeId;

use crate::color::{ColorSpace, CssColorExpr, ResolvedColor};
use crate::computed::{apply_declaration, ComputedStyle};
use crate::properties::{
    is_logical_property, resolve_logical_to_physical, CssToken, PropertyIdKind, PropertyValue,
    SpecifiedDeclaration,
};
use crate::stylesheet::{KeyframesRule, RegisteredProperty};
use crate::values::{
    AnimationCompositionMode, AnimationDirectionMode, AnimationFillModeKind,
    AnimationPlayStateMode, BoxShadowValue, CalcExpr, ColorRgba, CornerRadius, CssColor,
    DisplayMode, EvalContext, FilterOp, InterpolateSizeMode, LengthPercentage,
    LengthPercentageAuto, LengthUnit, LineHeightValue, OverlayMode, SizeValue, TimingFunction,
    TransformOp, TransitionBehaviorMode, TransitionSpec, VerticalAlignMode, VisibilityMode,
    ZIndexValue,
};
use crate::values_math::{
    format_math_f64, CalcSizeBasis, CalcSizeExpr, MathNode, MathUnit, RotateSpec, ScaleSpec,
    TranslateSpec, ValueKind, MAX_CSS_PX,
};

/// Maximum expanded AST node count for `calc-size()` interpolation per CSSWG #10369.
const CALC_SIZE_MAX_EXPANSION_NODES: usize = 128;

/// Single keyframe entry for a Web Animations API (`element.animate(...)` / `KeyframeEffect`) effect.
#[derive(Debug, Clone, PartialEq)]
pub struct WebAnimationKeyframe {
    pub offset: Option<f32>,
    pub computed_offset: f64,
    pub composite: Option<AnimationCompositionMode>,
    pub easing: Option<TimingFunction>,
    pub declarations: Vec<SpecifiedDeclaration>,
}

/// W3C Web Animations Level 1 §4.5 effect phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectPhase {
    Idle,
    Before,
    Active,
    After,
}

/// Input parameters for W3C Web Animations Level 1 §4.5/§4.8 effect timing calculations.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectTimingInput {
    pub delay_ms: f64,
    pub end_delay_ms: f64,
    pub fill_mode: AnimationFillModeKind,
    pub fill_is_auto: bool,
    pub iteration_start: f64,
    pub iterations: f64,
    /// `None` means `"auto"` (which computes to `0.0` in Web Animations Level 1).
    pub duration_ms: Option<f64>,
    pub direction: AnimationDirectionMode,
    pub easing: TimingFunction,
}

impl Default for EffectTimingInput {
    fn default() -> Self {
        Self {
            delay_ms: 0.0,
            end_delay_ms: 0.0,
            fill_mode: AnimationFillModeKind::None,
            fill_is_auto: true,
            iteration_start: 0.0,
            iterations: 1.0,
            duration_ms: None,
            direction: AnimationDirectionMode::Normal,
            easing: TimingFunction::Linear,
        }
    }
}

/// Exact W3C Web Animations Level 1 §4.5/§4.8 computed timing output.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ComputedEffectTiming {
    pub end_time: f64,
    pub active_duration: f64,
    pub local_time: Option<f64>,
    pub active_time: Option<f64>,
    pub phase: EffectPhase,
    pub progress: Option<f64>,
    pub current_iteration: Option<f64>,
    pub before_flag: bool,
}

/// Computes exact W3C Web Animations Level 1 §4.5–§4.8 timing values in `f64` precision.
#[must_use]
pub fn compute_effect_timing(
    timing: &EffectTimingInput,
    local_time: Option<f64>,
    playback_rate: f64,
) -> ComputedEffectTiming {
    let iter_dur = timing.duration_ms.unwrap_or(0.0).max(0.0);
    let iterations = timing.iterations.max(0.0);
    let active_duration = if iter_dur == 0.0 || iterations == 0.0 {
        0.0
    } else if iterations.is_infinite() || iter_dur.is_infinite() {
        f64::INFINITY
    } else {
        iter_dur * iterations
    };
    let end_time = if active_duration.is_infinite() {
        f64::INFINITY
    } else {
        (timing.delay_ms + active_duration + timing.end_delay_ms).max(0.0)
    };
    let Some(lt) = local_time else {
        return ComputedEffectTiming {
            end_time,
            active_duration,
            local_time: None,
            active_time: None,
            phase: EffectPhase::Idle,
            progress: None,
            current_iteration: None,
            before_flag: false,
        };
    };

    let eff_fill = if timing.fill_is_auto {
        AnimationFillModeKind::None
    } else {
        timing.fill_mode
    };
    let fill_back = matches!(
        eff_fill,
        AnimationFillModeKind::Backwards | AnimationFillModeKind::Both
    );
    let fill_fwd = matches!(
        eff_fill,
        AnimationFillModeKind::Forwards | AnimationFillModeKind::Both
    );

    let before_boundary = timing.delay_ms.min(end_time).max(0.0);
    let after_boundary = (timing.delay_ms + active_duration).min(end_time).max(0.0);
    let in_before = lt < before_boundary || (lt == before_boundary && playback_rate < 0.0);
    let in_after =
        !in_before && (lt > after_boundary || (lt == after_boundary && playback_rate >= 0.0));
    let phase = if in_before {
        EffectPhase::Before
    } else if in_after {
        EffectPhase::After
    } else {
        EffectPhase::Active
    };

    let active_time = match phase {
        EffectPhase::Before => {
            if fill_back {
                Some((lt - timing.delay_ms).max(0.0))
            } else {
                None
            }
        }
        EffectPhase::Active => Some(lt - timing.delay_ms),
        EffectPhase::After => {
            if fill_fwd {
                Some((lt - timing.delay_ms).min(active_duration).max(0.0))
            } else {
                None
            }
        }
        EffectPhase::Idle => None,
    };

    let Some(at) = active_time else {
        return ComputedEffectTiming {
            end_time,
            active_duration,
            local_time: Some(lt),
            active_time: None,
            phase,
            progress: None,
            current_iteration: None,
            before_flag: false,
        };
    };

    let overall_progress = if iter_dur == 0.0 {
        if phase == EffectPhase::Before {
            timing.iteration_start
        } else {
            timing.iteration_start + iterations
        }
    } else {
        (at / iter_dur) + timing.iteration_start
    };

    let mut simple_iter_progress = if overall_progress.is_infinite() {
        timing.iteration_start % 1.0
    } else {
        overall_progress % 1.0
    };
    let at_end_boundary = (phase == EffectPhase::Active || phase == EffectPhase::After)
        && active_duration.is_finite()
        && (at - active_duration).abs() < 1e-12
        && iterations != 0.0;
    if simple_iter_progress == 0.0 && at_end_boundary {
        simple_iter_progress = 1.0;
    }

    let current_iteration = if phase == EffectPhase::After && iterations.is_infinite() {
        f64::INFINITY
    } else if simple_iter_progress == 1.0 {
        (overall_progress.floor() - 1.0).max(0.0)
    } else {
        overall_progress.floor()
    };

    let current_dir_forwards = match timing.direction {
        AnimationDirectionMode::Normal => true,
        AnimationDirectionMode::Reverse => false,
        AnimationDirectionMode::Alternate => {
            if current_iteration.is_infinite() {
                true
            } else {
                (current_iteration as u64).is_multiple_of(2)
            }
        }
        AnimationDirectionMode::AlternateReverse => {
            if current_iteration.is_infinite() {
                false
            } else {
                !(current_iteration as u64).is_multiple_of(2)
            }
        }
    };

    let directed_progress = if current_dir_forwards {
        simple_iter_progress
    } else {
        1.0 - simple_iter_progress
    };
    let before_flag = (phase == EffectPhase::Before && current_dir_forwards)
        || (phase == EffectPhase::After && !current_dir_forwards);
    let progress = timing
        .easing
        .evaluate_f64_with_before_flag(directed_progress, before_flag);

    ComputedEffectTiming {
        end_time,
        active_duration,
        local_time: Some(lt),
        active_time: Some(at),
        phase,
        progress: Some(progress),
        current_iteration: Some(current_iteration),
        before_flag,
    }
}

/// Live Web Animation effect attached to a DOM element via `Element.prototype.animate()` or `Animation`.
#[derive(Debug, Clone, PartialEq)]
pub struct WebAnimationSpec {
    pub id: u64,
    pub keyframes: Vec<WebAnimationKeyframe>,
    pub duration_ms: f64,
    pub delay_ms: f64,
    pub end_delay_ms: f64,
    pub iteration_start: f64,
    pub timing: TimingFunction,
    pub iteration_count: f64,
    pub direction: AnimationDirectionMode,
    pub fill_mode: AnimationFillModeKind,
    pub fill_is_auto: bool,
    pub play_state: AnimationPlayStateMode,
    pub composite: AnimationCompositionMode,
    pub iteration_composite: AnimationCompositionMode,
    pub current_time_ms: Option<f64>,
    pub playback_rate: f64,
    pub pseudo_element: Option<String>,
    pub cancelled: bool,
    pub removed: bool,
}

/// Active CSS Transition running on a single property of a DOM element.
#[derive(Debug, Clone, PartialEq)]
pub struct ActiveTransition {
    pub prop: PropertyIdKind,
    pub custom_name: Option<InternedStr>,
    pub from_style: Arc<ComputedStyle>,
    pub to_style: Arc<ComputedStyle>,
    pub spec: TransitionSpec,
    pub start_elapsed_ms: f32,
}

/// Portable animation/transition state preserved across stylesheet rebuilds.
#[derive(Debug, Default, Clone)]
pub struct AnimationStateSnapshot {
    pub before_change: HashMap<NodeId, Arc<ComputedStyle>>,
    pub active_transitions: HashMap<NodeId, Vec<ActiveTransition>>,
    pub web_animations: HashMap<NodeId, Vec<WebAnimationSpec>>,
    pub random_doc_seed: Option<u64>,
}

#[must_use]
#[rustfmt::skip]
pub const fn is_property_not_animatable(prop: PropertyIdKind) -> bool {
    matches!(
        prop,
        PropertyIdKind::InterpolateSize | PropertyIdKind::Animation | PropertyIdKind::AnimationName
        | PropertyIdKind::AnimationDuration | PropertyIdKind::AnimationDelay | PropertyIdKind::AnimationTimingFunction
        | PropertyIdKind::AnimationIterationCount | PropertyIdKind::AnimationDirection | PropertyIdKind::AnimationFillMode
        | PropertyIdKind::AnimationPlayState | PropertyIdKind::AnimationComposition
        | PropertyIdKind::Transition | PropertyIdKind::TransitionProperty | PropertyIdKind::TransitionDuration
        | PropertyIdKind::TransitionDelay | PropertyIdKind::TransitionTimingFunction | PropertyIdKind::TransitionBehavior
        | PropertyIdKind::Direction | PropertyIdKind::WritingMode | PropertyIdKind::TextOrientation
        | PropertyIdKind::UnicodeBidi | PropertyIdKind::Contain
    )
}

#[must_use]
#[rustfmt::skip]
pub fn resolve_transition_property_name(raw: &str, style: &ComputedStyle) -> Option<(PropertyIdKind, Option<InternedStr>)> {
    if raw.starts_with("--") { return Some((PropertyIdKind::Custom, Some(rix_core::intern_str(raw)))); }
    let pk = PropertyIdKind::from_name(&raw.to_ascii_lowercase())?;
    let phys = if is_logical_property(pk) { resolve_logical_to_physical(pk, style.text.writing_mode, style.text.direction) } else { pk };
    Some((phys, None))
}

#[must_use]
#[rustfmt::skip]
pub fn compute_animation_progress(
    duration_ms: f32, delay_ms: f32, iteration_count: f32,
    direction: AnimationDirectionMode, fill_mode: AnimationFillModeKind, current_time_ms: f32,
) -> Option<f32> {
    let local_time = current_time_ms - delay_ms;
    let fill_back = matches!(fill_mode, AnimationFillModeKind::Backwards | AnimationFillModeKind::Both);
    let fill_fwd = matches!(fill_mode, AnimationFillModeKind::Forwards | AnimationFillModeKind::Both);
    if local_time < 0.0 { return fill_back.then(|| apply_direction(0.0, 0, direction)); }
    if duration_ms <= 0.0 {
        if !fill_fwd { return None; }
        let end_p = if iteration_count == 0.0 { 0.0 } else { 1.0 };
        let last_i = if iteration_count.is_finite() && iteration_count >= 1.0 { (iteration_count.ceil() as u64).saturating_sub(1) } else { 0 };
        return Some(apply_direction(end_p, last_i, direction));
    }
    let active_dur = if iteration_count.is_infinite() { f32::INFINITY } else { duration_ms * iteration_count.max(0.0) };
    if local_time >= active_dur {
        if !fill_fwd { return None; }
        if iteration_count == 0.0 { return Some(apply_direction(0.0, 0, direction)); }
        let frac = iteration_count.fract();
        let (raw_p, idx) = if frac == 0.0 { (1.0, (iteration_count as u64).saturating_sub(1)) } else { (frac, iteration_count.floor() as u64) };
        return Some(apply_direction(raw_p, idx, direction));
    }
    let ratio = local_time / duration_ms;
    let idx = ratio.floor() as u64;
    Some(apply_direction((ratio - idx as f32).clamp(0.0, 1.0), idx, direction))
}

#[rustfmt::skip]
fn apply_direction(p: f32, iter_idx: u64, dir: AnimationDirectionMode) -> f32 {
    let rev = match dir {
        AnimationDirectionMode::Normal => false, AnimationDirectionMode::Reverse => true,
        AnimationDirectionMode::Alternate => !iter_idx.is_multiple_of(2),
        AnimationDirectionMode::AlternateReverse => iter_idx.is_multiple_of(2),
    };
    if rev { 1.0 - p } else { p }
}

#[must_use]
#[rustfmt::skip]
pub fn compute_transition_eased_progress(spec: &TransitionSpec, elapsed_ms: f32, start_ms: f32) -> Option<f64> {
    if spec.duration_ms + spec.delay_ms <= 0.0 { return None; }
    let local = f64::from(elapsed_ms - start_ms) - f64::from(spec.delay_ms);
    if spec.duration_ms <= 0.0 { return (local < 0.0).then_some(0.0); }
    if local < 0.0 { return Some(0.0); }
    if local >= f64::from(spec.duration_ms) { return None; }
    Some(spec.timing.evaluate_f64((local / f64::from(spec.duration_ms)).clamp(0.0, 1.0)))
}

/// Applies CSS Animations (`@keyframes`), Web Animations (`element.animate()`), and
/// CSS Transitions onto `base_style` per W3C Cascade Level 5 origin order.
#[allow(clippy::too_many_arguments)]
#[rustfmt::skip]
pub fn apply_animations_and_transitions<'a, F>(
    base_style: Arc<ComputedStyle>, parent: Option<&ComputedStyle>, dark_mode: bool, elapsed_ms: f32,
    keyframes_lookup: &F, web_anims: Option<&[WebAnimationSpec]>, before_change: Option<&Arc<ComputedStyle>>,
    active_transitions: &mut Vec<ActiveTransition>, registered_props: &HashMap<InternedStr, RegisteredProperty>,
) -> Arc<ComputedStyle>
where F: Fn(&str) -> Option<&'a KeyframesRule>,
{
    let has_css = base_style.effects.as_ref().is_some_and(|e| !e.animations.is_empty());
    let has_web = web_anims.is_some_and(|w| w.iter().any(|a| !a.cancelled && !a.removed));
    let has_tr = base_style.effects.as_ref().is_some_and(|e| !e.transitions.is_empty()) || !active_transitions.is_empty();
    if !has_css && !has_web && !has_tr { return base_style; }

    let mut current = (*base_style).clone();
    let eval = current.text.eval_ctx;

    if has_css {
        for anim in &base_style.effects.as_ref().unwrap().animations {
            if anim.name.as_str().eq_ignore_ascii_case("none") { continue; }
            let Some(kf_rule) = keyframes_lookup(anim.name.as_str()) else { continue };
            let Some(raw_p) = compute_animation_progress(anim.duration_ms, anim.delay_ms, anim.iteration_count, anim.direction, anim.fill_mode, elapsed_ms) else { continue };
            let stops = build_css_keyframe_stops(kf_rule, &current);
            apply_keyframe_effect(&mut current, &base_style, parent, dark_mode, &eval, &stops, f64::from(raw_p), false, 0, AnimationCompositionMode::Replace, &anim.timing, anim.composition, registered_props);
        }
    }
    if let Some(wanims) = web_anims {
        for wa in wanims {
            if wa.cancelled || wa.removed || wa.keyframes.is_empty() || wa.pseudo_element.is_some() { continue; }
            apply_single_web_animation(&mut current, parent, dark_mode, &eval, wa, false, registered_props);
        }
    }
    if has_tr {
        let after = Arc::new(current.clone());
        let specs = base_style.effects.as_ref().map(|e| e.transitions.clone()).unwrap_or_default();
        active_transitions.retain(|tr| {
            find_matching_transition_spec(&specs, tr.prop, tr.custom_name.as_ref(), &after)
                .is_some_and(|m| compute_transition_eased_progress(&m, elapsed_ms, tr.start_elapsed_ms).is_some())
        });
        if let Some(before) = before_change {
            // CSS Transitions L2 §3: transitions do not start from `display: none`.
            if before.display != DisplayMode::None {
                for (prop, cname, spec) in collect_transition_candidates(before, &after, &specs) {
                    if is_property_not_animatable(prop) || compute_transition_eased_progress(&spec, elapsed_ms, elapsed_ms).is_none()
                        || !properties_differ(before, &after, prop, cname.as_ref()) { continue; }
                    if let Some(ex) = active_transitions.iter_mut().find(|t| t.prop == prop && t.custom_name == cname) {
                        if !properties_differ(&ex.to_style, &after, prop, cname.as_ref()) { ex.spec = spec; }
                        else { ex.from_style = Arc::clone(before); ex.to_style = Arc::clone(&after); ex.spec = spec; ex.start_elapsed_ms = elapsed_ms; }
                    } else {
                        active_transitions.push(ActiveTransition {
                            prop, custom_name: cname, from_style: Arc::clone(before), to_style: Arc::clone(&after), spec, start_elapsed_ms: elapsed_ms,
                        });
                    }
                }
            }
        }
        let allow_kw = after.text.interpolate_size == InterpolateSizeMode::AllowKeywords;
        for tr in active_transitions.iter() {
            if let Some(p) = compute_transition_eased_progress(&tr.spec, elapsed_ms, tr.start_elapsed_ms) {
                let allow_disc = tr.spec.behavior == TransitionBehaviorMode::AllowDiscrete;
                interpolate_property_into(&mut current, &tr.from_style, &tr.to_style, tr.prop, tr.custom_name.as_ref(), p, true, allow_disc, allow_kw, &eval, registered_props);
            }
        }
    }
    current.opacity = current.opacity.clamp(0.0, 1.0);
    Arc::new(current)
}

/// Applies Web Animations targeting a specific pseudo-element (`"::before"`, `"::after"`, etc.).
#[must_use]
pub fn apply_web_animations_for_pseudo(
    base_style: Arc<ComputedStyle>,
    parent: Option<&ComputedStyle>,
    dark_mode: bool,
    web_anims: &[WebAnimationSpec],
    pseudo_name: &str,
    registered_props: &HashMap<InternedStr, RegisteredProperty>,
) -> Arc<ComputedStyle> {
    let mut current = (*base_style).clone();
    let eval = current.text.eval_ctx;
    for wa in web_anims {
        if wa.cancelled || wa.removed || wa.keyframes.is_empty() {
            continue;
        }
        if wa.pseudo_element.as_deref() != Some(pseudo_name) {
            continue;
        }
        apply_single_web_animation(
            &mut current,
            parent,
            dark_mode,
            &eval,
            wa,
            false,
            registered_props,
        );
    }
    current.opacity = current.opacity.clamp(0.0, 1.0);
    Arc::new(current)
}

/// Evaluates `base_style` applying only Web Animations up to and including `commit_anim_id`
/// for W3C Web Animations §4.4.17 (`Animation.prototype.commitStyles()`, CSSWG #5394).
#[must_use]
pub fn apply_web_animations_for_commit(
    base_style: Arc<ComputedStyle>,
    parent: Option<&ComputedStyle>,
    dark_mode: bool,
    web_anims: &[WebAnimationSpec],
    commit_anim_id: u64,
    registered_props: &HashMap<InternedStr, RegisteredProperty>,
) -> Arc<ComputedStyle> {
    let mut current = (*base_style).clone();
    let eval = current.text.eval_ctx;
    for wa in web_anims {
        if wa.id > commit_anim_id || wa.pseudo_element.is_some() || wa.keyframes.is_empty() {
            continue;
        }
        let is_target = wa.id == commit_anim_id;
        if wa.cancelled || (!is_target && wa.removed) {
            continue;
        }
        apply_single_web_animation(
            &mut current,
            parent,
            dark_mode,
            &eval,
            wa,
            is_target,
            registered_props,
        );
    }
    current.opacity = current.opacity.clamp(0.0, 1.0);
    Arc::new(current)
}

#[allow(clippy::too_many_arguments)]
fn apply_single_web_animation(
    current: &mut ComputedStyle,
    parent: Option<&ComputedStyle>,
    dark_mode: bool,
    eval: &EvalContext,
    wa: &WebAnimationSpec,
    include_active_endpoints: bool,
    registered_props: &HashMap<InternedStr, RegisteredProperty>,
) {
    let mut timing_in = EffectTimingInput {
        delay_ms: wa.delay_ms,
        end_delay_ms: wa.end_delay_ms,
        fill_mode: wa.fill_mode,
        fill_is_auto: wa.fill_is_auto,
        iteration_start: wa.iteration_start,
        iterations: wa.iteration_count,
        duration_ms: Some(wa.duration_ms),
        direction: wa.direction,
        easing: wa.timing.clone(),
    };
    let mut ct = compute_effect_timing(&timing_in, wa.current_time_ms, wa.playback_rate);
    if ct.progress.is_none() && include_active_endpoints {
        if let Some(lt) = wa.current_time_ms {
            let active_end = timing_in.delay_ms + ct.active_duration;
            if lt >= timing_in.delay_ms - 1e-9 && lt <= active_end + 1e-9 {
                timing_in.fill_mode = AnimationFillModeKind::Both;
                timing_in.fill_is_auto = false;
                ct = compute_effect_timing(&timing_in, wa.current_time_ms, wa.playback_rate);
            }
        }
    }
    let Some(transformed_p) = ct.progress else {
        return;
    };
    let cur_iter = ct.current_iteration.map_or(0u64, |v| {
        if v.is_finite() && v > 0.0 {
            v.floor() as u64
        } else {
            0
        }
    });
    let stops = build_web_keyframe_stops(&wa.keyframes, current);
    let underlying = current.clone();
    apply_keyframe_effect(
        current,
        &underlying,
        parent,
        dark_mode,
        eval,
        &stops,
        transformed_p,
        ct.before_flag,
        cur_iter,
        wa.iteration_composite,
        &TimingFunction::Linear,
        wa.composite,
        registered_props,
    );
}

#[derive(Clone)]
struct NormalizedStop {
    offset: f32,
    composite: Option<AnimationCompositionMode>,
    easing: Option<TimingFunction>,
    declarations: Vec<SpecifiedDeclaration>,
}

#[rustfmt::skip]
fn resolve_kf_decls(decls: &[SpecifiedDeclaration], style: &ComputedStyle) -> Vec<SpecifiedDeclaration> {
    decls.iter().filter_map(|d| {
        if matches!(d.property, PropertyIdKind::AnimationComposition | PropertyIdKind::AnimationTimingFunction) { return None; }
        let mut r = d.clone();
        if is_logical_property(r.property) { r.property = resolve_logical_to_physical(r.property, style.text.writing_mode, style.text.direction); }
        (!is_property_not_animatable(r.property)).then_some(r)
    }).collect()
}

#[rustfmt::skip]
fn build_css_keyframe_stops(rule: &KeyframesRule, style: &ComputedStyle) -> Vec<NormalizedStop> {
    let mut stops = Vec::new();
    for step in &rule.steps {
        let comp = step.declarations.iter().rev().find_map(|d| match (d.property, &d.value) {
            (PropertyIdKind::AnimationComposition, PropertyValue::AnimationCompositionList(v)) => v.first().copied(), _ => None,
        });
        let easing = step.declarations.iter().rev().find_map(|d| match (d.property, &d.value) {
            (PropertyIdKind::AnimationTimingFunction, PropertyValue::TimingList(v)) => v.first().cloned(), _ => None,
        });
        let decls = resolve_kf_decls(&step.declarations, style);
        for &off in &step.offsets { stops.push(NormalizedStop { offset: off.clamp(0.0, 1.0), composite: comp, easing: easing.clone(), declarations: decls.clone() }); }
    }
    stops.sort_by(|a, b| a.offset.total_cmp(&b.offset));
    stops
}

#[rustfmt::skip]
fn build_web_keyframe_stops(kfs: &[WebAnimationKeyframe], style: &ComputedStyle) -> Vec<NormalizedStop> {
    let n = kfs.len();
    let mut stops: Vec<_> = kfs.iter().enumerate().map(|(i, kf)| {
        let off = if kf.computed_offset.is_finite() {
            kf.computed_offset as f32
        } else {
            kf.offset.unwrap_or_else(|| if n <= 1 { 1.0 } else { i as f32 / (n - 1) as f32 })
        };
        NormalizedStop { offset: off.clamp(0.0, 1.0), composite: kf.composite, easing: kf.easing.clone(), declarations: resolve_kf_decls(&kf.declarations, style) }
    }).collect();
    stops.sort_by(|a, b| a.offset.total_cmp(&b.offset));
    stops
}

#[allow(clippy::too_many_arguments)]
#[rustfmt::skip]
fn apply_keyframe_effect(
    target: &mut ComputedStyle, underlying: &ComputedStyle, parent: Option<&ComputedStyle>,
    dark_mode: bool, eval: &EvalContext, stops: &[NormalizedStop], raw_p: f64, before_flag: bool,
    current_iter: u64, iter_comp: AnimationCompositionMode,
    default_timing: &TimingFunction, default_comp: AnimationCompositionMode,
    registered_props: &HashMap<InternedStr, RegisteredProperty>,
) {
    let mut props: Vec<(PropertyIdKind, Option<InternedStr>)> = Vec::new();
    for s in stops { for d in &s.declarations { let k = (d.property, d.custom_name.clone()); if !props.contains(&k) { props.push(k); } } }
    let allow_kw = underlying.text.interpolate_size == InterpolateSizeMode::AllowKeywords;
    for (prop, cname) in props {
        let mut ps: Vec<(f64, Option<&SpecifiedDeclaration>, AnimationCompositionMode, Option<&TimingFunction>)> = Vec::new();
        for s in stops {
            if let Some(decl) = s.declarations.iter().rev().find(|d| d.property == prop && d.custom_name == cname) {
                let comp = s.composite.unwrap_or(default_comp);
                ps.push((f64::from(s.offset), Some(decl), comp, s.easing.as_ref()));
            }
        }
        if ps.is_empty() { continue; }
        if ps.first().is_none_or(|s| s.0 > 0.0) { ps.insert(0, (0.0, None, AnimationCompositionMode::Replace, None)); }
        if ps.last().is_none_or(|s| s.0 < 1.0) { ps.push((1.0, None, AnimationCompositionMode::Replace, None)); }
        // W3C Web Animations §5.5.5: handle overlapping keyframes at 0 / 1 when progress < 0 or >= 1.
        let (s0, s1) = if raw_p < 0.0 {
            let first = &ps[0];
            if ps.get(1).is_some_and(|n| (n.0 - first.0).abs() < 1e-6) { (first, first) }
            else { (first, &ps[1.min(ps.len() - 1)]) }
        } else if raw_p >= 1.0 {
            let last = &ps[ps.len() - 1];
            if ps.len() >= 2 && (ps[ps.len() - 2].0 - last.0).abs() < 1e-6 { (last, last) }
            else { (&ps[ps.len().saturating_sub(2)], last) }
        } else {
            let mut left_idx = 0usize;
            for (i, item) in ps.iter().enumerate() {
                if item.0 <= raw_p { left_idx = i; }
            }
            let right_idx = (left_idx + 1).min(ps.len() - 1);
            (&ps[left_idx], &ps[right_idx])
        };
        let span = s1.0 - s0.0;
        let seg_t = if span.abs() < 1e-12 { 1.0 } else { (raw_p - s0.0) / span };
        let eased_p = s0.3.unwrap_or(default_timing).evaluate_f64_with_before_flag(seg_t, before_flag);
        let mut f_st = eval_keyframe_endpoint(underlying, parent, dark_mode, eval, prop, cname.as_ref(), s0.1, s0.2, allow_kw, registered_props);
        let mut t_st = eval_keyframe_endpoint(underlying, parent, dark_mode, eval, prop, cname.as_ref(), s1.1, s1.2, allow_kw, registered_props);
        // W3C Web Animations §5.5.5: apply `iterationComposite: 'accumulate'`.
        if iter_comp == AnimationCompositionMode::Accumulate && current_iter > 0 {
            let last = &ps[ps.len() - 1];
            let last_st = eval_keyframe_endpoint(underlying, parent, dark_mode, eval, prop, cname.as_ref(), last.1, last.2, allow_kw, registered_props);
            let reps = current_iter.min(1024);
            for _ in 0..reps {
                let f_snap = f_st.clone();
                compose_property(&last_st, &mut f_st, prop, AnimationCompositionMode::Accumulate, allow_kw, eval);
                let _ = f_snap;
                compose_property(&last_st, &mut t_st, prop, AnimationCompositionMode::Accumulate, allow_kw, eval);
            }
        }
        interpolate_property_into(target, &f_st, &t_st, prop, cname.as_ref(), eased_p, false, true, allow_kw, eval, registered_props);
    }
}

#[allow(clippy::too_many_arguments)]
#[rustfmt::skip]
fn eval_keyframe_endpoint(
    underlying: &ComputedStyle, parent: Option<&ComputedStyle>, dark_mode: bool, eval: &EvalContext,
    prop: PropertyIdKind, cname: Option<&InternedStr>, decl_opt: Option<&SpecifiedDeclaration>,
    comp: AnimationCompositionMode, allow_kw: bool, registered_props: &HashMap<InternedStr, RegisteredProperty>,
) -> ComputedStyle {
    let Some(decl) = decl_opt else { return underlying.clone(); };
    let mut kf = underlying.clone();
    if prop == PropertyIdKind::Custom {
        if let (Some(cn), PropertyValue::Custom(toks)) = (cname, &decl.value) {
            apply_custom_prop_tokens(&mut kf, cn, toks, eval, registered_props);
        }
        return kf;
    }
    // Expand `var(...)` inside keyframes against the target element's custom properties!
    let resolved_decl_owned: Option<SpecifiedDeclaration> =
        if let PropertyValue::Unresolved(toks) = &decl.value {
            crate::computed::resolve_var_tokens(toks, &underlying.custom_properties).and_then(|exp| {
                let s = crate::properties::serialize_css_tokens(&exp);
                let mut missing = Vec::new();
                let parsed = crate::properties::parse_inline_style(
                    &format!("{}: {}", prop.css_name(), s.trim()),
                    &mut missing,
                );
                parsed.into_iter().rev().find(|d| {
                    let dp = if is_logical_property(d.property) {
                        resolve_logical_to_physical(d.property, underlying.text.writing_mode, underlying.text.direction)
                    } else {
                        d.property
                    };
                    dp == prop
                })
            })
        } else {
            None
        };
    let eff_decl = resolved_decl_owned.as_ref().unwrap_or(decl);
    // W3C Web Animations: do NOT clamp `opacity` keyframe values prior to interpolation/composition.
    if prop == PropertyIdKind::Opacity {
        match &eff_decl.value {
            PropertyValue::Number(v) => kf.opacity = *v,
            PropertyValue::MathVal(n) => {
                let v = n.eval_scalar(eval, Some((1.0, true)));
                kf.opacity = if v.is_nan() { 1.0 } else { v as f32 };
            }
            _ => apply_declaration(&mut kf, eff_decl, parent, eval, dark_mode),
        }
    } else {
        apply_declaration(&mut kf, eff_decl, parent, eval, dark_mode);
    }
    if comp != AnimationCompositionMode::Replace { compose_property(underlying, &mut kf, prop, comp, allow_kw, eval); }
    kf
}

#[rustfmt::skip]
fn registered_syntax_kind(reg: &RegisteredProperty) -> (&str, Option<ValueKind>) {
    let syn = reg.syntax.as_str().trim().trim_matches(|c| c == '"' || c == '\'').trim();
    let vk = match syn {
        "<time>" => Some(ValueKind::Time), "<length>" => Some(ValueKind::Length),
        "<length-percentage>" => Some(ValueKind::LengthPercentage), "<percentage>" => Some(ValueKind::Percentage),
        "<number>" => Some(ValueKind::Number), "<integer>" => Some(ValueKind::Integer),
        "<angle>" => Some(ValueKind::Angle), "<resolution>" => Some(ValueKind::Resolution), _ => None,
    };
    (syn, vk)
}

#[rustfmt::skip]
fn apply_custom_prop_tokens(
    style: &mut ComputedStyle, cname: &InternedStr, toks: &[CssToken],
    eval: &EvalContext, registered_props: &HashMap<InternedStr, RegisteredProperty>,
) {
    let raw = crate::properties::serialize_css_tokens(toks);
    let trimmed = raw.trim();
    if let Some(reg) = registered_props.get(cname) {
        let (syn, vk) = registered_syntax_kind(reg);
        let eval_str = if let Some(kind) = vk {
            crate::values_math::parse_math_value(trimmed, kind).map(|mut pm| {
                let mut rc = 0u32;
                pm.node.bind_random_property(cname.as_str(), &mut rc);
                pm.node.serialize_computed(eval, None, kind, None, None)
            })
        } else if syn == "<color>" {
            crate::color::parse_css_color(trimmed).map(|c| c.resolve(ResolvedColor::from_rgba(style.text.color), false, eval).to_computed_css_string())
        } else { None };
        if let Some(s) = eval_str {
            Arc::make_mut(&mut style.custom_properties).insert(cname.clone(), crate::properties::tokenize_custom_value_str(&s));
            return;
        }
    }
    Arc::make_mut(&mut style.custom_properties).insert(cname.clone(), toks.to_vec());
}

#[rustfmt::skip]
fn find_matching_transition_spec(specs: &[TransitionSpec], prop: PropertyIdKind, cname: Option<&InternedStr>, style: &ComputedStyle) -> Option<TransitionSpec> {
    let mut matched = None;
    for sp in specs {
        let name = sp.property.as_str();
        if name.eq_ignore_ascii_case("none") { continue; }
        if name.eq_ignore_ascii_case("all") { if prop != PropertyIdKind::Custom { matched = Some(sp.clone()); } }
        else if let Some((rp, rc)) = resolve_transition_property_name(name, style) {
            if rp == prop && rc.as_ref() == cname { matched = Some(sp.clone()); }
        }
    }
    matched
}

#[rustfmt::skip]
fn collect_transition_candidates(before: &ComputedStyle, after: &ComputedStyle, specs: &[TransitionSpec]) -> Vec<(PropertyIdKind, Option<InternedStr>, TransitionSpec)> {
    use PropertyIdKind as P;
    let mut out: Vec<(P, Option<InternedStr>, TransitionSpec)> = Vec::new();
    let mut has_all = false;
    for sp in specs {
        let name = sp.property.as_str();
        if name.eq_ignore_ascii_case("none") { continue; }
        if name.eq_ignore_ascii_case("all") { has_all = true; continue; }
        if let Some((pk, cn)) = resolve_transition_property_name(name, after) {
            if let Some(slot) = out.iter_mut().find(|(p, c, _)| *p == pk && *c == cn) { slot.2 = sp.clone(); }
            else { out.push((pk, cn, sp.clone())); }
        }
    }
    if has_all {
        let mut all_p = Vec::new();
        if !Arc::ptr_eq(&before.box_model, &after.box_model) && before.box_model != after.box_model {
            all_p.extend_from_slice(&[P::Width, P::Height, P::MinWidth, P::MinHeight, P::MaxWidth, P::MaxHeight,
                P::MarginTop, P::MarginRight, P::MarginBottom, P::MarginLeft, P::PaddingTop, P::PaddingRight,
                P::PaddingBottom, P::PaddingLeft, P::Top, P::Right, P::Bottom, P::Left, P::ColumnCount]);
        }
        if !Arc::ptr_eq(&before.visual, &after.visual) && before.visual != after.visual {
            all_p.extend_from_slice(&[P::BackgroundColor, P::BorderTopColor, P::BorderRightColor, P::BorderBottomColor, P::BorderLeftColor, P::OutlineColor]);
        }
        if before.display != after.display { all_p.push(P::Display); }
        if before.position != after.position { all_p.push(P::Position); }
        if before.overlay != after.overlay { all_p.push(P::Overlay); }
        if before.opacity != after.opacity { all_p.push(P::Opacity); }
        if before.white_space_trim != after.white_space_trim { all_p.push(P::WhiteSpaceTrim); }
        if before.wrap_inside != after.wrap_inside { all_p.push(P::WrapInside); }
        if before.text_group_align != after.text_group_align { all_p.push(P::TextGroupAlign); }
        if !Arc::ptr_eq(&before.text, &after.text) && before.text != after.text {
            all_p.extend_from_slice(&[P::Color, P::FontSize, P::FontWeight, P::LineHeight, P::LetterSpacing, P::WordSpacing,
                P::TextIndent, P::TextAlign, P::TextAlignAll, P::TextTransform, P::WordBreak, P::OverflowWrap,
                P::WhiteSpaceCollapse, P::TextWrapMode, P::TextWrapStyle, P::TextAutospace, P::TextAutospaceSize,
                P::TextSpacingTrim, P::HangingPunctuation, P::Hyphens, P::HyphenateLimitChars, P::LineBreak,
                P::TextFit, P::WordSpaceTransform, P::Orphans, P::Widows, P::TabSize, P::TextJustify, P::TextAlignLast,
                P::HyphenateCharacter, P::HyphenateLimitLines, P::BorderSpacing, P::BorderCollapse, P::EmptyCells, P::CaptionSide]);
        }
        if before.z_index != after.z_index { all_p.push(P::ZIndex); }
        if before.flex_grid != after.flex_grid { all_p.extend_from_slice(&[P::FlexGrow, P::FlexShrink, P::FlexBasis, P::Order, P::FlexDirection, P::FlexWrap, P::AlignItems, P::AlignSelf, P::AlignContent, P::JustifyContent, P::JustifyItems, P::JustifySelf, P::GridTemplateColumns, P::GridTemplateRows, P::GridTemplateAreas, P::GridAutoColumns, P::GridAutoRows, P::GridAutoFlow, P::GridRowStart, P::GridRowEnd, P::GridColumnStart, P::GridColumnEnd, P::FitTolerance, P::GridLanesDirection, P::GridLanesPack]); }
        if before.effects != after.effects { all_p.extend_from_slice(&[P::Scale, P::Rotate, P::Translate, P::Transform, P::ReadingFlow, P::ReadingOrder]); }
        for pk in all_p {
            if let Some(sp) = find_matching_transition_spec(specs, pk, None, after) {
                if !out.iter().any(|(p, c, _)| *p == pk && c.is_none()) { out.push((pk, None, sp)); }
            }
        }
    }
    out
}

#[rustfmt::skip]
fn properties_differ(a: &ComputedStyle, b: &ComputedStyle, prop: PropertyIdKind, cname: Option<&InternedStr>) -> bool {
    use PropertyIdKind as P;
    match prop {
        P::Width => a.box_model.width != b.box_model.width, P::Height => a.box_model.height != b.box_model.height,
        P::MinWidth => a.box_model.min_width != b.box_model.min_width, P::MinHeight => a.box_model.min_height != b.box_model.min_height,
        P::MaxWidth => a.box_model.max_width != b.box_model.max_width, P::MaxHeight => a.box_model.max_height != b.box_model.max_height,
        P::MarginTop => a.box_model.margin.top != b.box_model.margin.top, P::MarginRight => a.box_model.margin.right != b.box_model.margin.right,
        P::MarginBottom => a.box_model.margin.bottom != b.box_model.margin.bottom, P::MarginLeft => a.box_model.margin.left != b.box_model.margin.left,
        P::PaddingTop => a.box_model.padding.top != b.box_model.padding.top, P::PaddingRight => a.box_model.padding.right != b.box_model.padding.right,
        P::PaddingBottom => a.box_model.padding.bottom != b.box_model.padding.bottom, P::PaddingLeft => a.box_model.padding.left != b.box_model.padding.left,
        P::Top => a.box_model.inset.top != b.box_model.inset.top, P::Right => a.box_model.inset.right != b.box_model.inset.right,
        P::Bottom => a.box_model.inset.bottom != b.box_model.inset.bottom, P::Left => a.box_model.inset.left != b.box_model.inset.left,
        P::Color => a.text.color != b.text.color || a.text.color_computed != b.text.color_computed,
        P::BackgroundColor => a.visual.background_color != b.visual.background_color || a.visual.background_color_computed != b.visual.background_color_computed,
        P::Opacity => a.opacity != b.opacity, P::FontSize => a.text.font_size_px != b.text.font_size_px,
        P::FontWeight => a.text.font_weight != b.text.font_weight, P::LineHeight => a.text.line_height != b.text.line_height,
        P::LetterSpacing => a.text.letter_spacing != b.text.letter_spacing, P::WordSpacing => a.text.word_spacing != b.text.word_spacing,
        P::TextIndent => a.text.text_indent != b.text.text_indent || a.text.text_indent_hanging != b.text.text_indent_hanging || a.text.text_indent_each_line != b.text.text_indent_each_line,
        P::TextAlign | P::TextAlignAll => a.text.text_align != b.text.text_align,
        P::TextTransform => a.text.text_transform_flags != b.text.text_transform_flags,
        P::WordBreak => a.text.word_break != b.text.word_break,
        P::OverflowWrap => a.text.overflow_wrap != b.text.overflow_wrap,
        P::WhiteSpaceCollapse => a.text.white_space_collapse != b.text.white_space_collapse,
        P::WhiteSpaceTrim => a.white_space_trim != b.white_space_trim,
        P::TextWrapMode => a.text.text_wrap_mode != b.text.text_wrap_mode,
        P::TextWrapStyle => a.text.text_wrap_style != b.text.text_wrap_style,
        P::TextAutospace => a.text.text_autospace != b.text.text_autospace,
        P::TextAutospaceSize => a.text.text_autospace_size != b.text.text_autospace_size,
        P::TextSpacingTrim => a.text.text_spacing_trim != b.text.text_spacing_trim,
        P::HangingPunctuation => a.text.hanging_punctuation != b.text.hanging_punctuation,
        P::Hyphens => a.text.hyphens != b.text.hyphens,
        P::HyphenateLimitChars => a.text.hyphenate_limit_chars != b.text.hyphenate_limit_chars,
        P::LineBreak => a.text.line_break != b.text.line_break,
        P::TextFit => a.text.text_fit != b.text.text_fit,
        P::TextGroupAlign => a.text_group_align != b.text_group_align,
        P::WordSpaceTransform => a.text.word_space_transform != b.text.word_space_transform,
        P::WrapInside => a.wrap_inside != b.wrap_inside,
        P::ZIndex => a.z_index != b.z_index,
        P::Orphans => a.text.orphans != b.text.orphans, P::Widows => a.text.widows != b.text.widows,
        P::Display => a.display != b.display,
        P::Position => a.position != b.position,
        P::Overlay => a.overlay != b.overlay,
        P::ColumnCount => a.box_model.column_count != b.box_model.column_count,
        P::BorderSpacing => a.text.border_spacing_x != b.text.border_spacing_x || a.text.border_spacing_y != b.text.border_spacing_y,
        P::BorderCollapse => a.text.border_collapse != b.text.border_collapse,
        P::EmptyCells => a.text.empty_cells != b.text.empty_cells,
        P::TabSize => a.text.tab_size_num != b.text.tab_size_num || a.text.tab_size_len != b.text.tab_size_len,
        P::TextJustify => a.text.text_justify != b.text.text_justify,
        P::TextAlignLast => a.text.text_align_last != b.text.text_align_last,
        P::HyphenateCharacter => a.text.hyphenate_character != b.text.hyphenate_character,
        P::HyphenateLimitLines => a.text.hyphenate_limit_lines != b.text.hyphenate_limit_lines,
        P::Order => a.flex_grid.as_ref().map_or(0, |f| f.order) != b.flex_grid.as_ref().map_or(0, |f| f.order),
        P::FlexGrow => a.flex_grid.as_ref().map_or(0.0, |f| f.flex_grow) != b.flex_grid.as_ref().map_or(0.0, |f| f.flex_grow),
        P::FlexShrink => a.flex_grid.as_ref().map_or(1.0, |f| f.flex_shrink) != b.flex_grid.as_ref().map_or(1.0, |f| f.flex_shrink),
        P::FlexBasis => a.flex_grid.as_ref().map(|f| (&f.flex_basis, &f.flex_basis_size)) != b.flex_grid.as_ref().map(|f| (&f.flex_basis, &f.flex_basis_size)),
        P::FlexDirection => a.flex_grid.as_ref().map(|f| f.flex_direction) != b.flex_grid.as_ref().map(|f| f.flex_direction),
        P::FlexWrap => a.flex_grid.as_ref().map(|f| f.flex_wrap) != b.flex_grid.as_ref().map(|f| f.flex_wrap),
        P::AlignItems => a.flex_grid.as_ref().map(|f| f.align_items) != b.flex_grid.as_ref().map(|f| f.align_items),
        P::AlignSelf => a.flex_grid.as_ref().map(|f| f.align_self) != b.flex_grid.as_ref().map(|f| f.align_self),
        P::AlignContent => a.flex_grid.as_ref().map(|f| f.align_content) != b.flex_grid.as_ref().map(|f| f.align_content),
        P::JustifyContent => a.flex_grid.as_ref().map(|f| f.justify_content) != b.flex_grid.as_ref().map(|f| f.justify_content),
        P::JustifyItems => a.flex_grid.as_ref().map(|f| f.justify_items) != b.flex_grid.as_ref().map(|f| f.justify_items),
        P::JustifySelf => a.flex_grid.as_ref().map(|f| f.justify_self) != b.flex_grid.as_ref().map(|f| f.justify_self),
        P::GridTemplateColumns => a.flex_grid.as_ref().map(|f| &f.grid_template_columns_list) != b.flex_grid.as_ref().map(|f| &f.grid_template_columns_list),
        P::GridTemplateRows => a.flex_grid.as_ref().map(|f| &f.grid_template_rows_list) != b.flex_grid.as_ref().map(|f| &f.grid_template_rows_list),
        P::GridTemplateAreas => a.flex_grid.as_ref().map(|f| &f.grid_template_areas) != b.flex_grid.as_ref().map(|f| &f.grid_template_areas),
        P::GridAutoColumns => a.flex_grid.as_ref().map(|f| &f.grid_auto_columns) != b.flex_grid.as_ref().map(|f| &f.grid_auto_columns),
        P::GridAutoRows => a.flex_grid.as_ref().map(|f| &f.grid_auto_rows) != b.flex_grid.as_ref().map(|f| &f.grid_auto_rows),
        P::GridAutoFlow => a.flex_grid.as_ref().map(|f| f.grid_auto_flow) != b.flex_grid.as_ref().map(|f| f.grid_auto_flow),
        P::GridRowStart => a.flex_grid.as_ref().map(|f| &f.grid_row_start) != b.flex_grid.as_ref().map(|f| &f.grid_row_start),
        P::GridRowEnd => a.flex_grid.as_ref().map(|f| &f.grid_row_end) != b.flex_grid.as_ref().map(|f| &f.grid_row_end),
        P::GridColumnStart => a.flex_grid.as_ref().map(|f| &f.grid_column_start) != b.flex_grid.as_ref().map(|f| &f.grid_column_start),
        P::GridColumnEnd => a.flex_grid.as_ref().map(|f| &f.grid_column_end) != b.flex_grid.as_ref().map(|f| &f.grid_column_end),
        P::FitTolerance => a.flex_grid.as_ref().map(|f| &f.fit_tolerance) != b.flex_grid.as_ref().map(|f| &f.fit_tolerance),
        P::GridLanesDirection => a.flex_grid.as_ref().map(|f| f.grid_lanes_direction) != b.flex_grid.as_ref().map(|f| f.grid_lanes_direction),
        P::GridLanesPack => a.flex_grid.as_ref().map(|f| f.grid_lanes_pack) != b.flex_grid.as_ref().map(|f| f.grid_lanes_pack),
        P::ReadingFlow => a.effects.as_ref().map(|e| e.reading_flow) != b.effects.as_ref().map(|e| e.reading_flow),
        P::ReadingOrder => a.effects.as_ref().map_or(0, |e| e.reading_order) != b.effects.as_ref().map_or(0, |e| e.reading_order),
        P::Custom => cname.and_then(|n| a.custom_properties.get(n)) != cname.and_then(|n| b.custom_properties.get(n)),
        _ => true,
    }
}

#[rustfmt::skip]
fn compose_lp(u: &LengthPercentage, k: &LengthPercentage, eval: &EvalContext, clamp_non_neg: bool) -> LengthPercentage {
    let sum = MathNode::Sum(vec![math_node_from_lp(u, eval, false), math_node_from_lp(k, eval, false)]).simplify(Some(eval), None);
    let lp = lp_from_math_node(sum);
    if clamp_non_neg { clamp_lp_non_neg(lp) } else { lp }
}

#[rustfmt::skip]
fn clamp_lp_non_neg(lp: LengthPercentage) -> LengthPercentage {
    match lp {
        LengthPercentage::Length(v, u) => LengthPercentage::Length(v.max(0.0), u),
        LengthPercentage::Percent(p) => LengthPercentage::Percent(p.max(0.0)),
        other => other,
    }
}

#[rustfmt::skip]
fn compose_lpa(u: &LengthPercentageAuto, k: &LengthPercentageAuto, eval: &EvalContext) -> LengthPercentageAuto {
    match (u, k) {
        (LengthPercentageAuto::Value(a), LengthPercentageAuto::Value(b)) => LengthPercentageAuto::Value(compose_lp(a, b, eval, false)),
        _ => k.clone(),
    }
}

#[rustfmt::skip]
fn compose_property(
    underlying: &ComputedStyle, kf: &mut ComputedStyle, prop: PropertyIdKind,
    mode: AnimationCompositionMode, allow_kw: bool, eval: &EvalContext,
) {
    use PropertyIdKind as P;
    match prop {
        P::Width => Arc::make_mut(&mut kf.box_model).width = compose_size_value(&underlying.box_model.width, &kf.box_model.width, mode, allow_kw, eval),
        P::Height => Arc::make_mut(&mut kf.box_model).height = compose_size_value(&underlying.box_model.height, &kf.box_model.height, mode, allow_kw, eval),
        P::MinWidth => Arc::make_mut(&mut kf.box_model).min_width = compose_size_value(&underlying.box_model.min_width, &kf.box_model.min_width, mode, allow_kw, eval),
        P::MinHeight => Arc::make_mut(&mut kf.box_model).min_height = compose_size_value(&underlying.box_model.min_height, &kf.box_model.min_height, mode, allow_kw, eval),
        P::MaxWidth => Arc::make_mut(&mut kf.box_model).max_width = compose_size_value(&underlying.box_model.max_width, &kf.box_model.max_width, mode, allow_kw, eval),
        P::MaxHeight => Arc::make_mut(&mut kf.box_model).max_height = compose_size_value(&underlying.box_model.max_height, &kf.box_model.max_height, mode, allow_kw, eval),
        P::Left => Arc::make_mut(&mut kf.box_model).inset.left = compose_lpa(&underlying.box_model.inset.left, &kf.box_model.inset.left, eval),
        P::Right => Arc::make_mut(&mut kf.box_model).inset.right = compose_lpa(&underlying.box_model.inset.right, &kf.box_model.inset.right, eval),
        P::Top => Arc::make_mut(&mut kf.box_model).inset.top = compose_lpa(&underlying.box_model.inset.top, &kf.box_model.inset.top, eval),
        P::Bottom => Arc::make_mut(&mut kf.box_model).inset.bottom = compose_lpa(&underlying.box_model.inset.bottom, &kf.box_model.inset.bottom, eval),
        P::MarginLeft => Arc::make_mut(&mut kf.box_model).margin.left = compose_lpa(&underlying.box_model.margin.left, &kf.box_model.margin.left, eval),
        P::MarginRight => Arc::make_mut(&mut kf.box_model).margin.right = compose_lpa(&underlying.box_model.margin.right, &kf.box_model.margin.right, eval),
        P::MarginTop => Arc::make_mut(&mut kf.box_model).margin.top = compose_lpa(&underlying.box_model.margin.top, &kf.box_model.margin.top, eval),
        P::MarginBottom => Arc::make_mut(&mut kf.box_model).margin.bottom = compose_lpa(&underlying.box_model.margin.bottom, &kf.box_model.margin.bottom, eval),
        P::PaddingLeft => Arc::make_mut(&mut kf.box_model).padding.left = compose_lp(&underlying.box_model.padding.left, &kf.box_model.padding.left, eval, true),
        P::PaddingRight => Arc::make_mut(&mut kf.box_model).padding.right = compose_lp(&underlying.box_model.padding.right, &kf.box_model.padding.right, eval, true),
        P::PaddingTop => Arc::make_mut(&mut kf.box_model).padding.top = compose_lp(&underlying.box_model.padding.top, &kf.box_model.padding.top, eval, true),
        P::PaddingBottom => Arc::make_mut(&mut kf.box_model).padding.bottom = compose_lp(&underlying.box_model.padding.bottom, &kf.box_model.padding.bottom, eval, true),
        P::TextIndent => {
            if underlying.text.text_indent_hanging == kf.text.text_indent_hanging
                && underlying.text.text_indent_each_line == kf.text.text_indent_each_line
            {
                Arc::make_mut(&mut kf.text).text_indent =
                    compose_lp(&underlying.text.text_indent, &kf.text.text_indent, eval, false);
            }
        }
        P::TextAutospaceSize => {
            Arc::make_mut(&mut kf.text).text_autospace_size =
                underlying.text.text_autospace_size + kf.text.text_autospace_size;
        }
        P::HyphenateLimitChars => {
            let (u, k) = (underlying.text.hyphenate_limit_chars, kf.text.hyphenate_limit_chars);
            let add_opt = |a: Option<i32>, b: Option<i32>| -> Option<Option<i32>> {
                match (a, b) {
                    (None, None) => Some(None),
                    (Some(av), Some(bv)) => Some(Some(av.saturating_add(bv))),
                    _ => None,
                }
            };
            if let (Some(w), Some(b), Some(a)) = (add_opt(u.word, k.word), add_opt(u.before, k.before), add_opt(u.after, k.after)) {
                Arc::make_mut(&mut kf.text).hyphenate_limit_chars = crate::text_css::HyphenateLimitChars { word: w, before: b, after: a };
            }
        }
        P::LetterSpacing => Arc::make_mut(&mut kf.text).letter_spacing = compose_lp(&underlying.text.letter_spacing, &kf.text.letter_spacing, eval, false),
        P::WordSpacing => Arc::make_mut(&mut kf.text).word_spacing = compose_lp(&underlying.text.word_spacing, &kf.text.word_spacing, eval, false),
        P::BorderSpacing => {
            let x = compose_lp(&underlying.text.border_spacing_x, &kf.text.border_spacing_x, eval, true);
            let y = compose_lp(&underlying.text.border_spacing_y, &kf.text.border_spacing_y, eval, true);
            let txt = Arc::make_mut(&mut kf.text); txt.border_spacing_x = x; txt.border_spacing_y = y;
        }
        P::TabSize => {
            match (&underlying.text.tab_size_len, &kf.text.tab_size_len) {
                (Some(a), Some(b)) => { let v = compose_lp(a, b, eval, true); Arc::make_mut(&mut kf.text).tab_size_len = Some(v); }
                (None, None) => Arc::make_mut(&mut kf.text).tab_size_num = (underlying.text.tab_size_num + kf.text.tab_size_num).max(0.0),
                _ => {}
            }
        }
        P::Order => {
            let u = underlying.flex_grid.as_ref().map_or(0, |f| f.order);
            let k = kf.flex_grid.as_ref().map_or(0, |f| f.order);
            kf.flex_grid_mut().order = u.saturating_add(k);
        }
        P::FlexGrow => {
            let u = underlying.flex_grid.as_ref().map_or(0.0, |f| f.flex_grow);
            let k = kf.flex_grid.as_ref().map_or(0.0, |f| f.flex_grow);
            kf.flex_grid_mut().flex_grow = (u + k).max(0.0);
        }
        P::FlexShrink => {
            let u = underlying.flex_grid.as_ref().map_or(1.0, |f| f.flex_shrink);
            let k = kf.flex_grid.as_ref().map_or(1.0, |f| f.flex_shrink);
            kf.flex_grid_mut().flex_shrink = (u + k).max(0.0);
        }
        P::FlexBasis => {
            let get_fb = |st: &ComputedStyle| st.flex_grid.as_ref().and_then(|f| f.flex_basis_size.clone()).unwrap_or_else(|| match st.flex_grid.as_ref().map(|f| &f.flex_basis) {
                Some(LengthPercentageAuto::Value(lp)) => SizeValue::Length(lp.clone()), _ => SizeValue::Auto,
            });
            let res = compose_size_value(&get_fb(underlying), &get_fb(kf), mode, allow_kw, eval);
            let fg = kf.flex_grid_mut();
            match &res {
                SizeValue::Length(lp) => { fg.flex_basis = LengthPercentageAuto::Value(lp.clone()); fg.flex_basis_size = None; }
                SizeValue::Auto => { fg.flex_basis = LengthPercentageAuto::Auto; fg.flex_basis_size = None; }
                _ => { fg.flex_basis = LengthPercentageAuto::Auto; fg.flex_basis_size = Some(res); }
            }
        }
        P::ZIndex => if let (ZIndexValue::Integer(u), ZIndexValue::Integer(k)) = (underlying.z_index, kf.z_index) { kf.z_index = ZIndexValue::Integer(u.saturating_add(k)); },
        P::HyphenateLimitLines => if let (Some(u), Some(k)) = (underlying.text.hyphenate_limit_lines, kf.text.hyphenate_limit_lines) { Arc::make_mut(&mut kf.text).hyphenate_limit_lines = Some(u.saturating_add(k).max(0)); },
        P::ReadingOrder => {
            let u = underlying.effects.as_ref().map_or(0, |e| e.reading_order);
            let k = kf.effects.as_ref().map_or(0, |e| e.reading_order);
            kf.effects_mut().reading_order = u.saturating_add(k);
        }
        P::Color => { let c = compose_resolved_color(resolved_text_color(underlying, eval), resolved_text_color(kf, eval)); let t = Arc::make_mut(&mut kf.text); t.color = c.to_srgb_u8(); t.color_computed = Some(Arc::new(CssColorExpr::Resolved(c))); }
        P::BackgroundColor => { let c = compose_resolved_color(resolved_bg_color(underlying, eval), resolved_bg_color(kf, eval)); let v = Arc::make_mut(&mut kf.visual); v.background_color = c.to_srgb_u8(); v.background_color_computed = Some(Arc::new(CssColorExpr::Resolved(c))); }
        P::BorderTopColor | P::BorderRightColor | P::BorderBottomColor | P::BorderLeftColor => {
            let idx = match prop { P::BorderTopColor => 0, P::BorderRightColor => 1, P::BorderBottomColor => 2, _ => 3 };
            let c = compose_resolved_color(resolved_border_color(underlying, idx, eval), resolved_border_color(kf, idx, eval));
            let v = Arc::make_mut(&mut kf.visual);
            v.border_color_computed[idx] = Some(Arc::new(CssColorExpr::Resolved(c)));
            let rgba = CssColor::Rgba(c.to_srgb_u8());
            match idx { 0 => v.border_color.top = rgba, 1 => v.border_color.right = rgba, 2 => v.border_color.bottom = rgba, _ => v.border_color.left = rgba }
        }
        P::OutlineColor => {
            let c = compose_resolved_color(resolved_outline_color(underlying, eval), resolved_outline_color(kf, eval));
            let v = Arc::make_mut(&mut kf.visual);
            v.outline_color_computed = Some(Arc::new(CssColorExpr::Resolved(c)));
            v.outline_color = CssColor::Rgba(c.to_srgb_u8());
        }
        P::TextDecorationColor => {
            let c = compose_resolved_color(resolved_text_decor_color(underlying, eval), resolved_text_decor_color(kf, eval));
            let v = Arc::make_mut(&mut kf.visual);
            v.text_decoration_color_computed = Some(Arc::new(CssColorExpr::Resolved(c)));
            v.text_decoration_color = Some(CssColor::Rgba(c.to_srgb_u8()));
        }
        P::Opacity => kf.opacity += underlying.opacity,
        P::BorderTopWidth => Arc::make_mut(&mut kf.box_model).border_width.top = (underlying.box_model.border_width.top + kf.box_model.border_width.top).max(0.0),
        P::BorderRightWidth => Arc::make_mut(&mut kf.box_model).border_width.right = (underlying.box_model.border_width.right + kf.box_model.border_width.right).max(0.0),
        P::BorderBottomWidth => Arc::make_mut(&mut kf.box_model).border_width.bottom = (underlying.box_model.border_width.bottom + kf.box_model.border_width.bottom).max(0.0),
        P::BorderLeftWidth => Arc::make_mut(&mut kf.box_model).border_width.left = (underlying.box_model.border_width.left + kf.box_model.border_width.left).max(0.0),
        P::OutlineWidth => Arc::make_mut(&mut kf.visual).outline_width = (underlying.visual.outline_width + kf.visual.outline_width).max(0.0),
        P::OutlineOffset => Arc::make_mut(&mut kf.visual).outline_offset = compose_lp(&underlying.visual.outline_offset, &kf.visual.outline_offset, eval, false),
        P::RowGap => {
            let u = underlying.flex_grid.as_ref().map_or(LengthPercentage::Zero, |f| f.row_gap.clone());
            let k = kf.flex_grid.as_ref().map_or(LengthPercentage::Zero, |f| f.row_gap.clone());
            kf.flex_grid_mut().row_gap = compose_lp(&u, &k, eval, true);
        }
        P::ColumnGap => {
            let u = underlying.flex_grid.as_ref().map_or(LengthPercentage::Zero, |f| f.column_gap.clone());
            let k = kf.flex_grid.as_ref().map_or(LengthPercentage::Zero, |f| f.column_gap.clone());
            kf.flex_grid_mut().column_gap = compose_lp(&u, &k, eval, true);
        }
        P::BorderTopLeftRadius | P::BorderTopRightRadius | P::BorderBottomRightRadius | P::BorderBottomLeftRadius => {
            let (u, k) = match prop {
                P::BorderTopLeftRadius => (&underlying.visual.border_radius.top_left, &kf.visual.border_radius.top_left),
                P::BorderTopRightRadius => (&underlying.visual.border_radius.top_right, &kf.visual.border_radius.top_right),
                P::BorderBottomRightRadius => (&underlying.visual.border_radius.bottom_right, &kf.visual.border_radius.bottom_right),
                _ => (&underlying.visual.border_radius.bottom_left, &kf.visual.border_radius.bottom_left),
            };
            let res = CornerRadius { x: compose_lp(&u.x, &k.x, eval, true), y: compose_lp(&u.y, &k.y, eval, true) };
            let br = &mut Arc::make_mut(&mut kf.visual).border_radius;
            match prop {
                P::BorderTopLeftRadius => br.top_left = res, P::BorderTopRightRadius => br.top_right = res,
                P::BorderBottomRightRadius => br.bottom_right = res, _ => br.bottom_left = res,
            }
        }
        P::BackgroundPosition => {
            let (ux, uy) = &underlying.visual.background_position;
            let (kx, ky) = &kf.visual.background_position;
            Arc::make_mut(&mut kf.visual).background_position = (compose_lp(ux, kx, eval, false), compose_lp(uy, ky, eval, false));
        }
        P::FontSize => {
            let px = (underlying.text.font_size_px + kf.text.font_size_px).max(0.0);
            let t = Arc::make_mut(&mut kf.text); t.font_size_px = px; t.eval_ctx.font_size_px = px;
        }
        P::FontWeight => Arc::make_mut(&mut kf.text).font_weight = (underlying.text.font_weight + kf.text.font_weight).clamp(1.0, 1000.0),
        P::ColumnCount => if let (Some(u), Some(k)) = (underlying.box_model.column_count, kf.box_model.column_count) { Arc::make_mut(&mut kf.box_model).column_count = Some(u.saturating_add(k).max(1)); },
        P::Orphans => Arc::make_mut(&mut kf.text).orphans = underlying.text.orphans.saturating_add(kf.text.orphans).max(1),
        P::Widows => Arc::make_mut(&mut kf.text).widows = underlying.text.widows.saturating_add(kf.text.widows).max(1),
        P::BoxShadow => {
            let u = underlying.effects.as_ref().map(|e| e.box_shadow.as_slice()).unwrap_or(&[]);
            let k = kf.effects.as_ref().map(|e| e.box_shadow.as_slice()).unwrap_or(&[]);
            kf.effects_mut().box_shadow = compose_box_shadows(u, k, mode, underlying.text.color, kf.text.color, eval);
        }
        P::Filter => {
            let u = underlying.effects.as_ref().map(|e| e.filter.as_slice()).unwrap_or(&[]);
            let k = kf.effects.as_ref().map(|e| e.filter.as_slice()).unwrap_or(&[]);
            kf.effects_mut().filter = compose_filters(u, k, mode, underlying.text.color, kf.text.color, eval);
        }
        P::Scale => {
            let u = underlying.effects.as_ref().map_or(ScaleSpec::None, |e| e.scale.clone());
            let k = kf.effects.as_ref().map_or(ScaleSpec::None, |e| e.scale.clone());
            kf.effects_mut().scale = compose_scale(&u, &k, mode, eval);
        }
        P::Rotate => {
            let u = underlying.effects.as_ref().map_or(RotateSpec::None, |e| e.rotate.clone());
            let k = kf.effects.as_ref().map_or(RotateSpec::None, |e| e.rotate.clone());
            kf.effects_mut().rotate = compose_rotate(&u, &k, eval);
        }
        P::Translate => {
            let u = underlying.effects.as_ref().map_or(TranslateSpec::None, |e| e.translate.clone());
            let k = kf.effects.as_ref().map_or(TranslateSpec::None, |e| e.translate.clone());
            kf.effects_mut().translate = compose_translate(&u, &k, eval);
        }
        P::Transform => {
            let u_ops = extract_transform_ops(underlying, eval);
            let k_ops = extract_transform_ops(kf, eval);
            let res = compose_transforms(&u_ops, &k_ops, mode, eval);
            let eff = kf.effects_mut();
            eff.transform = res;
            eff.transform_raw = None;
        }
        P::OffsetDistance | P::ShapeMargin | P::StrokeWidth | P::StrokeDashoffset
        | P::BackgroundPositionX | P::BackgroundPositionY | P::ScrollPaddingTop | P::ScrollPaddingRight
        | P::ScrollPaddingBottom | P::ScrollPaddingLeft | P::SvgCx | P::SvgCy | P::SvgRx | P::SvgRy
        | P::SvgX | P::SvgY | P::BorderImageSlice => {
            if let (Some(un), Some(kn)) = (
                underlying.effects.as_ref().and_then(|e| e.extra_math_props.get(&prop)),
                kf.effects.as_ref().and_then(|e| e.extra_math_props.get(&prop)),
            ) {
                let sum = MathNode::Sum(vec![un.clone(), kn.clone()]).simplify(Some(eval), None);
                kf.effects_mut().extra_math_props.insert(prop, sum);
            }
        }
        P::GridTemplateColumns => {
            let u = underlying.flex_grid.as_ref().map_or(crate::grid_css::GridTemplateTrackList::None, |f| f.grid_template_columns_list.clone());
            let k = kf.flex_grid.as_ref().map_or(crate::grid_css::GridTemplateTrackList::None, |f| f.grid_template_columns_list.clone());
            let c = crate::grid_css::compose_track_list(&u, &k, mode, eval);
            let fg = kf.flex_grid_mut();
            fg.grid_template_columns = c.flatten_simple_tracks();
            fg.grid_template_columns_list = c;
        }
        P::GridTemplateRows => {
            let u = underlying.flex_grid.as_ref().map_or(crate::grid_css::GridTemplateTrackList::None, |f| f.grid_template_rows_list.clone());
            let k = kf.flex_grid.as_ref().map_or(crate::grid_css::GridTemplateTrackList::None, |f| f.grid_template_rows_list.clone());
            let c = crate::grid_css::compose_track_list(&u, &k, mode, eval);
            let fg = kf.flex_grid_mut();
            fg.grid_template_rows = c.flatten_simple_tracks();
            fg.grid_template_rows_list = c;
        }
        P::FitTolerance => {
            let u = underlying.flex_grid.as_ref().map_or(crate::grid_css::FitToleranceValue::Normal, |f| f.fit_tolerance.clone());
            let k = kf.flex_grid.as_ref().map_or(crate::grid_css::FitToleranceValue::Normal, |f| f.fit_tolerance.clone());
            kf.flex_grid_mut().fit_tolerance = crate::grid_css::compose_fit_tolerance(&u, &k, mode, eval);
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
#[rustfmt::skip]
fn interpolate_property_into(
    target: &mut ComputedStyle, from: &ComputedStyle, to: &ComputedStyle,
    prop: PropertyIdKind, cname: Option<&InternedStr>, p: f64,
    is_transition: bool, allow_discrete: bool, allow_kw: bool,
    eval: &EvalContext, registered_props: &HashMap<InternedStr, RegisteredProperty>,
) {
    use PropertyIdKind as P;
    let use_from_disc = allow_discrete && p < 0.5;
    match prop {
        P::Width | P::Height | P::MinWidth | P::MinHeight | P::MaxWidth | P::MaxHeight => {
            let (fv, tv) = match prop {
                P::Width => (&from.box_model.width, &to.box_model.width), P::Height => (&from.box_model.height, &to.box_model.height),
                P::MinWidth => (&from.box_model.min_width, &to.box_model.min_width), P::MinHeight => (&from.box_model.min_height, &to.box_model.min_height),
                P::MaxWidth => (&from.box_model.max_width, &to.box_model.max_width), _ => (&from.box_model.max_height, &to.box_model.max_height),
            };
            let res = interpolate_size_value(fv, tv, p, allow_kw, eval, is_transition).unwrap_or_else(|| if use_from_disc { fv.clone() } else { tv.clone() });
            let bm = Arc::make_mut(&mut target.box_model);
            match prop { P::Width => bm.width = res, P::Height => bm.height = res, P::MinWidth => bm.min_width = res, P::MinHeight => bm.min_height = res, P::MaxWidth => bm.max_width = res, _ => bm.max_height = res }
        }
        P::Left | P::Right | P::Top | P::Bottom | P::MarginLeft | P::MarginRight | P::MarginTop | P::MarginBottom => {
            let (fv, tv) = match prop {
                P::Left => (&from.box_model.inset.left, &to.box_model.inset.left), P::Right => (&from.box_model.inset.right, &to.box_model.inset.right),
                P::Top => (&from.box_model.inset.top, &to.box_model.inset.top), P::Bottom => (&from.box_model.inset.bottom, &to.box_model.inset.bottom),
                P::MarginLeft => (&from.box_model.margin.left, &to.box_model.margin.left), P::MarginRight => (&from.box_model.margin.right, &to.box_model.margin.right),
                P::MarginTop => (&from.box_model.margin.top, &to.box_model.margin.top), _ => (&from.box_model.margin.bottom, &to.box_model.margin.bottom),
            };
            let res = match (fv, tv) {
                (LengthPercentageAuto::Value(a), LengthPercentageAuto::Value(b)) => LengthPercentageAuto::Value(interpolate_lp(a, b, p, eval, is_transition)),
                _ => if use_from_disc { fv.clone() } else { tv.clone() },
            };
            let bm = Arc::make_mut(&mut target.box_model);
            match prop { P::Left => bm.inset.left = res, P::Right => bm.inset.right = res, P::Top => bm.inset.top = res, P::Bottom => bm.inset.bottom = res, P::MarginLeft => bm.margin.left = res, P::MarginRight => bm.margin.right = res, P::MarginTop => bm.margin.top = res, _ => bm.margin.bottom = res }
        }
        P::PaddingTop | P::PaddingRight | P::PaddingBottom | P::PaddingLeft | P::LetterSpacing | P::WordSpacing => {
            let (fv, tv) = match prop {
                P::PaddingTop => (&from.box_model.padding.top, &to.box_model.padding.top), P::PaddingRight => (&from.box_model.padding.right, &to.box_model.padding.right),
                P::PaddingBottom => (&from.box_model.padding.bottom, &to.box_model.padding.bottom), P::PaddingLeft => (&from.box_model.padding.left, &to.box_model.padding.left),
                P::LetterSpacing => (&from.text.letter_spacing, &to.text.letter_spacing),
                _ => (&from.text.word_spacing, &to.text.word_spacing),
            };
            let mut res = interpolate_lp(fv, tv, p, eval, is_transition);
            if matches!(prop, P::PaddingTop | P::PaddingRight | P::PaddingBottom | P::PaddingLeft) {
                res = clamp_lp_non_neg(res);
            }
            match prop {
                P::PaddingTop => Arc::make_mut(&mut target.box_model).padding.top = res, P::PaddingRight => Arc::make_mut(&mut target.box_model).padding.right = res,
                P::PaddingBottom => Arc::make_mut(&mut target.box_model).padding.bottom = res, P::PaddingLeft => Arc::make_mut(&mut target.box_model).padding.left = res,
                P::LetterSpacing => Arc::make_mut(&mut target.text).letter_spacing = res,
                _ => Arc::make_mut(&mut target.text).word_spacing = res,
            }
        }
        P::TextIndent => {
            let t = Arc::make_mut(&mut target.text);
            if from.text.text_indent_hanging == to.text.text_indent_hanging
                && from.text.text_indent_each_line == to.text.text_indent_each_line
            {
                t.text_indent = interpolate_lp(&from.text.text_indent, &to.text.text_indent, p, eval, is_transition);
                t.text_indent_hanging = from.text.text_indent_hanging;
                t.text_indent_each_line = from.text.text_indent_each_line;
            } else {
                let src = if use_from_disc { &from.text } else { &to.text };
                t.text_indent = src.text_indent.clone();
                t.text_indent_hanging = src.text_indent_hanging;
                t.text_indent_each_line = src.text_indent_each_line;
            }
        }
        P::Display => {
            target.display = if is_transition && !allow_discrete {
                to.display
            } else if from.display == DisplayMode::None && to.display != DisplayMode::None {
                if p > 0.0 { to.display } else { DisplayMode::None }
            } else if from.display != DisplayMode::None && to.display == DisplayMode::None {
                if p < 1.0 { from.display } else { DisplayMode::None }
            } else if use_from_disc {
                from.display
            } else {
                to.display
            };
        }
        P::Overlay => {
            target.overlay = if is_transition && !allow_discrete {
                to.overlay
            } else if from.overlay == OverlayMode::None && to.overlay == OverlayMode::Auto {
                if p > 0.0 { OverlayMode::Auto } else { OverlayMode::None }
            } else if from.overlay == OverlayMode::Auto && to.overlay == OverlayMode::None {
                if p < 1.0 { OverlayMode::Auto } else { OverlayMode::None }
            } else {
                to.overlay
            };
        }
        P::Position => target.position = if use_from_disc { from.position } else { to.position },
        P::Order => {
            let (a, b) = (from.flex_grid.as_ref().map_or(0, |f| f.order), to.flex_grid.as_ref().map_or(0, |f| f.order));
            target.flex_grid_mut().order = ((f64::from(a) * (1.0 - p) + f64::from(b) * p) + 0.5).floor() as i32;
        }
        P::FlexGrow => {
            let (a, b) = (from.flex_grid.as_ref().map_or(0.0, |f| f.flex_grow), to.flex_grid.as_ref().map_or(0.0, |f| f.flex_grow));
            target.flex_grid_mut().flex_grow = (f64::from(a) * (1.0 - p) + f64::from(b) * p).max(0.0) as f32;
        }
        P::FlexShrink => {
            let (a, b) = (from.flex_grid.as_ref().map_or(1.0, |f| f.flex_shrink), to.flex_grid.as_ref().map_or(1.0, |f| f.flex_shrink));
            target.flex_grid_mut().flex_shrink = (f64::from(a) * (1.0 - p) + f64::from(b) * p).max(0.0) as f32;
        }
        P::BorderSpacing => {
            let x = clamp_lp_non_neg(interpolate_lp(&from.text.border_spacing_x, &to.text.border_spacing_x, p, eval, is_transition));
            let y = clamp_lp_non_neg(interpolate_lp(&from.text.border_spacing_y, &to.text.border_spacing_y, p, eval, is_transition));
            let txt = Arc::make_mut(&mut target.text); txt.border_spacing_x = x; txt.border_spacing_y = y;
        }
        P::BorderCollapse => Arc::make_mut(&mut target.text).border_collapse = if use_from_disc { from.text.border_collapse } else { to.text.border_collapse },
        P::EmptyCells => Arc::make_mut(&mut target.text).empty_cells = if use_from_disc { from.text.empty_cells } else { to.text.empty_cells },
        P::TabSize => {
            let t = Arc::make_mut(&mut target.text);
            match (&from.text.tab_size_len, &to.text.tab_size_len) {
                (Some(a), Some(b)) => { t.tab_size_len = Some(clamp_lp_non_neg(interpolate_lp(a, b, p, eval, is_transition))); }
                (None, None) => { t.tab_size_len = None; t.tab_size_num = (f64::from(from.text.tab_size_num) * (1.0 - p) + f64::from(to.text.tab_size_num) * p).max(0.0) as f32; }
                _ => {
                    let src = if use_from_disc { &from.text } else { &to.text };
                    t.tab_size_len = src.tab_size_len.clone();
                    t.tab_size_num = src.tab_size_num;
                }
            }
        }
        P::TextJustify => Arc::make_mut(&mut target.text).text_justify = if use_from_disc { from.text.text_justify } else { to.text.text_justify },
        P::TextAlignLast => Arc::make_mut(&mut target.text).text_align_last = if use_from_disc { from.text.text_align_last } else { to.text.text_align_last },
        P::HyphenateCharacter => Arc::make_mut(&mut target.text).hyphenate_character = if use_from_disc { from.text.hyphenate_character.clone() } else { to.text.hyphenate_character.clone() },
        P::HyphenateLimitLines => Arc::make_mut(&mut target.text).hyphenate_limit_lines = match (from.text.hyphenate_limit_lines, to.text.hyphenate_limit_lines) {
            (Some(a), Some(b)) => Some((((f64::from(a) * (1.0 - p) + f64::from(b) * p) + 0.5).floor() as i32).max(0)),
            _ => if use_from_disc { from.text.hyphenate_limit_lines } else { to.text.hyphenate_limit_lines },
        },
        P::HyphenateLimitChars => Arc::make_mut(&mut target.text).hyphenate_limit_chars = from.text.hyphenate_limit_chars.interpolate(to.text.hyphenate_limit_chars, p, use_from_disc),
        P::Hyphens => Arc::make_mut(&mut target.text).hyphens = if use_from_disc { from.text.hyphens } else { to.text.hyphens },
        P::LineBreak => Arc::make_mut(&mut target.text).line_break = if use_from_disc { from.text.line_break } else { to.text.line_break },
        P::TextAlign | P::TextAlignAll => Arc::make_mut(&mut target.text).text_align = if use_from_disc { from.text.text_align } else { to.text.text_align },
        P::TextTransform => {
            let src = if use_from_disc { &from.text } else { &to.text };
            let t = Arc::make_mut(&mut target.text);
            t.text_transform = src.text_transform;
            t.text_transform_flags = src.text_transform_flags;
        }
        P::WordBreak => Arc::make_mut(&mut target.text).word_break = if use_from_disc { from.text.word_break } else { to.text.word_break },
        P::OverflowWrap => Arc::make_mut(&mut target.text).overflow_wrap = if use_from_disc { from.text.overflow_wrap } else { to.text.overflow_wrap },
        P::WhiteSpaceCollapse => {
            let t = Arc::make_mut(&mut target.text);
            t.white_space_collapse = if use_from_disc { from.text.white_space_collapse } else { to.text.white_space_collapse };
            t.white_space = crate::text_css::derive_white_space_mode(t.white_space_collapse, t.text_wrap_mode);
        }
        P::TextWrapMode => {
            let t = Arc::make_mut(&mut target.text);
            t.text_wrap_mode = if use_from_disc { from.text.text_wrap_mode } else { to.text.text_wrap_mode };
            t.white_space = crate::text_css::derive_white_space_mode(t.white_space_collapse, t.text_wrap_mode);
        }
        P::TextWrapStyle => Arc::make_mut(&mut target.text).text_wrap_style = if use_from_disc { from.text.text_wrap_style } else { to.text.text_wrap_style },
        P::WhiteSpaceTrim => target.white_space_trim = if use_from_disc { from.white_space_trim } else { to.white_space_trim },
        P::TextAutospace => Arc::make_mut(&mut target.text).text_autospace = if use_from_disc { from.text.text_autospace } else { to.text.text_autospace },
        P::TextAutospaceSize => Arc::make_mut(&mut target.text).text_autospace_size = (f64::from(from.text.text_autospace_size) * (1.0 - p) + f64::from(to.text.text_autospace_size) * p) as f32,
        P::TextSpacingTrim => Arc::make_mut(&mut target.text).text_spacing_trim = if use_from_disc { from.text.text_spacing_trim } else { to.text.text_spacing_trim },
        P::HangingPunctuation => Arc::make_mut(&mut target.text).hanging_punctuation = if use_from_disc { from.text.hanging_punctuation } else { to.text.hanging_punctuation },
        P::TextFit => Arc::make_mut(&mut target.text).text_fit = if use_from_disc { from.text.text_fit } else { to.text.text_fit },
        P::TextGroupAlign => target.text_group_align = if use_from_disc { from.text_group_align } else { to.text_group_align },
        P::WordSpaceTransform => Arc::make_mut(&mut target.text).word_space_transform = if use_from_disc { from.text.word_space_transform } else { to.text.word_space_transform },
        P::WrapInside => target.wrap_inside = if use_from_disc { from.wrap_inside } else { to.wrap_inside },
        P::ReadingFlow => target.effects_mut().reading_flow = if use_from_disc { from.effects.as_ref().map_or_else(Default::default, |e| e.reading_flow) } else { to.effects.as_ref().map_or_else(Default::default, |e| e.reading_flow) },
        P::ReadingOrder => {
            let (a, b) = (from.effects.as_ref().map_or(0, |e| e.reading_order), to.effects.as_ref().map_or(0, |e| e.reading_order));
            target.effects_mut().reading_order = ((f64::from(a) * (1.0 - p) + f64::from(b) * p) + 0.5).floor() as i32;
        }
        P::Opacity => target.opacity = (f64::from(from.opacity) * (1.0 - p) + f64::from(to.opacity) * p) as f32,
        P::Visibility => {
            target.visibility = if p <= 0.0 {
                from.visibility
            } else if p >= 1.0 {
                to.visibility
            } else if from.visibility == VisibilityMode::Visible || to.visibility == VisibilityMode::Visible {
                VisibilityMode::Visible
            } else if use_from_disc {
                from.visibility
            } else {
                to.visibility
            };
        }
        P::Float => target.float_mode = if use_from_disc { from.float_mode } else { to.float_mode },
        P::Clear => target.clear_mode = if use_from_disc { from.clear_mode } else { to.clear_mode },
        P::BoxSizing => target.box_sizing = if use_from_disc { from.box_sizing } else { to.box_sizing },
        P::OverflowX => target.overflow.x = if use_from_disc { from.overflow.x } else { to.overflow.x },
        P::OverflowY => target.overflow.y = if use_from_disc { from.overflow.y } else { to.overflow.y },
        P::PointerEvents => target.pointer_events = if use_from_disc { from.pointer_events } else { to.pointer_events },
        P::Cursor => target.cursor = if use_from_disc { from.cursor } else { to.cursor },
        P::CaptionSide => Arc::make_mut(&mut target.text).caption_side = if use_from_disc { from.text.caption_side } else { to.text.caption_side },
        P::FontFamily => Arc::make_mut(&mut target.text).font_family = if use_from_disc { from.text.font_family.clone() } else { to.text.font_family.clone() },
        P::Resize => Arc::make_mut(&mut target.visual).resize = if use_from_disc { from.visual.resize } else { to.visual.resize },
        P::OverscrollBehaviorX => Arc::make_mut(&mut target.visual).overscroll_behavior_x = if use_from_disc { from.visual.overscroll_behavior_x } else { to.visual.overscroll_behavior_x },
        P::OverscrollBehaviorY => Arc::make_mut(&mut target.visual).overscroll_behavior_y = if use_from_disc { from.visual.overscroll_behavior_y } else { to.visual.overscroll_behavior_y },
        P::ScrollBehavior => Arc::make_mut(&mut target.visual).scroll_behavior = if use_from_disc { from.visual.scroll_behavior } else { to.visual.scroll_behavior },
        P::ContentVisibility => Arc::make_mut(&mut target.visual).content_visibility = if use_from_disc { from.visual.content_visibility } else { to.visual.content_visibility },
        P::UserSelect => Arc::make_mut(&mut target.visual).user_select = if use_from_disc { from.visual.user_select } else { to.visual.user_select },
        P::Zoom => Arc::make_mut(&mut target.visual).zoom = (f64::from(from.visual.zoom) * (1.0 - p) + f64::from(to.visual.zoom) * p).max(0.0) as f32,
        P::TableLayout => Arc::make_mut(&mut target.box_model).table_layout = if use_from_disc { from.box_model.table_layout } else { to.box_model.table_layout },
        P::TextOverflow => Arc::make_mut(&mut target.box_model).text_overflow = if use_from_disc { from.box_model.text_overflow } else { to.box_model.text_overflow },
        P::Content => Arc::make_mut(&mut target.box_model).content = if use_from_disc { from.box_model.content.clone() } else { to.box_model.content.clone() },
        P::VerticalAlign => {
            Arc::make_mut(&mut target.box_model).vertical_align = match (from.box_model.vertical_align, to.box_model.vertical_align) {
                (VerticalAlignMode::LengthPx(a), VerticalAlignMode::LengthPx(b)) => {
                    VerticalAlignMode::LengthPx((f64::from(a) * (1.0 - p) + f64::from(b) * p) as f32)
                }
                _ => if use_from_disc { from.box_model.vertical_align } else { to.box_model.vertical_align },
            };
        }
        P::BorderTopWidth => Arc::make_mut(&mut target.box_model).border_width.top = (f64::from(from.box_model.border_width.top) * (1.0 - p) + f64::from(to.box_model.border_width.top) * p).max(0.0) as f32,
        P::BorderRightWidth => Arc::make_mut(&mut target.box_model).border_width.right = (f64::from(from.box_model.border_width.right) * (1.0 - p) + f64::from(to.box_model.border_width.right) * p).max(0.0) as f32,
        P::BorderBottomWidth => Arc::make_mut(&mut target.box_model).border_width.bottom = (f64::from(from.box_model.border_width.bottom) * (1.0 - p) + f64::from(to.box_model.border_width.bottom) * p).max(0.0) as f32,
        P::BorderLeftWidth => Arc::make_mut(&mut target.box_model).border_width.left = (f64::from(from.box_model.border_width.left) * (1.0 - p) + f64::from(to.box_model.border_width.left) * p).max(0.0) as f32,
        P::BorderTopStyle => Arc::make_mut(&mut target.visual).border_style.top = if use_from_disc { from.visual.border_style.top } else { to.visual.border_style.top },
        P::BorderRightStyle => Arc::make_mut(&mut target.visual).border_style.right = if use_from_disc { from.visual.border_style.right } else { to.visual.border_style.right },
        P::BorderBottomStyle => Arc::make_mut(&mut target.visual).border_style.bottom = if use_from_disc { from.visual.border_style.bottom } else { to.visual.border_style.bottom },
        P::BorderLeftStyle => Arc::make_mut(&mut target.visual).border_style.left = if use_from_disc { from.visual.border_style.left } else { to.visual.border_style.left },
        P::BorderTopColor | P::BorderRightColor | P::BorderBottomColor | P::BorderLeftColor => {
            let idx = match prop { P::BorderTopColor => 0, P::BorderRightColor => 1, P::BorderBottomColor => 2, _ => 3 };
            let c = interpolate_resolved_color(resolved_border_color(from, idx, eval), resolved_border_color(to, idx, eval), p);
            let v = Arc::make_mut(&mut target.visual);
            v.border_color_computed[idx] = Some(Arc::new(CssColorExpr::Resolved(c)));
            let rgba = CssColor::Rgba(c.to_srgb_u8());
            match idx { 0 => v.border_color.top = rgba, 1 => v.border_color.right = rgba, 2 => v.border_color.bottom = rgba, _ => v.border_color.left = rgba }
        }
        P::BorderTopLeftRadius | P::BorderTopRightRadius | P::BorderBottomRightRadius | P::BorderBottomLeftRadius => {
            let (fr, tr) = match prop {
                P::BorderTopLeftRadius => (&from.visual.border_radius.top_left, &to.visual.border_radius.top_left),
                P::BorderTopRightRadius => (&from.visual.border_radius.top_right, &to.visual.border_radius.top_right),
                P::BorderBottomRightRadius => (&from.visual.border_radius.bottom_right, &to.visual.border_radius.bottom_right),
                _ => (&from.visual.border_radius.bottom_left, &to.visual.border_radius.bottom_left),
            };
            let res = CornerRadius {
                x: clamp_lp_non_neg(interpolate_lp(&fr.x, &tr.x, p, eval, is_transition)),
                y: clamp_lp_non_neg(interpolate_lp(&fr.y, &tr.y, p, eval, is_transition)),
            };
            let br = &mut Arc::make_mut(&mut target.visual).border_radius;
            match prop { P::BorderTopLeftRadius => br.top_left = res, P::BorderTopRightRadius => br.top_right = res, P::BorderBottomRightRadius => br.bottom_right = res, _ => br.bottom_left = res }
        }
        P::OutlineWidth => Arc::make_mut(&mut target.visual).outline_width = (f64::from(from.visual.outline_width) * (1.0 - p) + f64::from(to.visual.outline_width) * p).max(0.0) as f32,
        P::OutlineStyle => Arc::make_mut(&mut target.visual).outline_style = if use_from_disc { from.visual.outline_style } else { to.visual.outline_style },
        P::OutlineOffset => Arc::make_mut(&mut target.visual).outline_offset = interpolate_lp(&from.visual.outline_offset, &to.visual.outline_offset, p, eval, is_transition),
        P::OutlineColor => {
            let c = interpolate_resolved_color(resolved_outline_color(from, eval), resolved_outline_color(to, eval), p);
            let v = Arc::make_mut(&mut target.visual);
            v.outline_color_computed = Some(Arc::new(CssColorExpr::Resolved(c)));
            v.outline_color = CssColor::Rgba(c.to_srgb_u8());
        }
        P::TextDecorationLine => Arc::make_mut(&mut target.visual).text_decoration_line = if use_from_disc { from.visual.text_decoration_line } else { to.visual.text_decoration_line },
        P::TextDecorationColor => {
            let c = interpolate_resolved_color(resolved_text_decor_color(from, eval), resolved_text_decor_color(to, eval), p);
            let v = Arc::make_mut(&mut target.visual);
            v.text_decoration_color_computed = Some(Arc::new(CssColorExpr::Resolved(c)));
            v.text_decoration_color = Some(CssColor::Rgba(c.to_srgb_u8()));
        }
        P::BackgroundLayers => {
            let src = if use_from_disc { &from.visual } else { &to.visual };
            let v = Arc::make_mut(&mut target.visual);
            v.background_layers = src.background_layers.clone();
            v.background_image_raw = src.background_image_raw.clone();
        }
        P::BackgroundPosition => {
            let (fx, fy) = &from.visual.background_position;
            let (tx, ty) = &to.visual.background_position;
            Arc::make_mut(&mut target.visual).background_position = (
                interpolate_lp(fx, tx, p, eval, is_transition),
                interpolate_lp(fy, ty, p, eval, is_transition),
            );
        }
        P::ObjectPosition => {
            let src = if use_from_disc { &from.visual } else { &to.visual };
            Arc::make_mut(&mut target.visual).object_position = src.object_position.clone();
        }
        P::RowGap => {
            let fa = from.flex_grid.as_ref().map_or(LengthPercentage::Zero, |f| f.row_gap.clone());
            let tb = to.flex_grid.as_ref().map_or(LengthPercentage::Zero, |f| f.row_gap.clone());
            target.flex_grid_mut().row_gap = clamp_lp_non_neg(interpolate_lp(&fa, &tb, p, eval, is_transition));
        }
        P::ColumnGap => {
            let fa = from.flex_grid.as_ref().map_or(LengthPercentage::Zero, |f| f.column_gap.clone());
            let tb = to.flex_grid.as_ref().map_or(LengthPercentage::Zero, |f| f.column_gap.clone());
            target.flex_grid_mut().column_gap = clamp_lp_non_neg(interpolate_lp(&fa, &tb, p, eval, is_transition));
        }
        P::TransformOrigin => {
            let fo = from.effects.as_ref().map(|e| e.transform_origin.clone()).unwrap_or_default();
            let to_o = to.effects.as_ref().map(|e| e.transform_origin.clone()).unwrap_or_default();
            target.effects_mut().transform_origin = crate::values::TransformOrigin {
                x: interpolate_lp(&fo.x, &to_o.x, p, eval, is_transition),
                y: interpolate_lp(&fo.y, &to_o.y, p, eval, is_transition),
                z: (f64::from(fo.z) * (1.0 - p) + f64::from(to_o.z) * p) as f32,
            };
        }
        P::MixBlendMode => target.effects_mut().mix_blend_mode = if use_from_disc { from.effects.as_ref().map_or_else(Default::default, |e| e.mix_blend_mode) } else { to.effects.as_ref().map_or_else(Default::default, |e| e.mix_blend_mode) },
        P::ClipPath => target.effects_mut().clip_path = if use_from_disc { from.effects.as_ref().map_or_else(Default::default, |e| e.clip_path.clone()) } else { to.effects.as_ref().map_or_else(Default::default, |e| e.clip_path.clone()) },
        P::ViewTransitionName => target.effects_mut().view_transition_name = if use_from_disc { from.effects.as_ref().and_then(|e| e.view_transition_name.clone()) } else { to.effects.as_ref().and_then(|e| e.view_transition_name.clone()) },
        P::BoxShadow => {
            let fa = from.effects.as_ref().map(|e| e.box_shadow.as_slice()).unwrap_or(&[]);
            let tb = to.effects.as_ref().map(|e| e.box_shadow.as_slice()).unwrap_or(&[]);
            target.effects_mut().box_shadow = interpolate_box_shadows(fa, tb, p, from.text.color, to.text.color, eval)
                .unwrap_or_else(|| if use_from_disc { fa.to_vec() } else { tb.to_vec() });
        }
        P::Filter => {
            let fa = from.effects.as_ref().map(|e| e.filter.as_slice()).unwrap_or(&[]);
            let tb = to.effects.as_ref().map(|e| e.filter.as_slice()).unwrap_or(&[]);
            target.effects_mut().filter = interpolate_filters(fa, tb, p, from.text.color, to.text.color, eval)
                .unwrap_or_else(|| if use_from_disc { fa.to_vec() } else { tb.to_vec() });
        }
        P::OffsetDistance | P::ShapeMargin | P::StrokeWidth | P::StrokeDashoffset
        | P::BackgroundPositionX | P::BackgroundPositionY | P::ScrollPaddingTop | P::ScrollPaddingRight
        | P::ScrollPaddingBottom | P::ScrollPaddingLeft | P::SvgCx | P::SvgCy | P::SvgRx | P::SvgRy
        | P::SvgX | P::SvgY | P::BorderImageSlice => {
            let fa = from.effects.as_ref().and_then(|e| e.extra_math_props.get(&prop));
            let tb = to.effects.as_ref().and_then(|e| e.extra_math_props.get(&prop));
            match (fa, tb) {
                (Some(a), Some(b)) => {
                    let combined = MathNode::Sum(vec![scale_math_for_interp(a.clone(), 1.0 - p), scale_math_for_interp(b.clone(), p)]).simplify(Some(eval), None);
                    target.effects_mut().extra_math_props.insert(prop, combined);
                }
                _ => {
                    if let Some(src) = if use_from_disc { fa } else { tb } {
                        target.effects_mut().extra_math_props.insert(prop, src.clone());
                    } else if let Some(eff) = target.effects.as_mut() {
                        Arc::make_mut(eff).extra_math_props.remove(&prop);
                    }
                }
            }
        }
        P::FontSize => {
            let px = (f64::from(from.text.font_size_px) * (1.0 - p) + f64::from(to.text.font_size_px) * p).max(0.0) as f32;
            let t = Arc::make_mut(&mut target.text); t.font_size_px = px; t.eval_ctx.font_size_px = px;
        }
        P::FontWeight => Arc::make_mut(&mut target.text).font_weight = (f64::from(from.text.font_weight) * (1.0 - p) + f64::from(to.text.font_weight) * p).clamp(1.0, 1000.0) as f32,
        P::FontStyle => Arc::make_mut(&mut target.text).font_style = if use_from_disc { from.text.font_style } else { to.text.font_style },
        P::LineHeight => Arc::make_mut(&mut target.text).line_height = match (&from.text.line_height, &to.text.line_height) {
            (LineHeightValue::Length(a), LineHeightValue::Length(b)) => LineHeightValue::Length(interpolate_lp(a, b, p, eval, is_transition)),
            (LineHeightValue::Number(a), LineHeightValue::Number(b)) => LineHeightValue::Number((f64::from(*a) * (1.0 - p) + f64::from(*b) * p).max(0.0) as f32),
            _ => if use_from_disc { from.text.line_height.clone() } else { to.text.line_height.clone() },
        },
        P::ZIndex => target.z_index = match (from.z_index, to.z_index) {
            (ZIndexValue::Integer(a), ZIndexValue::Integer(b)) => ZIndexValue::Integer(((f64::from(a) * (1.0 - p) + f64::from(b) * p) + 0.5).floor() as i32),
            _ => if use_from_disc { from.z_index } else { to.z_index },
        },
        P::Orphans => Arc::make_mut(&mut target.text).orphans = (((f64::from(from.text.orphans) * (1.0 - p) + f64::from(to.text.orphans) * p) + 0.5).floor() as i32).max(1),
        P::Widows => Arc::make_mut(&mut target.text).widows = (((f64::from(from.text.widows) * (1.0 - p) + f64::from(to.text.widows) * p) + 0.5).floor() as i32).max(1),
        P::ColumnCount => Arc::make_mut(&mut target.box_model).column_count = match (from.box_model.column_count, to.box_model.column_count) {
            (Some(a), Some(b)) => Some((((f64::from(a) * (1.0 - p) + f64::from(b) * p) + 0.5).floor() as i32).max(1)),
            _ => if use_from_disc { from.box_model.column_count } else { to.box_model.column_count },
        },
        P::Color => {
            let c = interpolate_resolved_color(resolved_text_color(from, eval), resolved_text_color(to, eval), p);
            let t = Arc::make_mut(&mut target.text); t.color = c.to_srgb_u8(); t.color_computed = Some(Arc::new(CssColorExpr::Resolved(c)));
        }
        P::BackgroundColor => {
            let c = interpolate_resolved_color(resolved_bg_color(from, eval), resolved_bg_color(to, eval), p);
            let v = Arc::make_mut(&mut target.visual); v.background_color = c.to_srgb_u8(); v.background_color_computed = Some(Arc::new(CssColorExpr::Resolved(c)));
        }
        P::Scale => {
            let (fs, ts) = (from.effects.as_ref().map_or(ScaleSpec::None, |e| e.scale.clone()), to.effects.as_ref().map_or(ScaleSpec::None, |e| e.scale.clone()));
            target.effects_mut().scale = interpolate_scale(&fs, &ts, p, eval).unwrap_or(if use_from_disc { fs } else { ts });
        }
        P::Rotate => {
            let (fr, tr) = (from.effects.as_ref().map_or(RotateSpec::None, |e| e.rotate.clone()), to.effects.as_ref().map_or(RotateSpec::None, |e| e.rotate.clone()));
            target.effects_mut().rotate = interpolate_rotate(&fr, &tr, p, eval).unwrap_or(if use_from_disc { fr } else { tr });
        }
        P::Translate => {
            let (ft, tt) = (from.effects.as_ref().map_or(TranslateSpec::None, |e| e.translate.clone()), to.effects.as_ref().map_or(TranslateSpec::None, |e| e.translate.clone()));
            target.effects_mut().translate = interpolate_translate(&ft, &tt, p, eval).unwrap_or(if use_from_disc { ft } else { tt });
        }
        P::Transform => {
            let fa = extract_transform_ops(from, eval);
            let tb = extract_transform_ops(to, eval);
            let res = interpolate_transforms(&fa, &tb, p, eval).unwrap_or(if use_from_disc { fa } else { tb });
            let eff = target.effects_mut();
            eff.transform = res;
            eff.transform_raw = None;
        }
        P::Custom => if let Some(cn) = cname {
            if let Some(toks) = interpolate_custom_property(cn, from.custom_properties.get(cn), to.custom_properties.get(cn), p, allow_discrete, eval, registered_props) {
                Arc::make_mut(&mut target.custom_properties).insert(cn.clone(), toks);
            }
        },
        P::FlexBasis => {
            let get_fb = |st: &ComputedStyle| st.flex_grid.as_ref().and_then(|f| f.flex_basis_size.clone()).unwrap_or_else(|| match st.flex_grid.as_ref().map(|f| &f.flex_basis) {
                Some(LengthPercentageAuto::Value(lp)) => SizeValue::Length(lp.clone()), _ => SizeValue::Auto,
            });
            let (fs, ts) = (get_fb(from), get_fb(to));
            let res = interpolate_size_value(&fs, &ts, p, allow_kw, eval, is_transition).unwrap_or(if use_from_disc { fs } else { ts });
            let fg = target.flex_grid_mut();
            match &res {
                SizeValue::Length(lp) => {
                    let clamped = match lp {
                        LengthPercentage::Length(v, u) => LengthPercentage::Length(v.max(0.0), *u),
                        LengthPercentage::Percent(v) => LengthPercentage::Percent(v.max(0.0)),
                        other => other.clone(),
                    };
                    fg.flex_basis = LengthPercentageAuto::Value(clamped);
                    fg.flex_basis_size = None;
                }
                SizeValue::Auto => { fg.flex_basis = LengthPercentageAuto::Auto; fg.flex_basis_size = None; }
                _ => { fg.flex_basis = LengthPercentageAuto::Auto; fg.flex_basis_size = Some(res); }
            }
        }
        P::FlexDirection => {
            let src = if use_from_disc { from } else { to };
            target.flex_grid_mut().flex_direction = src.flex_grid.as_ref().map_or_else(Default::default, |f| f.flex_direction);
        }
        P::FlexWrap => {
            let src = if use_from_disc { from } else { to };
            target.flex_grid_mut().flex_wrap = src.flex_grid.as_ref().map_or_else(Default::default, |f| f.flex_wrap);
        }
        P::AlignItems => {
            let src = if use_from_disc { from } else { to };
            target.flex_grid_mut().align_items = src.flex_grid.as_ref().map_or_else(Default::default, |f| f.align_items);
        }
        P::AlignSelf => {
            let src = if use_from_disc { from } else { to };
            target.flex_grid_mut().align_self = src.flex_grid.as_ref().map_or(crate::values::AlignValue::Auto, |f| f.align_self);
        }
        P::AlignContent => {
            let src = if use_from_disc { from } else { to };
            target.flex_grid_mut().align_content = src.flex_grid.as_ref().map_or_else(Default::default, |f| f.align_content);
        }
        P::JustifyContent => {
            let src = if use_from_disc { from } else { to };
            target.flex_grid_mut().justify_content = src.flex_grid.as_ref().map_or_else(Default::default, |f| f.justify_content);
        }
        P::JustifyItems => {
            let src = if use_from_disc { from } else { to };
            target.flex_grid_mut().justify_items = src.flex_grid.as_ref().map_or_else(Default::default, |f| f.justify_items);
        }
        P::JustifySelf => {
            let src = if use_from_disc { from } else { to };
            target.flex_grid_mut().justify_self = src.flex_grid.as_ref().map_or(crate::values::AlignValue::Auto, |f| f.justify_self);
        }
        P::GridTemplateColumns => {
            let fa = from.flex_grid.as_ref().map_or(crate::grid_css::GridTemplateTrackList::None, |f| f.grid_template_columns_list.clone());
            let tb = to.flex_grid.as_ref().map_or(crate::grid_css::GridTemplateTrackList::None, |f| f.grid_template_columns_list.clone());
            let res = crate::grid_css::interpolate_track_list(&fa, &tb, p, eval).unwrap_or(if use_from_disc { fa } else { tb });
            let fg = target.flex_grid_mut();
            fg.grid_template_columns = res.flatten_simple_tracks();
            fg.grid_template_columns_list = res;
        }
        P::GridTemplateRows => {
            let fa = from.flex_grid.as_ref().map_or(crate::grid_css::GridTemplateTrackList::None, |f| f.grid_template_rows_list.clone());
            let tb = to.flex_grid.as_ref().map_or(crate::grid_css::GridTemplateTrackList::None, |f| f.grid_template_rows_list.clone());
            let res = crate::grid_css::interpolate_track_list(&fa, &tb, p, eval).unwrap_or(if use_from_disc { fa } else { tb });
            let fg = target.flex_grid_mut();
            fg.grid_template_rows = res.flatten_simple_tracks();
            fg.grid_template_rows_list = res;
        }
        P::FitTolerance => {
            let fa = from.flex_grid.as_ref().map_or(crate::grid_css::FitToleranceValue::Normal, |f| f.fit_tolerance.clone());
            let tb = to.flex_grid.as_ref().map_or(crate::grid_css::FitToleranceValue::Normal, |f| f.fit_tolerance.clone());
            target.flex_grid_mut().fit_tolerance = crate::grid_css::interpolate_fit_tolerance(&fa, &tb, p, eval).unwrap_or(if use_from_disc { fa } else { tb });
        }
        P::GridTemplateAreas => {
            let src = if use_from_disc { from } else { to };
            target.flex_grid_mut().grid_template_areas = src.flex_grid.as_ref().map_or_else(Default::default, |f| f.grid_template_areas.clone());
        }
        P::GridAutoColumns => {
            let src = if use_from_disc { from } else { to };
            target.flex_grid_mut().grid_auto_columns = src.flex_grid.as_ref().map(|f| f.grid_auto_columns.clone()).unwrap_or_default();
        }
        P::GridAutoRows => {
            let src = if use_from_disc { from } else { to };
            target.flex_grid_mut().grid_auto_rows = src.flex_grid.as_ref().map(|f| f.grid_auto_rows.clone()).unwrap_or_default();
        }
        P::GridAutoFlow => {
            let src = if use_from_disc { from } else { to };
            target.flex_grid_mut().grid_auto_flow = src.flex_grid.as_ref().map_or_else(Default::default, |f| f.grid_auto_flow);
        }
        P::GridRowStart => {
            let src = if use_from_disc { from } else { to };
            target.flex_grid_mut().grid_row_start = src.flex_grid.as_ref().map_or_else(Default::default, |f| f.grid_row_start.clone());
        }
        P::GridRowEnd => {
            let src = if use_from_disc { from } else { to };
            target.flex_grid_mut().grid_row_end = src.flex_grid.as_ref().map_or_else(Default::default, |f| f.grid_row_end.clone());
        }
        P::GridColumnStart => {
            let src = if use_from_disc { from } else { to };
            target.flex_grid_mut().grid_column_start = src.flex_grid.as_ref().map_or_else(Default::default, |f| f.grid_column_start.clone());
        }
        P::GridColumnEnd => {
            let src = if use_from_disc { from } else { to };
            target.flex_grid_mut().grid_column_end = src.flex_grid.as_ref().map_or_else(Default::default, |f| f.grid_column_end.clone());
        }
        P::GridLanesDirection => {
            let src = if use_from_disc { from } else { to };
            target.flex_grid_mut().grid_lanes_direction = src.flex_grid.as_ref().map_or_else(Default::default, |f| f.grid_lanes_direction);
        }
        P::GridLanesPack => {
            let src = if use_from_disc { from } else { to };
            target.flex_grid_mut().grid_lanes_pack = src.flex_grid.as_ref().map_or_else(Default::default, |f| f.grid_lanes_pack);
        }
        _ => {}
    }
}

#[rustfmt::skip]
fn math_node_from_lp(lp: &LengthPercentage, eval: &EvalContext, clamp_inf: bool) -> MathNode {
    let raw = match lp {
        LengthPercentage::Zero => MathNode::Dimension(0.0, MathUnit::Length(LengthUnit::Px)),
        LengthPercentage::Length(v, u) => MathNode::Dimension(f64::from(u.resolve_px(*v, eval)), MathUnit::Length(LengthUnit::Px)),
        LengthPercentage::Percent(p) => MathNode::Percent(f64::from(*p) * 100.0),
        LengthPercentage::Calc(c) => match c.as_ref() {
            CalcExpr::Math(n) => n.simplify(Some(eval), None),
            other if other.has_percentage() => {
                let z = other.resolve_px(0.0, eval);
                let pct = other.resolve_px(100.0, eval) - z;
                MathNode::Sum(vec![MathNode::Percent(f64::from(pct)), MathNode::Dimension(f64::from(z), MathUnit::Length(LengthUnit::Px))]).simplify(Some(eval), None)
            }
            other => MathNode::Dimension(f64::from(other.resolve_px(0.0, eval)), MathUnit::Length(LengthUnit::Px)),
        },
    };
    if clamp_inf { if let MathNode::Dimension(v, u) = raw { if v.is_infinite() { return MathNode::Dimension(v.clamp(-MAX_CSS_PX, MAX_CSS_PX), u); } } }
    raw
}

#[rustfmt::skip]
fn lp_from_math_node(n: MathNode) -> LengthPercentage {
    match n {
        MathNode::Dimension(v, MathUnit::Length(LengthUnit::Px)) if v.is_finite() => LengthPercentage::px(v as f32),
        MathNode::Percent(p) if p.is_finite() => LengthPercentage::Percent((p / 100.0) as f32),
        other => LengthPercentage::Calc(Box::new(CalcExpr::Math(Box::new(other)))),
    }
}

#[rustfmt::skip]
fn scale_math_for_interp(n: MathNode, w: f64) -> MathNode {
    if w == 0.0 { if let MathNode::Dimension(v, u) = n { if !v.is_finite() { return MathNode::Dimension(0.0, u); } } }
    MathNode::Product(vec![MathNode::Number(w), n])
}

#[rustfmt::skip]
fn interpolate_lp(a: &LengthPercentage, b: &LengthPercentage, p: f64, eval: &EvalContext, is_tr: bool) -> LengthPercentage {
    if p == 0.0 { return lp_from_math_node(math_node_from_lp(a, eval, is_tr)); }
    if p == 1.0 { return lp_from_math_node(math_node_from_lp(b, eval, is_tr)); }
    let combined = MathNode::Sum(vec![scale_math_for_interp(math_node_from_lp(a, eval, is_tr), 1.0 - p), scale_math_for_interp(math_node_from_lp(b, eval, is_tr), p)]).simplify(Some(eval), None);
    lp_from_math_node(combined)
}

#[rustfmt::skip]
fn normalize_size_for_interp(val: &SizeValue, allow_kw: bool, eval: &EvalContext, clamp_inf: bool) -> Option<(CalcSizeBasis, MathNode)> {
    match val {
        SizeValue::None => None,
        SizeValue::Length(lp) => Some((CalcSizeBasis::Any, math_node_from_lp(lp, eval, clamp_inf))),
        SizeValue::Auto if allow_kw => Some((CalcSizeBasis::Auto, MathNode::SizeKeyword)),
        SizeValue::MinContent if allow_kw => Some((CalcSizeBasis::MinContent, MathNode::SizeKeyword)),
        SizeValue::MaxContent if allow_kw => Some((CalcSizeBasis::MaxContent, MathNode::SizeKeyword)),
        SizeValue::FitContent(None) if allow_kw => Some((CalcSizeBasis::FitContent, MathNode::SizeKeyword)),
        SizeValue::Stretch if allow_kw => Some((CalcSizeBasis::Stretch, MathNode::SizeKeyword)),
        SizeValue::Content if allow_kw => Some((CalcSizeBasis::Content, MathNode::SizeKeyword)),
        SizeValue::CalcSize(cs) => cs.normalize_for_interpolation(eval, CALC_SIZE_MAX_EXPANSION_NODES),
        _ => None,
    }
}

#[rustfmt::skip]
fn combine_calc_size_bases(a: &CalcSizeBasis, b: &CalcSizeBasis) -> Option<CalcSizeBasis> {
    match (a, b) { (CalcSizeBasis::Any, o) | (o, CalcSizeBasis::Any) => Some(o.clone()), (x, y) if x == y => Some(x.clone()), _ => None }
}

#[rustfmt::skip]
fn compose_size_value(underlying: &SizeValue, kf: &SizeValue, mode: AnimationCompositionMode, allow_kw: bool, eval: &EvalContext) -> SizeValue {
    if mode == AnimationCompositionMode::Replace { return kf.clone(); }
    let eff = allow_kw || matches!(underlying, SizeValue::CalcSize(_)) || matches!(kf, SizeValue::CalcSize(_));
    let (Some((bu, cu)), Some((bk, ck))) = (normalize_size_for_interp(underlying, eff, eval, false), normalize_size_for_interp(kf, eff, eval, false)) else { return kf.clone(); };
    let Some(basis) = combine_calc_size_bases(&bu, &bk) else { return kf.clone(); };
    let sum = MathNode::Sum(vec![cu, ck]).simplify(Some(eval), None);
    if basis == CalcSizeBasis::Any && !matches!(underlying, SizeValue::CalcSize(_)) && !matches!(kf, SizeValue::CalcSize(_)) { SizeValue::Length(lp_from_math_node(sum)) }
    else { SizeValue::CalcSize(Box::new(CalcSizeExpr { basis, calculation: Box::new(sum) })) }
}

#[rustfmt::skip]
fn interpolate_size_value(from: &SizeValue, to: &SizeValue, p: f64, allow_kw: bool, eval: &EvalContext, is_tr: bool) -> Option<SizeValue> {
    if let (SizeValue::Length(a), SizeValue::Length(b)) = (from, to) { return Some(SizeValue::Length(interpolate_lp(a, b, p, eval, is_tr))); }
    let eff = allow_kw || matches!(from, SizeValue::CalcSize(_)) || matches!(to, SizeValue::CalcSize(_));
    let (b1, c1) = normalize_size_for_interp(from, eff, eval, is_tr)?;
    let (b2, c2) = normalize_size_for_interp(to, eff, eval, is_tr)?;
    let basis = combine_calc_size_bases(&b1, &b2)?;
    let combined = MathNode::Sum(vec![scale_math_for_interp(c1, 1.0 - p), scale_math_for_interp(c2, p)]).simplify(Some(eval), None);
    if basis == CalcSizeBasis::Any && !matches!(from, SizeValue::CalcSize(_)) && !matches!(to, SizeValue::CalcSize(_)) { Some(SizeValue::Length(lp_from_math_node(combined))) }
    else { Some(SizeValue::CalcSize(Box::new(CalcSizeExpr { basis, calculation: Box::new(combined) }))) }
}

#[rustfmt::skip]
fn resolved_text_color(style: &ComputedStyle, eval: &EvalContext) -> ResolvedColor {
    style.text.color_computed.as_deref().map_or_else(|| ResolvedColor::from_rgba(style.text.color), |e| e.resolve(ResolvedColor::from_rgba(style.text.color), false, eval))
}

#[rustfmt::skip]
fn resolved_bg_color(style: &ComputedStyle, eval: &EvalContext) -> ResolvedColor {
    let cur = resolved_text_color(style, eval);
    style.visual.background_color_computed.as_deref().map_or_else(|| ResolvedColor::from_rgba(style.visual.background_color), |e| e.resolve(cur, false, eval))
}

#[rustfmt::skip]
fn resolved_border_color(style: &ComputedStyle, idx: usize, eval: &EvalContext) -> ResolvedColor {
    let cur = resolved_text_color(style, eval);
    if let Some(expr) = style.visual.border_color_computed[idx].as_deref() {
        return expr.resolve(cur, false, eval);
    }
    let cc = match idx {
        0 => style.visual.border_color.top, 1 => style.visual.border_color.right,
        2 => style.visual.border_color.bottom, _ => style.visual.border_color.left,
    };
    match cc {
        CssColor::Rgba(r) => ResolvedColor::from_rgba(r),
        CssColor::CurrentColor => cur,
        CssColor::LightDark(l, _) => ResolvedColor::from_rgba(l),
        CssColor::SystemAccent => ResolvedColor::from_rgba(ColorRgba::rgba(0, 122, 255, 255)),
    }
}

#[rustfmt::skip]
fn resolved_outline_color(style: &ComputedStyle, eval: &EvalContext) -> ResolvedColor {
    let cur = resolved_text_color(style, eval);
    if let Some(expr) = style.visual.outline_color_computed.as_deref() {
        return expr.resolve(cur, false, eval);
    }
    match style.visual.outline_color {
        CssColor::Rgba(r) => ResolvedColor::from_rgba(r),
        CssColor::CurrentColor => cur,
        CssColor::LightDark(l, _) => ResolvedColor::from_rgba(l),
        CssColor::SystemAccent => ResolvedColor::from_rgba(ColorRgba::rgba(0, 122, 255, 255)),
    }
}

#[rustfmt::skip]
fn resolved_text_decor_color(style: &ComputedStyle, eval: &EvalContext) -> ResolvedColor {
    let cur = resolved_text_color(style, eval);
    if let Some(expr) = style.visual.text_decoration_color_computed.as_deref() {
        return expr.resolve(cur, false, eval);
    }
    match style.visual.text_decoration_color {
        Some(CssColor::Rgba(r)) => ResolvedColor::from_rgba(r),
        Some(CssColor::LightDark(l, _)) => ResolvedColor::from_rgba(l),
        _ => cur,
    }
}

#[rustfmt::skip]
fn resolved_to_srgb_f64(c: ResolvedColor) -> (f64, f64, f64, f64) {
    let s = c.convert_to(ColorSpace::Srgb);
    (s.c[0].unwrap_or(0.0), s.c[1].unwrap_or(0.0), s.c[2].unwrap_or(0.0), s.alpha.unwrap_or(1.0))
}

#[rustfmt::skip]
fn interpolate_resolved_color(a: ResolvedColor, b: ResolvedColor, p: f64) -> ResolvedColor {
    let ((ar, ag, ab, aa), (br, bg, bb, ba)) = (resolved_to_srgb_f64(a), resolved_to_srgb_f64(b));
    let inv = 1.0 - p;
    let alpha_raw = aa * inv + ba * p;
    let alpha = alpha_raw.clamp(0.0, 1.0);
    let (r, g, bl) = if alpha_raw > 1e-9 { ((ar * aa * inv + br * ba * p) / alpha_raw, (ag * aa * inv + bg * ba * p) / alpha_raw, (ab * aa * inv + bb * ba * p) / alpha_raw) } else { (0.0, 0.0, 0.0) };
    let space = if a.space == ColorSpace::SrgbLegacy && b.space == ColorSpace::SrgbLegacy { ColorSpace::SrgbLegacy } else { ColorSpace::Srgb };
    ResolvedColor { space, c: [Some(r.clamp(0.0, 1.0)), Some(g.clamp(0.0, 1.0)), Some(bl.clamp(0.0, 1.0))], alpha: Some(alpha) }
}

#[rustfmt::skip]
fn compose_resolved_color(u: ResolvedColor, k: ResolvedColor) -> ResolvedColor {
    let ((ur, ug, ub, ua), (kr, kg, kb, ka)) = (resolved_to_srgb_f64(u), resolved_to_srgb_f64(k));
    let space = if u.space == ColorSpace::SrgbLegacy && k.space == ColorSpace::SrgbLegacy { ColorSpace::SrgbLegacy } else { ColorSpace::Srgb };
    let a = (ua + ka).min(1.0);
    // Keep non-clamped RGB channels during intermediate composition (clamped at interpolation/serialization).
    let (r, g, b) = if (ua - 1.0).abs() < 1e-9 && (ka - 1.0).abs() < 1e-9 {
        (ur + kr, ug + kg, ub + kb)
    } else if a > 1e-9 {
        ((ur * ua + kr * ka) / a, (ug * ua + kg * ka) / a, (ub * ua + kb * ka) / a)
    } else { (0.0, 0.0, 0.0) };
    ResolvedColor { space, c: [Some(r), Some(g), Some(b)], alpha: Some(a) }
}

#[rustfmt::skip]
fn resolve_css_color(c: CssColor, cur: ColorRgba) -> ResolvedColor {
    match c {
        CssColor::Rgba(r) => ResolvedColor::from_rgba(r),
        CssColor::CurrentColor => ResolvedColor::from_rgba(cur),
        CssColor::LightDark(l, _) => ResolvedColor::from_rgba(l),
        CssColor::SystemAccent => ResolvedColor::from_rgba(ColorRgba::rgba(0, 122, 255, 255)),
    }
}

#[rustfmt::skip]
fn zero_box_shadow(inset: bool) -> BoxShadowValue {
    BoxShadowValue {
        offset_x: LengthPercentage::px(0.0), offset_y: LengthPercentage::px(0.0),
        blur: LengthPercentage::px(0.0), spread: LengthPercentage::px(0.0),
        color: CssColor::Rgba(ColorRgba::TRANSPARENT), inset,
    }
}

#[rustfmt::skip]
fn interpolate_box_shadows(a: &[BoxShadowValue], b: &[BoxShadowValue], p: f64, cc_a: ColorRgba, cc_b: ColorRgba, eval: &EvalContext) -> Option<Vec<BoxShadowValue>> {
    let len = a.len().max(b.len());
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let sa = a.get(i).cloned().unwrap_or_else(|| zero_box_shadow(b[i].inset));
        let sb = b.get(i).cloned().unwrap_or_else(|| zero_box_shadow(a[i].inset));
        if sa.inset != sb.inset { return None; }
        let ca = resolve_css_color(sa.color, cc_a);
        let cb = resolve_css_color(sb.color, cc_b);
        out.push(BoxShadowValue {
            offset_x: interpolate_lp(&sa.offset_x, &sb.offset_x, p, eval, false),
            offset_y: interpolate_lp(&sa.offset_y, &sb.offset_y, p, eval, false),
            blur: clamp_lp_non_neg(interpolate_lp(&sa.blur, &sb.blur, p, eval, false)),
            spread: interpolate_lp(&sa.spread, &sb.spread, p, eval, false),
            color: CssColor::Rgba(interpolate_resolved_color(ca, cb, p).to_srgb_u8()),
            inset: sa.inset,
        });
    }
    Some(out)
}

#[rustfmt::skip]
fn compose_box_shadows(u: &[BoxShadowValue], k: &[BoxShadowValue], mode: AnimationCompositionMode, cc_u: ColorRgba, cc_k: ColorRgba, eval: &EvalContext) -> Vec<BoxShadowValue> {
    if mode == AnimationCompositionMode::Add {
        let mut out = u.to_vec();
        out.extend_from_slice(k);
        return out;
    }
    let len = u.len().max(k.len());
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let su = u.get(i).cloned().unwrap_or_else(|| zero_box_shadow(k[i].inset));
        let sk = k.get(i).cloned().unwrap_or_else(|| zero_box_shadow(u[i].inset));
        if su.inset != sk.inset { return k.to_vec(); }
        let cu = resolve_css_color(su.color, cc_u);
        let ck = resolve_css_color(sk.color, cc_k);
        out.push(BoxShadowValue {
            offset_x: compose_lp(&su.offset_x, &sk.offset_x, eval, false),
            offset_y: compose_lp(&su.offset_y, &sk.offset_y, eval, false),
            blur: compose_lp(&su.blur, &sk.blur, eval, true),
            spread: compose_lp(&su.spread, &sk.spread, eval, false),
            color: CssColor::Rgba(compose_resolved_color(cu, ck).to_srgb_u8()),
            inset: su.inset,
        });
    }
    out
}

#[rustfmt::skip]
fn lacuna_filter_op(op: &FilterOp) -> FilterOp {
    match op {
        FilterOp::Blur(_) => FilterOp::Blur(LengthPercentage::px(0.0)),
        FilterOp::Brightness(_) => FilterOp::Brightness(1.0),
        FilterOp::Contrast(_) => FilterOp::Contrast(1.0),
        FilterOp::Grayscale(_) => FilterOp::Grayscale(0.0),
        FilterOp::Opacity(_) => FilterOp::Opacity(1.0),
        FilterOp::Saturate(_) => FilterOp::Saturate(1.0),
        FilterOp::Invert(_) => FilterOp::Invert(0.0),
        FilterOp::Sepia(_) => FilterOp::Sepia(0.0),
        FilterOp::HueRotateDeg(_) => FilterOp::HueRotateDeg(0.0),
        FilterOp::DropShadow { .. } => FilterOp::DropShadow {
            offset_x: LengthPercentage::px(0.0), offset_y: LengthPercentage::px(0.0),
            blur: LengthPercentage::px(0.0), color: CssColor::Rgba(ColorRgba::TRANSPARENT),
        },
    }
}

#[rustfmt::skip]
fn interpolate_filters(a: &[FilterOp], b: &[FilterOp], p: f64, cc_a: ColorRgba, cc_b: ColorRgba, eval: &EvalContext) -> Option<Vec<FilterOp>> {
    let len = a.len().max(b.len());
    let mut out = Vec::with_capacity(len);
    let lerp = |x: f32, y: f32| (f64::from(x) * (1.0 - p) + f64::from(y) * p) as f32;
    for i in 0..len {
        let fa = a.get(i).cloned().unwrap_or_else(|| lacuna_filter_op(&b[i]));
        let fb = b.get(i).cloned().unwrap_or_else(|| lacuna_filter_op(&a[i]));
        let op = match (&fa, &fb) {
            (FilterOp::Blur(x), FilterOp::Blur(y)) => FilterOp::Blur(clamp_lp_non_neg(interpolate_lp(x, y, p, eval, false))),
            (FilterOp::Brightness(x), FilterOp::Brightness(y)) => FilterOp::Brightness(lerp(*x, *y).max(0.0)),
            (FilterOp::Contrast(x), FilterOp::Contrast(y)) => FilterOp::Contrast(lerp(*x, *y).max(0.0)),
            (FilterOp::Saturate(x), FilterOp::Saturate(y)) => FilterOp::Saturate(lerp(*x, *y).max(0.0)),
            (FilterOp::Grayscale(x), FilterOp::Grayscale(y)) => FilterOp::Grayscale(lerp(*x, *y).clamp(0.0, 1.0)),
            (FilterOp::Invert(x), FilterOp::Invert(y)) => FilterOp::Invert(lerp(*x, *y).clamp(0.0, 1.0)),
            (FilterOp::Sepia(x), FilterOp::Sepia(y)) => FilterOp::Sepia(lerp(*x, *y).clamp(0.0, 1.0)),
            (FilterOp::Opacity(x), FilterOp::Opacity(y)) => FilterOp::Opacity(lerp(*x, *y).clamp(0.0, 1.0)),
            (FilterOp::HueRotateDeg(x), FilterOp::HueRotateDeg(y)) => FilterOp::HueRotateDeg(lerp(*x, *y)),
            (
                FilterOp::DropShadow { offset_x: ax, offset_y: ay, blur: ab, color: ac },
                FilterOp::DropShadow { offset_x: bx, offset_y: by, blur: bb, color: bc },
            ) => FilterOp::DropShadow {
                offset_x: interpolate_lp(ax, bx, p, eval, false),
                offset_y: interpolate_lp(ay, by, p, eval, false),
                blur: clamp_lp_non_neg(interpolate_lp(ab, bb, p, eval, false)),
                color: CssColor::Rgba(interpolate_resolved_color(resolve_css_color(*ac, cc_a), resolve_css_color(*bc, cc_b), p).to_srgb_u8()),
            },
            _ => return None,
        };
        out.push(op);
    }
    Some(out)
}

#[rustfmt::skip]
fn compose_filters(u: &[FilterOp], k: &[FilterOp], mode: AnimationCompositionMode, cc_u: ColorRgba, cc_k: ColorRgba, eval: &EvalContext) -> Vec<FilterOp> {
    if mode == AnimationCompositionMode::Add {
        let mut out = u.to_vec();
        out.extend_from_slice(k);
        return out;
    }
    let len = u.len().max(k.len());
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        let fu = u.get(i).cloned().unwrap_or_else(|| lacuna_filter_op(&k[i]));
        let fk = k.get(i).cloned().unwrap_or_else(|| lacuna_filter_op(&u[i]));
        let op = match (&fu, &fk) {
            (FilterOp::Blur(x), FilterOp::Blur(y)) => FilterOp::Blur(compose_lp(x, y, eval, true)),
            (FilterOp::Brightness(x), FilterOp::Brightness(y)) => FilterOp::Brightness((x + y - 1.0).max(0.0)),
            (FilterOp::Contrast(x), FilterOp::Contrast(y)) => FilterOp::Contrast((x + y - 1.0).max(0.0)),
            (FilterOp::Saturate(x), FilterOp::Saturate(y)) => FilterOp::Saturate((x + y - 1.0).max(0.0)),
            (FilterOp::Opacity(x), FilterOp::Opacity(y)) => FilterOp::Opacity((x + y - 1.0).clamp(0.0, 1.0)),
            (FilterOp::Grayscale(x), FilterOp::Grayscale(y)) => FilterOp::Grayscale((x + y).clamp(0.0, 1.0)),
            (FilterOp::Invert(x), FilterOp::Invert(y)) => FilterOp::Invert((x + y).clamp(0.0, 1.0)),
            (FilterOp::Sepia(x), FilterOp::Sepia(y)) => FilterOp::Sepia((x + y).clamp(0.0, 1.0)),
            (FilterOp::HueRotateDeg(x), FilterOp::HueRotateDeg(y)) => FilterOp::HueRotateDeg(x + y),
            (
                FilterOp::DropShadow { offset_x: ax, offset_y: ay, blur: ab, color: ac },
                FilterOp::DropShadow { offset_x: bx, offset_y: by, blur: bb, color: bc },
            ) => FilterOp::DropShadow {
                offset_x: compose_lp(ax, bx, eval, false),
                offset_y: compose_lp(ay, by, eval, false),
                blur: compose_lp(ab, bb, eval, true),
                color: CssColor::Rgba(compose_resolved_color(resolve_css_color(*ac, cc_u), resolve_css_color(*bc, cc_k)).to_srgb_u8()),
            },
            _ => return k.to_vec(),
        };
        out.push(op);
    }
    out
}

#[rustfmt::skip]
fn compose_scale(u: &ScaleSpec, k: &ScaleSpec, mode: AnimationCompositionMode, eval: &EvalContext) -> ScaleSpec {
    let to_xyz = |s: &ScaleSpec| match s {
        ScaleSpec::None => [1.0, 1.0, 1.0],
        ScaleSpec::Values(v) => { let g = |i: usize, d: f64| v.get(i).map_or(d, |n| n.eval_scalar(eval, Some((1.0, true)))); let x = g(0, 1.0); [x, if v.len() == 1 { x } else { g(1, 1.0) }, g(2, 1.0)] }
    };
    let (a, b) = (to_xyz(u), to_xyz(k));
    let c = if mode == AnimationCompositionMode::Add {
        [a[0] * b[0], a[1] * b[1], a[2] * b[2]]
    } else {
        [a[0] + b[0] - 1.0, a[1] + b[1] - 1.0, a[2] + b[2] - 1.0]
    };
    ScaleSpec::Values(vec![MathNode::Number(c[0]), MathNode::Number(c[1]), MathNode::Number(c[2])])
}

#[rustfmt::skip]
fn compose_rotate(u: &RotateSpec, k: &RotateSpec, eval: &EvalContext) -> RotateSpec {
    let to_ad = |r: &RotateSpec| match r {
        RotateSpec::None => (None, 0.0),
        RotateSpec::Angle { axis, angle } => (axis.as_ref().map(|[x, y, z]| [x.eval_scalar(eval, None), y.eval_scalar(eval, None), z.eval_scalar(eval, None)]), angle.eval_scalar(eval, None)),
    };
    let ((ax_a, deg_a), (ax_b, deg_b)) = (to_ad(u), to_ad(k));
    let axis = match (ax_a, ax_b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some([MathNode::Number(x[0]), MathNode::Number(x[1]), MathNode::Number(x[2])]),
        (Some(x), Some(y)) if x == y => Some([MathNode::Number(x[0]), MathNode::Number(x[1]), MathNode::Number(x[2])]),
        _ => return k.clone(),
    };
    RotateSpec::Angle { axis, angle: MathNode::Dimension(deg_a + deg_b, MathUnit::Deg) }
}

#[rustfmt::skip]
fn translate_xyz_nodes(t: &TranslateSpec) -> (MathNode, MathNode, MathNode) {
    let zero = MathNode::Dimension(0.0, MathUnit::Length(LengthUnit::Px));
    match t {
        TranslateSpec::None => (zero.clone(), zero.clone(), zero),
        TranslateSpec::Values { x, y, z } => (x.clone(), y.clone().unwrap_or_else(|| zero.clone()), z.clone().unwrap_or(zero)),
    }
}

#[rustfmt::skip]
fn interpolate_translate(a: &TranslateSpec, b: &TranslateSpec, p: f64, eval: &EvalContext) -> Option<TranslateSpec> {
    if p == 0.0 { return Some(a.clone()); }
    if p == 1.0 { return Some(b.clone()); }
    let ((ax, ay, az), (bx, by, bz)) = (translate_xyz_nodes(a), translate_xyz_nodes(b));
    let ix = MathNode::Sum(vec![scale_math_for_interp(ax, 1.0 - p), scale_math_for_interp(bx, p)]).simplify(Some(eval), None);
    let iy = MathNode::Sum(vec![scale_math_for_interp(ay, 1.0 - p), scale_math_for_interp(by, p)]).simplify(Some(eval), None);
    let iz = MathNode::Sum(vec![scale_math_for_interp(az, 1.0 - p), scale_math_for_interp(bz, p)]).simplify(Some(eval), None);
    let z_zero = iz.eval_scalar(eval, None).abs() < 1e-6;
    let y_zero = z_zero && iy.eval_scalar(eval, Some((0.0, false))).abs() < 1e-6 && !matches!(iy, MathNode::Percent(_) | MathNode::Sum(_));
    Some(TranslateSpec::Values { x: ix, y: (!y_zero).then_some(iy), z: (!z_zero).then_some(iz) })
}

#[rustfmt::skip]
fn compose_translate(u: &TranslateSpec, k: &TranslateSpec, eval: &EvalContext) -> TranslateSpec {
    let ((ux, uy, uz), (kx, ky, kz)) = (translate_xyz_nodes(u), translate_xyz_nodes(k));
    let sx = MathNode::Sum(vec![ux, kx]).simplify(Some(eval), None);
    let sy = MathNode::Sum(vec![uy, ky]).simplify(Some(eval), None);
    let sz = MathNode::Sum(vec![uz, kz]).simplify(Some(eval), None);
    let z_zero = sz.eval_scalar(eval, None).abs() < 1e-6;
    let y_zero = z_zero && sy.eval_scalar(eval, Some((0.0, false))).abs() < 1e-6 && !matches!(sy, MathNode::Percent(_) | MathNode::Sum(_));
    TranslateSpec::Values { x: sx, y: (!y_zero).then_some(sy), z: (!z_zero).then_some(sz) }
}

#[rustfmt::skip]
fn extract_transform_ops(style: &ComputedStyle, eval: &EvalContext) -> Vec<TransformOp> {
    let Some(eff) = style.effects.as_ref() else { return Vec::new(); };
    if !eff.transform.is_empty() {
        return eff.transform.clone();
    }
    if let Some(raw) = eff.transform_raw.as_deref() {
        if let Some(mat_str) = crate::values_math::compute_transform_matrix_css(raw, eval) {
            if let Some(inner) = mat_str.strip_prefix("matrix(").and_then(|s| s.strip_suffix(')')) {
                let nums: Vec<f32> = inner.split(',').filter_map(|p| p.trim().parse::<f32>().ok()).collect();
                if nums.len() == 6 {
                    return vec![TransformOp::Matrix([nums[0], nums[1], nums[2], nums[3], nums[4], nums[5]])];
                }
            }
            if let Some(inner) = mat_str.strip_prefix("matrix3d(").and_then(|s| s.strip_suffix(')')) {
                let nums: Vec<f32> = inner.split(',').filter_map(|p| p.trim().parse::<f32>().ok()).collect();
                if nums.len() == 16 {
                    let mut m = [0.0f32; 16];
                    m.copy_from_slice(&nums);
                    return vec![TransformOp::Matrix3d(m)];
                }
            }
        }
    }
    Vec::new()
}

#[rustfmt::skip]
fn ops_to_2d_matrix(ops: &[TransformOp], eval: &EvalContext) -> Option<[f64; 6]> {
    let mul = |a: [f64; 6], b: [f64; 6]| [
        a[0]*b[0] + a[2]*b[1], a[1]*b[0] + a[3]*b[1],
        a[0]*b[2] + a[2]*b[3], a[1]*b[2] + a[3]*b[3],
        a[0]*b[4] + a[2]*b[5] + a[4], a[1]*b[4] + a[3]*b[5] + a[5],
    ];
    let mut m = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
    for op in ops {
        let step = match op {
            TransformOp::Translate(x, y) => [1.0, 0.0, 0.0, 1.0, f64::from(x.resolve_px(0.0, eval)), f64::from(y.resolve_px(0.0, eval))],
            TransformOp::Scale(sx, sy) => [f64::from(*sx), 0.0, 0.0, f64::from(*sy), 0.0, 0.0],
            TransformOp::RotateDeg(d) => { let r = f64::from(*d).to_radians(); let (s, c) = (r.sin(), r.cos()); [c, s, -s, c, 0.0, 0.0] }
            TransformOp::SkewDeg(ax, ay) => [1.0, f64::from(*ay).to_radians().tan(), f64::from(*ax).to_radians().tan(), 1.0, 0.0, 0.0],
            TransformOp::Matrix(v) => [f64::from(v[0]), f64::from(v[1]), f64::from(v[2]), f64::from(v[3]), f64::from(v[4]), f64::from(v[5])],
            _ => return None,
        };
        m = mul(m, step);
    }
    Some(m)
}

/// W3C CSS Transforms Level 1 §16 2D matrix decomposition: `(tx, ty, sx, sy, angle_rad, m11, m12, m21, m22)`.
#[rustfmt::skip]
fn decompose_2d_matrix(m: [f64; 6]) -> Option<(f64, f64, f64, f64, f64, f64)> {
    let (mut row0x, mut row0y, mut row1x, mut row1y, tx, ty) = (m[0], m[1], m[2], m[3], m[4], m[5]);
    if (row0x * row1y - row0y * row1x).abs() < 1e-12 { return None; }
    let mut sx = row0x.hypot(row0y);
    row0x /= sx; row0y /= sx;
    let mut skew = row0x * row1x + row0y * row1y;
    row1x -= row0x * skew; row1y -= row0y * skew;
    let sy = row1x.hypot(row1y);
    if sy < 1e-12 { return None; }
    row1x /= sy; row1y /= sy;
    skew /= sy;
    if row0x * row1y - row0y * row1x < 0.0 {
        sx = -sx; row0x = -row0x; row0y = -row0y;
    }
    let angle = row0y.atan2(row0x);
    Some((tx, ty, sx, sy, angle, skew))
}

#[rustfmt::skip]
fn recompose_2d_matrix(tx: f64, ty: f64, sx: f64, sy: f64, angle: f64, skew: f64) -> [f32; 6] {
    let (s, c) = (angle.sin(), angle.cos());
    // M = T * R * SkewX * S
    let a = c * sx;
    let b = s * sx;
    let cc = (c * skew - s) * sy;
    let d = (s * skew + c) * sy;
    [a as f32, b as f32, cc as f32, d as f32, tx as f32, ty as f32]
}

#[rustfmt::skip]
fn interpolate_transform_op(a: &TransformOp, b: &TransformOp, p: f64, eval: &EvalContext) -> Option<TransformOp> {
    let lf = |x: f32, y: f32| (f64::from(x) * (1.0 - p) + f64::from(y) * p) as f32;
    match (a, b) {
        (TransformOp::Translate(ax, ay), TransformOp::Translate(bx, by)) => Some(TransformOp::Translate(interpolate_lp(ax, bx, p, eval, false), interpolate_lp(ay, by, p, eval, false))),
        (TransformOp::Translate3d(ax, ay, az), TransformOp::Translate3d(bx, by, bz)) => Some(TransformOp::Translate3d(interpolate_lp(ax, bx, p, eval, false), interpolate_lp(ay, by, p, eval, false), lf(*az, *bz))),
        (TransformOp::TranslateZ(az), TransformOp::TranslateZ(bz)) => Some(TransformOp::TranslateZ(lf(*az, *bz))),
        (TransformOp::Scale(ax, ay), TransformOp::Scale(bx, by)) => Some(TransformOp::Scale(lf(*ax, *bx), lf(*ay, *by))),
        (TransformOp::Scale3d(ax, ay, az), TransformOp::Scale3d(bx, by, bz)) => Some(TransformOp::Scale3d(lf(*ax, *bx), lf(*ay, *by), lf(*az, *bz))),
        (TransformOp::ScaleZ(az), TransformOp::ScaleZ(bz)) => Some(TransformOp::ScaleZ(lf(*az, *bz))),
        (TransformOp::RotateDeg(ad), TransformOp::RotateDeg(bd)) => Some(TransformOp::RotateDeg(lf(*ad, *bd))),
        (TransformOp::RotateXDeg(ad), TransformOp::RotateXDeg(bd)) => Some(TransformOp::RotateXDeg(lf(*ad, *bd))),
        (TransformOp::RotateYDeg(ad), TransformOp::RotateYDeg(bd)) => Some(TransformOp::RotateYDeg(lf(*ad, *bd))),
        (TransformOp::SkewDeg(ax, ay), TransformOp::SkewDeg(bx, by)) => Some(TransformOp::SkewDeg(lf(*ax, *bx), lf(*ay, *by))),
        _ => None,
    }
}

#[rustfmt::skip]
fn identity_transform_op(op: &TransformOp) -> TransformOp {
    match op {
        TransformOp::Translate(_, _) => TransformOp::Translate(LengthPercentage::px(0.0), LengthPercentage::px(0.0)),
        TransformOp::Translate3d(_, _, _) => TransformOp::Translate3d(LengthPercentage::px(0.0), LengthPercentage::px(0.0), 0.0),
        TransformOp::TranslateZ(_) => TransformOp::TranslateZ(0.0),
        TransformOp::Scale(_, _) => TransformOp::Scale(1.0, 1.0),
        TransformOp::Scale3d(_, _, _) => TransformOp::Scale3d(1.0, 1.0, 1.0),
        TransformOp::ScaleZ(_) => TransformOp::ScaleZ(1.0),
        TransformOp::RotateDeg(_) => TransformOp::RotateDeg(0.0),
        TransformOp::RotateXDeg(_) => TransformOp::RotateXDeg(0.0),
        TransformOp::RotateYDeg(_) => TransformOp::RotateYDeg(0.0),
        TransformOp::Rotate3d(x, y, z, _) => TransformOp::Rotate3d(*x, *y, *z, 0.0),
        TransformOp::SkewDeg(_, _) => TransformOp::SkewDeg(0.0, 0.0),
        TransformOp::Perspective(d) => TransformOp::Perspective(*d),
        TransformOp::Matrix(_) => TransformOp::Matrix([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]),
        TransformOp::Matrix3d(_) => TransformOp::Matrix3d([1.0,0.0,0.0,0.0, 0.0,1.0,0.0,0.0, 0.0,0.0,1.0,0.0, 0.0,0.0,0.0,1.0]),
    }
}

#[rustfmt::skip]
fn interpolate_transforms(a: &[TransformOp], b: &[TransformOp], p: f64, eval: &EvalContext) -> Option<Vec<TransformOp>> {
    if a.is_empty() && b.is_empty() { return Some(Vec::new()); }
    if a.is_empty() {
        let mut out = Vec::with_capacity(b.len());
        let mut ok = true;
        for op in b {
            if let Some(i) = interpolate_transform_op(&identity_transform_op(op), op, p, eval) { out.push(i); } else { ok = false; break; }
        }
        if ok { return Some(out); }
    } else if b.is_empty() {
        let mut out = Vec::with_capacity(a.len());
        let mut ok = true;
        for op in a {
            if let Some(i) = interpolate_transform_op(op, &identity_transform_op(op), p, eval) { out.push(i); } else { ok = false; break; }
        }
        if ok { return Some(out); }
    } else if a.len() == b.len() {
        let mut out = Vec::with_capacity(a.len());
        let mut ok = true;
        for (oa, ob) in a.iter().zip(b.iter()) {
            if let Some(i) = interpolate_transform_op(oa, ob, p, eval) { out.push(i); } else { ok = false; break; }
        }
        if ok { return Some(out); }
    }
    // W3C CSS Transforms §16 fallback: decompose 2D matrices and interpolate components.
    let (ma, mb) = (ops_to_2d_matrix(a, eval)?, ops_to_2d_matrix(b, eval)?);
    let (da, db) = (decompose_2d_matrix(ma)?, decompose_2d_matrix(mb)?);
    let lerp = |x: f64, y: f64| x * (1.0 - p) + y * p;
    Some(vec![TransformOp::Matrix(recompose_2d_matrix(
        lerp(da.0, db.0), lerp(da.1, db.1), lerp(da.2, db.2), lerp(da.3, db.3), lerp(da.4, db.4), lerp(da.5, db.5),
    ))])
}

#[rustfmt::skip]
fn compose_transforms(u: &[TransformOp], k: &[TransformOp], mode: AnimationCompositionMode, eval: &EvalContext) -> Vec<TransformOp> {
    if mode == AnimationCompositionMode::Add {
        let mut out = u.to_vec();
        out.extend_from_slice(k);
        return out;
    }
    if u.len() == k.len() && !u.is_empty() {
        let mut out = Vec::with_capacity(u.len());
        let mut ok = true;
        for (a, b) in u.iter().zip(k.iter()) {
            match (a, b) {
                (TransformOp::Translate(ax, ay), TransformOp::Translate(bx, by)) => out.push(TransformOp::Translate(compose_lp(ax, bx, eval, false), compose_lp(ay, by, eval, false))),
                (TransformOp::Scale(ax, ay), TransformOp::Scale(bx, by)) => out.push(TransformOp::Scale(ax + bx - 1.0, ay + by - 1.0)),
                (TransformOp::RotateDeg(ad), TransformOp::RotateDeg(bd)) => out.push(TransformOp::RotateDeg(ad + bd)),
                (TransformOp::SkewDeg(ax, ay), TransformOp::SkewDeg(bx, by)) => out.push(TransformOp::SkewDeg(ax + bx, ay + by)),
                _ => { ok = false; break; }
            }
        }
        if ok { return out; }
    }
    if let (Some(mu), Some(mk)) = (ops_to_2d_matrix(u, eval), ops_to_2d_matrix(k, eval)) {
        if let (Some(du), Some(dk)) = (decompose_2d_matrix(mu), decompose_2d_matrix(mk)) {
            return vec![TransformOp::Matrix(recompose_2d_matrix(
                du.0 + dk.0, du.1 + dk.1, du.2 + dk.2 - 1.0, du.3 + dk.3 - 1.0, du.4 + dk.4, du.5 + dk.5,
            ))];
        }
    }
    let mut out = u.to_vec();
    out.extend_from_slice(k);
    out
}

#[rustfmt::skip]
fn interpolate_scale(a: &ScaleSpec, b: &ScaleSpec, p: f64, eval: &EvalContext) -> Option<ScaleSpec> {
    if p == 0.0 { return Some(a.clone()); }
    let to_xyz = |s: &ScaleSpec| match s {
        ScaleSpec::None => [1.0, 1.0, 1.0],
        ScaleSpec::Values(v) => { let g = |i: usize, d: f64| v.get(i).map_or(d, |n| n.eval_scalar(eval, Some((1.0, true)))); let x = g(0, 1.0); [x, if v.len() == 1 { x } else { g(1, 1.0) }, g(2, 1.0)] }
    };
    let (va, vb) = (to_xyz(a), to_xyz(b));
    Some(ScaleSpec::Values(vec![MathNode::Number(va[0] * (1.0 - p) + vb[0] * p), MathNode::Number(va[1] * (1.0 - p) + vb[1] * p), MathNode::Number(va[2] * (1.0 - p) + vb[2] * p)]))
}

#[rustfmt::skip]
fn interpolate_rotate(a: &RotateSpec, b: &RotateSpec, p: f64, eval: &EvalContext) -> Option<RotateSpec> {
    if p == 0.0 { return Some(a.clone()); }
    let to_ad = |r: &RotateSpec| match r {
        RotateSpec::None => (None, 0.0),
        RotateSpec::Angle { axis, angle } => (axis.as_ref().map(|[x, y, z]| [x.eval_scalar(eval, None), y.eval_scalar(eval, None), z.eval_scalar(eval, None)]), angle.eval_scalar(eval, None)),
    };
    let ((ax_a, deg_a), (ax_b, deg_b)) = (to_ad(a), to_ad(b));
    let axis = match (ax_a, ax_b) {
        (None, None) => None,
        (Some(x), None) | (None, Some(x)) => Some([MathNode::Number(x[0]), MathNode::Number(x[1]), MathNode::Number(x[2])]),
        (Some(x), Some(y)) if x == y => Some([MathNode::Number(x[0]), MathNode::Number(x[1]), MathNode::Number(x[2])]),
        _ => return None,
    };
    Some(RotateSpec::Angle { axis, angle: MathNode::Dimension(deg_a * (1.0 - p) + deg_b * p, MathUnit::Deg) })
}

#[rustfmt::skip]
fn interpolate_custom_property(
    cname: &InternedStr, fv: Option<&[CssToken]>, tv: Option<&[CssToken]>,
    p: f64, allow_discrete: bool, eval: &EvalContext, registered_props: &HashMap<InternedStr, RegisteredProperty>,
) -> Option<Vec<CssToken>> {
    let f_str = fv.map(crate::properties::serialize_css_tokens).unwrap_or_default();
    let t_str = tv.map(crate::properties::serialize_css_tokens).unwrap_or_default();
    if let Some(reg) = registered_props.get(cname) {
        let (syn, vk) = registered_syntax_kind(reg);
        if let Some(kind) = vk {
            if let (Some(pa), Some(pb)) = (crate::values_math::parse_math_value(f_str.trim(), kind), crate::values_math::parse_math_value(t_str.trim(), kind)) {
                if kind == ValueKind::LengthPercentage {
                    let combined = MathNode::Sum(vec![
                        scale_math_for_interp(pa.node, 1.0 - p),
                        scale_math_for_interp(pb.node, p),
                    ]);
                    let s = combined.serialize_computed(eval, None, kind, None, None);
                    return Some(crate::properties::tokenize_custom_value_str(&s));
                }
                let (va, vb) = (pa.node.eval_scalar(eval, None), pb.node.eval_scalar(eval, None));
                if va.is_finite() && vb.is_finite() {
                    let v = va * (1.0 - p) + vb * p;
                    let s = match kind {
                        ValueKind::Integer => format!("{}", (v + 0.5).floor() as i32), ValueKind::Number => format_math_f64(v),
                        ValueKind::Length | ValueKind::LengthPercentage => format!("{}px", format_math_f64(v)),
                        ValueKind::Percentage => format!("{}%", format_math_f64(v)), ValueKind::Angle => format!("{}deg", format_math_f64(v)),
                        ValueKind::Time => format!("{}s", format_math_f64(v)), ValueKind::Resolution => format!("{}dppx", format_math_f64(v)),
                        _ => format_math_f64(v),
                    };
                    return Some(crate::properties::tokenize_custom_value_str(&s));
                }
            }
        } else if syn == "<color>" {
            if let (Some(ca), Some(cb)) = (crate::color::parse_css_color(f_str.trim()), crate::color::parse_css_color(t_str.trim())) {
                let cur = ResolvedColor::from_rgba(ColorRgba::BLACK);
                let out = interpolate_resolved_color(ca.resolve(cur, false, eval), cb.resolve(cur, false, eval), p);
                return Some(crate::properties::tokenize_custom_value_str(&out.to_computed_css_string()));
            }
        }
    }
    if allow_discrete && p < 0.5 { fv.map(|s| s.to_vec()) } else { tv.map(|s| s.to_vec()) }
}

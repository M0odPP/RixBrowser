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
    AnimationPlayStateMode, CalcExpr, ColorRgba, DisplayMode, EvalContext, InterpolateSizeMode,
    LengthPercentage, LengthPercentageAuto, LengthUnit, LineHeightValue, OverlayMode, SizeValue,
    TimingFunction, TransitionBehaviorMode, TransitionSpec, ZIndexValue,
};
use crate::values_math::{
    format_math_f64, CalcSizeBasis, CalcSizeExpr, MathNode, MathUnit, RotateSpec, ScaleSpec,
    TranslateSpec, ValueKind, MAX_CSS_PX,
};

/// Maximum expanded AST node count for `calc-size()` interpolation per CSSWG #10369.
const CALC_SIZE_MAX_EXPANSION_NODES: usize = 128;

/// Single keyframe entry for a Web Animations API (`element.animate(...)`) effect.
#[derive(Debug, Clone, PartialEq)]
pub struct WebAnimationKeyframe {
    pub offset: Option<f32>,
    pub composite: Option<AnimationCompositionMode>,
    pub easing: Option<TimingFunction>,
    pub declarations: Vec<SpecifiedDeclaration>,
}

/// Live Web Animation effect attached to a DOM element via `Element.prototype.animate()`.
#[derive(Debug, Clone, PartialEq)]
pub struct WebAnimationSpec {
    pub id: u64,
    pub keyframes: Vec<WebAnimationKeyframe>,
    pub duration_ms: f32,
    pub delay_ms: f32,
    pub timing: TimingFunction,
    pub iteration_count: f32,
    pub direction: AnimationDirectionMode,
    pub fill_mode: AnimationFillModeKind,
    pub play_state: AnimationPlayStateMode,
    pub composite: AnimationCompositionMode,
    pub current_time_ms: Option<f32>,
    pub cancelled: bool,
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
        | PropertyIdKind::Direction | PropertyIdKind::WritingMode | PropertyIdKind::All
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
    let has_web = web_anims.is_some_and(|w| w.iter().any(|a| !a.cancelled));
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
            apply_keyframe_effect(&mut current, &base_style, parent, dark_mode, &eval, &stops, raw_p, anim.timing, anim.composition, registered_props);
        }
    }
    if let Some(wanims) = web_anims {
        for wa in wanims {
            if wa.cancelled || wa.keyframes.is_empty() { continue; }
            let t = wa.current_time_ms.unwrap_or(elapsed_ms);
            let Some(raw_p) = compute_animation_progress(wa.duration_ms, wa.delay_ms, wa.iteration_count, wa.direction, wa.fill_mode, t) else { continue };
            let stops = build_web_keyframe_stops(&wa.keyframes, &current);
            apply_keyframe_effect(&mut current, &base_style, parent, dark_mode, &eval, &stops, raw_p, wa.timing, wa.composite, registered_props);
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
    Arc::new(current)
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
            (PropertyIdKind::AnimationTimingFunction, PropertyValue::TimingList(v)) => v.first().copied(), _ => None,
        });
        let decls = resolve_kf_decls(&step.declarations, style);
        for &off in &step.offsets { stops.push(NormalizedStop { offset: off.clamp(0.0, 1.0), composite: comp, easing, declarations: decls.clone() }); }
    }
    stops.sort_by(|a, b| a.offset.total_cmp(&b.offset));
    stops
}

#[rustfmt::skip]
fn build_web_keyframe_stops(kfs: &[WebAnimationKeyframe], style: &ComputedStyle) -> Vec<NormalizedStop> {
    let n = kfs.len();
    let mut stops: Vec<_> = kfs.iter().enumerate().map(|(i, kf)| {
        let off = kf.offset.unwrap_or_else(|| if n <= 1 { 1.0 } else { i as f32 / (n - 1) as f32 });
        NormalizedStop { offset: off.clamp(0.0, 1.0), composite: kf.composite, easing: kf.easing, declarations: resolve_kf_decls(&kf.declarations, style) }
    }).collect();
    stops.sort_by(|a, b| a.offset.total_cmp(&b.offset));
    stops
}

#[allow(clippy::too_many_arguments)]
#[rustfmt::skip]
fn apply_keyframe_effect(
    target: &mut ComputedStyle, underlying: &ComputedStyle, parent: Option<&ComputedStyle>,
    dark_mode: bool, eval: &EvalContext, stops: &[NormalizedStop], raw_p: f32,
    default_timing: TimingFunction, default_comp: AnimationCompositionMode,
    registered_props: &HashMap<InternedStr, RegisteredProperty>,
) {
    let mut props: Vec<(PropertyIdKind, Option<InternedStr>)> = Vec::new();
    for s in stops { for d in &s.declarations { let k = (d.property, d.custom_name.clone()); if !props.contains(&k) { props.push(k); } } }
    let allow_kw = underlying.text.interpolate_size == InterpolateSizeMode::AllowKeywords;
    for (prop, cname) in props {
        let mut ps: Vec<(f32, Option<&SpecifiedDeclaration>, AnimationCompositionMode, Option<TimingFunction>)> = Vec::new();
        for s in stops {
            if let Some(decl) = s.declarations.iter().rev().find(|d| d.property == prop && d.custom_name == cname) {
                let comp = s.composite.unwrap_or(default_comp);
                if ps.last().is_some_and(|l| (l.0 - s.offset).abs() < 1e-6) { *ps.last_mut().unwrap() = (s.offset, Some(decl), comp, s.easing); }
                else { ps.push((s.offset, Some(decl), comp, s.easing)); }
            }
        }
        if ps.first().is_none_or(|s| s.0 > 0.0) { ps.insert(0, (0.0, None, AnimationCompositionMode::Replace, None)); }
        if ps.last().is_none_or(|s| s.0 < 1.0) { ps.push((1.0, None, AnimationCompositionMode::Replace, None)); }
        let mut seg = 0usize;
        for (i, item) in ps.iter().enumerate().take(ps.len().saturating_sub(1)) { if raw_p >= item.0 { seg = i; } }
        let (s0, s1) = (&ps[seg], &ps[(seg + 1).min(ps.len() - 1)]);
        let span = f64::from(s1.0 - s0.0);
        let seg_t = if span <= 0.0 { 1.0 } else { (f64::from(raw_p - s0.0) / span).clamp(0.0, 1.0) };
        let eased_p = s0.3.unwrap_or(default_timing).evaluate_f64(seg_t);
        let f_st = eval_keyframe_endpoint(underlying, parent, dark_mode, eval, prop, cname.as_ref(), s0.1, s0.2, allow_kw, registered_props);
        let t_st = eval_keyframe_endpoint(underlying, parent, dark_mode, eval, prop, cname.as_ref(), s1.1, s1.2, allow_kw, registered_props);
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
    apply_declaration(&mut kf, decl, parent, eval, dark_mode);
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
                P::PaddingBottom, P::PaddingLeft, P::Top, P::Right, P::Bottom, P::Left, P::ColumnCount, P::BorderSpacing]);
        }
        if !Arc::ptr_eq(&before.visual, &after.visual) && before.visual != after.visual {
            all_p.extend_from_slice(&[P::BackgroundColor, P::BorderTopColor, P::BorderRightColor, P::BorderBottomColor, P::BorderLeftColor, P::OutlineColor]);
        }
        if before.display != after.display { all_p.push(P::Display); }
        if before.position != after.position { all_p.push(P::Position); }
        if before.overlay != after.overlay { all_p.push(P::Overlay); }
        if before.opacity != after.opacity { all_p.push(P::Opacity); }
        if !Arc::ptr_eq(&before.text, &after.text) && before.text != after.text {
            all_p.extend_from_slice(&[P::Color, P::FontSize, P::FontWeight, P::LineHeight, P::LetterSpacing, P::WordSpacing,
                P::TextIndent, P::Orphans, P::Widows, P::TabSize, P::TextJustify, P::TextAlignLast,
                P::HyphenateCharacter, P::HyphenateLimitLines]);
        }
        if before.z_index != after.z_index { all_p.push(P::ZIndex); }
        if before.flex_grid != after.flex_grid { all_p.extend_from_slice(&[P::FlexGrow, P::FlexShrink, P::FlexBasis, P::Order, P::FlexDirection, P::FlexWrap, P::AlignItems, P::AlignSelf, P::AlignContent, P::JustifyContent, P::JustifyItems, P::JustifySelf]); }
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
        P::TextIndent => a.text.text_indent != b.text.text_indent, P::ZIndex => a.z_index != b.z_index,
        P::Orphans => a.text.orphans != b.text.orphans, P::Widows => a.text.widows != b.text.widows,
        P::Display => a.display != b.display,
        P::Position => a.position != b.position,
        P::Overlay => a.overlay != b.overlay,
        P::ColumnCount => a.box_model.column_count != b.box_model.column_count,
        P::BorderSpacing => a.box_model.border_spacing_x != b.box_model.border_spacing_x || a.box_model.border_spacing_y != b.box_model.border_spacing_y,
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
        P::TextIndent => Arc::make_mut(&mut kf.text).text_indent = compose_lp(&underlying.text.text_indent, &kf.text.text_indent, eval, false),
        P::LetterSpacing => Arc::make_mut(&mut kf.text).letter_spacing = compose_lp(&underlying.text.letter_spacing, &kf.text.letter_spacing, eval, false),
        P::WordSpacing => Arc::make_mut(&mut kf.text).word_spacing = compose_lp(&underlying.text.word_spacing, &kf.text.word_spacing, eval, false),
        P::BorderSpacing => {
            let x = compose_lp(&underlying.box_model.border_spacing_x, &kf.box_model.border_spacing_x, eval, true);
            let y = compose_lp(&underlying.box_model.border_spacing_y, &kf.box_model.border_spacing_y, eval, true);
            let bm = Arc::make_mut(&mut kf.box_model); bm.border_spacing_x = x; bm.border_spacing_y = y;
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
        P::HyphenateLimitLines => if let (Some(u), Some(k)) = (underlying.text.hyphenate_limit_lines, kf.text.hyphenate_limit_lines) { Arc::make_mut(&mut kf.text).hyphenate_limit_lines = Some(u.saturating_add(k).max(1)); },
        P::ReadingOrder => {
            let u = underlying.effects.as_ref().map_or(0, |e| e.reading_order);
            let k = kf.effects.as_ref().map_or(0, |e| e.reading_order);
            kf.effects_mut().reading_order = u.saturating_add(k);
        }
        P::Color => { let c = compose_resolved_color(resolved_text_color(underlying, eval), resolved_text_color(kf, eval)); let t = Arc::make_mut(&mut kf.text); t.color = c.to_srgb_u8(); t.color_computed = Some(Arc::new(CssColorExpr::Resolved(c))); }
        P::BackgroundColor => { let c = compose_resolved_color(resolved_bg_color(underlying, eval), resolved_bg_color(kf, eval)); let v = Arc::make_mut(&mut kf.visual); v.background_color = c.to_srgb_u8(); v.background_color_computed = Some(Arc::new(CssColorExpr::Resolved(c))); }
        P::Opacity => kf.opacity = (underlying.opacity + kf.opacity).clamp(0.0, 1.0),
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
        P::PaddingTop | P::PaddingRight | P::PaddingBottom | P::PaddingLeft | P::TextIndent | P::LetterSpacing | P::WordSpacing => {
            let (fv, tv) = match prop {
                P::PaddingTop => (&from.box_model.padding.top, &to.box_model.padding.top), P::PaddingRight => (&from.box_model.padding.right, &to.box_model.padding.right),
                P::PaddingBottom => (&from.box_model.padding.bottom, &to.box_model.padding.bottom), P::PaddingLeft => (&from.box_model.padding.left, &to.box_model.padding.left),
                P::TextIndent => (&from.text.text_indent, &to.text.text_indent), P::LetterSpacing => (&from.text.letter_spacing, &to.text.letter_spacing),
                _ => (&from.text.word_spacing, &to.text.word_spacing),
            };
            let mut res = interpolate_lp(fv, tv, p, eval, is_transition);
            if matches!(prop, P::PaddingTop | P::PaddingRight | P::PaddingBottom | P::PaddingLeft) {
                res = clamp_lp_non_neg(res);
            }
            match prop {
                P::PaddingTop => Arc::make_mut(&mut target.box_model).padding.top = res, P::PaddingRight => Arc::make_mut(&mut target.box_model).padding.right = res,
                P::PaddingBottom => Arc::make_mut(&mut target.box_model).padding.bottom = res, P::PaddingLeft => Arc::make_mut(&mut target.box_model).padding.left = res,
                P::TextIndent => Arc::make_mut(&mut target.text).text_indent = res, P::LetterSpacing => Arc::make_mut(&mut target.text).letter_spacing = res,
                _ => Arc::make_mut(&mut target.text).word_spacing = res,
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
            let x = clamp_lp_non_neg(interpolate_lp(&from.box_model.border_spacing_x, &to.box_model.border_spacing_x, p, eval, is_transition));
            let y = clamp_lp_non_neg(interpolate_lp(&from.box_model.border_spacing_y, &to.box_model.border_spacing_y, p, eval, is_transition));
            let bm = Arc::make_mut(&mut target.box_model); bm.border_spacing_x = x; bm.border_spacing_y = y;
        }
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
            (Some(a), Some(b)) => Some((((f64::from(a) * (1.0 - p) + f64::from(b) * p) + 0.5).floor() as i32).max(1)),
            _ => if use_from_disc { from.text.hyphenate_limit_lines } else { to.text.hyphenate_limit_lines },
        },
        P::ReadingFlow => target.effects_mut().reading_flow = if use_from_disc { from.effects.as_ref().map_or_else(Default::default, |e| e.reading_flow) } else { to.effects.as_ref().map_or_else(Default::default, |e| e.reading_flow) },
        P::ReadingOrder => {
            let (a, b) = (from.effects.as_ref().map_or(0, |e| e.reading_order), to.effects.as_ref().map_or(0, |e| e.reading_order));
            target.effects_mut().reading_order = ((f64::from(a) * (1.0 - p) + f64::from(b) * p) + 0.5).floor() as i32;
        }
        P::Opacity => target.opacity = (f64::from(from.opacity) * (1.0 - p) + f64::from(to.opacity) * p).clamp(0.0, 1.0) as f32,
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
            let src = if p <= 0.0 || use_from_disc { from } else { to };
            target.effects_mut().translate = src.effects.as_ref().map_or(TranslateSpec::None, |e| e.translate.clone());
        }
        P::Transform => {
            let src = if p <= 0.0 || use_from_disc { from } else { to };
            let eff = target.effects_mut();
            eff.transform = src.effects.as_ref().map(|e| e.transform.clone()).unwrap_or_default();
            eff.transform_raw = src.effects.as_ref().and_then(|e| e.transform_raw.clone());
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
fn resolved_to_srgb_f64(c: ResolvedColor) -> (f64, f64, f64, f64) {
    let s = c.convert_to(ColorSpace::Srgb);
    (s.c[0].unwrap_or(0.0), s.c[1].unwrap_or(0.0), s.c[2].unwrap_or(0.0), s.alpha.unwrap_or(1.0))
}

#[rustfmt::skip]
fn interpolate_resolved_color(a: ResolvedColor, b: ResolvedColor, p: f64) -> ResolvedColor {
    let ((ar, ag, ab, aa), (br, bg, bb, ba)) = (resolved_to_srgb_f64(a), resolved_to_srgb_f64(b));
    let inv = 1.0 - p;
    let alpha = (aa * inv + ba * p).clamp(0.0, 1.0);
    let (r, g, bl) = if alpha > 1e-9 { ((ar * aa * inv + br * ba * p) / alpha, (ag * aa * inv + bg * ba * p) / alpha, (ab * aa * inv + bb * ba * p) / alpha) } else { (0.0, 0.0, 0.0) };
    let space = if a.space == ColorSpace::SrgbLegacy && b.space == ColorSpace::SrgbLegacy { ColorSpace::SrgbLegacy } else { ColorSpace::Srgb };
    ResolvedColor { space, c: [Some(r.clamp(0.0, 1.0)), Some(g.clamp(0.0, 1.0)), Some(bl.clamp(0.0, 1.0))], alpha: Some(alpha) }
}

#[rustfmt::skip]
fn compose_resolved_color(u: ResolvedColor, k: ResolvedColor) -> ResolvedColor {
    let ((ur, ug, ub, ua), (kr, kg, kb, ka)) = (resolved_to_srgb_f64(u), resolved_to_srgb_f64(k));
    let space = if u.space == ColorSpace::SrgbLegacy && k.space == ColorSpace::SrgbLegacy { ColorSpace::SrgbLegacy } else { ColorSpace::Srgb };
    ResolvedColor { space, c: [Some((ur * ua + kr * ka).clamp(0.0, 1.0)), Some((ug * ua + kg * ka).clamp(0.0, 1.0)), Some((ub * ua + kb * ka).clamp(0.0, 1.0))], alpha: Some((ua + ka).clamp(0.0, 1.0)) }
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

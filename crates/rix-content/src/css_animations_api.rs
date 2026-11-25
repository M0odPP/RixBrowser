//! W3C CSS Transitions Level 1/2 & CSS Animations Level 1/2 WebIDL runtime:
//! - `TransitionEvent` & `AnimationEvent` WebIDL interfaces
//! - `CSSTransition` & `CSSAnimation` subclasses of `Animation`
//! - Two-way synchronization between `rix_css::StyleContext` and `WebAnimationSystem`
//! - Exact W3C CSS Transitions 1 §6.2 & CSS Animations 2 §4.2 event state machines

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use rix_css::{
    compute_effect_timing, AnimationCompositionMode, AnimationFillModeKind, AnimationPlayStateMode,
    EffectPhase, EffectTimingInput, KeyframesRule, PropertyIdKind, PropertyValue, TimingFunction,
    TransitionControlOverride,
};
use rix_dom::NodeId;
use rix_js::promise::{alloc_promise, reject_promise, resolve_promise};
use rix_js::{HeapHandle, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};

use crate::cssom::format_computed_property_resolved;
use crate::dom_bridge::SharedHost;
use crate::dom_interfaces::define_to_string_tag;
use crate::event_loop::{run_microtask_checkpoint, SharedEventLoop};
use crate::events::{fire_dom_node_event_with_proto, register_constructed_event, SharedEvents};
use crate::web_animations_core::{
    anim_current_time, anim_play_state, get_or_create_doc_timeline, sync_target_to_style_ctx,
    AnimOrigin, AnimState, CssEventPhase, EffectState, PendingTask, QueuedCssEvent, ReplaceState,
    SharedWebAnimations, WebAnimationSystem,
};
use crate::web_animations_timing::{
    css_prop_to_js_keyframe, is_supported_animatable_css_prop, KeyframeComposite, ProcessedKeyframe,
};
use crate::web_apis::{
    arg, bind_ctor_proto, create_dom_exception, define_global, define_host_getter,
};

/// Installs `TransitionEvent`, `AnimationEvent`, `CSSTransition`, and `CSSAnimation`
/// onto `vm`, and activates `WebAnimationSystem` management of CSS `@keyframes` animations.
pub(crate) fn install_css_animations_and_transitions(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    wa: &SharedWebAnimations,
) {
    install_transition_event(vm, events, wa);
    install_animation_event(vm, events, wa);
    install_css_transition_iface(vm, wa);
    install_css_animation_iface(vm, wa);
    host.borrow_mut().set_css_animations_managed(true);
}

fn install_transition_event(vm: &mut JsVm, events: &SharedEvents, wa: &SharedWebAnimations) {
    let ev_proto = events.borrow().event_proto.unwrap_or(vm.object_proto);
    let proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(ev_proto)));
    define_to_string_tag(vm, proto, "TransitionEvent");
    wa.borrow_mut().protos.transition_event_proto = Some(proto);

    define_host_getter(vm, proto, "propertyName", |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let k = vm.heap.intern_key("__rix_te_prop__");
        Ok(vm.heap.object_get_own(h, k, &vm.shapes).map_or_else(
            || JsValue::from_string(vm.heap.alloc_string("")),
            |p| p.value,
        ))
    });
    define_host_getter(vm, proto, "elapsedTime", |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let k = vm.heap.intern_key("__rix_te_elapsed__");
        Ok(vm
            .heap
            .object_get_own(h, k, &vm.shapes)
            .map_or(JsValue::from_f64(0.0), |p| p.value))
    });
    define_host_getter(vm, proto, "pseudoElement", |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let k = vm.heap.intern_key("__rix_te_pseudo__");
        Ok(vm.heap.object_get_own(h, k, &vm.shapes).map_or_else(
            || JsValue::from_string(vm.heap.alloc_string("")),
            |p| p.value,
        ))
    });
    define_host_getter(vm, proto, "animation", |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let k = vm.heap.intern_key("__rix_te_anim__");
        Ok(vm
            .heap
            .object_get_own(h, k, &vm.shapes)
            .map_or(JsValue::NULL, |p| p.value))
    });

    let ev_r = Rc::clone(events);
    let ctor = vm.alloc_host_fn("TransitionEvent", move |vm, _, args| {
        if args.is_empty() {
            return Err(vm.type_error("TransitionEvent requires a type argument"));
        }
        let ev_type = vm.to_js_string(arg(args, 0))?;
        let init = arg(args, 1);
        let mut prop_name = String::new();
        let mut elapsed = 0.0_f64;
        let mut pseudo = String::new();
        let mut anim_val = JsValue::NULL;
        let (mut bubbles, mut cancelable, mut composed) = (false, false, false);
        if !init.is_undefined() && !init.is_null() {
            let Some(ih) = init.as_object() else {
                return Err(vm.type_error("TransitionEventInit must be an object"));
            };
            let kp = vm.heap.intern_key("propertyName");
            let vp = vm.get_property_by_key(ih, kp)?;
            if !vp.is_undefined() {
                prop_name = vm.to_js_string(vp)?;
            }
            let ke = vm.heap.intern_key("elapsedTime");
            let ve = vm.get_property_by_key(ih, ke)?;
            if !ve.is_undefined() {
                let n = vm.to_number(ve)?;
                if !n.is_finite() {
                    return Err(vm.type_error("elapsedTime must be a finite number"));
                }
                elapsed = n;
            }
            let kps = vm.heap.intern_key("pseudoElement");
            let vps = vm.get_property_by_key(ih, kps)?;
            if !vps.is_undefined() {
                pseudo = vm.to_js_string(vps)?;
            }
            let ka = vm.heap.intern_key("animation");
            let va = vm.get_property_by_key(ih, ka)?;
            if !va.is_undefined() && !va.is_null() {
                anim_val = va;
            }
            let kb = vm.heap.intern_key("bubbles");
            let vb = vm.get_property_by_key(ih, kb)?;
            bubbles = vm.to_boolean(vb);
            let kc = vm.heap.intern_key("cancelable");
            let vc = vm.get_property_by_key(ih, kc)?;
            cancelable = vm.to_boolean(vc);
            let kco = vm.heap.intern_key("composed");
            let vco = vm.get_property_by_key(ih, kco)?;
            composed = vm.to_boolean(vco);
        }
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        register_constructed_event(&ev_r, obj, ev_type, bubbles, cancelable, composed);
        let k1 = vm.heap.intern_key("__rix_te_prop__");
        let s1 = vm.heap.alloc_string(prop_name);
        vm.heap.object_define_data(
            obj,
            k1,
            JsValue::from_string(s1),
            PropertyAttrs::empty(),
            &mut vm.shapes,
        );
        let k2 = vm.heap.intern_key("__rix_te_elapsed__");
        vm.heap.object_define_data(
            obj,
            k2,
            JsValue::from_f64(elapsed),
            PropertyAttrs::empty(),
            &mut vm.shapes,
        );
        let k3 = vm.heap.intern_key("__rix_te_pseudo__");
        let s3 = vm.heap.alloc_string(pseudo);
        vm.heap.object_define_data(
            obj,
            k3,
            JsValue::from_string(s3),
            PropertyAttrs::empty(),
            &mut vm.shapes,
        );
        let k4 = vm.heap.intern_key("__rix_te_anim__");
        vm.heap
            .object_define_data(obj, k4, anim_val, PropertyAttrs::empty(), &mut vm.shapes);
        Ok(JsValue::from_object(obj))
    });
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "TransitionEvent", JsValue::from_object(ctor));
}

fn install_animation_event(vm: &mut JsVm, events: &SharedEvents, wa: &SharedWebAnimations) {
    let ev_proto = events.borrow().event_proto.unwrap_or(vm.object_proto);
    let proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(ev_proto)));
    define_to_string_tag(vm, proto, "AnimationEvent");
    wa.borrow_mut().protos.animation_event_proto = Some(proto);

    define_host_getter(vm, proto, "animationName", |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let k = vm.heap.intern_key("__rix_ae_name__");
        Ok(vm.heap.object_get_own(h, k, &vm.shapes).map_or_else(
            || JsValue::from_string(vm.heap.alloc_string("")),
            |p| p.value,
        ))
    });
    define_host_getter(vm, proto, "elapsedTime", |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let k = vm.heap.intern_key("__rix_ae_elapsed__");
        Ok(vm
            .heap
            .object_get_own(h, k, &vm.shapes)
            .map_or(JsValue::from_f64(0.0), |p| p.value))
    });
    define_host_getter(vm, proto, "pseudoElement", |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let k = vm.heap.intern_key("__rix_ae_pseudo__");
        Ok(vm.heap.object_get_own(h, k, &vm.shapes).map_or_else(
            || JsValue::from_string(vm.heap.alloc_string("")),
            |p| p.value,
        ))
    });
    define_host_getter(vm, proto, "animation", |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let k = vm.heap.intern_key("__rix_ae_anim__");
        Ok(vm
            .heap
            .object_get_own(h, k, &vm.shapes)
            .map_or(JsValue::NULL, |p| p.value))
    });

    let ev_r = Rc::clone(events);
    let ctor = vm.alloc_host_fn("AnimationEvent", move |vm, _, args| {
        if args.is_empty() {
            return Err(vm.type_error("AnimationEvent requires a type argument"));
        }
        let ev_type = vm.to_js_string(arg(args, 0))?;
        let init = arg(args, 1);
        let mut anim_name = String::new();
        let mut elapsed = 0.0_f64;
        let mut pseudo = String::new();
        let mut anim_val = JsValue::NULL;
        let (mut bubbles, mut cancelable, mut composed) = (false, false, false);
        if !init.is_undefined() && !init.is_null() {
            let Some(ih) = init.as_object() else {
                return Err(vm.type_error("AnimationEventInit must be an object"));
            };
            let kn = vm.heap.intern_key("animationName");
            let vn = vm.get_property_by_key(ih, kn)?;
            if !vn.is_undefined() {
                anim_name = vm.to_js_string(vn)?;
            }
            let ke = vm.heap.intern_key("elapsedTime");
            let ve = vm.get_property_by_key(ih, ke)?;
            if !ve.is_undefined() {
                let n = vm.to_number(ve)?;
                if !n.is_finite() {
                    return Err(vm.type_error("elapsedTime must be a finite number"));
                }
                elapsed = n;
            }
            let kps = vm.heap.intern_key("pseudoElement");
            let vps = vm.get_property_by_key(ih, kps)?;
            if !vps.is_undefined() {
                pseudo = vm.to_js_string(vps)?;
            }
            let ka = vm.heap.intern_key("animation");
            let va = vm.get_property_by_key(ih, ka)?;
            if !va.is_undefined() && !va.is_null() {
                anim_val = va;
            }
            let kb = vm.heap.intern_key("bubbles");
            let vb = vm.get_property_by_key(ih, kb)?;
            bubbles = vm.to_boolean(vb);
            let kc = vm.heap.intern_key("cancelable");
            let vc = vm.get_property_by_key(ih, kc)?;
            cancelable = vm.to_boolean(vc);
            let kco = vm.heap.intern_key("composed");
            let vco = vm.get_property_by_key(ih, kco)?;
            composed = vm.to_boolean(vco);
        }
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        register_constructed_event(&ev_r, obj, ev_type, bubbles, cancelable, composed);
        let k1 = vm.heap.intern_key("__rix_ae_name__");
        let s1 = vm.heap.alloc_string(anim_name);
        vm.heap.object_define_data(
            obj,
            k1,
            JsValue::from_string(s1),
            PropertyAttrs::empty(),
            &mut vm.shapes,
        );
        let k2 = vm.heap.intern_key("__rix_ae_elapsed__");
        vm.heap.object_define_data(
            obj,
            k2,
            JsValue::from_f64(elapsed),
            PropertyAttrs::empty(),
            &mut vm.shapes,
        );
        let k3 = vm.heap.intern_key("__rix_ae_pseudo__");
        let s3 = vm.heap.alloc_string(pseudo);
        vm.heap.object_define_data(
            obj,
            k3,
            JsValue::from_string(s3),
            PropertyAttrs::empty(),
            &mut vm.shapes,
        );
        let k4 = vm.heap.intern_key("__rix_ae_anim__");
        vm.heap
            .object_define_data(obj, k4, anim_val, PropertyAttrs::empty(), &mut vm.shapes);
        Ok(JsValue::from_object(obj))
    });
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "AnimationEvent", JsValue::from_object(ctor));
}

fn install_css_transition_iface(vm: &mut JsVm, wa: &SharedWebAnimations) {
    let anim_proto = wa
        .borrow()
        .protos
        .animation_proto
        .unwrap_or(vm.object_proto);
    let proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(anim_proto)));
    define_to_string_tag(vm, proto, "CSSTransition");
    wa.borrow_mut().protos.css_transition_proto = Some(proto);

    let wa_g = Rc::clone(wa);
    define_host_getter(vm, proto, "transitionProperty", move |vm, this_val, _| {
        let Some(ah) = this_val.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let prop = wa_g
            .borrow()
            .animations
            .get(&ah)
            .and_then(|a| match &a.origin {
                AnimOrigin::CssTransition {
                    transition_property,
                    ..
                } => Some(transition_property.clone()),
                _ => None,
            })
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        Ok(JsValue::from_string(vm.heap.alloc_string(prop)))
    });

    let ctor = vm.alloc_host_fn("CSSTransition", |vm, _, _| {
        Err(vm.type_error("Illegal constructor"))
    });
    bind_ctor_proto(vm, ctor, proto);
    let k_anim = vm.heap.intern_key("Animation");
    if let Some(anim_ctor) = vm
        .heap
        .object_get_own(vm.global, k_anim, &vm.shapes)
        .and_then(|p| p.value.as_object())
    {
        vm.heap
            .object_set_prototype(ctor, Some(anim_ctor), &mut vm.shapes);
    }
    define_global(vm, "CSSTransition", JsValue::from_object(ctor));
}

fn install_css_animation_iface(vm: &mut JsVm, wa: &SharedWebAnimations) {
    let anim_proto = wa
        .borrow()
        .protos
        .animation_proto
        .unwrap_or(vm.object_proto);
    let proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(anim_proto)));
    define_to_string_tag(vm, proto, "CSSAnimation");
    wa.borrow_mut().protos.css_animation_proto = Some(proto);

    let wa_g = Rc::clone(wa);
    define_host_getter(vm, proto, "animationName", move |vm, this_val, _| {
        let Some(ah) = this_val.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let name = wa_g
            .borrow()
            .animations
            .get(&ah)
            .and_then(|a| match &a.origin {
                AnimOrigin::CssAnimation { animation_name, .. } => Some(animation_name.clone()),
                _ => None,
            })
            .ok_or_else(|| vm.type_error("Illegal invocation"))?;
        Ok(JsValue::from_string(vm.heap.alloc_string(name)))
    });

    let ctor = vm.alloc_host_fn("CSSAnimation", |vm, _, _| {
        Err(vm.type_error("Illegal constructor"))
    });
    bind_ctor_proto(vm, ctor, proto);
    let k_anim = vm.heap.intern_key("Animation");
    if let Some(anim_ctor) = vm
        .heap
        .object_get_own(vm.global, k_anim, &vm.shapes)
        .and_then(|p| p.value.as_object())
    {
        vm.heap
            .object_set_prototype(ctor, Some(anim_ctor), &mut vm.shapes);
    }
    define_global(vm, "CSSAnimation", JsValue::from_object(ctor));
}

// ---------------------------------------------------------------------------
// Synchronization between `StyleContext` and `WebAnimationSystem`
// ---------------------------------------------------------------------------

fn serialize_keyframe_decl_value(
    decl: &rix_css::SpecifiedDeclaration,
    css_p: &str,
    target_style: &rix_css::ComputedStyle,
    parent_style: Option<&rix_css::ComputedStyle>,
) -> String {
    if decl.property == PropertyIdKind::Custom {
        if let PropertyValue::Custom(toks) = &decl.value {
            return rix_css::properties::serialize_css_tokens(toks)
                .trim()
                .to_string();
        }
        return String::new();
    }
    let resolved_owned = if let PropertyValue::Unresolved(toks) = &decl.value {
        rix_css::computed::resolve_var_tokens(toks, &target_style.custom_properties).and_then(
            |exp| {
                let s = rix_css::properties::serialize_css_tokens(&exp);
                let mut missing = Vec::new();
                let parsed = rix_css::parse_inline_style(
                    &format!("{}: {}", decl.property.css_name(), s.trim()),
                    &mut missing,
                );
                parsed
                    .into_iter()
                    .rev()
                    .find(|d| d.property == decl.property)
            },
        )
    } else {
        None
    };
    let eff_decl = resolved_owned.as_ref().unwrap_or(decl);
    let mut tmp = target_style.clone();
    let eval = tmp.text.eval_ctx;
    rix_css::computed::apply_declaration(&mut tmp, eff_decl, parent_style, &eval, false);
    format_computed_property_resolved(&tmp, css_p, None, false, None)
}

fn keyframes_equal(a: &[ProcessedKeyframe], b: &[ProcessedKeyframe]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b.iter()).all(|(x, y)| {
            x.offset == y.offset
                && x.easing_str == y.easing_str
                && x.composite == y.composite
                && x.props == y.props
                && x.declarations == y.declarations
        })
}

fn build_processed_keyframes_from_rule(
    rule: &KeyframesRule,
    default_timing: &TimingFunction,
    target_style: &rix_css::ComputedStyle,
    parent_style: Option<&rix_css::ComputedStyle>,
) -> Vec<ProcessedKeyframe> {
    let mut frames: Vec<ProcessedKeyframe> = Vec::new();
    for step in &rule.steps {
        let comp = step
            .declarations
            .iter()
            .rev()
            .find_map(|d| match (d.property, &d.value) {
                (
                    PropertyIdKind::AnimationComposition,
                    PropertyValue::AnimationCompositionList(v),
                ) if !d.important => v.first().copied(),
                _ => None,
            })
            .map(|c| match c {
                AnimationCompositionMode::Replace => KeyframeComposite::Replace,
                AnimationCompositionMode::Add => KeyframeComposite::Add,
                AnimationCompositionMode::Accumulate => KeyframeComposite::Accumulate,
            })
            .unwrap_or(KeyframeComposite::Auto);
        let easing = step
            .declarations
            .iter()
            .rev()
            .find_map(|d| match (d.property, &d.value) {
                (PropertyIdKind::AnimationTimingFunction, PropertyValue::TimingList(v))
                    if !d.important =>
                {
                    v.first().cloned()
                }
                _ => None,
            })
            .unwrap_or_else(|| default_timing.clone());
        let easing_str = easing.to_css_string();
        let decls: Vec<rix_css::SpecifiedDeclaration> = step
            .declarations
            .iter()
            .filter(|d| {
                !d.important
                    && !matches!(
                        d.property,
                        PropertyIdKind::AnimationComposition
                            | PropertyIdKind::AnimationTimingFunction
                    )
                    && !rix_css::animations::is_property_not_animatable(d.property)
            })
            .cloned()
            .collect();
        let mut props: Vec<(String, String, String)> = Vec::new();
        for d in &decls {
            let css_p = d
                .custom_name
                .as_ref()
                .map(|c| c.as_str().to_string())
                .unwrap_or_else(|| d.property.css_name().to_string());
            if !is_supported_animatable_css_prop(&css_p) {
                continue;
            }
            let js_k = css_prop_to_js_keyframe(&css_p);
            let ser = serialize_keyframe_decl_value(d, &css_p, target_style, parent_style);
            if let Some(slot) = props.iter_mut().find(|(_, cp, _)| cp == &css_p) {
                slot.2 = ser;
            } else {
                props.push((js_k, css_p, ser));
            }
        }
        if props.is_empty() {
            continue;
        }
        for &off in &step.offsets {
            let clamped = f64::from(off.clamp(0.0, 1.0));
            // W3C CSS Animations 2 §4.3: override duplicate properties at the same offset,
            // and merge keyframes sharing identical `(offset, easing_str, composite)`.
            for prev in &mut frames {
                if (prev.computed_offset - clamped).abs() < 1e-6 {
                    for (_, cp, _) in &props {
                        prev.props.retain(|(_, pcp, _)| pcp != cp);
                        prev.declarations.retain(|pd| {
                            let p_name = pd
                                .custom_name
                                .as_ref()
                                .map(|c| c.as_str())
                                .unwrap_or_else(|| pd.property.css_name());
                            p_name != cp.as_str()
                        });
                    }
                }
            }
            if let Some(existing) = frames.iter_mut().find(|f| {
                (f.computed_offset - clamped).abs() < 1e-6
                    && f.easing_str == easing_str
                    && f.composite == comp
            }) {
                existing.props.extend(props.clone());
                existing.declarations.extend(decls.clone());
            } else {
                frames.push(ProcessedKeyframe {
                    offset: Some(clamped),
                    computed_offset: clamped,
                    easing: easing.clone(),
                    easing_str: easing_str.clone(),
                    composite: comp,
                    props: props.clone(),
                    declarations: decls.clone(),
                });
            }
        }
    }
    frames.retain(|f| !f.props.is_empty());
    frames.sort_by(|a, b| a.computed_offset.total_cmp(&b.computed_offset));
    frames
}

/// Fast-path CSS style flush invoked on Web Animations / CSS Animations getters that require
/// up-to-date style per W3C CSS Animations 2 §4.1 & CSS Transitions 2 §3.1.
pub(crate) fn flush_if_styles_dirty(vm: &mut JsVm, host: &SharedHost, wa: &SharedWebAnimations) {
    if host.borrow().styles_need_flush() {
        flush_and_sync_css_animations(vm, host, wa);
    }
}

fn is_node_hidden_by_display_none(host: &SharedHost, node: NodeId) -> bool {
    let hb = host.borrow();
    let doc = hb.document.borrow();
    if !doc.is_connected(node) {
        return true;
    }
    let mut cur = Some(node);
    while let Some(c) = cur {
        if let Some(st) = hb.style_ctx.style_for(c) {
            if st.display == rix_css::DisplayMode::None {
                return true;
            }
        }
        cur = doc.parent(c);
    }
    false
}

/// Flushes pending styles and synchronizes CSS Transitions (`CSSTransition`) and CSS
/// `@keyframes` Animations (`CSSAnimation`) between `StyleContext` and `WebAnimationSystem`.
pub(crate) fn flush_and_sync_css_animations(
    vm: &mut JsVm,
    host: &SharedHost,
    wa: &SharedWebAnimations,
) {
    let clock_ms = wa.borrow().frame_time_ms as f32;
    {
        let mut hb = host.borrow_mut();
        hb.set_animation_clock_ms(clock_ms);
        let _ = hb.ensure_styles();
    }

    let (cancelled_ids, active_trs, css_anim_cands) = {
        let mut hb = host.borrow_mut();
        let cancelled = hb.drain_cancelled_transition_ids();
        let trs = hb.all_active_transitions();
        let anims = hb.all_css_animation_candidates();
        (cancelled, trs, anims)
    };

    {
        let sys = wa.borrow();
        if cancelled_ids.is_empty()
            && active_trs.is_empty()
            && css_anim_cands.is_empty()
            && sys.css_transition_map.is_empty()
            && sys.css_animation_map.is_empty()
        {
            return;
        }
    }

    let mut dirty_nodes: HashSet<NodeId> = HashSet::new();

    // 1. Cancel CSSTransitions whose underlying style-driven transition was cancelled.
    for tr_id in cancelled_ids {
        let ah_opt = wa.borrow_mut().css_transition_map.remove(&tr_id);
        let Some(ah) = ah_opt else {
            host.borrow_mut().set_transition_override(tr_id, None, None);
            continue;
        };
        cancel_css_owned_animation(vm, host, wa, ah, true);
        host.borrow_mut().set_transition_override(tr_id, None, None);
    }

    // 2. Wrap newly started CSS transitions into `CSSTransition` instances.
    let kf_proto = wa
        .borrow()
        .protos
        .keyframe_effect_proto
        .unwrap_or(vm.object_proto);
    let tr_proto = wa
        .borrow()
        .protos
        .css_transition_proto
        .unwrap_or(vm.object_proto);
    for (node, tr) in active_trs {
        if wa.borrow().css_transition_map.contains_key(&tr.id) {
            continue;
        }
        let prop_name = tr
            .custom_name
            .as_ref()
            .map(|c| c.as_str().to_string())
            .unwrap_or_else(|| tr.prop.css_name().to_string());
        let from_val =
            format_computed_property_resolved(&tr.from_style, &prop_name, None, false, None);
        let to_val = format_computed_property_resolved(&tr.to_style, &prop_name, None, false, None);
        let js_prop = css_prop_to_js_keyframe(&prop_name);
        let kf_from = ProcessedKeyframe {
            offset: Some(0.0),
            computed_offset: 0.0,
            easing: TimingFunction::Linear,
            easing_str: "linear".to_string(),
            composite: KeyframeComposite::Auto,
            props: vec![(js_prop.clone(), prop_name.clone(), from_val)],
            declarations: Vec::new(),
        };
        let kf_to = ProcessedKeyframe {
            offset: Some(1.0),
            computed_offset: 1.0,
            easing: TimingFunction::Linear,
            easing_str: "linear".to_string(),
            composite: KeyframeComposite::Auto,
            props: vec![(js_prop, prop_name.clone(), to_val)],
            declarations: Vec::new(),
        };
        let eff = EffectState {
            target: Some(node),
            pseudo_element: tr.pseudo_element.clone(),
            timing: EffectTimingInput {
                delay_ms: f64::from(tr.spec.delay_ms),
                end_delay_ms: 0.0,
                fill_mode: AnimationFillModeKind::Backwards,
                fill_is_auto: false,
                iteration_start: 0.0,
                iterations: 1.0,
                duration_ms: Some(f64::from(tr.spec.duration_ms)),
                direction: rix_css::AnimationDirectionMode::Normal,
                easing: tr.spec.timing.clone(),
            },
            easing_str: tr.spec.timing.to_css_string(),
            composite: AnimationCompositionMode::Replace,
            iteration_composite: AnimationCompositionMode::Replace,
            keyframes: vec![kf_from, kf_to],
            css_default_timing: None,
            owner_anim: None,
        };
        let eff_h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(kf_proto)));
        let anim_h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(tr_proto)));
        let ready_promise = alloc_promise(vm);
        let finished_promise = alloc_promise(vm);
        let owner_doc = host.borrow().owner_doc_of(node);
        let tl_h = get_or_create_doc_timeline(vm, host, wa, owner_doc);

        let mut sys = wa.borrow_mut();
        let seq_id = sys.alloc_seq_id();
        let mut eff_owned = eff;
        eff_owned.owner_anim = Some(anim_h);
        sys.effects.insert(eff_h, eff_owned);
        sys.animations.insert(
            anim_h,
            AnimState {
                seq_id,
                id: String::new(),
                effect: Some(eff_h),
                timeline: Some(tl_h),
                start_time: None,
                hold_time: Some(0.0),
                playback_rate: 1.0,
                pending_playback_rate: None,
                pending_task: PendingTask::Play,
                replace_state: ReplaceState::Active,
                ready_promise,
                ready_settled: false,
                finished_promise,
                finished_settled: false,
                origin: AnimOrigin::CssTransition {
                    transition_id: tr.id,
                    owning_element: Some(node),
                    event_target: node,
                    pseudo_element: tr.pseudo_element.clone(),
                    transition_property: prop_name,
                    generation: tr.generation,
                    prev_phase: CssEventPhase::Idle,
                    last_active_time_ms: 0.0,
                    effect_replaced: false,
                },
            },
        );
        sys.css_transition_map.insert(tr.id, anim_h);
    }

    // 3. Reconcile CSS `@keyframes` animations (`CSSAnimation`).
    let anim_proto = wa
        .borrow()
        .protos
        .css_animation_proto
        .unwrap_or(vm.object_proto);
    let mut seen_keys: HashSet<(NodeId, Option<String>, String, usize)> = HashSet::new();

    for (node, pseudo, specs) in css_anim_cands {
        if is_node_hidden_by_display_none(host, node) {
            continue;
        }
        let mut name_occ: HashMap<String, usize> = HashMap::new();
        for (list_idx, spec) in specs.iter().enumerate() {
            let raw_name = spec.name.as_str();
            if raw_name.eq_ignore_ascii_case("none") {
                continue;
            }
            let unquoted = raw_name
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or(raw_name)
                .to_string();
            let kf_rule = host
                .borrow()
                .keyframes_rule(&unquoted)
                .or_else(|| host.borrow().keyframes_rule(raw_name));
            let Some(kf_rule) = kf_rule else {
                continue;
            };
            let occ_entry = name_occ.entry(unquoted.clone()).or_insert(0);
            let occ = *occ_entry;
            *occ_entry += 1;
            let key = (node, pseudo.clone(), unquoted.clone(), occ);
            seen_keys.insert(key.clone());

            let (target_style, parent_style_opt) = {
                let hb = host.borrow();
                let ts = hb.style_ctx.style_for(node).cloned().unwrap_or_default();
                let ps = hb
                    .document
                    .borrow()
                    .parent(node)
                    .and_then(|pid| hb.style_ctx.style_for(pid).cloned());
                (ts, ps)
            };
            let processed_kfs = build_processed_keyframes_from_rule(
                &kf_rule,
                &spec.timing,
                &target_style,
                parent_style_opt.as_deref(),
            );
            let css_wants_paused = spec.play_state == AnimationPlayStateMode::Paused;
            let css_timeline_none = spec.timeline.as_str().eq_ignore_ascii_case("none");

            let existing_ah = wa.borrow().css_animation_map.get(&key).copied();
            if let Some(ah) = existing_ah {
                let owner_doc = host.borrow().owner_doc_of(node);
                let doc_tl = get_or_create_doc_timeline(vm, host, wa, owner_doc);
                let need_new_ready = {
                    let sys = wa.borrow();
                    sys.animations.get(&ah).is_some_and(|a| {
                        if let AnimOrigin::CssAnimation {
                            play_state_overridden,
                            css_paused,
                            ..
                        } = &a.origin
                        {
                            !*play_state_overridden
                                && *css_paused != css_wants_paused
                                && a.ready_settled
                        } else {
                            false
                        }
                    })
                };
                let fresh_ready = need_new_ready.then(|| alloc_promise(vm));
                let mut changed = false;
                {
                    let mut sys = wa.borrow_mut();
                    let cur_t = sys
                        .animations
                        .get(&ah)
                        .and_then(|a| anim_current_time(&sys, host, a));
                    let (eff_h_opt, eff_ov, kf_ov, t_mask, ps_ov, tl_ov, prev_css_paused) =
                        match sys.animations.get_mut(&ah) {
                            Some(a) => {
                                if let AnimOrigin::CssAnimation {
                                    list_index,
                                    effect_overridden,
                                    keyframes_overridden,
                                    timing_overridden_mask,
                                    play_state_overridden,
                                    timeline_overridden,
                                    css_paused,
                                    ..
                                } = &mut a.origin
                                {
                                    if *list_index != list_idx {
                                        *list_index = list_idx;
                                        changed = true;
                                    }
                                    (
                                        a.effect,
                                        *effect_overridden,
                                        *keyframes_overridden,
                                        *timing_overridden_mask,
                                        *play_state_overridden,
                                        *timeline_overridden,
                                        *css_paused,
                                    )
                                } else {
                                    continue;
                                }
                            }
                            None => continue,
                        };
                    if !eff_ov {
                        if let Some(eh) = eff_h_opt {
                            if let Some(eff) = sys.effects.get_mut(&eh) {
                                let mut new_timing = eff.timing.clone();
                                if t_mask & 1 == 0 {
                                    new_timing.delay_ms = f64::from(spec.delay_ms);
                                }
                                if t_mask & 2 == 0 {
                                    new_timing.direction = spec.direction;
                                }
                                if t_mask & 4 == 0 {
                                    new_timing.duration_ms = Some(f64::from(spec.duration_ms));
                                }
                                if t_mask & 32 == 0 {
                                    new_timing.fill_mode = spec.fill_mode;
                                    new_timing.fill_is_auto = false;
                                }
                                if t_mask & 128 == 0 {
                                    new_timing.iterations = f64::from(spec.iteration_count);
                                }
                                if eff.timing != new_timing {
                                    eff.timing = new_timing;
                                    changed = true;
                                }
                                if eff.composite != spec.composition {
                                    eff.composite = spec.composition;
                                    changed = true;
                                }
                                if !kf_ov
                                    && (!keyframes_equal(&eff.keyframes, &processed_kfs)
                                        || eff.css_default_timing.as_ref() != Some(&spec.timing))
                                {
                                    eff.keyframes = processed_kfs;
                                    eff.css_default_timing = Some(spec.timing.clone());
                                    changed = true;
                                }
                            }
                        }
                    }
                    if !tl_ov {
                        let desired_tl = if css_timeline_none {
                            None
                        } else {
                            Some(doc_tl)
                        };
                        if let Some(a) = sys.animations.get_mut(&ah) {
                            if a.timeline != desired_tl {
                                a.timeline = desired_tl;
                                changed = true;
                            }
                        }
                    }
                    if !ps_ov && prev_css_paused != css_wants_paused {
                        if let Some(a) = sys.animations.get_mut(&ah) {
                            if let AnimOrigin::CssAnimation { css_paused, .. } = &mut a.origin {
                                *css_paused = css_wants_paused;
                            }
                            if css_wants_paused {
                                if a.hold_time.is_none() {
                                    a.hold_time = cur_t.or(Some(0.0));
                                }
                                a.pending_task = PendingTask::Pause;
                            } else {
                                a.pending_task = PendingTask::Play;
                            }
                            if let Some(rp) = fresh_ready {
                                a.ready_promise = rp;
                                a.ready_settled = false;
                            }
                            changed = true;
                        }
                    }
                }
                if changed {
                    dirty_nodes.insert(node);
                }
            } else {
                let owner_doc = host.borrow().owner_doc_of(node);
                let doc_tl = get_or_create_doc_timeline(vm, host, wa, owner_doc);
                let timeline = if css_timeline_none {
                    None
                } else {
                    Some(doc_tl)
                };
                let eff = EffectState {
                    target: Some(node),
                    pseudo_element: pseudo.clone(),
                    timing: EffectTimingInput {
                        delay_ms: f64::from(spec.delay_ms),
                        end_delay_ms: 0.0,
                        fill_mode: spec.fill_mode,
                        fill_is_auto: false,
                        iteration_start: 0.0,
                        iterations: f64::from(spec.iteration_count),
                        duration_ms: Some(f64::from(spec.duration_ms)),
                        direction: spec.direction,
                        easing: TimingFunction::Linear,
                    },
                    easing_str: "linear".to_string(),
                    composite: spec.composition,
                    iteration_composite: AnimationCompositionMode::Replace,
                    keyframes: processed_kfs,
                    css_default_timing: Some(spec.timing.clone()),
                    owner_anim: None,
                };
                let eff_h = vm
                    .heap
                    .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(kf_proto)));
                let anim_h = vm
                    .heap
                    .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(anim_proto)));
                let ready_promise = alloc_promise(vm);
                let finished_promise = alloc_promise(vm);
                let mut sys = wa.borrow_mut();
                let seq_id = sys.alloc_seq_id();
                let mut eff_owned = eff;
                eff_owned.owner_anim = Some(anim_h);
                sys.effects.insert(eff_h, eff_owned);
                sys.animations.insert(
                    anim_h,
                    AnimState {
                        seq_id,
                        id: String::new(),
                        effect: Some(eff_h),
                        timeline,
                        start_time: None,
                        hold_time: Some(0.0),
                        playback_rate: 1.0,
                        pending_playback_rate: None,
                        pending_task: if css_wants_paused {
                            PendingTask::Pause
                        } else {
                            PendingTask::Play
                        },
                        replace_state: ReplaceState::Active,
                        ready_promise,
                        ready_settled: false,
                        finished_promise,
                        finished_settled: false,
                        origin: AnimOrigin::CssAnimation {
                            owning_element: Some(node),
                            event_target: node,
                            pseudo_element: pseudo.clone(),
                            animation_name: unquoted,
                            list_index: list_idx,
                            effect_overridden: false,
                            keyframes_overridden: false,
                            timing_overridden_mask: 0,
                            play_state_overridden: false,
                            timeline_overridden: false,
                            css_paused: css_wants_paused,
                            prev_phase: CssEventPhase::Idle,
                            prev_iteration: None,
                            last_active_time_ms: 0.0,
                        },
                    },
                );
                sys.css_animation_map.insert(key, anim_h);
                dirty_nodes.insert(node);
            }
        }
    }

    // 4. Cancel CSSAnimations removed from computed `animation-name` or hidden via `display: none`.
    let removed_anim_keys: Vec<(NodeId, Option<String>, String, usize)> = wa
        .borrow()
        .css_animation_map
        .keys()
        .filter(|k| !seen_keys.contains(*k))
        .cloned()
        .collect();
    for key in removed_anim_keys {
        let node = key.0;
        let ah_opt = wa.borrow_mut().css_animation_map.remove(&key);
        if let Some(ah) = ah_opt {
            cancel_css_owned_animation(vm, host, wa, ah, true);
            dirty_nodes.insert(node);
        }
    }

    if !dirty_nodes.is_empty() {
        for node in dirty_nodes {
            sync_target_to_style_ctx(host, wa, node);
        }
        let _ = host.borrow_mut().ensure_styles();
    }
}

fn cancel_css_owned_animation(
    vm: &mut JsVm,
    host: &SharedHost,
    wa: &SharedWebAnimations,
    anim_h: HeapHandle,
    disassociate: bool,
) {
    let info = {
        let mut sys = wa.borrow_mut();
        let ps = sys
            .animations
            .get(&anim_h)
            .map_or("idle", |a| anim_play_state(&sys, host, a));
        let Some(anim) = sys.animations.get_mut(&anim_h) else {
            return;
        };
        let was_idle = ps == "idle";
        let old_ready = (!anim.ready_settled).then_some(anim.ready_promise);
        let old_finished = (!anim.finished_settled).then_some(anim.finished_promise);
        let seq_id = anim.seq_id;
        let mut ev_opt = None;
        match &mut anim.origin {
            AnimOrigin::CssTransition {
                owning_element,
                event_target,
                pseudo_element,
                transition_property,
                prev_phase,
                last_active_time_ms,
                ..
            } => {
                if disassociate {
                    *owning_element = None;
                }
                if matches!(*prev_phase, CssEventPhase::Before | CssEventPhase::Active) {
                    let elapsed_s = if *prev_phase == CssEventPhase::Before {
                        0.0
                    } else {
                        *last_active_time_ms / 1000.0
                    };
                    ev_opt = Some(QueuedCssEvent {
                        target_node: *event_target,
                        anim_h,
                        is_transition: true,
                        event_type: "transitioncancel",
                        name: transition_property.clone(),
                        elapsed_time_s: elapsed_s.max(0.0),
                        pseudo_element: pseudo_element.clone().unwrap_or_default(),
                        scheduled_time: 0.0,
                        seq_id,
                    });
                }
                *prev_phase = CssEventPhase::Idle;
            }
            AnimOrigin::CssAnimation {
                owning_element,
                event_target,
                pseudo_element,
                animation_name,
                prev_phase,
                prev_iteration,
                last_active_time_ms,
                ..
            } => {
                if disassociate {
                    *owning_element = None;
                }
                if *prev_phase == CssEventPhase::Active {
                    ev_opt = Some(QueuedCssEvent {
                        target_node: *event_target,
                        anim_h,
                        is_transition: false,
                        event_type: "animationcancel",
                        name: animation_name.clone(),
                        elapsed_time_s: (*last_active_time_ms / 1000.0).max(0.0),
                        pseudo_element: pseudo_element.clone().unwrap_or_default(),
                        scheduled_time: 0.0,
                        seq_id,
                    });
                }
                *prev_phase = CssEventPhase::Idle;
                *prev_iteration = None;
            }
            AnimOrigin::Script => {}
        }
        anim.hold_time = None;
        anim.start_time = None;
        anim.pending_task = PendingTask::None;
        anim.pending_playback_rate = None;
        if let Some(ev) = ev_opt {
            sys.queued_css_events.push(ev);
        }
        Some((was_idle, old_ready, old_finished))
    };

    let Some((was_idle, old_ready, old_finished)) = info else {
        return;
    };
    if !was_idle {
        let abort_err = create_dom_exception(vm, "AbortError", "Animation was cancelled").value;
        if let Some(rp) = old_ready {
            reject_promise(vm, rp, abort_err);
        }
        if let Some(fp) = old_finished {
            reject_promise(vm, fp, abort_err);
        }
        let new_ready = alloc_promise(vm);
        resolve_promise(vm, new_ready, JsValue::from_object(anim_h));
        let new_finished = alloc_promise(vm);
        if let Some(anim) = wa.borrow_mut().animations.get_mut(&anim_h) {
            anim.ready_promise = new_ready;
            anim.ready_settled = true;
            anim.finished_promise = new_finished;
            anim.finished_settled = false;
        }
    }
}

/// Evaluates W3C CSS Transitions 1 §6.2 & CSS Animations 2 §4.2 event state transitions
/// for a single animation and queues any newly triggered DOM events.
pub(crate) fn update_css_events_for_anim(
    sys: &mut WebAnimationSystem,
    host: &SharedHost,
    anim_h: HeapHandle,
) {
    let Some(anim) = sys.animations.get(&anim_h) else {
        return;
    };
    if matches!(anim.origin, AnimOrigin::Script) {
        return;
    }
    let ct = anim_current_time(sys, host, anim);
    let ps = anim_play_state(sys, host, anim);
    let seq_id = anim.seq_id;
    let sched_time = sys.frame_time_ms;

    // Extract effect timing (if effect is null while running/finished, treat as zero-duration After).
    let (new_phase, active_time_ms, active_dur_ms, delay_ms, iter_dur_ms, iter_start, cur_iter) =
        if ps == "idle" {
            (CssEventPhase::Idle, 0.0, 0.0, 0.0, 0.0, 0.0, None)
        } else if let Some(eh) = anim.effect {
            if let Some(eff) = sys.effects.get(&eh) {
                let comp = compute_effect_timing(&eff.timing, ct, anim.playback_rate);
                let ph = match comp.phase {
                    EffectPhase::Idle => CssEventPhase::Idle,
                    EffectPhase::Before => CssEventPhase::Before,
                    EffectPhase::Active => CssEventPhase::Active,
                    EffectPhase::After => CssEventPhase::After,
                };
                let iter_u64 = comp.current_iteration.and_then(|v| {
                    if v.is_finite() && v >= 0.0 {
                        Some(v.floor() as u64)
                    } else {
                        None
                    }
                });
                (
                    ph,
                    comp.active_time.unwrap_or(0.0),
                    comp.active_duration,
                    eff.timing.delay_ms,
                    eff.timing.duration_ms.unwrap_or(0.0),
                    eff.timing.iteration_start,
                    iter_u64,
                )
            } else {
                (CssEventPhase::After, 0.0, 0.0, 0.0, 0.0, 0.0, None)
            }
        } else {
            // W3C CSS Animations 2 / CSS Transitions 2: setting `effect = null` on a running
            // animation transitions its phase to `After` (dispatching `animationend` / `transitionend`).
            (CssEventPhase::After, 0.0, 0.0, 0.0, 0.0, 0.0, None)
        };

    let start_elapsed_s = (-delay_ms).max(0.0).min(active_dur_ms) / 1000.0;
    let end_elapsed_s = if active_dur_ms.is_finite() {
        active_dur_ms.max(0.0) / 1000.0
    } else {
        0.0
    };

    let mut new_events: Vec<QueuedCssEvent> = Vec::new();
    let Some(anim_mut) = sys.animations.get_mut(&anim_h) else {
        return;
    };
    match &mut anim_mut.origin {
        AnimOrigin::CssTransition {
            event_target,
            pseudo_element,
            transition_property,
            prev_phase,
            last_active_time_ms,
            ..
        } => {
            let old = *prev_phase;
            let pseudo_s = pseudo_element.clone().unwrap_or_default();
            let push = |ev_vec: &mut Vec<QueuedCssEvent>, ev_type: &'static str, elapsed_s: f64| {
                ev_vec.push(QueuedCssEvent {
                    target_node: *event_target,
                    anim_h,
                    is_transition: true,
                    event_type: ev_type,
                    name: transition_property.clone(),
                    elapsed_time_s: elapsed_s.max(0.0),
                    pseudo_element: pseudo_s.clone(),
                    scheduled_time: sched_time,
                    seq_id,
                });
            };
            if old != new_phase {
                match (old, new_phase) {
                    (CssEventPhase::Idle, CssEventPhase::Before) => {
                        push(&mut new_events, "transitionrun", start_elapsed_s);
                    }
                    (CssEventPhase::Idle, CssEventPhase::Active) => {
                        push(&mut new_events, "transitionrun", start_elapsed_s);
                        push(&mut new_events, "transitionstart", start_elapsed_s);
                    }
                    (CssEventPhase::Idle, CssEventPhase::After) => {
                        push(&mut new_events, "transitionrun", start_elapsed_s);
                        push(&mut new_events, "transitionstart", start_elapsed_s);
                        push(&mut new_events, "transitionend", end_elapsed_s);
                    }
                    (CssEventPhase::Before, CssEventPhase::Active) => {
                        push(&mut new_events, "transitionstart", start_elapsed_s);
                    }
                    (CssEventPhase::Before, CssEventPhase::After) => {
                        push(&mut new_events, "transitionstart", start_elapsed_s);
                        push(&mut new_events, "transitionend", end_elapsed_s);
                    }
                    (CssEventPhase::Before, CssEventPhase::Idle) => {
                        push(&mut new_events, "transitioncancel", 0.0);
                    }
                    (CssEventPhase::Active, CssEventPhase::After) => {
                        push(&mut new_events, "transitionend", end_elapsed_s);
                    }
                    (CssEventPhase::Active, CssEventPhase::Before) => {
                        push(&mut new_events, "transitionend", start_elapsed_s);
                    }
                    (CssEventPhase::Active, CssEventPhase::Idle) => {
                        push(
                            &mut new_events,
                            "transitioncancel",
                            *last_active_time_ms / 1000.0,
                        );
                    }
                    (CssEventPhase::After, CssEventPhase::Active) => {
                        push(&mut new_events, "transitionstart", end_elapsed_s);
                    }
                    (CssEventPhase::After, CssEventPhase::Before) => {
                        push(&mut new_events, "transitionstart", end_elapsed_s);
                        push(&mut new_events, "transitionend", start_elapsed_s);
                    }
                    _ => {}
                }
                *prev_phase = new_phase;
            }
            if new_phase == CssEventPhase::Active {
                *last_active_time_ms = active_time_ms;
            }
        }
        AnimOrigin::CssAnimation {
            event_target,
            pseudo_element,
            animation_name,
            prev_phase,
            prev_iteration,
            last_active_time_ms,
            ..
        } => {
            let old = *prev_phase;
            let pseudo_s = pseudo_element.clone().unwrap_or_default();
            let push = |ev_vec: &mut Vec<QueuedCssEvent>, ev_type: &'static str, elapsed_s: f64| {
                ev_vec.push(QueuedCssEvent {
                    target_node: *event_target,
                    anim_h,
                    is_transition: false,
                    event_type: ev_type,
                    name: animation_name.clone(),
                    elapsed_time_s: elapsed_s.max(0.0),
                    pseudo_element: pseudo_s.clone(),
                    scheduled_time: sched_time,
                    seq_id,
                });
            };
            match (old, new_phase) {
                (CssEventPhase::Idle | CssEventPhase::Before, CssEventPhase::Active) => {
                    push(&mut new_events, "animationstart", start_elapsed_s);
                }
                (CssEventPhase::Idle | CssEventPhase::Before, CssEventPhase::After) => {
                    push(&mut new_events, "animationstart", start_elapsed_s);
                    push(&mut new_events, "animationend", end_elapsed_s);
                }
                (CssEventPhase::Active, CssEventPhase::Active) => {
                    if let (Some(prev_i), Some(cur_i)) = (*prev_iteration, cur_iter) {
                        if cur_i != prev_i {
                            let boundary = if cur_i > prev_i { cur_i } else { prev_i };
                            let iter_elapsed =
                                ((boundary as f64 - iter_start).max(0.0) * iter_dur_ms) / 1000.0;
                            push(&mut new_events, "animationiteration", iter_elapsed);
                        }
                    }
                }
                (CssEventPhase::Active, CssEventPhase::After) => {
                    push(&mut new_events, "animationend", end_elapsed_s);
                }
                (CssEventPhase::Active, CssEventPhase::Before) => {
                    push(&mut new_events, "animationend", start_elapsed_s);
                }
                (CssEventPhase::Active, CssEventPhase::Idle) => {
                    push(
                        &mut new_events,
                        "animationcancel",
                        *last_active_time_ms / 1000.0,
                    );
                }
                (CssEventPhase::After, CssEventPhase::Active) => {
                    push(&mut new_events, "animationstart", end_elapsed_s);
                }
                (CssEventPhase::After, CssEventPhase::Before) => {
                    push(&mut new_events, "animationstart", end_elapsed_s);
                    push(&mut new_events, "animationend", start_elapsed_s);
                }
                _ => {}
            }
            *prev_phase = new_phase;
            *prev_iteration = if new_phase == CssEventPhase::Active {
                *last_active_time_ms = active_time_ms;
                cur_iter
            } else {
                None
            };
        }
        AnimOrigin::Script => {}
    }

    sys.queued_css_events.extend(new_events);
}

pub(crate) fn update_css_events_for_all(host: &SharedHost, wa: &SharedWebAnimations) {
    let handles: Vec<HeapHandle> = {
        let sys = wa.borrow();
        let mut hs: Vec<(u64, HeapHandle)> = sys
            .animations
            .iter()
            .filter(|(_, a)| !matches!(a.origin, AnimOrigin::Script))
            .map(|(&h, a)| (a.seq_id, h))
            .collect();
        hs.sort_by_key(|(s, _)| *s);
        hs.into_iter().map(|(_, h)| h).collect()
    };
    let mut sys = wa.borrow_mut();
    for ah in handles {
        update_css_events_for_anim(&mut sys, host, ah);
    }
}

/// Dispatches all queued `TransitionEvent`s and `AnimationEvent`s in spec order.
pub(crate) fn dispatch_queued_css_events(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    wa: &SharedWebAnimations,
) {
    let mut to_dispatch = std::mem::take(&mut wa.borrow_mut().queued_css_events);
    if to_dispatch.is_empty() {
        return;
    }
    to_dispatch.sort_by(|a, b| {
        a.scheduled_time
            .total_cmp(&b.scheduled_time)
            .then_with(|| a.seq_id.cmp(&b.seq_id))
    });
    let (tr_proto, an_proto) = {
        let sys = wa.borrow();
        let fb = events.borrow().event_proto.unwrap_or(vm.object_proto);
        (
            sys.protos.transition_event_proto.unwrap_or(fb),
            sys.protos.animation_event_proto.unwrap_or(fb),
        )
    };
    for ev in to_dispatch {
        let name_val = JsValue::from_string(vm.heap.alloc_string(&ev.name));
        let pseudo_val = JsValue::from_string(vm.heap.alloc_string(&ev.pseudo_element));
        let elapsed_val = JsValue::from_f64(ev.elapsed_time_s);
        let anim_val = JsValue::from_object(ev.anim_h);
        if ev.is_transition {
            let _ = fire_dom_node_event_with_proto(
                vm,
                host,
                events,
                event_loop,
                ev.target_node,
                ev.event_type,
                true,
                false,
                tr_proto,
                &[
                    ("__rix_te_prop__", name_val),
                    ("__rix_te_elapsed__", elapsed_val),
                    ("__rix_te_pseudo__", pseudo_val),
                    ("__rix_te_anim__", anim_val),
                ],
            );
        } else {
            let _ = fire_dom_node_event_with_proto(
                vm,
                host,
                events,
                event_loop,
                ev.target_node,
                ev.event_type,
                true,
                false,
                an_proto,
                &[
                    ("__rix_ae_name__", name_val),
                    ("__rix_ae_elapsed__", elapsed_val),
                    ("__rix_ae_pseudo__", pseudo_val),
                    ("__rix_ae_anim__", anim_val),
                ],
            );
        }
    }
    run_microtask_checkpoint(event_loop, vm);
}

/// Pushes a `CSSTransition`'s current JS timing/cancel/disassociated override to `StyleContext`.
pub(crate) fn sync_css_transition_override(
    host: &SharedHost,
    wa: &SharedWebAnimations,
    anim_h: HeapHandle,
) {
    let info = {
        let sys = wa.borrow();
        let Some(anim) = sys.animations.get(&anim_h) else {
            return;
        };
        if let AnimOrigin::CssTransition {
            transition_id,
            owning_element,
            effect_replaced,
            ..
        } = &anim.origin
        {
            let ps = anim_play_state(&sys, host, anim);
            let ct = anim_current_time(&sys, host, anim);
            let ov = TransitionControlOverride {
                current_time_ms: ct,
                cancelled: ps == "idle" || anim.effect.is_none(),
                disassociated: *effect_replaced,
            };
            Some((*transition_id, *owning_element, ov))
        } else {
            None
        }
    };
    if let Some((tr_id, target, ov)) = info {
        host.borrow_mut()
            .set_transition_override(tr_id, target, Some(ov));
    }
}

//! W3C Web Animations Level 1 JS WebIDL interface bindings (`AnimationTimeline`,
//! `DocumentTimeline`, `AnimationPlaybackEvent`, `AnimationEffect`, `KeyframeEffect`,
//! `Animation`, `Element.prototype.animate`, `getAnimations`).
//!
//! Core timing, state machine, keyframe parsing, and VSync dispatch live in
//! `web_animations_core` and `web_animations_timing`.

use std::cell::RefCell;
use std::rc::Rc;

use rix_css::compute_effect_timing;
use rix_dom::NodeData;
use rix_js::promise::resolve_promise;
use rix_js::{HeapHandle, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};

use crate::cssom::{format_computed_property_resolved, set_inline_style_css_text};
use crate::dom_bridge::{require_node, wrap_node, SharedHost};
use crate::dom_interfaces::define_to_string_tag;
use crate::events::{
    install_object_event_handler_accessor, register_constructed_event, SharedEvents,
};
use crate::web_animations_core::{
    alloc_animation_object, anim_current_time, anim_effect_end, anim_play_state, apply_timing_dict,
    build_timing_js_object, cancel_animation, collect_animations_for_scope, finish_animation,
    get_or_create_doc_timeline, parse_css_numberish_time_ms, pause_animation, play_animation,
    process_keyframes_arg, reverse_animation, sync_anim_target, sync_target_to_style_ctx,
    update_finished_state, validate_target_arg, EffectState, PendingTask, ReplaceState,
    TimelineState,
};
pub use crate::web_animations_core::{
    update_animations_and_send_events, SharedWebAnimations, WebAnimationSystem,
};
use crate::web_animations_timing::{
    composite_as_str, parse_composite_exact, parse_iteration_composite_exact,
    validate_pseudo_element,
};
use crate::web_apis::{
    arg, bind_ctor_proto, create_dom_exception, define_global, define_host_accessor,
    define_host_getter, define_host_method,
};

/// Installs the complete W3C Web Animations Level 1 API surface onto `vm`.
pub fn install_web_animations(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
) -> SharedWebAnimations {
    let wa = Rc::new(RefCell::new(WebAnimationSystem::new()));
    let event_target_proto = host.borrow().prototypes.event_target_proto;

    install_timelines(vm, host, &wa);
    install_playback_event(vm, events, &wa);
    install_effects(vm, host, &wa);
    install_animation_iface(vm, host, events, &wa, event_target_proto);
    crate::css_animations_api::install_css_animations_and_transitions(vm, host, events, &wa);
    install_animatable_and_get_animations(vm, host, &wa);

    wa
}

fn install_timelines(vm: &mut JsVm, host: &SharedHost, wa: &SharedWebAnimations) {
    let tl_proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    define_to_string_tag(vm, tl_proto, "AnimationTimeline");
    let doc_tl_proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(tl_proto)));
    define_to_string_tag(vm, doc_tl_proto, "DocumentTimeline");
    {
        let mut sys = wa.borrow_mut();
        sys.protos.timeline_proto = Some(tl_proto);
        sys.protos.doc_timeline_proto = Some(doc_tl_proto);
    }

    // AnimationTimeline.prototype.currentTime
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_getter(vm, tl_proto, "currentTime", move |vm, this_val, _| {
            let Some(th) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let sys = wa_r.borrow();
            if !sys.timelines.contains_key(&th) {
                return Err(vm.type_error("Illegal invocation"));
            }
            Ok(sys
                .timeline_current_time(&h_r, th)
                .map_or(JsValue::NULL, JsValue::from_f64))
        });
    }
    // AnimationTimeline.prototype.duration (Web Animations L2: null for DocumentTimeline)
    {
        let wa_r = Rc::clone(wa);
        define_host_getter(vm, tl_proto, "duration", move |vm, this_val, _| {
            let Some(th) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if !wa_r.borrow().timelines.contains_key(&th) {
                return Err(vm.type_error("Illegal invocation"));
            }
            Ok(JsValue::NULL)
        });
    }

    // Illegal AnimationTimeline constructor
    let tl_ctor = vm.alloc_host_fn("AnimationTimeline", |vm, _, _| {
        Err(vm.type_error("Illegal constructor"))
    });
    bind_ctor_proto(vm, tl_ctor, tl_proto);
    define_global(vm, "AnimationTimeline", JsValue::from_object(tl_ctor));

    // new DocumentTimeline(options?)
    let h_ctor = Rc::clone(host);
    let wa_ctor = Rc::clone(wa);
    let doc_tl_ctor = vm.alloc_host_fn("DocumentTimeline", move |vm, _, args| {
        let mut origin_time = 0.0;
        let opt = arg(args, 0);
        if !opt.is_undefined() && !opt.is_null() {
            let Some(oh) = opt.as_object() else {
                return Err(vm.type_error("DocumentTimelineOptions must be an object"));
            };
            let k_ot = vm.heap.intern_key("originTime");
            let ot_v = vm.get_property_by_key(oh, k_ot)?;
            if !ot_v.is_undefined() {
                let n = vm.to_number(ot_v)?;
                if !n.is_finite() {
                    return Err(vm.type_error("originTime must be a finite number"));
                }
                origin_time = n;
            }
        }
        let main_root = h_ctor.borrow().document.borrow().root();
        let obj = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Ordinary,
            Some(doc_tl_proto),
        ));
        wa_ctor.borrow_mut().timelines.insert(
            obj,
            TimelineState {
                doc_node: main_root,
                origin_time,
            },
        );
        Ok(JsValue::from_object(obj))
    });
    bind_ctor_proto(vm, doc_tl_ctor, doc_tl_proto);
    vm.heap
        .object_set_prototype(doc_tl_ctor, Some(tl_ctor), &mut vm.shapes);
    define_global(vm, "DocumentTimeline", JsValue::from_object(doc_tl_ctor));

    // Document.prototype.timeline (per-document [SameObject] DocumentTimeline)
    let doc_proto = host.borrow().prototypes.document_proto;
    let h_doc = Rc::clone(host);
    let wa_doc = Rc::clone(wa);
    define_host_getter(vm, doc_proto, "timeline", move |vm, this_val, _| {
        let doc_node = require_node(vm, &h_doc, this_val)?;
        let tl_h = get_or_create_doc_timeline(vm, &h_doc, &wa_doc, doc_node);
        Ok(JsValue::from_object(tl_h))
    });
}

fn install_playback_event(vm: &mut JsVm, events: &SharedEvents, wa: &SharedWebAnimations) {
    let ev_proto = events.borrow().event_proto.unwrap_or(vm.object_proto);
    let proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(ev_proto)));
    define_to_string_tag(vm, proto, "AnimationPlaybackEvent");
    wa.borrow_mut().protos.playback_event_proto = Some(proto);

    define_host_getter(vm, proto, "currentTime", |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let k = vm.heap.intern_key("__rix_wa_ct__");
        Ok(vm
            .heap
            .object_get_own(h, k, &vm.shapes)
            .map_or(JsValue::NULL, |p| p.value))
    });
    define_host_getter(vm, proto, "timelineTime", |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Illegal invocation"));
        };
        let k = vm.heap.intern_key("__rix_wa_tlt__");
        Ok(vm
            .heap
            .object_get_own(h, k, &vm.shapes)
            .map_or(JsValue::NULL, |p| p.value))
    });

    let ev_r = Rc::clone(events);
    let ctor = vm.alloc_host_fn("AnimationPlaybackEvent", move |vm, _, args| {
        if args.is_empty() {
            return Err(vm.type_error("AnimationPlaybackEvent requires a type argument"));
        }
        let ev_type = vm.to_js_string(arg(args, 0))?;
        let init = arg(args, 1);
        let (mut ct, mut tlt, mut bubbles, mut cancelable) =
            (JsValue::NULL, JsValue::NULL, false, false);
        if let Some(ih) = init.as_object() {
            let k_ct = vm.heap.intern_key("currentTime");
            let v_ct = vm.get_property_by_key(ih, k_ct)?;
            if !v_ct.is_undefined() && !v_ct.is_null() {
                let n = vm.to_number(v_ct)?;
                if !n.is_finite() {
                    return Err(vm.type_error("currentTime must be finite"));
                }
                ct = JsValue::from_f64(n);
            }
            let k_tlt = vm.heap.intern_key("timelineTime");
            let v_tlt = vm.get_property_by_key(ih, k_tlt)?;
            if !v_tlt.is_undefined() && !v_tlt.is_null() {
                let n = vm.to_number(v_tlt)?;
                if !n.is_finite() {
                    return Err(vm.type_error("timelineTime must be finite"));
                }
                tlt = JsValue::from_f64(n);
            }
            let kb = vm.heap.intern_key("bubbles");
            let vb = vm.get_property_by_key(ih, kb)?;
            bubbles = vm.to_boolean(vb);
            let kc = vm.heap.intern_key("cancelable");
            let vc = vm.get_property_by_key(ih, kc)?;
            cancelable = vm.to_boolean(vc);
        }
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        register_constructed_event(&ev_r, obj, ev_type, bubbles, cancelable, false);
        let k1 = vm.heap.intern_key("__rix_wa_ct__");
        vm.heap
            .object_define_data(obj, k1, ct, PropertyAttrs::empty(), &mut vm.shapes);
        let k2 = vm.heap.intern_key("__rix_wa_tlt__");
        vm.heap
            .object_define_data(obj, k2, tlt, PropertyAttrs::empty(), &mut vm.shapes);
        Ok(JsValue::from_object(obj))
    });
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "AnimationPlaybackEvent", JsValue::from_object(ctor));
}

fn install_effects(vm: &mut JsVm, host: &SharedHost, wa: &SharedWebAnimations) {
    let eff_proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    define_to_string_tag(vm, eff_proto, "AnimationEffect");
    let kf_proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(eff_proto)));
    define_to_string_tag(vm, kf_proto, "KeyframeEffect");
    {
        let mut sys = wa.borrow_mut();
        sys.protos.effect_proto = Some(eff_proto);
        sys.protos.keyframe_effect_proto = Some(kf_proto);
    }

    // AnimationEffect.prototype.getTiming()
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_method(vm, eff_proto, "getTiming", move |vm, this_val, _| {
            let Some(eh) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            crate::css_animations_api::flush_if_styles_dirty(vm, &h_r, &wa_r);
            let (timing, easing_str) = {
                let sys = wa_r.borrow();
                let Some(e) = sys.effects.get(&eh) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                (e.timing.clone(), e.easing_str.clone())
            };
            Ok(JsValue::from_object(build_timing_js_object(
                vm,
                &timing,
                &easing_str,
            )))
        });
    }

    // AnimationEffect.prototype.getComputedTiming()
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_method(
            vm,
            eff_proto,
            "getComputedTiming",
            move |vm, this_val, _| {
                let Some(eh) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                crate::css_animations_api::flush_if_styles_dirty(vm, &h_r, &wa_r);
                let (timing, easing_str, ct, rate) = {
                    let sys = wa_r.borrow();
                    let Some(e) = sys.effects.get(&eh) else {
                        return Err(vm.type_error("Illegal invocation"));
                    };
                    let (ct, rate) = e
                        .owner_anim
                        .and_then(|ah| sys.animations.get(&ah))
                        .map_or((None, 1.0), |a| {
                            (anim_current_time(&sys, &h_r, a), a.playback_rate)
                        });
                    (e.timing.clone(), e.easing_str.clone(), ct, rate)
                };
                let comp = compute_effect_timing(&timing, ct, rate);
                let mut timing_resolved = timing;
                timing_resolved.duration_ms = Some(timing_resolved.duration_ms.unwrap_or(0.0));
                timing_resolved.fill_is_auto = false;
                let obj = build_timing_js_object(vm, &timing_resolved, &easing_str);
                let set = |vm: &mut JsVm, k: &str, v: JsValue| {
                    let pk = vm.heap.intern_key(k);
                    vm.heap
                        .object_define_data(obj, pk, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
                };
                set(vm, "startTime", JsValue::from_f64(0.0));
                set(vm, "endTime", JsValue::from_f64(comp.end_time));
                set(
                    vm,
                    "activeDuration",
                    JsValue::from_f64(comp.active_duration),
                );
                set(
                    vm,
                    "localTime",
                    comp.local_time.map_or(JsValue::NULL, JsValue::from_f64),
                );
                set(
                    vm,
                    "progress",
                    comp.progress.map_or(JsValue::NULL, JsValue::from_f64),
                );
                set(
                    vm,
                    "currentIteration",
                    comp.current_iteration
                        .map_or(JsValue::NULL, JsValue::from_f64),
                );
                Ok(JsValue::from_object(obj))
            },
        );
    }

    // AnimationEffect.prototype.updateTiming(timing?)
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_method(vm, eff_proto, "updateTiming", move |vm, this_val, args| {
            let Some(eh) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let (mut timing, mut easing_str, owner_anim, target) = {
                let sys = wa_r.borrow();
                let Some(e) = sys.effects.get(&eh) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                (
                    e.timing.clone(),
                    e.easing_str.clone(),
                    e.owner_anim,
                    e.target,
                )
            };
            let opts_arg = arg(args, 0);
            let mut set_mask: u8 = 0;
            if opts_arg.is_number() {
                set_mask |= 4;
            } else if let Some(oh) = opts_arg.as_object() {
                for (name, bit) in [
                    ("delay", 1u8),
                    ("direction", 2),
                    ("duration", 4),
                    ("easing", 8),
                    ("endDelay", 16),
                    ("fill", 32),
                    ("iterationStart", 64),
                    ("iterations", 128),
                ] {
                    let k = vm.heap.intern_key(name);
                    if !vm.get_property_by_key(oh, k)?.is_undefined() {
                        set_mask |= bit;
                    }
                }
            }
            apply_timing_dict(vm, opts_arg, &mut timing, &mut easing_str, None)?;
            {
                let mut sys = wa_r.borrow_mut();
                if let Some(e) = sys.effects.get_mut(&eh) {
                    e.timing = timing;
                    e.easing_str = easing_str;
                }
                if let Some(ah) = owner_anim {
                    if let Some(a) = sys.animations.get_mut(&ah) {
                        if let crate::web_animations_core::AnimOrigin::CssAnimation {
                            timing_overridden_mask,
                            ..
                        } = &mut a.origin
                        {
                            *timing_overridden_mask |= set_mask;
                        }
                    }
                }
            }
            if let Some(ah) = owner_anim {
                update_finished_state(vm, &h_r, &wa_r, ah, false, false);
            }
            if let Some(t) = target {
                sync_target_to_style_ctx(&h_r, &wa_r, t);
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    // KeyframeEffect.prototype.target
    {
        let h_g = Rc::clone(host);
        let wa_g = Rc::clone(wa);
        let h_s = Rc::clone(host);
        let wa_s = Rc::clone(wa);
        define_host_accessor(
            vm,
            kf_proto,
            "target",
            move |vm, this_val, _| {
                let Some(eh) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                let target = {
                    let sys = wa_g.borrow();
                    let Some(e) = sys.effects.get(&eh) else {
                        return Err(vm.type_error("Illegal invocation"));
                    };
                    e.target
                };
                Ok(target.map_or(JsValue::NULL, |n| wrap_node(vm, &h_g, n)))
            },
            move |vm, this_val, args| {
                let Some(eh) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                if !wa_s.borrow().effects.contains_key(&eh) {
                    return Err(vm.type_error("Illegal invocation"));
                }
                let new_target = validate_target_arg(vm, &h_s, arg(args, 0))?;
                let old_target = wa_s
                    .borrow_mut()
                    .effects
                    .get_mut(&eh)
                    .and_then(|e| std::mem::replace(&mut e.target, new_target));
                if let Some(ot) = old_target {
                    sync_target_to_style_ctx(&h_s, &wa_s, ot);
                }
                if let Some(nt) = new_target {
                    sync_target_to_style_ctx(&h_s, &wa_s, nt);
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // KeyframeEffect.prototype.pseudoElement
    {
        let h_s = Rc::clone(host);
        let wa_g = Rc::clone(wa);
        let wa_s = Rc::clone(wa);
        define_host_accessor(
            vm,
            kf_proto,
            "pseudoElement",
            move |vm, this_val, _| {
                let Some(eh) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                let ps = {
                    let sys = wa_g.borrow();
                    let Some(e) = sys.effects.get(&eh) else {
                        return Err(vm.type_error("Illegal invocation"));
                    };
                    e.pseudo_element.clone()
                };
                Ok(ps.map_or(JsValue::NULL, |s| {
                    JsValue::from_string(vm.heap.alloc_string(s))
                }))
            },
            move |vm, this_val, args| {
                let Some(eh) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                if !wa_s.borrow().effects.contains_key(&eh) {
                    return Err(vm.type_error("Illegal invocation"));
                }
                let v = arg(args, 0);
                let raw = if v.is_null() {
                    None
                } else {
                    Some(vm.to_js_string(v)?)
                };
                let validated = validate_pseudo_element(raw.as_deref())
                    .map_err(|e| create_dom_exception(vm, "SyntaxError", &e))?;
                let target = wa_s.borrow_mut().effects.get_mut(&eh).and_then(|e| {
                    e.pseudo_element = validated;
                    e.target
                });
                if let Some(t) = target {
                    sync_target_to_style_ctx(&h_s, &wa_s, t);
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // KeyframeEffect.prototype.composite
    {
        let h_s = Rc::clone(host);
        let wa_g = Rc::clone(wa);
        let wa_s = Rc::clone(wa);
        define_host_accessor(
            vm,
            kf_proto,
            "composite",
            move |vm, this_val, _| {
                let Some(eh) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                let c = wa_g
                    .borrow()
                    .effects
                    .get(&eh)
                    .map(|e| e.composite)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                Ok(JsValue::from_string(
                    vm.heap.alloc_string(composite_as_str(c)),
                ))
            },
            move |vm, this_val, args| {
                let Some(eh) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                if !wa_s.borrow().effects.contains_key(&eh) {
                    return Err(vm.type_error("Illegal invocation"));
                }
                let v = arg(args, 0);
                if v.is_null() {
                    return Err(vm.type_error("Invalid composite 'null'"));
                }
                let s = vm.to_js_string(v)?;
                let c = parse_composite_exact(&s)
                    .ok_or_else(|| vm.type_error(format!("Invalid composite '{s}'")))?;
                let target = wa_s.borrow_mut().effects.get_mut(&eh).and_then(|e| {
                    e.composite = c;
                    e.target
                });
                if let Some(t) = target {
                    sync_target_to_style_ctx(&h_s, &wa_s, t);
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // KeyframeEffect.prototype.iterationComposite
    {
        let h_s = Rc::clone(host);
        let wa_g = Rc::clone(wa);
        let wa_s = Rc::clone(wa);
        define_host_accessor(
            vm,
            kf_proto,
            "iterationComposite",
            move |vm, this_val, _| {
                let Some(eh) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                let c = wa_g
                    .borrow()
                    .effects
                    .get(&eh)
                    .map(|e| e.iteration_composite)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                Ok(JsValue::from_string(
                    vm.heap.alloc_string(composite_as_str(c)),
                ))
            },
            move |vm, this_val, args| {
                let Some(eh) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                if !wa_s.borrow().effects.contains_key(&eh) {
                    return Err(vm.type_error("Illegal invocation"));
                }
                let v = arg(args, 0);
                if v.is_null() {
                    return Err(vm.type_error("Invalid iterationComposite 'null'"));
                }
                let s = vm.to_js_string(v)?;
                let c = parse_iteration_composite_exact(&s)
                    .ok_or_else(|| vm.type_error(format!("Invalid iterationComposite '{s}'")))?;
                let target = wa_s.borrow_mut().effects.get_mut(&eh).and_then(|e| {
                    e.iteration_composite = c;
                    e.target
                });
                if let Some(t) = target {
                    sync_target_to_style_ctx(&h_s, &wa_s, t);
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // KeyframeEffect.prototype.getKeyframes()
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_method(vm, kf_proto, "getKeyframes", move |vm, this_val, _| {
            let Some(eh) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            crate::css_animations_api::flush_if_styles_dirty(vm, &h_r, &wa_r);
            let frames = wa_r
                .borrow()
                .effects
                .get(&eh)
                .map(|e| e.keyframes.clone())
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            let arr = vm.alloc_array();
            for kf in frames {
                let obj = vm.alloc_ordinary_object();
                let set = |vm: &mut JsVm, k: &str, v: JsValue| {
                    let pk = vm.heap.intern_key(k);
                    vm.heap
                        .object_define_data(obj, pk, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
                };
                set(
                    vm,
                    "offset",
                    kf.offset.map_or(JsValue::NULL, JsValue::from_f64),
                );
                set(vm, "computedOffset", JsValue::from_f64(kf.computed_offset));
                let es = vm.heap.alloc_string(&kf.easing_str);
                set(vm, "easing", JsValue::from_string(es));
                let cs = vm.heap.alloc_string(kf.composite.as_str());
                set(vm, "composite", JsValue::from_string(cs));
                for (js_k, _, val_s) in &kf.props {
                    let vs = vm.heap.alloc_string(val_s);
                    set(vm, js_k, JsValue::from_string(vs));
                }
                vm.heap.object_push_element(arr, JsValue::from_object(obj));
            }
            Ok(JsValue::from_object(arr))
        });
    }

    // KeyframeEffect.prototype.setKeyframes(keyframes)
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_method(vm, kf_proto, "setKeyframes", move |vm, this_val, args| {
            let Some(eh) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if !wa_r.borrow().effects.contains_key(&eh) {
                return Err(vm.type_error("Illegal invocation"));
            }
            let frames = process_keyframes_arg(vm, arg(args, 0))?;
            let (target, owner_anim) = {
                let mut sys = wa_r.borrow_mut();
                let (t, oa) = sys
                    .effects
                    .get_mut(&eh)
                    .map(|e| {
                        e.keyframes = frames;
                        e.css_default_timing = None;
                        (e.target, e.owner_anim)
                    })
                    .unwrap_or((None, None));
                if let Some(ah) = oa {
                    if let Some(a) = sys.animations.get_mut(&ah) {
                        match &mut a.origin {
                            crate::web_animations_core::AnimOrigin::CssTransition {
                                effect_replaced,
                                ..
                            } => {
                                *effect_replaced = true;
                            }
                            crate::web_animations_core::AnimOrigin::CssAnimation {
                                keyframes_overridden,
                                ..
                            } => {
                                *keyframes_overridden = true;
                            }
                            crate::web_animations_core::AnimOrigin::Script => {}
                        }
                    }
                }
                (t, oa)
            };
            if let Some(ah) = owner_anim {
                crate::css_animations_api::sync_css_transition_override(&h_r, &wa_r, ah);
            }
            if let Some(t) = target {
                sync_target_to_style_ctx(&h_r, &wa_r, t);
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    let eff_ctor = vm.alloc_host_fn("AnimationEffect", |vm, _, _| {
        Err(vm.type_error("Illegal constructor"))
    });
    bind_ctor_proto(vm, eff_ctor, eff_proto);
    define_global(vm, "AnimationEffect", JsValue::from_object(eff_ctor));

    // new KeyframeEffect(source) OR new KeyframeEffect(target, keyframes, options?)
    let h_ctor = Rc::clone(host);
    let wa_ctor = Rc::clone(wa);
    let kf_ctor = vm.alloc_host_fn("KeyframeEffect", move |vm, _, args| {
        if args.is_empty() {
            return Err(vm.type_error("KeyframeEffect constructor requires at least 1 argument"));
        }
        // Copy constructor when single arg is an existing KeyframeEffect!
        if args.len() == 1 {
            if let Some(src_h) = arg(args, 0).as_object() {
                let cloned_opt = wa_ctor.borrow().effects.get(&src_h).cloned();
                if let Some(mut cloned) = cloned_opt {
                    cloned.owner_anim = None;
                    let obj = vm
                        .heap
                        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(kf_proto)));
                    wa_ctor.borrow_mut().effects.insert(obj, cloned);
                    return Ok(JsValue::from_object(obj));
                }
            }
            return Err(vm.type_error(
                "KeyframeEffect constructor requires a KeyframeEffect source or (target, keyframes)",
            ));
        }

        let target = validate_target_arg(vm, &h_ctor, arg(args, 0))?;
        let keyframes = process_keyframes_arg(vm, arg(args, 1))?;
        let mut eff = EffectState::new_default(target);
        eff.keyframes = keyframes;
        apply_timing_dict(
            vm,
            arg(args, 2),
            &mut eff.timing,
            &mut eff.easing_str,
            Some((
                &mut eff.composite,
                &mut eff.iteration_composite,
                &mut eff.pseudo_element,
                None,
            )),
        )?;
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(kf_proto)));
        wa_ctor.borrow_mut().effects.insert(obj, eff);
        Ok(JsValue::from_object(obj))
    });
    bind_ctor_proto(vm, kf_ctor, kf_proto);
    vm.heap
        .object_set_prototype(kf_ctor, Some(eff_ctor), &mut vm.shapes);
    define_global(vm, "KeyframeEffect", JsValue::from_object(kf_ctor));
}

fn install_animation_iface(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    wa: &SharedWebAnimations,
    event_target_proto: HeapHandle,
) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(event_target_proto),
    ));
    define_to_string_tag(vm, proto, "Animation");
    wa.borrow_mut().protos.animation_proto = Some(proto);

    // onfinish / oncancel / onremove
    for attr in ["onfinish", "oncancel", "onremove"] {
        let wa_eh = Rc::clone(wa);
        install_object_event_handler_accessor(vm, proto, events, attr, move |vm, this_val| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if !wa_eh.borrow().animations.contains_key(&h) {
                return Err(vm.type_error("Illegal invocation"));
            }
            Ok(h)
        });
    }

    // id
    {
        let wa_g = Rc::clone(wa);
        let wa_s = Rc::clone(wa);
        define_host_accessor(
            vm,
            proto,
            "id",
            move |vm, this_val, _| {
                let Some(ah) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                let s = wa_g
                    .borrow()
                    .animations
                    .get(&ah)
                    .map(|a| a.id.clone())
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                Ok(JsValue::from_string(vm.heap.alloc_string(s)))
            },
            move |vm, this_val, args| {
                let Some(ah) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                let s = vm.to_js_string(arg(args, 0))?;
                let mut sys = wa_s.borrow_mut();
                let Some(a) = sys.animations.get_mut(&ah) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                a.id = s;
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // effect
    {
        let h_s = Rc::clone(host);
        let wa_g = Rc::clone(wa);
        let wa_s = Rc::clone(wa);
        define_host_accessor(
            vm,
            proto,
            "effect",
            move |vm, this_val, _| {
                let Some(ah) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                let eff = wa_g
                    .borrow()
                    .animations
                    .get(&ah)
                    .map(|a| a.effect)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                Ok(eff.map_or(JsValue::NULL, JsValue::from_object))
            },
            move |vm, this_val, args| {
                let Some(ah) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                if !wa_s.borrow().animations.contains_key(&ah) {
                    return Err(vm.type_error("Illegal invocation"));
                }
                let v = arg(args, 0);
                let new_eff = if v.is_null() || v.is_undefined() {
                    None
                } else if let Some(eh) = v.as_object() {
                    if !wa_s.borrow().effects.contains_key(&eh) {
                        return Err(vm.type_error("effect must be an AnimationEffect or null"));
                    }
                    Some(eh)
                } else {
                    return Err(vm.type_error("effect must be an AnimationEffect or null"));
                };
                let (old_target, new_target) = {
                    let mut sys = wa_s.borrow_mut();
                    let old_eff = sys.animations.get(&ah).and_then(|a| a.effect);
                    if old_eff == new_eff {
                        return Ok(JsValue::UNDEFINED);
                    }
                    let ot = old_eff.and_then(|eh| {
                        let e = sys.effects.get_mut(&eh)?;
                        if e.owner_anim == Some(ah) {
                            e.owner_anim = None;
                        }
                        e.target
                    });
                    let nt = new_eff.and_then(|eh| {
                        let prev = sys.effects.get_mut(&eh)?.owner_anim.replace(ah);
                        if let Some(prev_ah) = prev {
                            if let Some(pa) = sys.animations.get_mut(&prev_ah) {
                                pa.effect = None;
                            }
                        }
                        sys.effects.get(&eh).and_then(|e| e.target)
                    });
                    if let Some(a) = sys.animations.get_mut(&ah) {
                        a.effect = new_eff;
                        match &mut a.origin {
                            crate::web_animations_core::AnimOrigin::CssAnimation {
                                effect_overridden,
                                ..
                            } => {
                                *effect_overridden = true;
                            }
                            crate::web_animations_core::AnimOrigin::CssTransition {
                                effect_replaced,
                                ..
                            } => {
                                *effect_replaced = new_eff.is_some();
                            }
                            crate::web_animations_core::AnimOrigin::Script => {}
                        }
                    }
                    (ot, nt)
                };
                update_finished_state(vm, &h_s, &wa_s, ah, false, false);
                crate::css_animations_api::update_css_events_for_anim(
                    &mut wa_s.borrow_mut(),
                    &h_s,
                    ah,
                );
                crate::css_animations_api::sync_css_transition_override(&h_s, &wa_s, ah);
                if let Some(t) = old_target {
                    sync_target_to_style_ctx(&h_s, &wa_s, t);
                }
                if let Some(t) = new_target {
                    sync_target_to_style_ctx(&h_s, &wa_s, t);
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // timeline
    {
        let h_s = Rc::clone(host);
        let wa_g = Rc::clone(wa);
        let wa_s = Rc::clone(wa);
        define_host_accessor(
            vm,
            proto,
            "timeline",
            move |vm, this_val, _| {
                let Some(ah) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                let tl = wa_g
                    .borrow()
                    .animations
                    .get(&ah)
                    .map(|a| a.timeline)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                Ok(tl.map_or(JsValue::NULL, JsValue::from_object))
            },
            move |vm, this_val, args| {
                let Some(ah) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                if !wa_s.borrow().animations.contains_key(&ah) {
                    return Err(vm.type_error("Illegal invocation"));
                }
                let v = arg(args, 0);
                let new_tl = if v.is_null() || v.is_undefined() {
                    None
                } else if let Some(th) = v.as_object() {
                    if !wa_s.borrow().timelines.contains_key(&th) {
                        return Err(vm.type_error("timeline must be an AnimationTimeline or null"));
                    }
                    Some(th)
                } else {
                    return Err(vm.type_error("timeline must be an AnimationTimeline or null"));
                };
                {
                    let mut sys = wa_s.borrow_mut();
                    if let Some(anim) = sys.animations.get_mut(&ah) {
                        if let crate::web_animations_core::AnimOrigin::CssAnimation {
                            timeline_overridden,
                            ..
                        } = &mut anim.origin
                        {
                            *timeline_overridden = true;
                        }
                        if anim.timeline == new_tl {
                            return Ok(JsValue::UNDEFINED);
                        }
                        let was_running = anim.start_time.is_some();
                        anim.timeline = new_tl;
                        // W3C Web Animations §4.4.3 ("Setting the timeline of an animation"):
                        // Changing the timeline preserves `start_time` (never re-anchors it to
                        // keep `old_ct`); clear `hold_time` when started so the new timeline
                        // immediately drives `currentTime`!
                        if was_running && anim.pending_task != PendingTask::Pause {
                            anim.hold_time = None;
                        }
                    }
                }
                update_finished_state(vm, &h_s, &wa_s, ah, false, false);
                sync_anim_target(&h_s, &wa_s, ah);
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // startTime
    {
        let h_g = Rc::clone(host);
        let h_s = Rc::clone(host);
        let wa_g = Rc::clone(wa);
        let wa_s = Rc::clone(wa);
        define_host_accessor(
            vm,
            proto,
            "startTime",
            move |vm, this_val, _| {
                let Some(ah) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                crate::css_animations_api::flush_if_styles_dirty(vm, &h_g, &wa_g);
                let st = wa_g
                    .borrow()
                    .animations
                    .get(&ah)
                    .map(|a| a.start_time)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                Ok(st.map_or(JsValue::NULL, JsValue::from_f64))
            },
            move |vm, this_val, args| {
                let Some(ah) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                if !wa_s.borrow().animations.contains_key(&ah) {
                    return Err(vm.type_error("Illegal invocation"));
                }
                let v = arg(args, 0);
                let new_st = if v.is_null() {
                    None
                } else {
                    let n = parse_css_numberish_time_ms(vm, v)?;
                    if !n.is_finite() {
                        return Err(vm.type_error("startTime must be a finite number or null"));
                    }
                    Some(n)
                };
                let ready_to_resolve = {
                    let mut sys = wa_s.borrow_mut();
                    let prev_ct = sys
                        .animations
                        .get(&ah)
                        .and_then(|a| anim_current_time(&sys, &h_s, a));
                    let tl_t = sys
                        .animations
                        .get(&ah)
                        .and_then(|a| a.timeline)
                        .and_then(|th| sys.timeline_current_time(&h_s, th));
                    let Some(anim) = sys.animations.get_mut(&ah) else {
                        return Ok(JsValue::UNDEFINED);
                    };
                    if let Some(pr) = anim.pending_playback_rate.take() {
                        anim.playback_rate = pr;
                    }
                    anim.start_time = new_st;
                    if new_st.is_some() {
                        if anim.playback_rate != 0.0 {
                            anim.hold_time = None;
                        } else if anim.hold_time.is_none() {
                            anim.hold_time = prev_ct.or(Some(0.0));
                        }
                    } else {
                        anim.hold_time = prev_ct;
                    }
                    let was_pending = anim.pending_task != PendingTask::None;
                    anim.pending_task = PendingTask::None;
                    if was_pending && !anim.ready_settled && (new_st.is_none() || tl_t.is_some()) {
                        anim.ready_settled = true;
                        Some(anim.ready_promise)
                    } else {
                        None
                    }
                };
                if let Some(rp) = ready_to_resolve {
                    resolve_promise(vm, rp, JsValue::from_object(ah));
                }
                update_finished_state(vm, &h_s, &wa_s, ah, true, false);
                crate::css_animations_api::update_css_events_for_anim(
                    &mut wa_s.borrow_mut(),
                    &h_s,
                    ah,
                );
                sync_anim_target(&h_s, &wa_s, ah);
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // currentTime
    {
        let h_g = Rc::clone(host);
        let h_s = Rc::clone(host);
        let wa_g = Rc::clone(wa);
        let wa_s = Rc::clone(wa);
        define_host_accessor(
            vm,
            proto,
            "currentTime",
            move |vm, this_val, _| {
                let Some(ah) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                crate::css_animations_api::flush_if_styles_dirty(vm, &h_g, &wa_g);
                let sys = wa_g.borrow();
                let Some(a) = sys.animations.get(&ah) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                Ok(anim_current_time(&sys, &h_g, a).map_or(JsValue::NULL, JsValue::from_f64))
            },
            move |vm, this_val, args| {
                let Some(ah) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                if !wa_s.borrow().animations.contains_key(&ah) {
                    return Err(vm.type_error("Illegal invocation"));
                }
                let v = arg(args, 0);
                if v.is_null() {
                    let has_ct = {
                        let sys = wa_s.borrow();
                        sys.animations
                            .get(&ah)
                            .and_then(|a| anim_current_time(&sys, &h_s, a))
                            .is_some()
                    };
                    if has_ct {
                        return Err(vm.type_error(
                            "Cannot set currentTime to null while animation has a current time",
                        ));
                    }
                    return Ok(JsValue::UNDEFINED);
                }
                let seek_time = parse_css_numberish_time_ms(vm, v)?;
                if !seek_time.is_finite() {
                    return Err(vm.type_error("currentTime must be a finite number"));
                }
                let ready_to_resolve = {
                    let mut sys = wa_s.borrow_mut();
                    let tl_t = sys
                        .animations
                        .get(&ah)
                        .and_then(|a| a.timeline)
                        .and_then(|th| sys.timeline_current_time(&h_s, th));
                    if let Some(anim) = sys.animations.get_mut(&ah) {
                        let mut resolve_ready = None;
                        if anim.pending_task == PendingTask::Pause {
                            if let Some(pr) = anim.pending_playback_rate.take() {
                                anim.playback_rate = pr;
                            }
                            anim.hold_time = Some(seek_time);
                            anim.start_time = None;
                            anim.pending_task = PendingTask::None;
                            if !anim.ready_settled {
                                anim.ready_settled = true;
                                resolve_ready = Some(anim.ready_promise);
                            }
                        } else if anim.hold_time.is_some()
                            || anim.start_time.is_none()
                            || tl_t.is_none()
                            || anim.playback_rate == 0.0
                        {
                            anim.hold_time = Some(seek_time);
                        } else if let Some(tt) = tl_t {
                            anim.start_time = Some(tt - seek_time / anim.playback_rate);
                        }
                        if tl_t.is_none() {
                            anim.start_time = None;
                        }
                        if anim.replace_state == ReplaceState::Removed {
                            anim.replace_state = ReplaceState::Active;
                        }
                        resolve_ready
                    } else {
                        None
                    }
                };
                if let Some(rp) = ready_to_resolve {
                    resolve_promise(vm, rp, JsValue::from_object(ah));
                }
                update_finished_state(vm, &h_s, &wa_s, ah, true, false);
                crate::css_animations_api::update_css_events_for_anim(
                    &mut wa_s.borrow_mut(),
                    &h_s,
                    ah,
                );
                sync_anim_target(&h_s, &wa_s, ah);
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // playbackRate
    {
        let h_s = Rc::clone(host);
        let wa_g = Rc::clone(wa);
        let wa_s = Rc::clone(wa);
        define_host_accessor(
            vm,
            proto,
            "playbackRate",
            move |vm, this_val, _| {
                let Some(ah) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                let r = wa_g
                    .borrow()
                    .animations
                    .get(&ah)
                    .map(|a| a.playback_rate)
                    .ok_or_else(|| vm.type_error("Illegal invocation"))?;
                Ok(JsValue::from_f64(r))
            },
            move |vm, this_val, args| {
                let Some(ah) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                if !wa_s.borrow().animations.contains_key(&ah) {
                    return Err(vm.type_error("Illegal invocation"));
                }
                let rate = vm.to_number(arg(args, 0))?;
                if !rate.is_finite() {
                    return Err(vm.type_error("playbackRate must be a finite number"));
                }
                {
                    let mut sys = wa_s.borrow_mut();
                    let prev_ct = sys
                        .animations
                        .get(&ah)
                        .and_then(|a| anim_current_time(&sys, &h_s, a));
                    let tl_t = sys
                        .animations
                        .get(&ah)
                        .and_then(|a| a.timeline)
                        .and_then(|th| sys.timeline_current_time(&h_s, th));
                    if let Some(anim) = sys.animations.get_mut(&ah) {
                        anim.pending_playback_rate = None;
                        anim.playback_rate = rate;
                        if let Some(ct) = prev_ct {
                            if anim.hold_time.is_some()
                                || anim.start_time.is_none()
                                || tl_t.is_none()
                                || rate == 0.0
                            {
                                anim.hold_time = Some(ct);
                            } else if let Some(tt) = tl_t {
                                anim.start_time = Some(tt - ct / rate);
                                anim.hold_time = None;
                            }
                        }
                    }
                }
                update_finished_state(vm, &h_s, &wa_s, ah, true, false);
                sync_anim_target(&h_s, &wa_s, ah);
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // Readonly getters: playState, replaceState, pending, ready, finished
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_getter(vm, proto, "playState", move |vm, this_val, _| {
            let Some(ah) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            crate::css_animations_api::flush_if_styles_dirty(vm, &h_r, &wa_r);
            let sys = wa_r.borrow();
            let Some(a) = sys.animations.get(&ah) else {
                return Err(vm.type_error("Illegal invocation"));
            };
            Ok(JsValue::from_string(
                vm.heap.alloc_string(anim_play_state(&sys, &h_r, a)),
            ))
        });
    }
    {
        let wa_r = Rc::clone(wa);
        define_host_getter(vm, proto, "replaceState", move |vm, this_val, _| {
            let Some(ah) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let rs = wa_r
                .borrow()
                .animations
                .get(&ah)
                .map(|a| a.replace_state)
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            Ok(JsValue::from_string(vm.heap.alloc_string(rs.as_str())))
        });
    }
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_getter(vm, proto, "pending", move |vm, this_val, _| {
            let Some(ah) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            crate::css_animations_api::flush_if_styles_dirty(vm, &h_r, &wa_r);
            let p = wa_r
                .borrow()
                .animations
                .get(&ah)
                .map(|a| a.pending_task != PendingTask::None)
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            Ok(JsValue::from_bool(p))
        });
    }
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_getter(vm, proto, "ready", move |vm, this_val, _| {
            let Some(ah) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            crate::css_animations_api::flush_if_styles_dirty(vm, &h_r, &wa_r);
            let p = wa_r
                .borrow()
                .animations
                .get(&ah)
                .map(|a| a.ready_promise)
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            Ok(JsValue::from_object(p))
        });
    }
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_getter(vm, proto, "finished", move |vm, this_val, _| {
            let Some(ah) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            crate::css_animations_api::flush_if_styles_dirty(vm, &h_r, &wa_r);
            let p = wa_r
                .borrow()
                .animations
                .get(&ah)
                .map(|a| a.finished_promise)
                .ok_or_else(|| vm.type_error("Illegal invocation"))?;
            Ok(JsValue::from_object(p))
        });
    }

    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_getter(vm, proto, "overallProgress", move |vm, this_val, _| {
            let Some(ah) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let sys = wa_r.borrow();
            let Some(a) = sys.animations.get(&ah) else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if a.effect.is_none() {
                return Ok(JsValue::NULL);
            }
            let Some(ct) = anim_current_time(&sys, &h_r, a) else {
                return Ok(JsValue::NULL);
            };
            let end = anim_effect_end(&sys, a);
            let prog = if end == 0.0 {
                if ct < 0.0 {
                    0.0
                } else {
                    1.0
                }
            } else if end.is_infinite() {
                0.0
            } else {
                (ct / end).clamp(0.0, 1.0)
            };
            Ok(JsValue::from_f64(prog))
        });
    }

    // Methods: play, pause, finish, cancel, reverse, updatePlaybackRate, persist, commitStyles
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_method(vm, proto, "play", move |vm, this_val, _| {
            let Some(ah) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if !wa_r.borrow().animations.contains_key(&ah) {
                return Err(vm.type_error("Illegal invocation"));
            }
            play_animation(vm, &h_r, &wa_r, ah, true)?;
            if let Some(a) = wa_r.borrow_mut().animations.get_mut(&ah) {
                if let crate::web_animations_core::AnimOrigin::CssAnimation {
                    play_state_overridden,
                    ..
                } = &mut a.origin
                {
                    *play_state_overridden = true;
                }
            }
            crate::css_animations_api::update_css_events_for_anim(&mut wa_r.borrow_mut(), &h_r, ah);
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_method(vm, proto, "pause", move |vm, this_val, _| {
            let Some(ah) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if !wa_r.borrow().animations.contains_key(&ah) {
                return Err(vm.type_error("Illegal invocation"));
            }
            pause_animation(vm, &h_r, &wa_r, ah)?;
            if let Some(a) = wa_r.borrow_mut().animations.get_mut(&ah) {
                if let crate::web_animations_core::AnimOrigin::CssAnimation {
                    play_state_overridden,
                    ..
                } = &mut a.origin
                {
                    *play_state_overridden = true;
                }
            }
            crate::css_animations_api::update_css_events_for_anim(&mut wa_r.borrow_mut(), &h_r, ah);
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_method(vm, proto, "finish", move |vm, this_val, _| {
            let Some(ah) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if !wa_r.borrow().animations.contains_key(&ah) {
                return Err(vm.type_error("Illegal invocation"));
            }
            finish_animation(vm, &h_r, &wa_r, ah)?;
            crate::css_animations_api::update_css_events_for_anim(&mut wa_r.borrow_mut(), &h_r, ah);
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_method(vm, proto, "cancel", move |vm, this_val, _| {
            let Some(ah) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if !wa_r.borrow().animations.contains_key(&ah) {
                return Err(vm.type_error("Illegal invocation"));
            }
            cancel_animation(vm, &h_r, &wa_r, ah);
            crate::css_animations_api::update_css_events_for_anim(&mut wa_r.borrow_mut(), &h_r, ah);
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_method(vm, proto, "reverse", move |vm, this_val, _| {
            let Some(ah) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            if !wa_r.borrow().animations.contains_key(&ah) {
                return Err(vm.type_error("Illegal invocation"));
            }
            reverse_animation(vm, &h_r, &wa_r, ah)?;
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_method(
            vm,
            proto,
            "updatePlaybackRate",
            move |vm, this_val, args| {
                let Some(ah) = this_val.as_object() else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                if !wa_r.borrow().animations.contains_key(&ah) {
                    return Err(vm.type_error("Illegal invocation"));
                }
                let rate = vm.to_number(arg(args, 0))?;
                if !rate.is_finite() {
                    return Err(vm.type_error("playbackRate must be a finite number"));
                }
                let ps = {
                    let mut sys = wa_r.borrow_mut();
                    let ps = anim_play_state(&sys, &h_r, sys.animations.get(&ah).unwrap());
                    if let Some(a) = sys.animations.get_mut(&ah) {
                        a.pending_playback_rate = Some(rate);
                    }
                    ps
                };
                match ps {
                    "idle" | "paused" => {
                        if let Some(a) = wa_r.borrow_mut().animations.get_mut(&ah) {
                            a.playback_rate = rate;
                            a.pending_playback_rate = None;
                        }
                    }
                    "finished" => {
                        let tl_t = {
                            let sys = wa_r.borrow();
                            sys.animations
                                .get(&ah)
                                .and_then(|a| a.timeline)
                                .and_then(|th| sys.timeline_current_time(&h_r, th))
                        };
                        if let Some(a) = wa_r.borrow_mut().animations.get_mut(&ah) {
                            let ct = a.hold_time.unwrap_or(0.0);
                            a.playback_rate = rate;
                            a.pending_playback_rate = None;
                            if rate != 0.0 {
                                if let Some(tt) = tl_t {
                                    a.start_time = Some(tt - ct / rate);
                                }
                            }
                        }
                        update_finished_state(vm, &h_r, &wa_r, ah, false, false);
                    }
                    _ => {
                        play_animation(vm, &h_r, &wa_r, ah, false)?;
                    }
                }
                sync_anim_target(&h_r, &wa_r, ah);
                Ok(JsValue::UNDEFINED)
            },
        );
    }
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_method(vm, proto, "persist", move |vm, this_val, _| {
            let Some(ah) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let mut sys = wa_r.borrow_mut();
            let Some(a) = sys.animations.get_mut(&ah) else {
                return Err(vm.type_error("Illegal invocation"));
            };
            a.replace_state = ReplaceState::Persisted;
            drop(sys);
            sync_anim_target(&h_r, &wa_r, ah);
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_method(vm, proto, "commitStyles", move |vm, this_val, _| {
            let Some(ah) = this_val.as_object() else {
                return Err(vm.type_error("Illegal invocation"));
            };
            let (seq_id, target, pseudo, css_props) = {
                let sys = wa_r.borrow();
                let Some(a) = sys.animations.get(&ah) else {
                    return Err(vm.type_error("Illegal invocation"));
                };
                let Some(eh) = a.effect else {
                    return Ok(JsValue::UNDEFINED);
                };
                let Some(eff) = sys.effects.get(&eh) else {
                    return Ok(JsValue::UNDEFINED);
                };
                let mut props: Vec<String> = Vec::new();
                for kf in &eff.keyframes {
                    for (_, cp, _) in &kf.props {
                        if !props.contains(cp) {
                            props.push(cp.clone());
                        }
                    }
                }
                (a.seq_id, eff.target, eff.pseudo_element.clone(), props)
            };
            let Some(nid) = target else {
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Cannot commitStyles() on an effect with no target",
                ));
            };
            // W3C §4.4.17: check pseudo-element & ElementCSSInlineStyle BEFORE rendered check!
            if pseudo.is_some() {
                return Err(create_dom_exception(
                    vm,
                    "NoModificationAllowedError",
                    "Cannot commitStyles() on a pseudo-element target",
                ));
            }
            let supports_inline_style = {
                let hb = h_r.borrow();
                let doc = hb.document.borrow();
                match doc.get(nid) {
                    Some(NodeData::Element(e)) => {
                        let ns: &str = e.name.ns.as_ref();
                        ns.is_empty()
                            || ns == "http://www.w3.org/1999/xhtml"
                            || ns == "http://www.w3.org/2000/svg"
                            || ns == "http://www.w3.org/1998/Math/MathML"
                    }
                    _ => false,
                }
            };
            if !supports_inline_style {
                return Err(create_dom_exception(
                    vm,
                    "NoModificationAllowedError",
                    "Target element does not support inline styles",
                ));
            }
            let is_connected = h_r.borrow().document.borrow().is_connected(nid);
            if !is_connected {
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Target element is not rendered",
                ));
            }
            // Check target and all ancestors for `display: none` (`display: contents` is rendered!).
            let is_hidden_by_display_none = {
                let mut hb = h_r.borrow_mut();
                let _ = hb.ensure_styles();
                let mut cur = Some(nid);
                let mut hidden = false;
                while let Some(c) = cur {
                    if let Some(st) = hb.computed_style_for_node(c) {
                        if st.display == rix_css::DisplayMode::None {
                            hidden = true;
                            break;
                        }
                    }
                    cur = hb.document.borrow().parent(c);
                }
                hidden
            };
            if is_hidden_by_display_none {
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Target element is not rendered",
                ));
            }
            let (style, pfg) = {
                let mut hb = h_r.borrow_mut();
                (
                    hb.compute_style_for_web_anim_commit(nid, seq_id),
                    hb.parent_is_flex_or_grid(nid),
                )
            };
            let Some(style) = style else {
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Target element is not rendered",
                ));
            };
            let ch_px = Some(h_r.borrow_mut().ch_advance_px(&style));
            let mut existing = h_r
                .borrow()
                .document
                .borrow()
                .get(nid)
                .and_then(|d| match d {
                    NodeData::Element(e) => e.get_attribute("style").map(str::to_string),
                    _ => None,
                })
                .unwrap_or_default();
            for prop in css_props {
                // Resolve logical properties to physical properties per W3C §4.4.17.
                let phys_prop = match rix_css::PropertyIdKind::from_name(&prop) {
                    Some(pk) if rix_css::is_logical_property(pk) => {
                        rix_css::resolve_logical_to_physical(
                            pk,
                            style.text.writing_mode,
                            style.text.direction,
                        )
                        .css_name()
                        .to_string()
                    }
                    _ => prop.clone(),
                };
                let val_str = if phys_prop == "line-height" {
                    if let rix_css::LineHeightValue::Number(n) = style.text.line_height {
                        rix_css::values_math::format_math_f64(f64::from(n))
                    } else {
                        format_computed_property_resolved(&style, &phys_prop, None, pfg, ch_px)
                    }
                } else {
                    format_computed_property_resolved(&style, &phys_prop, None, pfg, ch_px)
                };
                if !val_str.is_empty() {
                    if !existing.is_empty() && !existing.trim_end().ends_with(';') {
                        existing.push(';');
                    }
                    existing.push_str(&format!(" {phys_prop}: {val_str};"));
                }
            }
            set_inline_style_css_text(vm, &h_r, nid, existing.trim());
            Ok(JsValue::UNDEFINED)
        });
    }

    // new Animation(effect?, timeline?)
    let h_ctor = Rc::clone(host);
    let wa_ctor = Rc::clone(wa);
    let ctor = vm.alloc_host_fn("Animation", move |vm, _, args| {
        let eff_v = arg(args, 0);
        let effect = if eff_v.is_undefined() || eff_v.is_null() {
            None
        } else if let Some(eh) = eff_v.as_object() {
            if !wa_ctor.borrow().effects.contains_key(&eh) {
                return Err(vm.type_error("Animation effect must be an AnimationEffect or null"));
            }
            Some(eh)
        } else {
            return Err(vm.type_error("Animation effect must be an AnimationEffect or null"));
        };

        let tl_v = arg(args, 1);
        let timeline = if tl_v.is_undefined() {
            let main_root = h_ctor.borrow().document.borrow().root();
            Some(get_or_create_doc_timeline(vm, &h_ctor, &wa_ctor, main_root))
        } else if tl_v.is_null() {
            None
        } else if let Some(th) = tl_v.as_object() {
            if !wa_ctor.borrow().timelines.contains_key(&th) {
                return Err(
                    vm.type_error("Animation timeline must be an AnimationTimeline or null")
                );
            }
            Some(th)
        } else {
            return Err(vm.type_error("Animation timeline must be an AnimationTimeline or null"));
        };

        let anim_h = alloc_animation_object(vm, &wa_ctor, effect, timeline, String::new());
        sync_anim_target(&h_ctor, &wa_ctor, anim_h);
        Ok(JsValue::from_object(anim_h))
    });
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "Animation", JsValue::from_object(ctor));
}

fn install_animatable_and_get_animations(
    vm: &mut JsVm,
    host: &SharedHost,
    wa: &SharedWebAnimations,
) {
    let (elem_proto, doc_proto, sr_proto) = {
        let p = &host.borrow().prototypes;
        (p.element_proto, p.document_proto, p.shadow_root_proto)
    };

    // Element.prototype.animate(keyframes, options?)
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_method(vm, elem_proto, "animate", move |vm, this_val, args| {
            let node = require_node(vm, &h_r, this_val)?;
            let keyframes = process_keyframes_arg(vm, arg(args, 0))?;
            let mut eff = EffectState::new_default(Some(node));
            eff.keyframes = keyframes;
            let mut anim_id_str = String::new();
            apply_timing_dict(
                vm,
                arg(args, 1),
                &mut eff.timing,
                &mut eff.easing_str,
                Some((
                    &mut eff.composite,
                    &mut eff.iteration_composite,
                    &mut eff.pseudo_element,
                    Some(&mut anim_id_str),
                )),
            )?;
            let kf_proto = wa_r
                .borrow()
                .protos
                .keyframe_effect_proto
                .unwrap_or(vm.object_proto);
            let eff_h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(kf_proto)));
            wa_r.borrow_mut().effects.insert(eff_h, eff);

            let owner_doc = h_r.borrow().owner_doc_of(node);
            let tl_h = get_or_create_doc_timeline(vm, &h_r, &wa_r, owner_doc);
            let anim_h = alloc_animation_object(vm, &wa_r, Some(eff_h), Some(tl_h), anim_id_str);
            play_animation(vm, &h_r, &wa_r, anim_h, true)?;
            Ok(JsValue::from_object(anim_h))
        });
    }

    // Element.prototype.getAnimations(options?)
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_method(
            vm,
            elem_proto,
            "getAnimations",
            move |vm, this_val, args| {
                let node = require_node(vm, &h_r, this_val)?;
                let mut subtree = false;
                let mut pseudo_filter: Option<String> = None;
                if let Some(oh) = arg(args, 0).as_object() {
                    let ks = vm.heap.intern_key("subtree");
                    let vs = vm.get_property_by_key(oh, ks)?;
                    subtree = vm.to_boolean(vs);
                    let kp = vm.heap.intern_key("pseudoElement");
                    let vp = vm.get_property_by_key(oh, kp)?;
                    if !vp.is_undefined() && !vp.is_null() {
                        let s = vm.to_js_string(vp)?;
                        pseudo_filter = validate_pseudo_element(Some(&s))
                            .map_err(|e| create_dom_exception(vm, "SyntaxError", &e))?;
                    }
                }
                crate::css_animations_api::flush_and_sync_css_animations(vm, &h_r, &wa_r);
                let matching = collect_animations_for_scope(
                    &h_r,
                    &wa_r,
                    Some(node),
                    subtree,
                    pseudo_filter.as_deref(),
                    None,
                );
                let arr = vm.alloc_array();
                for ah in matching {
                    vm.heap.object_push_element(arr, JsValue::from_object(ah));
                }
                Ok(JsValue::from_object(arr))
            },
        );
    }

    // Document.prototype.getAnimations()
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_method(vm, doc_proto, "getAnimations", move |vm, this_val, _| {
            let doc_node = require_node(vm, &h_r, this_val)?;
            crate::css_animations_api::flush_and_sync_css_animations(vm, &h_r, &wa_r);
            let matching =
                collect_animations_for_scope(&h_r, &wa_r, None, true, None, Some(doc_node));
            let arr = vm.alloc_array();
            for ah in matching {
                vm.heap.object_push_element(arr, JsValue::from_object(ah));
            }
            Ok(JsValue::from_object(arr))
        });
    }

    // ShadowRoot.prototype.getAnimations()
    {
        let h_r = Rc::clone(host);
        let wa_r = Rc::clone(wa);
        define_host_method(vm, sr_proto, "getAnimations", move |vm, this_val, _| {
            let sr_node = require_node(vm, &h_r, this_val)?;
            crate::css_animations_api::flush_and_sync_css_animations(vm, &h_r, &wa_r);
            let matching =
                collect_animations_for_scope(&h_r, &wa_r, Some(sr_node), true, None, None);
            let arr = vm.alloc_array();
            for ah in matching {
                vm.heap.object_push_element(arr, JsValue::from_object(ah));
            }
            Ok(JsValue::from_object(arr))
        });
    }
}

//! Complete W3C Web Animations Level 1 runtime (`AnimationTimeline`, `DocumentTimeline`,
//! `AnimationPlaybackEvent`, `AnimationEffect`, `KeyframeEffect`, `Animation`,
//! `Element.prototype.animate`, `Element/Document/ShadowRoot.prototype.getAnimations`).
//!
//! Reference: https://drafts.csswg.org/web-animations-1/

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use rix_css::{
    compute_effect_timing, AnimationCompositionMode, AnimationPlayStateMode, EffectPhase,
    EffectTimingInput, TimingFunction, WebAnimationKeyframe, WebAnimationSpec,
};
use rix_dom::{NodeData, NodeId};
use rix_js::promise::{alloc_promise, reject_promise, resolve_promise};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};

use crate::dom_bridge::{require_node, SharedHost};
use crate::event_loop::{run_microtask_checkpoint, SharedEventLoop};
use crate::events::{fire_object_event_with_proto, SharedEvents};
use crate::web_animations_timing::{
    compute_missing_keyframe_offsets, css_prop_to_js_keyframe, direction_as_str, fill_mode_as_str,
    is_supported_animatable_css_prop, js_keyframe_prop_to_css, parse_composite_exact,
    parse_direction_exact, parse_easing_strict, parse_fill_mode_exact,
    parse_iteration_composite_exact, parse_keyframe_css_prop, validate_keyframe_offsets,
    validate_pseudo_element, KeyframeComposite, ProcessedKeyframe,
};
use crate::web_apis::create_dom_exception;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingTask {
    None,
    Play,
    Pause,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplaceState {
    Active,
    Removed,
    Persisted,
}

impl ReplaceState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Removed => "removed",
            Self::Persisted => "persisted",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct TimelineState {
    pub(crate) doc_node: NodeId,
    pub(crate) origin_time: f64,
}

#[derive(Debug, Clone)]
pub(crate) struct EffectState {
    pub(crate) target: Option<NodeId>,
    pub(crate) pseudo_element: Option<String>,
    pub(crate) timing: EffectTimingInput,
    pub(crate) easing_str: String,
    pub(crate) composite: AnimationCompositionMode,
    pub(crate) iteration_composite: AnimationCompositionMode,
    pub(crate) keyframes: Vec<ProcessedKeyframe>,
    pub(crate) owner_anim: Option<HeapHandle>,
}

impl EffectState {
    pub(crate) fn new_default(target: Option<NodeId>) -> Self {
        Self {
            target,
            pseudo_element: None,
            timing: EffectTimingInput::default(),
            easing_str: "linear".to_string(),
            composite: AnimationCompositionMode::Replace,
            iteration_composite: AnimationCompositionMode::Replace,
            keyframes: Vec::new(),
            owner_anim: None,
        }
    }

    pub(crate) fn end_time(&self) -> f64 {
        compute_effect_timing(&self.timing, None, 1.0).end_time
    }
}

pub(crate) struct AnimState {
    pub(crate) seq_id: u64,
    pub(crate) id: String,
    pub(crate) effect: Option<HeapHandle>,
    pub(crate) timeline: Option<HeapHandle>,
    pub(crate) start_time: Option<f64>,
    pub(crate) hold_time: Option<f64>,
    pub(crate) playback_rate: f64,
    pub(crate) pending_playback_rate: Option<f64>,
    pub(crate) pending_task: PendingTask,
    pub(crate) replace_state: ReplaceState,
    pub(crate) ready_promise: HeapHandle,
    pub(crate) ready_settled: bool,
    pub(crate) finished_promise: HeapHandle,
    pub(crate) finished_settled: bool,
}

pub(crate) struct QueuedPlaybackEvent {
    anim_h: HeapHandle,
    event_type: &'static str,
    current_time: Option<f64>,
    timeline_time: Option<f64>,
    scheduled_time: f64,
    seq_id: u64,
}

#[derive(Default)]
pub(crate) struct WebAnimProtos {
    pub(crate) timeline_proto: Option<HeapHandle>,
    pub(crate) doc_timeline_proto: Option<HeapHandle>,
    pub(crate) playback_event_proto: Option<HeapHandle>,
    pub(crate) effect_proto: Option<HeapHandle>,
    pub(crate) keyframe_effect_proto: Option<HeapHandle>,
    pub(crate) animation_proto: Option<HeapHandle>,
}

/// Content-runtime registry of Web Animations timelines, effects, and animations.
pub struct WebAnimationSystem {
    pub(crate) timelines: HashMap<HeapHandle, TimelineState>,
    pub(crate) doc_timelines: HashMap<NodeId, HeapHandle>,
    pub(crate) doc_creation_time_ms: HashMap<NodeId, f64>,
    pub(crate) effects: HashMap<HeapHandle, EffectState>,
    pub(crate) animations: HashMap<HeapHandle, AnimState>,
    pub(crate) queued_events: Vec<QueuedPlaybackEvent>,
    pub(crate) frame_time_ms: f64,
    pub(crate) next_seq_id: u64,
    pub(crate) protos: WebAnimProtos,
}

/// Shared handle to [`WebAnimationSystem`].
pub type SharedWebAnimations = Rc<RefCell<WebAnimationSystem>>;

impl Default for WebAnimationSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl WebAnimationSystem {
    #[must_use]
    pub fn new() -> Self {
        Self {
            timelines: HashMap::new(),
            doc_timelines: HashMap::new(),
            doc_creation_time_ms: HashMap::new(),
            effects: HashMap::new(),
            animations: HashMap::new(),
            queued_events: Vec::new(),
            frame_time_ms: 0.0,
            next_seq_id: 1,
            protos: WebAnimProtos::default(),
        }
    }

    pub(crate) fn alloc_seq_id(&mut self) -> u64 {
        let id = self.next_seq_id;
        self.next_seq_id = self.next_seq_id.wrapping_add(1).max(1);
        id
    }

    /// Returns current document timeline time for `tl_h`, or `None` if inactive.
    pub(crate) fn timeline_current_time(&self, host: &SharedHost, tl_h: HeapHandle) -> Option<f64> {
        let tl = self.timelines.get(&tl_h)?;
        if !doc_has_browsing_context(host, tl.doc_node) {
            return None;
        }
        let base = self
            .doc_creation_time_ms
            .get(&tl.doc_node)
            .copied()
            .unwrap_or(0.0);
        let doc_time = (self.frame_time_ms - base).max(0.0);
        Some(doc_time - tl.origin_time)
    }

    /// Collects GC roots for prototypes, document timelines, active animations, effects, and promises.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for p in [
            self.protos.timeline_proto,
            self.protos.doc_timeline_proto,
            self.protos.playback_event_proto,
            self.protos.effect_proto,
            self.protos.keyframe_effect_proto,
            self.protos.animation_proto,
        ]
        .into_iter()
        .flatten()
        {
            out.push(JsValue::from_object(p));
        }
        for &tl_h in self.doc_timelines.values() {
            out.push(JsValue::from_object(tl_h));
        }
        for (&ah, anim) in &self.animations {
            let keep = anim.pending_task != PendingTask::None
                || anim.start_time.is_some()
                || anim.hold_time.is_some();
            if keep {
                out.push(JsValue::from_object(ah));
            }
            if let Some(eh) = anim.effect {
                out.push(JsValue::from_object(eh));
            }
            if let Some(th) = anim.timeline {
                out.push(JsValue::from_object(th));
            }
            out.push(JsValue::from_object(anim.ready_promise));
            out.push(JsValue::from_object(anim.finished_promise));
        }
        for ev in &self.queued_events {
            out.push(JsValue::from_object(ev.anim_h));
        }
    }

    /// Sweeps dead JS wrapper handles after Major GC.
    pub fn sweep_dead(&mut self, heap: &rix_js::GcHeap) {
        self.timelines
            .retain(|h, _| heap.is_alive(*h) || self.doc_timelines.values().any(|v| v == h));
        self.effects.retain(|h, _| heap.is_alive(*h));
        self.animations.retain(|h, _| heap.is_alive(*h));
    }
}

pub(crate) fn doc_has_browsing_context(host: &SharedHost, doc_id: NodeId) -> bool {
    let hb = host.borrow();
    let main_root = hb.document.borrow().root();
    if doc_id == main_root {
        return true;
    }
    hb.doc_meta
        .get(&doc_id)
        .and_then(|m| m.default_view)
        .is_some()
}

pub(crate) fn get_or_create_doc_timeline(
    vm: &mut JsVm,
    host: &SharedHost,
    wa: &SharedWebAnimations,
    doc_node: NodeId,
) -> HeapHandle {
    if let Some(&h) = wa.borrow().doc_timelines.get(&doc_node) {
        return h;
    }
    let proto = wa
        .borrow()
        .protos
        .doc_timeline_proto
        .unwrap_or(vm.object_proto);
    let h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let mut sys = wa.borrow_mut();
    let now = sys.frame_time_ms;
    let main_root = host.borrow().document.borrow().root();
    sys.doc_creation_time_ms
        .entry(doc_node)
        .or_insert(if doc_node == main_root { 0.0 } else { now });
    sys.timelines.insert(
        h,
        TimelineState {
            doc_node,
            origin_time: 0.0,
        },
    );
    sys.doc_timelines.insert(doc_node, h);
    h
}

pub(crate) fn anim_current_time(
    sys: &WebAnimationSystem,
    host: &SharedHost,
    anim: &AnimState,
) -> Option<f64> {
    if let Some(ht) = anim.hold_time {
        return Some(ht);
    }
    let st = anim.start_time?;
    let tl_t = anim
        .timeline
        .and_then(|th| sys.timeline_current_time(host, th))?;
    Some((tl_t - st) * anim.playback_rate)
}

pub(crate) fn anim_effect_end(sys: &WebAnimationSystem, anim: &AnimState) -> f64 {
    anim.effect
        .and_then(|eh| sys.effects.get(&eh))
        .map_or(0.0, EffectState::end_time)
}

pub(crate) fn anim_play_state(
    sys: &WebAnimationSystem,
    host: &SharedHost,
    anim: &AnimState,
) -> &'static str {
    let ct = anim_current_time(sys, host, anim);
    if ct.is_none() && anim.start_time.is_none() && anim.pending_task == PendingTask::None {
        return "idle";
    }
    if anim.pending_task == PendingTask::Pause
        || (anim.start_time.is_none() && anim.pending_task != PendingTask::Play)
    {
        return "paused";
    }
    if let Some(t) = ct {
        let eff_end = anim_effect_end(sys, anim);
        if (anim.playback_rate > 0.0 && t >= eff_end) || (anim.playback_rate < 0.0 && t <= 0.0) {
            return "finished";
        }
    }
    "running"
}

pub(crate) fn anim_is_relevant(
    sys: &WebAnimationSystem,
    host: &SharedHost,
    anim: &AnimState,
) -> bool {
    if anim.replace_state == ReplaceState::Removed {
        return false;
    }
    let Some(eh) = anim.effect else {
        return false;
    };
    let Some(eff) = sys.effects.get(&eh) else {
        return false;
    };
    let ct = anim_current_time(sys, host, anim);
    let comp = compute_effect_timing(&eff.timing, ct, anim.playback_rate);
    // W3C §4.4.18: an animation is relevant if its associated effect is current OR in effect.
    let in_effect = comp.active_time.is_some();
    let is_current = match comp.phase {
        EffectPhase::Active => true,
        EffectPhase::Before => anim.playback_rate >= 0.0,
        EffectPhase::After => anim.playback_rate < 0.0,
        EffectPhase::Idle => false,
    };
    let is_running_or_pending = matches!(anim_play_state(sys, host, anim), "running" | "paused")
        || anim.pending_task != PendingTask::None;
    in_effect || (is_current && is_running_or_pending)
}

/// W3C Web Animations §4.4.10 ("Updating the finished state").
pub(crate) fn update_finished_state(
    vm: &mut JsVm,
    host: &SharedHost,
    wa: &SharedWebAnimations,
    anim_h: HeapHandle,
    did_seek: bool,
    synchronously_notify: bool,
) {
    enum FinishedAction {
        None,
        Resolve {
            promise: HeapHandle,
            ct: Option<f64>,
            tl_t: Option<f64>,
            seq_id: u64,
        },
        Recreate,
    }

    let action = {
        let mut sys = wa.borrow_mut();
        let tl_t = sys
            .animations
            .get(&anim_h)
            .and_then(|a| a.timeline)
            .and_then(|th| sys.timeline_current_time(host, th));
        let eff_end = sys
            .animations
            .get(&anim_h)
            .map_or(0.0, |a| anim_effect_end(&sys, a));
        let Some(anim) = sys.animations.get_mut(&anim_h) else {
            return;
        };

        let unconstrained_ct = if did_seek {
            anim.hold_time
                .or_else(|| Some((tl_t? - anim.start_time?) * anim.playback_rate))
        } else {
            anim.hold_time
                .or_else(|| Some((tl_t? - anim.start_time?) * anim.playback_rate))
        };

        if let Some(uct) = unconstrained_ct {
            if anim.start_time.is_some() && anim.pending_task == PendingTask::None {
                if anim.playback_rate > 0.0 && uct >= eff_end {
                    anim.hold_time = if did_seek {
                        Some(uct)
                    } else {
                        Some(anim.hold_time.map_or(eff_end, |prev| prev.max(eff_end)))
                    };
                } else if anim.playback_rate < 0.0 && uct <= 0.0 {
                    anim.hold_time = if did_seek {
                        Some(uct)
                    } else {
                        Some(anim.hold_time.map_or(0.0, |prev| prev.min(0.0)))
                    };
                } else if anim.playback_rate != 0.0 {
                    if let Some(tt) = tl_t {
                        if did_seek && anim.hold_time.is_some() {
                            anim.start_time = Some(tt - uct / anim.playback_rate);
                        }
                        anim.hold_time = None;
                    }
                }
            }
        }

        let ps = anim_play_state(&sys, host, sys.animations.get(&anim_h).unwrap());
        let anim = sys.animations.get_mut(&anim_h).unwrap();
        if ps == "finished" && !anim.finished_settled {
            anim.finished_settled = true;
            let ct = anim.hold_time;
            let seq_id = anim.seq_id;
            let promise = anim.finished_promise;
            FinishedAction::Resolve {
                promise,
                ct,
                tl_t,
                seq_id,
            }
        } else if ps != "finished" && anim.finished_settled {
            FinishedAction::Recreate
        } else {
            FinishedAction::None
        }
    };

    match action {
        FinishedAction::None => {}
        FinishedAction::Recreate => {
            let fresh = alloc_promise(vm);
            if let Some(anim) = wa.borrow_mut().animations.get_mut(&anim_h) {
                anim.finished_promise = fresh;
                anim.finished_settled = false;
            }
        }
        FinishedAction::Resolve {
            promise,
            ct,
            tl_t,
            seq_id,
        } => {
            resolve_promise(vm, promise, JsValue::from_object(anim_h));
            let _ = synchronously_notify;
            let sched_t = {
                let sys = wa.borrow();
                sys.animations
                    .get(&anim_h)
                    .and_then(|a| {
                        let st = a.start_time?;
                        let limit = if a.playback_rate > 0.0 {
                            anim_effect_end(&sys, a)
                        } else {
                            0.0
                        };
                        if a.playback_rate != 0.0 && limit.is_finite() {
                            Some(st + limit / a.playback_rate)
                        } else {
                            None
                        }
                    })
                    .or(tl_t)
                    .unwrap_or(f64::INFINITY)
            };
            wa.borrow_mut().queued_events.push(QueuedPlaybackEvent {
                anim_h,
                event_type: "finish",
                current_time: ct,
                timeline_time: tl_t,
                scheduled_time: sched_t,
                seq_id,
            });
        }
    }
}

/// Synchronizes all active animations targeting `node` into `host.style_ctx`.
pub(crate) fn sync_target_to_style_ctx(
    host: &SharedHost,
    wa: &SharedWebAnimations,
    target: NodeId,
) {
    let specs: Vec<WebAnimationSpec> = {
        let sys = wa.borrow();
        let mut list: Vec<(u64, WebAnimationSpec)> = Vec::new();
        for anim in sys.animations.values() {
            let Some(eh) = anim.effect else {
                continue;
            };
            let Some(eff) = sys.effects.get(&eh) else {
                continue;
            };
            if eff.target != Some(target) {
                continue;
            }
            let ct = anim_current_time(&sys, host, anim);
            let ps = anim_play_state(&sys, host, anim);
            let kfs: Vec<WebAnimationKeyframe> = eff
                .keyframes
                .iter()
                .map(|k| WebAnimationKeyframe {
                    offset: k.offset.map(|v| v as f32),
                    computed_offset: k.computed_offset,
                    composite: k.composite.to_css_mode(),
                    easing: Some(k.easing.clone()),
                    declarations: k.declarations.clone(),
                })
                .collect();
            list.push((
                anim.seq_id,
                WebAnimationSpec {
                    id: anim.seq_id,
                    keyframes: kfs,
                    duration_ms: eff.timing.duration_ms.unwrap_or(0.0),
                    delay_ms: eff.timing.delay_ms,
                    end_delay_ms: eff.timing.end_delay_ms,
                    iteration_start: eff.timing.iteration_start,
                    timing: eff.timing.easing.clone(),
                    iteration_count: eff.timing.iterations,
                    direction: eff.timing.direction,
                    fill_mode: eff.timing.fill_mode,
                    fill_is_auto: eff.timing.fill_is_auto,
                    play_state: if ps == "paused" {
                        AnimationPlayStateMode::Paused
                    } else {
                        AnimationPlayStateMode::Running
                    },
                    composite: eff.composite,
                    iteration_composite: eff.iteration_composite,
                    current_time_ms: ct,
                    playback_rate: anim.playback_rate,
                    pseudo_element: eff.pseudo_element.clone(),
                    cancelled: ps == "idle",
                    removed: anim.replace_state == ReplaceState::Removed,
                },
            ));
        }
        list.sort_by_key(|(seq, _)| *seq);
        list.into_iter().map(|(_, s)| s).collect()
    };
    host.borrow_mut().set_web_animations_for_node(target, specs);
}

pub(crate) fn sync_anim_target(host: &SharedHost, wa: &SharedWebAnimations, anim_h: HeapHandle) {
    let target = {
        let sys = wa.borrow();
        sys.animations
            .get(&anim_h)
            .and_then(|a| a.effect)
            .and_then(|eh| sys.effects.get(&eh))
            .and_then(|e| e.target)
    };
    if let Some(t) = target {
        sync_target_to_style_ctx(host, wa, t);
    }
}

pub(crate) fn sync_all_targets(host: &SharedHost, wa: &SharedWebAnimations) {
    let targets: HashSet<NodeId> = {
        let sys = wa.borrow();
        sys.effects.values().filter_map(|e| e.target).collect()
    };
    for t in targets {
        sync_target_to_style_ctx(host, wa, t);
    }
}

/// W3C Web Animations §4.4.8 ("Playing an animation").
pub(crate) fn play_animation(
    vm: &mut JsVm,
    host: &SharedHost,
    wa: &SharedWebAnimations,
    anim_h: HeapHandle,
    auto_rewind: bool,
) -> Result<(), JsException> {
    let (eff_rate, eff_end, ct, aborted_before) = {
        let sys = wa.borrow();
        let Some(anim) = sys.animations.get(&anim_h) else {
            return Ok(());
        };
        let eff_rate = anim.pending_playback_rate.unwrap_or(anim.playback_rate);
        let eff_end = anim_effect_end(&sys, anim);
        let ct = anim_current_time(&sys, host, anim);
        let aborted = anim.pending_task == PendingTask::Pause;
        (eff_rate, eff_end, ct, aborted)
    };

    let mut seek_time: Option<f64> = None;
    if auto_rewind {
        if eff_rate >= 0.0 && (ct.is_none() || ct.is_some_and(|t| t < 0.0 || t >= eff_end)) {
            seek_time = Some(0.0);
        } else if eff_rate < 0.0 && (ct.is_none() || ct.is_some_and(|t| t <= 0.0 || t > eff_end)) {
            if eff_end.is_infinite() {
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Cannot play an infinite animation in reverse without a finite currentTime",
                ));
            }
            seek_time = Some(eff_end);
        }
    } else if ct.is_none() {
        seek_time = Some(0.0);
    }

    let need_new_ready = {
        let sys = wa.borrow();
        sys.animations
            .get(&anim_h)
            .is_some_and(|a| a.ready_settled && !aborted_before)
    };
    let fresh_ready = if need_new_ready {
        Some(alloc_promise(vm))
    } else {
        None
    };

    {
        let mut sys = wa.borrow_mut();
        let tl_t = sys
            .animations
            .get(&anim_h)
            .and_then(|a| a.timeline)
            .and_then(|th| sys.timeline_current_time(host, th));
        if let Some(anim) = sys.animations.get_mut(&anim_h) {
            if let Some(st) = seek_time {
                anim.hold_time = Some(st);
            }
            let has_finite_tl = tl_t.is_some();
            if anim.hold_time.is_some() {
                anim.start_time = None;
            }
            if let Some(p) = fresh_ready {
                anim.ready_promise = p;
                anim.ready_settled = false;
            }
            if has_finite_tl {
                anim.pending_task = PendingTask::Play;
            } else {
                // Inactive timeline: stay pending play until timeline becomes active.
                anim.pending_task = PendingTask::Play;
            }
            if anim.replace_state == ReplaceState::Removed {
                anim.replace_state = ReplaceState::Active;
            }
        }
    }

    update_finished_state(vm, host, wa, anim_h, false, false);
    sync_anim_target(host, wa, anim_h);
    Ok(())
}

/// W3C Web Animations §4.4.9 ("Pausing an animation").
pub(crate) fn pause_animation(
    vm: &mut JsVm,
    host: &SharedHost,
    wa: &SharedWebAnimations,
    anim_h: HeapHandle,
) -> Result<(), JsException> {
    let (already_paused, ct, eff_rate, eff_end, need_ready) = {
        let sys = wa.borrow();
        let Some(anim) = sys.animations.get(&anim_h) else {
            return Ok(());
        };
        let ps = anim_play_state(&sys, host, anim);
        let already = ps == "paused" && anim.pending_task != PendingTask::Play;
        let ct = anim_current_time(&sys, host, anim);
        let eff_rate = anim.pending_playback_rate.unwrap_or(anim.playback_rate);
        let eff_end = anim_effect_end(&sys, anim);
        let need_ready = anim.ready_settled;
        (already, ct, eff_rate, eff_end, need_ready)
    };
    if already_paused {
        return Ok(());
    }

    let mut seek_time: Option<f64> = None;
    if ct.is_none() {
        if eff_rate >= 0.0 {
            seek_time = Some(0.0);
        } else {
            if eff_end.is_infinite() {
                return Err(create_dom_exception(
                    vm,
                    "InvalidStateError",
                    "Cannot pause an infinite animation in reverse without a currentTime",
                ));
            }
            seek_time = Some(eff_end);
        }
    }

    let fresh_ready = if need_ready {
        Some(alloc_promise(vm))
    } else {
        None
    };

    {
        let mut sys = wa.borrow_mut();
        if let Some(anim) = sys.animations.get_mut(&anim_h) {
            if let Some(st) = seek_time {
                anim.hold_time = Some(st);
            }
            if let Some(p) = fresh_ready {
                anim.ready_promise = p;
                anim.ready_settled = false;
            }
            anim.pending_task = PendingTask::Pause;
        }
    }
    update_finished_state(vm, host, wa, anim_h, false, false);
    sync_anim_target(host, wa, anim_h);
    Ok(())
}

/// W3C Web Animations §4.4.11 ("Finishing an animation").
pub(crate) fn finish_animation(
    vm: &mut JsVm,
    host: &SharedHost,
    wa: &SharedWebAnimations,
    anim_h: HeapHandle,
) -> Result<(), JsException> {
    let (eff_rate, eff_end) = {
        let sys = wa.borrow();
        let Some(anim) = sys.animations.get(&anim_h) else {
            return Ok(());
        };
        (
            anim.pending_playback_rate.unwrap_or(anim.playback_rate),
            anim_effect_end(&sys, anim),
        )
    };
    if eff_rate == 0.0 || (eff_rate > 0.0 && eff_end.is_infinite()) {
        return Err(create_dom_exception(
            vm,
            "InvalidStateError",
            "Cannot finish animation with zero playbackRate or infinite effect end",
        ));
    }

    let limit = if eff_rate > 0.0 { eff_end } else { 0.0 };
    let resolve_ready_opt = {
        let mut sys = wa.borrow_mut();
        let tl_t = sys
            .animations
            .get(&anim_h)
            .and_then(|a| a.timeline)
            .and_then(|th| sys.timeline_current_time(host, th));
        let Some(anim) = sys.animations.get_mut(&anim_h) else {
            return Ok(());
        };
        if let Some(pr) = anim.pending_playback_rate.take() {
            anim.playback_rate = pr;
        }
        anim.hold_time = Some(limit);
        let was_pending = anim.pending_task != PendingTask::None;
        anim.pending_task = PendingTask::None;
        if let Some(tt) = tl_t {
            anim.start_time = Some(tt - limit / anim.playback_rate);
        }
        if was_pending && !anim.ready_settled {
            anim.ready_settled = true;
            Some(anim.ready_promise)
        } else {
            None
        }
    };

    if let Some(rp) = resolve_ready_opt {
        resolve_promise(vm, rp, JsValue::from_object(anim_h));
    }
    update_finished_state(vm, host, wa, anim_h, true, true);
    sync_anim_target(host, wa, anim_h);
    Ok(())
}

/// W3C Web Animations §4.4.12 ("Canceling an animation").
pub(crate) fn cancel_animation(
    vm: &mut JsVm,
    host: &SharedHost,
    wa: &SharedWebAnimations,
    anim_h: HeapHandle,
) {
    let info = {
        let mut sys = wa.borrow_mut();
        let tl_t = sys
            .animations
            .get(&anim_h)
            .and_then(|a| a.timeline)
            .and_then(|th| sys.timeline_current_time(host, th));
        let ps = sys
            .animations
            .get(&anim_h)
            .map_or("idle", |a| anim_play_state(&sys, host, a));
        let Some(anim) = sys.animations.get_mut(&anim_h) else {
            return;
        };
        let was_idle = ps == "idle";
        let old_ready = if !anim.ready_settled {
            Some(anim.ready_promise)
        } else {
            None
        };
        let old_finished = if !anim.finished_settled {
            Some(anim.finished_promise)
        } else {
            None
        };
        anim.hold_time = None;
        anim.start_time = None;
        anim.pending_task = PendingTask::None;
        anim.pending_playback_rate = None;
        let seq_id = anim.seq_id;
        Some((was_idle, old_ready, old_finished, tl_t, seq_id))
    };

    let Some((was_idle, old_ready, old_finished, tl_t, seq_id)) = info else {
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
        {
            let mut sys = wa.borrow_mut();
            if let Some(anim) = sys.animations.get_mut(&anim_h) {
                anim.ready_promise = new_ready;
                anim.ready_settled = true;
                anim.finished_promise = new_finished;
                anim.finished_settled = false;
            }
            sys.queued_events.push(QueuedPlaybackEvent {
                anim_h,
                event_type: "cancel",
                current_time: None,
                timeline_time: tl_t,
                scheduled_time: tl_t.unwrap_or(f64::INFINITY),
                seq_id,
            });
        }
    }
    sync_anim_target(host, wa, anim_h);
}

/// W3C Web Animations §4.4.15 ("Reversing an animation").
pub(crate) fn reverse_animation(
    vm: &mut JsVm,
    host: &SharedHost,
    wa: &SharedWebAnimations,
    anim_h: HeapHandle,
) -> Result<(), JsException> {
    let (tl_active, orig_rate, orig_pending_rate) = {
        let sys = wa.borrow();
        let Some(anim) = sys.animations.get(&anim_h) else {
            return Ok(());
        };
        let active = anim
            .timeline
            .and_then(|th| sys.timeline_current_time(host, th))
            .is_some();
        (active, anim.playback_rate, anim.pending_playback_rate)
    };
    if !tl_active {
        return Err(create_dom_exception(
            vm,
            "InvalidStateError",
            "Cannot reverse an animation with an inactive timeline",
        ));
    }
    let effective_rate = orig_pending_rate.unwrap_or(orig_rate);
    {
        let mut sys = wa.borrow_mut();
        if let Some(anim) = sys.animations.get_mut(&anim_h) {
            anim.pending_playback_rate = Some(-effective_rate);
        }
    }
    if let Err(e) = play_animation(vm, host, wa, anim_h, true) {
        if let Some(anim) = wa.borrow_mut().animations.get_mut(&anim_h) {
            anim.pending_playback_rate = orig_pending_rate;
        }
        return Err(e);
    }
    Ok(())
}

/// W3C Web Animations §4.1.2 ("Update animations and send events"), invoked on every VSync
/// frame BEFORE `requestAnimationFrame` callbacks run!
pub fn update_animations_and_send_events(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    wa: &SharedWebAnimations,
    timestamp_ms: f64,
) {
    // 1. Update document timeline sample time.
    let anim_handles: Vec<HeapHandle> = {
        let mut sys = wa.borrow_mut();
        if timestamp_ms > sys.frame_time_ms {
            sys.frame_time_ms = timestamp_ms;
        }
        let mut hs: Vec<(u64, HeapHandle)> =
            sys.animations.iter().map(|(&h, a)| (a.seq_id, h)).collect();
        hs.sort_by_key(|(s, _)| *s);
        hs.into_iter().map(|(_, h)| h).collect()
    };

    // 2. Resolve pending play/pause tasks whose timelines are active.
    let mut ready_to_resolve: Vec<(HeapHandle, HeapHandle)> = Vec::new();
    {
        let mut sys = wa.borrow_mut();
        for &ah in &anim_handles {
            let tl_t = sys
                .animations
                .get(&ah)
                .and_then(|a| a.timeline)
                .and_then(|th| sys.timeline_current_time(host, th));
            let Some(tt) = tl_t else {
                continue;
            };
            let Some(anim) = sys.animations.get_mut(&ah) else {
                continue;
            };
            match anim.pending_task {
                PendingTask::None => {}
                PendingTask::Play => {
                    if let Some(pr) = anim.pending_playback_rate.take() {
                        anim.playback_rate = pr;
                    }
                    let ht = anim.hold_time.take();
                    if let Some(h) = ht {
                        if anim.playback_rate == 0.0 {
                            anim.start_time = Some(tt);
                            anim.hold_time = Some(h);
                        } else {
                            anim.start_time = Some(tt - h / anim.playback_rate);
                        }
                    } else if anim.start_time.is_none() {
                        anim.start_time = Some(tt);
                    }
                    anim.pending_task = PendingTask::None;
                    if !anim.ready_settled {
                        anim.ready_settled = true;
                        ready_to_resolve.push((anim.ready_promise, ah));
                    }
                }
                PendingTask::Pause => {
                    if let Some(pr) = anim.pending_playback_rate.take() {
                        anim.playback_rate = pr;
                    }
                    if anim.hold_time.is_none() {
                        if let Some(st) = anim.start_time {
                            anim.hold_time = Some((tt - st) * anim.playback_rate);
                        }
                    }
                    anim.start_time = None;
                    anim.pending_task = PendingTask::None;
                    if !anim.ready_settled {
                        anim.ready_settled = true;
                        ready_to_resolve.push((anim.ready_promise, ah));
                    }
                }
            }
        }
    }
    for (p, ah) in ready_to_resolve {
        resolve_promise(vm, p, JsValue::from_object(ah));
    }

    // 3. Update finished states across all animations.
    for &ah in &anim_handles {
        update_finished_state(vm, host, wa, ah, false, false);
    }

    // 4. Remove replaced animations (W3C Web Animations §4.4.19).
    remove_replaced_animations(host, wa);

    // 5. Sync all active animation states to CSS style context.
    sync_all_targets(host, wa);

    // 6. Run microtask checkpoint so `await anim.ready` / `await anim.finished` callbacks run
    // BEFORE event dispatch (HTML §8.1.7.3 / Web Animations §4.1.2).
    run_microtask_checkpoint(event_loop, vm);

    // 7. Snapshot and dispatch queued `AnimationPlaybackEvent`s (`finish`, `cancel`, `remove`)
    // BEFORE rAF! Events queued *during* event dispatch wait until the next frame.
    let mut to_dispatch = std::mem::take(&mut wa.borrow_mut().queued_events);
    to_dispatch.sort_by(|a, b| {
        a.scheduled_time
            .total_cmp(&b.scheduled_time)
            .then_with(|| a.seq_id.cmp(&b.seq_id))
    });
    let ev_proto = wa
        .borrow()
        .protos
        .playback_event_proto
        .unwrap_or_else(|| events.borrow().event_proto.unwrap_or(vm.object_proto));
    for ev in to_dispatch {
        let ct_val = ev.current_time.map_or(JsValue::NULL, JsValue::from_f64);
        let tl_val = ev.timeline_time.map_or(JsValue::NULL, JsValue::from_f64);
        fire_object_event_with_proto(
            vm,
            host,
            events,
            event_loop,
            ev.anim_h,
            ev.event_type,
            ev_proto,
            &[("__rix_wa_ct__", ct_val), ("__rix_wa_tlt__", tl_val)],
        );
    }
    sync_all_targets(host, wa);
    run_microtask_checkpoint(event_loop, vm);
}

/// W3C Web Animations §4.4.19 ("Removing replaced animations").
pub(crate) fn remove_replaced_animations(host: &SharedHost, wa: &SharedWebAnimations) {
    let mut sys = wa.borrow_mut();
    // Collect candidates grouped by `(target, pseudo_element)`, ordered by composite order (`seq_id`).
    type TargetKey = (NodeId, Option<String>);
    let mut by_target: HashMap<TargetKey, Vec<(u64, HeapHandle)>> = HashMap::new();
    for (&ah, anim) in &sys.animations {
        if anim.replace_state == ReplaceState::Removed {
            continue;
        }
        let Some(eh) = anim.effect else { continue };
        let Some(eff) = sys.effects.get(&eh) else {
            continue;
        };
        let Some(target) = eff.target else { continue };
        by_target
            .entry((target, eff.pseudo_element.clone()))
            .or_default()
            .push((anim.seq_id, ah));
    }

    let mut newly_removed: Vec<QueuedPlaybackEvent> = Vec::new();
    for group in by_target.values_mut() {
        group.sort_by_key(|(seq, _)| *seq);
        // Walk from highest priority downwards, tracking properties covered by filling-forwards
        // finished `replace` animations.
        let mut covered_props: HashSet<String> = HashSet::new();
        for &(_, ah) in group.iter().rev() {
            let Some(anim) = sys.animations.get(&ah) else {
                continue;
            };
            let ps = anim_play_state(&sys, host, anim);
            let ct = anim_current_time(&sys, host, anim);
            let Some(eh) = anim.effect else { continue };
            let Some(eff) = sys.effects.get(&eh) else {
                continue;
            };
            let comp = compute_effect_timing(&eff.timing, ct, anim.playback_rate);
            let is_filling_after =
                ps == "finished" && comp.phase == EffectPhase::After && comp.active_time.is_some();
            if !is_filling_after {
                continue;
            }
            let props: HashSet<String> = eff
                .keyframes
                .iter()
                .flat_map(|k| k.props.iter().map(|(_, css_p, _)| css_p.clone()))
                .collect();
            if anim.replace_state == ReplaceState::Active
                && !props.is_empty()
                && props.is_subset(&covered_props)
            {
                let tl_t = anim
                    .timeline
                    .and_then(|th| sys.timeline_current_time(host, th));
                let seq_id = anim.seq_id;
                if let Some(a_mut) = sys.animations.get_mut(&ah) {
                    a_mut.replace_state = ReplaceState::Removed;
                }
                newly_removed.push(QueuedPlaybackEvent {
                    anim_h: ah,
                    event_type: "remove",
                    current_time: ct,
                    timeline_time: tl_t,
                    scheduled_time: tl_t.unwrap_or(f64::INFINITY),
                    seq_id,
                });
            } else if eff.composite == AnimationCompositionMode::Replace
                && eff.keyframes.iter().all(|k| {
                    matches!(
                        k.composite,
                        KeyframeComposite::Auto | KeyframeComposite::Replace
                    )
                })
            {
                covered_props.extend(props);
            }
        }
    }
    sys.queued_events.extend(newly_removed);
}

// ---------------------------------------------------------------------------
// JS Keyframes & Options Parsing (W3C Web Animations §5.4 & §5.5)
// ---------------------------------------------------------------------------

fn read_iterable_values(
    vm: &mut JsVm,
    obj_h: HeapHandle,
) -> Result<Option<Vec<JsValue>>, JsException> {
    let sym_iter = vm.key_symbol_iterator;
    let iter_method = vm.get_property_by_key(obj_h, sym_iter)?;
    if iter_method.is_undefined() || iter_method.is_null() {
        return Ok(None);
    }
    let Some(mh) = iter_method.as_object() else {
        return Err(vm.type_error("@@iterator is not callable"));
    };
    if !vm.is_callable_handle(mh) {
        return Err(vm.type_error("@@iterator is not callable"));
    }
    let iter_val = vm.call_value(iter_method, JsValue::from_object(obj_h), &[])?;
    let Some(iter_h) = iter_val.as_object() else {
        return Err(vm.type_error("Iterator result is not an object"));
    };
    let k_next = vm.heap.intern_key("next");
    let k_done = vm.heap.intern_key("done");
    let k_val = vm.heap.intern_key("value");
    let next_fn = vm.get_property_by_key(iter_h, k_next)?;
    let mut out = Vec::new();
    for _ in 0..4096 {
        let step = vm.call_value(next_fn, iter_val, &[])?;
        let Some(sh) = step.as_object() else {
            return Err(vm.type_error("Iterator step result is not an object"));
        };
        let done = vm.get_property_by_key(sh, k_done)?;
        if vm.to_boolean(done) {
            break;
        }
        out.push(vm.get_property_by_key(sh, k_val)?);
    }
    Ok(Some(out))
}

fn parse_keyframe_composite_val(
    vm: &mut JsVm,
    v: JsValue,
) -> Result<KeyframeComposite, JsException> {
    if v.is_undefined() {
        return Ok(KeyframeComposite::Auto);
    }
    if v.is_null() {
        return Err(vm.type_error("Invalid keyframe composite 'null'"));
    }
    let s = vm.to_js_string(v)?;
    KeyframeComposite::parse_exact(&s)
        .ok_or_else(|| vm.type_error(format!("Invalid keyframe composite '{s}'")))
}

fn parse_easing_val(vm: &mut JsVm, v: JsValue) -> Result<(TimingFunction, String), JsException> {
    if v.is_undefined() {
        return Ok((TimingFunction::Linear, "linear".to_string()));
    }
    let s = vm.to_js_string(v)?;
    parse_easing_strict(&s).ok_or_else(|| vm.type_error(format!("Invalid easing '{s}'")))
}

fn parse_single_offset_val(vm: &mut JsVm, v: JsValue) -> Result<Option<f64>, JsException> {
    if v.is_undefined() || v.is_null() {
        return Ok(None);
    }
    let n = vm.to_number(v)?;
    if !n.is_finite() {
        return Err(vm.type_error("Keyframe offset must be a finite number"));
    }
    Ok(Some(n))
}

pub(crate) fn process_keyframes_arg(
    vm: &mut JsVm,
    val: JsValue,
) -> Result<Vec<ProcessedKeyframe>, JsException> {
    if val.is_null() || val.is_undefined() {
        return Ok(Vec::new());
    }
    let Some(obj_h) = val.as_object() else {
        return Err(vm.type_error("Keyframes argument must be an object or null"));
    };

    // W3C §5.5.2 step 4: check @@iterator first!
    if let Some(items) = read_iterable_values(vm, obj_h)? {
        let mut frames = Vec::with_capacity(items.len());
        let k_comp = vm.heap.intern_key("composite");
        let k_ease = vm.heap.intern_key("easing");
        let k_off = vm.heap.intern_key("offset");
        for item in items {
            if item.is_null() || item.is_undefined() {
                frames.push(ProcessedKeyframe {
                    offset: None,
                    computed_offset: f64::NAN,
                    easing: TimingFunction::Linear,
                    easing_str: "linear".to_string(),
                    composite: KeyframeComposite::Auto,
                    props: Vec::new(),
                    declarations: Vec::new(),
                });
                continue;
            }
            let Some(ih) = item.as_object() else {
                return Err(vm.type_error("Each keyframe must be an object"));
            };
            // WebIDL dictionary member read order: composite, easing, offset.
            let comp_v = vm.get_property_by_key(ih, k_comp)?;
            let composite = parse_keyframe_composite_val(vm, comp_v)?;
            let ease_v = vm.get_property_by_key(ih, k_ease)?;
            let (easing, easing_str) = parse_easing_val(vm, ease_v)?;
            let off_v = vm.get_property_by_key(ih, k_off)?;
            let offset = parse_single_offset_val(vm, off_v)?;

            let mut props = Vec::new();
            let mut declarations = Vec::new();
            for k in vm.own_enumerable_keys_including_elements(ih) {
                let Some(css_prop) = js_keyframe_prop_to_css(&k) else {
                    continue;
                };
                if !is_supported_animatable_css_prop(&css_prop) {
                    continue;
                }
                let pk = vm.heap.intern_key(&k);
                let pv = vm.get_property_by_key(ih, pk)?;
                if pv.is_undefined() || pv.is_null() {
                    continue;
                }
                let raw = vm.to_js_string(pv)?;
                if let Some((ser, decls)) = parse_keyframe_css_prop(&css_prop, &raw) {
                    let js_k = css_prop_to_js_keyframe(&css_prop);
                    props.push((js_k, css_prop, ser));
                    declarations.extend(decls);
                }
            }
            frames.push(ProcessedKeyframe {
                offset,
                computed_offset: f64::NAN,
                easing,
                easing_str,
                composite,
                props,
                declarations,
            });
        }
        validate_keyframe_offsets(&frames).map_err(|e| vm.type_error(e))?;
        compute_missing_keyframe_offsets(&mut frames);
        return Ok(frames);
    }

    // Property-indexed keyframe object (W3C §5.5.2 step 6).
    let k_comp = vm.heap.intern_key("composite");
    let k_ease = vm.heap.intern_key("easing");
    let k_off = vm.heap.intern_key("offset");

    let comp_v = vm.get_property_by_key(obj_h, k_comp)?;
    let composites: Vec<KeyframeComposite> = if comp_v.is_undefined() {
        vec![KeyframeComposite::Auto]
    } else if comp_v.is_null() {
        return Err(vm.type_error("Invalid composite 'null'"));
    } else if let Some(ch) = comp_v.as_object() {
        if let Some(elems) = read_iterable_values(vm, ch)? {
            let mut v = Vec::with_capacity(elems.len());
            for e in elems {
                v.push(parse_keyframe_composite_val(vm, e)?);
            }
            if v.is_empty() {
                vec![KeyframeComposite::Auto]
            } else {
                v
            }
        } else {
            vec![parse_keyframe_composite_val(vm, comp_v)?]
        }
    } else {
        vec![parse_keyframe_composite_val(vm, comp_v)?]
    };

    let ease_v = vm.get_property_by_key(obj_h, k_ease)?;
    let easings: Vec<(TimingFunction, String)> = if ease_v.is_undefined() {
        vec![(TimingFunction::Linear, "linear".to_string())]
    } else if let Some(eh) = ease_v.as_object() {
        if let Some(elems) = read_iterable_values(vm, eh)? {
            let mut v = Vec::with_capacity(elems.len());
            for e in elems {
                v.push(parse_easing_val(vm, e)?);
            }
            if v.is_empty() {
                vec![(TimingFunction::Linear, "linear".to_string())]
            } else {
                v
            }
        } else {
            vec![parse_easing_val(vm, ease_v)?]
        }
    } else {
        vec![parse_easing_val(vm, ease_v)?]
    };

    let off_v = vm.get_property_by_key(obj_h, k_off)?;
    let offsets: Vec<Option<f64>> = if off_v.is_undefined() || off_v.is_null() {
        Vec::new()
    } else if let Some(oh) = off_v.as_object() {
        if let Some(elems) = read_iterable_values(vm, oh)? {
            let mut v = Vec::with_capacity(elems.len());
            for e in elems {
                v.push(parse_single_offset_val(vm, e)?);
            }
            v
        } else {
            vec![parse_single_offset_val(vm, off_v)?]
        }
    } else {
        vec![parse_single_offset_val(vm, off_v)?]
    };

    // Collect each CSS property's list of values.
    type ParsedPropVal = Option<(String, Vec<rix_css::SpecifiedDeclaration>)>;
    type PropEntry = (String, String, Vec<ParsedPropVal>);
    let mut prop_entries: Vec<PropEntry> = Vec::new();
    for k in vm.own_enumerable_keys_including_elements(obj_h) {
        let Some(css_prop) = js_keyframe_prop_to_css(&k) else {
            continue;
        };
        if !is_supported_animatable_css_prop(&css_prop) {
            continue;
        }
        let pk = vm.heap.intern_key(&k);
        let pv = vm.get_property_by_key(obj_h, pk)?;
        if pv.is_undefined() {
            continue;
        }
        let raw_vals: Vec<String> = if let Some(vh) = pv.as_object() {
            if let Some(elems) = read_iterable_values(vm, vh)? {
                let mut rs = Vec::with_capacity(elems.len());
                for e in elems {
                    rs.push(vm.to_js_string(e)?);
                }
                rs
            } else {
                vec![vm.to_js_string(pv)?]
            }
        } else {
            vec![vm.to_js_string(pv)?]
        };
        if raw_vals.is_empty() {
            continue;
        }
        let parsed: Vec<_> = raw_vals
            .iter()
            .map(|r| parse_keyframe_css_prop(&css_prop, r))
            .collect();
        let js_k = css_prop_to_js_keyframe(&css_prop);
        prop_entries.push((js_k, css_prop, parsed));
    }

    if prop_entries.is_empty() {
        return Ok(Vec::new());
    }

    // Distribute each property's values across [0, 1] and merge entries sharing identical ratios.
    type KeyframeSlot = (
        f64,
        Vec<(String, String, String)>,
        Vec<rix_css::SpecifiedDeclaration>,
    );
    let mut slots: Vec<KeyframeSlot> = Vec::new();
    for (js_k, css_prop, vals) in prop_entries {
        let n = vals.len();
        for (i, entry_opt) in vals.into_iter().enumerate() {
            let ratio = if n == 1 {
                1.0
            } else {
                i as f64 / (n - 1) as f64
            };
            let pos = if let Some(idx) = slots
                .iter()
                .position(|(r, _, _)| (*r - ratio).abs() < 1e-12)
            {
                idx
            } else {
                slots.push((ratio, Vec::new(), Vec::new()));
                slots.len() - 1
            };
            if let Some((ser, decls)) = entry_opt {
                slots[pos].1.push((js_k.clone(), css_prop.clone(), ser));
                slots[pos].2.extend(decls);
            }
        }
    }
    slots.sort_by(|a, b| a.0.total_cmp(&b.0));

    let mut frames: Vec<ProcessedKeyframe> = slots
        .into_iter()
        .enumerate()
        .map(|(i, (_, props, declarations))| {
            let offset = offsets.get(i).copied().flatten();
            let (easing, easing_str) = easings[i % easings.len()].clone();
            let composite = composites[i % composites.len()];
            ProcessedKeyframe {
                offset,
                computed_offset: f64::NAN,
                easing,
                easing_str,
                composite,
                props,
                declarations,
            }
        })
        .collect();

    validate_keyframe_offsets(&frames).map_err(|e| vm.type_error(e))?;
    compute_missing_keyframe_offsets(&mut frames);
    Ok(frames)
}

/// Validates and applies `OptionalEffectTiming` / `KeyframeEffectOptions` atomically per W3C §5.4.5.
pub(crate) fn apply_timing_dict(
    vm: &mut JsVm,
    opts: JsValue,
    timing: &mut EffectTimingInput,
    easing_str: &mut String,
    extra: Option<(
        &mut AnimationCompositionMode,
        &mut AnimationCompositionMode,
        &mut Option<String>,
        Option<&mut String>,
    )>,
) -> Result<(), JsException> {
    if opts.is_undefined() || opts.is_null() {
        return Ok(());
    }
    if opts.is_number() {
        let d = opts.to_number_f64().unwrap_or(f64::NAN);
        if d.is_nan() || d < 0.0 {
            return Err(vm.type_error("Effect duration must be non-negative"));
        }
        timing.duration_ms = Some(d);
        return Ok(());
    }
    let Some(oh) = opts.as_object() else {
        return Err(vm.type_error("Effect options must be a number or object"));
    };

    let mut next_timing = timing.clone();
    let mut next_easing_str = easing_str.clone();
    let mut next_comp = extra.as_ref().map(|e| *e.0);
    let mut next_iter_comp = extra.as_ref().map(|e| *e.1);
    let mut next_pseudo = extra.as_ref().map(|e| e.2.clone());
    let mut next_id: Option<String> = None;

    let get_opt = |vm: &mut JsVm, name: &str| -> Result<Option<JsValue>, JsException> {
        let k = vm.heap.intern_key(name);
        let v = vm.get_property_by_key(oh, k)?;
        Ok((!v.is_undefined()).then_some(v))
    };

    if let Some(v) = get_opt(vm, "delay")? {
        let n = vm.to_number(v)?;
        if !n.is_finite() {
            return Err(vm.type_error("delay must be a finite number"));
        }
        next_timing.delay_ms = n;
    }
    if let Some(v) = get_opt(vm, "direction")? {
        let s = vm.to_js_string(v)?;
        next_timing.direction = parse_direction_exact(&s)
            .ok_or_else(|| vm.type_error(format!("Invalid direction '{s}'")))?;
    }
    if let Some(v) = get_opt(vm, "duration")? {
        if v.is_string() {
            let s = vm.to_js_string(v)?;
            if s == "auto" {
                next_timing.duration_ms = None;
            } else {
                let n = s.trim().parse::<f64>().unwrap_or(f64::NAN);
                if n.is_nan() || n < 0.0 {
                    return Err(vm.type_error(format!("Invalid duration '{s}'")));
                }
                next_timing.duration_ms = Some(n);
            }
        } else {
            let n = vm.to_number(v)?;
            if n.is_nan() || n < 0.0 {
                return Err(vm.type_error("duration must be non-negative"));
            }
            next_timing.duration_ms = Some(n);
        }
    }
    if let Some(v) = get_opt(vm, "easing")? {
        let (tf, canon) = parse_easing_val(vm, v)?;
        next_timing.easing = tf;
        next_easing_str = canon;
    }
    if let Some(v) = get_opt(vm, "endDelay")? {
        let n = vm.to_number(v)?;
        if !n.is_finite() {
            return Err(vm.type_error("endDelay must be a finite number"));
        }
        next_timing.end_delay_ms = n;
    }
    if let Some(v) = get_opt(vm, "fill")? {
        let s = vm.to_js_string(v)?;
        let (fm, is_auto) = parse_fill_mode_exact(&s)
            .ok_or_else(|| vm.type_error(format!("Invalid fill mode '{s}'")))?;
        next_timing.fill_mode = fm;
        next_timing.fill_is_auto = is_auto;
    }
    if let Some(v) = get_opt(vm, "iterationStart")? {
        let n = vm.to_number(v)?;
        if !n.is_finite() || n < 0.0 {
            return Err(vm.type_error("iterationStart must be a finite non-negative number"));
        }
        next_timing.iteration_start = n;
    }
    if let Some(v) = get_opt(vm, "iterations")? {
        let n = vm.to_number(v)?;
        if n.is_nan() || n < 0.0 {
            return Err(vm.type_error("iterations must be non-negative"));
        }
        next_timing.iterations = n;
    }

    if let Some((comp_out, iter_comp_out, pseudo_out, id_out)) = extra {
        if let Some(v) = get_opt(vm, "composite")? {
            if v.is_null() {
                return Err(vm.type_error("Invalid composite 'null'"));
            }
            let s = vm.to_js_string(v)?;
            next_comp = Some(
                parse_composite_exact(&s)
                    .ok_or_else(|| vm.type_error(format!("Invalid composite '{s}'")))?,
            );
        }
        if let Some(v) = get_opt(vm, "iterationComposite")? {
            if v.is_null() {
                return Err(vm.type_error("Invalid iterationComposite 'null'"));
            }
            let s = vm.to_js_string(v)?;
            next_iter_comp = Some(
                parse_iteration_composite_exact(&s)
                    .ok_or_else(|| vm.type_error(format!("Invalid iterationComposite '{s}'")))?,
            );
        }
        if let Some(v) = get_opt(vm, "pseudoElement")? {
            let p_str = if v.is_null() {
                None
            } else {
                Some(vm.to_js_string(v)?)
            };
            let validated = validate_pseudo_element(p_str.as_deref())
                .map_err(|e| create_dom_exception(vm, "SyntaxError", &e))?;
            next_pseudo = Some(validated);
        }
        if id_out.is_some() {
            if let Some(v) = get_opt(vm, "id")? {
                next_id = Some(vm.to_js_string(v)?);
            }
        }
        if let Some(c) = next_comp {
            *comp_out = c;
        }
        if let Some(ic) = next_iter_comp {
            *iter_comp_out = ic;
        }
        if let Some(ps) = next_pseudo {
            *pseudo_out = ps;
        }
        if let (Some(out), Some(id_val)) = (id_out, next_id) {
            *out = id_val;
        }
    }

    *timing = next_timing;
    *easing_str = next_easing_str;
    Ok(())
}

pub(crate) fn validate_target_arg(
    vm: &mut JsVm,
    host: &SharedHost,
    v: JsValue,
) -> Result<Option<NodeId>, JsException> {
    if v.is_null() || v.is_undefined() {
        return Ok(None);
    }
    let nid = require_node(vm, host, v)
        .map_err(|_| vm.type_error("KeyframeEffect target must be an Element or null"))?;
    let is_el = matches!(
        host.borrow().document.borrow().get(nid),
        Some(NodeData::Element(_))
    );
    if !is_el {
        return Err(vm.type_error("KeyframeEffect target must be an Element or null"));
    }
    Ok(Some(nid))
}

pub(crate) fn build_timing_js_object(
    vm: &mut JsVm,
    timing: &EffectTimingInput,
    easing_str: &str,
) -> HeapHandle {
    let obj = vm.alloc_ordinary_object();
    let set = |vm: &mut JsVm, k: &str, v: JsValue| {
        let pk = vm.heap.intern_key(k);
        vm.heap
            .object_define_data(obj, pk, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
    };
    set(vm, "delay", JsValue::from_f64(timing.delay_ms));
    set(vm, "endDelay", JsValue::from_f64(timing.end_delay_ms));
    let fill_s = vm
        .heap
        .alloc_string(fill_mode_as_str(timing.fill_mode, timing.fill_is_auto));
    set(vm, "fill", JsValue::from_string(fill_s));
    set(
        vm,
        "iterationStart",
        JsValue::from_f64(timing.iteration_start),
    );
    set(vm, "iterations", JsValue::from_f64(timing.iterations));
    let dur_val = match timing.duration_ms {
        Some(d) => JsValue::from_f64(d),
        None => JsValue::from_string(vm.heap.alloc_string("auto")),
    };
    set(vm, "duration", dur_val);
    let dir_s = vm.heap.alloc_string(direction_as_str(timing.direction));
    set(vm, "direction", JsValue::from_string(dir_s));
    let ease_s = vm.heap.alloc_string(easing_str);
    set(vm, "easing", JsValue::from_string(ease_s));
    obj
}

pub(crate) fn alloc_animation_object(
    vm: &mut JsVm,
    wa: &SharedWebAnimations,
    effect: Option<HeapHandle>,
    timeline: Option<HeapHandle>,
    id: String,
) -> HeapHandle {
    let proto = wa
        .borrow()
        .protos
        .animation_proto
        .unwrap_or(vm.object_proto);
    let anim_h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let ready_promise = alloc_promise(vm);
    resolve_promise(vm, ready_promise, JsValue::from_object(anim_h));
    let finished_promise = alloc_promise(vm);

    let mut sys = wa.borrow_mut();
    let seq_id = sys.alloc_seq_id();
    // W3C §4.4.2: detach `effect` from any previously owning animation!
    if let Some(eh) = effect {
        if let Some(prev_owner) = sys
            .effects
            .get_mut(&eh)
            .and_then(|e| e.owner_anim.replace(anim_h))
        {
            if let Some(prev_anim) = sys.animations.get_mut(&prev_owner) {
                prev_anim.effect = None;
            }
        }
    }
    sys.animations.insert(
        anim_h,
        AnimState {
            seq_id,
            id,
            effect,
            timeline,
            start_time: None,
            hold_time: None,
            playback_rate: 1.0,
            pending_playback_rate: None,
            pending_task: PendingTask::None,
            replace_state: ReplaceState::Active,
            ready_promise,
            ready_settled: true,
            finished_promise,
            finished_settled: false,
        },
    );
    anim_h
}

// ---------------------------------------------------------------------------
// Installation onto `ContentRuntime`'s `JsVm`

pub(crate) fn parse_css_numberish_time_ms(vm: &mut JsVm, v: JsValue) -> Result<f64, JsException> {
    if let Some(oh) = v.as_object() {
        let km = vm.heap.intern_key("__rix_css_unit_value__");
        if vm.heap.object_get_own(oh, km, &vm.shapes).is_some() {
            let kv = vm.heap.intern_key("value");
            let ku = vm.heap.intern_key("unit");
            let val = vm.get_property_by_key(oh, kv)?;
            let unit = vm.get_property_by_key(oh, ku)?;
            let n = vm.to_number(val)?;
            let u = vm.to_js_string(unit)?;
            return match u.as_str() {
                "number" | "ms" => Ok(n),
                "s" => Ok(n * 1000.0),
                _ => Err(vm.type_error(format!("Invalid time unit '{u}'"))),
            };
        }
    }
    vm.to_number(v)
}

pub(crate) fn collect_animations_for_scope(
    host: &SharedHost,
    wa: &SharedWebAnimations,
    root_node: Option<NodeId>,
    subtree: bool,
    pseudo_filter: Option<&str>,
    doc_scope: Option<NodeId>,
) -> Vec<HeapHandle> {
    let sys = wa.borrow();
    let hb = host.borrow();
    let doc = hb.document.borrow();
    let mut matches: Vec<(u64, HeapHandle)> = Vec::new();
    for (&ah, anim) in &sys.animations {
        if !anim_is_relevant(&sys, host, anim) {
            continue;
        }
        let Some(eh) = anim.effect else { continue };
        let Some(eff) = sys.effects.get(&eh) else {
            continue;
        };
        let Some(target) = eff.target else { continue };
        let in_scope = if let Some(doc_id) = doc_scope {
            hb.owner_doc_of(target) == doc_id && doc.is_connected(target)
        } else if let Some(root) = root_node {
            if let Some(pf) = pseudo_filter {
                target == root && eff.pseudo_element.as_deref() == Some(pf)
            } else if subtree {
                // Walk up ancestors stopping at ShadowRoot boundary unless `root` IS that ShadowRoot!
                let mut cur = Some(target);
                let mut hit = false;
                while let Some(c) = cur {
                    if c == root {
                        hit = true;
                        break;
                    }
                    if matches!(doc.get(c), Some(NodeData::ShadowRoot { .. })) {
                        break;
                    }
                    cur = doc.parent(c);
                }
                hit
            } else {
                target == root && eff.pseudo_element.is_none()
            }
        } else {
            false
        };
        if in_scope {
            matches.push((anim.seq_id, ah));
        }
    }
    matches.sort_by_key(|(seq, _)| *seq);
    matches.into_iter().map(|(_, h)| h).collect()
}

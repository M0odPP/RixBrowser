//! W3C Media Source Extensions (MSE) JS bindings for `ContentRuntime`:
//! - `MediaSource` (`isTypeSupported`, `readyState`, `duration`, `sourceBuffers`,
//!   `activeSourceBuffers`, `addSourceBuffer`, `removeSourceBuffer`, `endOfStream`,
//!   `setLiveSeekableRange`, `clearLiveSeekableRange`)
//! - `SourceBuffer` (`mode`, `updating`, `buffered`, `timestampOffset`,
//!   `appendWindowStart`, `appendWindowEnd`, `appendBuffer`, `abort`, `remove`, `changeType`)
//! - `SourceBufferList` & `TimeRanges`
//! - `HTMLMediaElement` MSE attachment hooks (`src`, `srcObject`, `buffered`, `duration`, `canPlayType`)

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use rix_dom::{NodeData, NodeId};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};
use rix_media::{
    is_type_supported, AppendMode, EndOfStreamError, MediaSourceEngine, MseError, ReadyState,
    TimeRanges,
};

use crate::dom_bridge::SharedHost;
use crate::event_loop::{SharedEventLoop, TaskPriority};
use crate::events::{fire_object_event, SharedEvents};
use crate::web_apis::{
    arg, bind_ctor_proto, create_dom_exception, define_global, define_host_accessor,
    define_host_getter, define_host_method, SharedWebApis,
};
use crate::webgpu_types::extract_gpu_buffer_source;

#[derive(Debug, Clone, Default)]
struct MediaSourceProtos {
    media_source_proto: Option<HeapHandle>,
    source_buffer_proto: Option<HeapHandle>,
    source_buffer_list_proto: Option<HeapHandle>,
    time_ranges_proto: Option<HeapHandle>,
}

/// Host entry backing one JS `MediaSource` instance.
pub struct MediaSourceEntry {
    pub engine: MediaSourceEngine,
    pub source_buffers_list: HeapHandle,
    pub active_source_buffers_list: HeapHandle,
    /// Ordered JS `SourceBuffer` handles matching `engine.source_buffers`.
    pub buffer_handles: Vec<HeapHandle>,
    pub attached_element: Option<NodeId>,
    /// Current playback head in seconds (updated by `HTMLMediaElement` playback in Phase 5.2).
    pub current_playback_time_s: f64,
}

#[derive(Debug, Clone)]
struct SourceBufferRef {
    parent_ms: HeapHandle,
    /// Monotonic token so async tasks ignore aborted/removed buffers cleanly.
    async_generation: u64,
}

/// Per-`ContentRuntime` Media Source Extensions registry.
pub struct MediaSourceSystem {
    protos: MediaSourceProtos,
    pub media_sources: HashMap<HeapHandle, MediaSourceEntry>,
    source_buffers: HashMap<HeapHandle, SourceBufferRef>,
    source_buffer_lists: HashMap<HeapHandle, (HeapHandle, bool)>,
    time_ranges: HashMap<HeapHandle, TimeRanges>,
    /// Media element `NodeId` -> attached `MediaSource` JS handle (via `src` or `srcObject`).
    pub attached_sources: HashMap<NodeId, HeapHandle>,
}

/// Shared handle to [`MediaSourceSystem`].
pub type SharedMediaSource = Rc<RefCell<MediaSourceSystem>>;

impl Default for MediaSourceSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl MediaSourceSystem {
    pub fn new() -> Self {
        Self {
            protos: MediaSourceProtos::default(),
            media_sources: HashMap::new(),
            source_buffers: HashMap::new(),
            source_buffer_lists: HashMap::new(),
            time_ranges: HashMap::new(),
            attached_sources: HashMap::new(),
        }
    }

    /// Returns the index of `sb_handle` inside its parent `MediaSourceEntry`.
    fn locate_source_buffer(&self, sb_handle: HeapHandle) -> Option<(HeapHandle, usize)> {
        let sb_ref = self.source_buffers.get(&sb_handle)?;
        let ms = self.media_sources.get(&sb_ref.parent_ms)?;
        let idx = ms.buffer_handles.iter().position(|&h| h == sb_handle)?;
        Some((sb_ref.parent_ms, idx))
    }

    /// Configures a custom coded-frame byte quota on a `SourceBuffer` (useful for testing eviction).
    pub fn set_source_buffer_quota(&mut self, sb_handle: HeapHandle, quota_bytes: usize) -> bool {
        let Some((ms_h, idx)) = self.locate_source_buffer(sb_handle) else {
            return false;
        };
        if let Some(ms) = self.media_sources.get_mut(&ms_h) {
            if let Some(sb) = ms.engine.source_buffers.get_mut(idx) {
                sb.quota_bytes = quota_bytes;
                return true;
            }
        }
        false
    }

    /// Collects all GC roots retained by live MSE objects.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for p in [
            self.protos.media_source_proto,
            self.protos.source_buffer_proto,
            self.protos.source_buffer_list_proto,
            self.protos.time_ranges_proto,
        ]
        .into_iter()
        .flatten()
        {
            out.push(JsValue::from_object(p));
        }
        for (&ms_h, entry) in &self.media_sources {
            out.push(JsValue::from_object(ms_h));
            out.push(JsValue::from_object(entry.source_buffers_list));
            out.push(JsValue::from_object(entry.active_source_buffers_list));
            for &sb_h in &entry.buffer_handles {
                out.push(JsValue::from_object(sb_h));
            }
        }
        for &ms_h in self.attached_sources.values() {
            out.push(JsValue::from_object(ms_h));
        }
    }

    /// Sweeps dead JS handles after Major GC.
    pub fn sweep_dead(&mut self, heap: &rix_js::GcHeap) {
        self.time_ranges.retain(|&h, _| heap.is_alive(h));
        self.source_buffer_lists.retain(|&h, _| heap.is_alive(h));
        self.source_buffers.retain(|&h, _| heap.is_alive(h));
        self.media_sources.retain(|&h, _| heap.is_alive(h));
        self.attached_sources.retain(|_, &mut h| heap.is_alive(h));
    }

    /// Allocates a JS `TimeRanges` wrapper around `ranges`.
    pub fn alloc_time_ranges(&mut self, vm: &mut JsVm, ranges: TimeRanges) -> JsValue {
        let proto = self.protos.time_ranges_proto.unwrap_or(vm.object_proto);
        let h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        self.time_ranges.insert(h, ranges);
        JsValue::from_object(h)
    }
}

fn mse_err_to_js(vm: &mut JsVm, err: MseError) -> JsException {
    if err.dom_name() == "TypeError" {
        vm.type_error(err.message())
    } else {
        create_dom_exception(vm, err.dom_name(), err.message())
    }
}

fn sync_sbl_indices(vm: &mut JsVm, list_h: HeapHandle, handles: &[HeapHandle]) {
    let old_len = {
        let obj = vm.heap.get_object(list_h);
        obj.map(|o| o.elements.len() as usize).unwrap_or(0)
    };
    for (idx, &sb_h) in handles.iter().enumerate() {
        let key = vm.heap.intern_key(&idx.to_string());
        vm.heap.object_define_data(
            list_h,
            key,
            JsValue::from_object(sb_h),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
    }
    for idx in handles.len()..old_len {
        let key = vm.heap.intern_key(&idx.to_string());
        vm.heap.object_delete_own(list_h, key, &mut vm.shapes);
    }
}

/// Attaches a `MediaSource` handle to `media_node`, transitioning `"closed" -> "open"` and
/// scheduling the asynchronous `"sourceopen"` event on the WHATWG Event Loop.
pub fn attach_media_source_to_element(
    media_node: NodeId,
    ms_handle: HeapHandle,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    ms_sys: &SharedMediaSource,
) {
    // Detach any previous MediaSource from this element first.
    let prev_ms = ms_sys
        .borrow_mut()
        .attached_sources
        .insert(media_node, ms_handle);
    if let Some(old_ms) = prev_ms {
        if old_ms != ms_handle {
            let should_close = ms_sys
                .borrow_mut()
                .media_sources
                .get_mut(&old_ms)
                .map(|e| {
                    e.attached_element = None;
                    e.engine.close()
                })
                .unwrap_or(false);
            if should_close {
                queue_object_event(host, events, event_loop, old_ms, "sourceclose");
            }
        }
    }

    let opened = {
        let mut sys = ms_sys.borrow_mut();
        if let Some(entry) = sys.media_sources.get_mut(&ms_handle) {
            entry.attached_element = Some(media_node);
            entry.engine.open()
        } else {
            false
        }
    };

    if opened {
        queue_object_event(host, events, event_loop, ms_handle, "sourceopen");
    }
}

/// Detaches any `MediaSource` currently attached to `media_node`.
pub fn detach_media_source_from_element(
    media_node: NodeId,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    ms_sys: &SharedMediaSource,
) {
    let old_ms = ms_sys.borrow_mut().attached_sources.remove(&media_node);
    if let Some(ms_h) = old_ms {
        let closed = ms_sys
            .borrow_mut()
            .media_sources
            .get_mut(&ms_h)
            .map(|e| {
                e.attached_element = None;
                e.engine.close()
            })
            .unwrap_or(false);
        if closed {
            queue_object_event(host, events, event_loop, ms_h, "sourceclose");
        }
    }
}

fn queue_object_event(
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    target: HeapHandle,
    ev_type: &'static str,
) {
    let h = Rc::clone(host);
    let ev = Rc::clone(events);
    let el = Rc::clone(event_loop);
    event_loop
        .borrow_mut()
        .post_host_task(TaskPriority::Timer, move |vm| {
            fire_object_event(vm, &h, &ev, &el, target, ev_type, &[]);
        });
}

/// Installs `MediaSource`, `SourceBuffer`, `SourceBufferList`, `TimeRanges`,
/// `HTMLMediaElement` MSE attachment hooks, and `URL.createObjectURL(MediaSource)` support.
pub fn install_media_source(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
) -> SharedMediaSource {
    let ms_sys = Rc::new(RefCell::new(MediaSourceSystem::new()));

    // Register validator so `URL.createObjectURL(mediaSource)` recognizes `MediaSource` instances.
    {
        let sys_v = Rc::clone(&ms_sys);
        web_apis
            .borrow_mut()
            .register_object_url_validator(move |h| sys_v.borrow().media_sources.contains_key(&h));
    }

    install_time_ranges_proto(vm, &ms_sys);
    install_source_buffer_list_proto(vm, host, &ms_sys);
    install_source_buffer_proto(vm, host, events, event_loop, &ms_sys);
    install_media_source_ctor(vm, host, events, event_loop, &ms_sys);
    install_media_element_mse_hooks(vm, host, events, event_loop, web_apis, &ms_sys);

    ms_sys
}

fn install_time_ranges_proto(vm: &mut JsVm, ms_sys: &SharedMediaSource) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    vm.heap.get_object_mut(proto).unwrap().is_prototype = true;
    ms_sys.borrow_mut().protos.time_ranges_proto = Some(proto);

    {
        let sys = Rc::clone(ms_sys);
        define_host_getter(vm, proto, "length", move |vm, this, _| {
            let Some(h) = this.as_object() else {
                return Err(vm.type_error("Incompatible TimeRanges receiver"));
            };
            let len = sys
                .borrow()
                .time_ranges
                .get(&h)
                .map(|tr| tr.len())
                .ok_or_else(|| vm.type_error("Incompatible TimeRanges receiver"))?;
            Ok(JsValue::from_number(len as f64))
        });
    }
    {
        let sys = Rc::clone(ms_sys);
        define_host_method(vm, proto, "start", move |vm, this, args| {
            let Some(h) = this.as_object() else {
                return Err(vm.type_error("Incompatible TimeRanges receiver"));
            };
            let idx = vm.to_number(arg(args, 0))?.max(0.0) as usize;
            let res = sys
                .borrow()
                .time_ranges
                .get(&h)
                .map(|tr| tr.start(idx))
                .ok_or_else(|| vm.type_error("Incompatible TimeRanges receiver"))?;
            res.map(JsValue::from_number)
                .map_err(|e| mse_err_to_js(vm, e))
        });
    }
    {
        let sys = Rc::clone(ms_sys);
        define_host_method(vm, proto, "end", move |vm, this, args| {
            let Some(h) = this.as_object() else {
                return Err(vm.type_error("Incompatible TimeRanges receiver"));
            };
            let idx = vm.to_number(arg(args, 0))?.max(0.0) as usize;
            let res = sys
                .borrow()
                .time_ranges
                .get(&h)
                .map(|tr| tr.end(idx))
                .ok_or_else(|| vm.type_error("Incompatible TimeRanges receiver"))?;
            res.map(JsValue::from_number)
                .map_err(|e| mse_err_to_js(vm, e))
        });
    }

    let ctor = vm.alloc_host_fn("TimeRanges", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: TimeRanges"))
    });
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "TimeRanges", JsValue::from_object(ctor));
}

fn install_source_buffer_list_proto(vm: &mut JsVm, host: &SharedHost, ms_sys: &SharedMediaSource) {
    let et_proto = host.borrow().prototypes.event_target_proto;
    let proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(et_proto)));
    vm.heap.get_object_mut(proto).unwrap().is_prototype = true;
    ms_sys.borrow_mut().protos.source_buffer_list_proto = Some(proto);

    {
        let sys = Rc::clone(ms_sys);
        define_host_getter(vm, proto, "length", move |vm, this, _| {
            let Some(h) = this.as_object() else {
                return Err(vm.type_error("Incompatible SourceBufferList receiver"));
            };
            let s = sys.borrow();
            let Some(&(ms_h, active_only)) = s.source_buffer_lists.get(&h) else {
                return Err(vm.type_error("Incompatible SourceBufferList receiver"));
            };
            let len = s
                .media_sources
                .get(&ms_h)
                .map(|entry| {
                    if active_only {
                        entry
                            .engine
                            .source_buffers
                            .iter()
                            .filter(|sb| !sb.tracks.is_empty() || !sb.buffered.is_empty())
                            .count()
                    } else {
                        entry.buffer_handles.len()
                    }
                })
                .unwrap_or(0);
            Ok(JsValue::from_number(len as f64))
        });
    }

    let ctor = vm.alloc_host_fn("SourceBufferList", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: SourceBufferList"))
    });
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "SourceBufferList", JsValue::from_object(ctor));
}

fn install_source_buffer_proto(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    ms_sys: &SharedMediaSource,
) {
    let et_proto = host.borrow().prototypes.event_target_proto;
    let proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(et_proto)));
    vm.heap.get_object_mut(proto).unwrap().is_prototype = true;
    ms_sys.borrow_mut().protos.source_buffer_proto = Some(proto);

    // `mode` getter/setter
    {
        let sys_g = Rc::clone(ms_sys);
        let sys_s = Rc::clone(ms_sys);
        define_host_accessor(
            vm,
            proto,
            "mode",
            move |vm, this, _| {
                let sb_h = require_sb(vm, &sys_g, this)?;
                let sys = sys_g.borrow();
                let (ms_h, idx) = sys.locate_source_buffer(sb_h).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "SourceBuffer removed")
                })?;
                let mode = sys.media_sources[&ms_h].engine.source_buffers[idx].mode;
                Ok(JsValue::from_string(vm.heap.alloc_string(mode.as_str())))
            },
            move |vm, this, args| {
                let sb_h = require_sb(vm, &sys_s, this)?;
                let mode_str = vm.value_to_rust_string(arg(args, 0));
                let Some(mode) = AppendMode::parse(&mode_str) else {
                    return Err(vm.type_error("Invalid SourceBuffer.mode value"));
                };
                let mut sys = sys_s.borrow_mut();
                let (ms_h, idx) = sys.locate_source_buffer(sb_h).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "SourceBuffer removed")
                })?;
                let res = sys
                    .media_sources
                    .get_mut(&ms_h)
                    .unwrap()
                    .engine
                    .source_buffers[idx]
                    .set_mode(mode);
                res.map_err(|e| mse_err_to_js(vm, e))?;
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // `updating` getter
    {
        let sys = Rc::clone(ms_sys);
        define_host_getter(vm, proto, "updating", move |vm, this, _| {
            let sb_h = require_sb(vm, &sys, this)?;
            let s = sys.borrow();
            let updating = s
                .locate_source_buffer(sb_h)
                .map(|(ms_h, idx)| s.media_sources[&ms_h].engine.source_buffers[idx].updating)
                .unwrap_or(false);
            Ok(JsValue::from_bool(updating))
        });
    }

    // `buffered` getter -> TimeRanges
    {
        let sys = Rc::clone(ms_sys);
        define_host_getter(vm, proto, "buffered", move |vm, this, _| {
            let sb_h = require_sb(vm, &sys, this)?;
            let ranges = {
                let s = sys.borrow();
                let (ms_h, idx) = s.locate_source_buffer(sb_h).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "SourceBuffer removed")
                })?;
                s.media_sources[&ms_h].engine.source_buffers[idx]
                    .buffered
                    .clone()
            };
            Ok(sys.borrow_mut().alloc_time_ranges(vm, ranges))
        });
    }

    // `timestampOffset` getter/setter
    {
        let sys_g = Rc::clone(ms_sys);
        let sys_s = Rc::clone(ms_sys);
        define_host_accessor(
            vm,
            proto,
            "timestampOffset",
            move |vm, this, _| {
                let sb_h = require_sb(vm, &sys_g, this)?;
                let s = sys_g.borrow();
                let (ms_h, idx) = s.locate_source_buffer(sb_h).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "SourceBuffer removed")
                })?;
                Ok(JsValue::from_number(
                    s.media_sources[&ms_h].engine.source_buffers[idx].timestamp_offset,
                ))
            },
            move |vm, this, args| {
                let sb_h = require_sb(vm, &sys_s, this)?;
                let v = vm.to_number(arg(args, 0))?;
                let mut s = sys_s.borrow_mut();
                let (ms_h, idx) = s.locate_source_buffer(sb_h).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "SourceBuffer removed")
                })?;
                s.media_sources
                    .get_mut(&ms_h)
                    .unwrap()
                    .engine
                    .source_buffers[idx]
                    .set_timestamp_offset(v)
                    .map_err(|e| mse_err_to_js(vm, e))?;
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // `appendWindowStart` getter/setter
    {
        let sys_g = Rc::clone(ms_sys);
        let sys_s = Rc::clone(ms_sys);
        define_host_accessor(
            vm,
            proto,
            "appendWindowStart",
            move |vm, this, _| {
                let sb_h = require_sb(vm, &sys_g, this)?;
                let s = sys_g.borrow();
                let (ms_h, idx) = s.locate_source_buffer(sb_h).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "SourceBuffer removed")
                })?;
                Ok(JsValue::from_number(
                    s.media_sources[&ms_h].engine.source_buffers[idx].append_window_start,
                ))
            },
            move |vm, this, args| {
                let sb_h = require_sb(vm, &sys_s, this)?;
                let v = vm.to_number(arg(args, 0))?;
                let mut s = sys_s.borrow_mut();
                let (ms_h, idx) = s.locate_source_buffer(sb_h).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "SourceBuffer removed")
                })?;
                s.media_sources
                    .get_mut(&ms_h)
                    .unwrap()
                    .engine
                    .source_buffers[idx]
                    .set_append_window_start(v)
                    .map_err(|e| mse_err_to_js(vm, e))?;
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // `appendWindowEnd` getter/setter
    {
        let sys_g = Rc::clone(ms_sys);
        let sys_s = Rc::clone(ms_sys);
        define_host_accessor(
            vm,
            proto,
            "appendWindowEnd",
            move |vm, this, _| {
                let sb_h = require_sb(vm, &sys_g, this)?;
                let s = sys_g.borrow();
                let (ms_h, idx) = s.locate_source_buffer(sb_h).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "SourceBuffer removed")
                })?;
                Ok(JsValue::from_number(
                    s.media_sources[&ms_h].engine.source_buffers[idx].append_window_end,
                ))
            },
            move |vm, this, args| {
                let sb_h = require_sb(vm, &sys_s, this)?;
                let v = vm.to_number(arg(args, 0))?;
                let mut s = sys_s.borrow_mut();
                let (ms_h, idx) = s.locate_source_buffer(sb_h).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "SourceBuffer removed")
                })?;
                s.media_sources
                    .get_mut(&ms_h)
                    .unwrap()
                    .engine
                    .source_buffers[idx]
                    .set_append_window_end(v)
                    .map_err(|e| mse_err_to_js(vm, e))?;
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // `appendBuffer(BufferSource)`
    {
        let sys = Rc::clone(ms_sys);
        let h_cl = Rc::clone(host);
        let ev_cl = Rc::clone(events);
        let el_cl = Rc::clone(event_loop);
        define_host_method(vm, proto, "appendBuffer", move |vm, this, args| {
            let sb_h = require_sb(vm, &sys, this)?;
            let data =
                extract_gpu_buffer_source(vm, arg(args, 0), None, None).ok_or_else(|| {
                    vm.type_error("appendBuffer requires an ArrayBuffer or ArrayBufferView")
                })?;

            let (ms_h, gen, reopened) = {
                let mut s = sys.borrow_mut();
                let (ms_h, idx) = s.locate_source_buffer(sb_h).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "SourceBuffer removed")
                })?;
                let ms_entry = s.media_sources.get_mut(&ms_h).unwrap();
                if ms_entry.engine.ready_state == ReadyState::Closed {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "MediaSource is closed",
                    ));
                }
                let sb = &mut ms_entry.engine.source_buffers[idx];
                if sb.updating {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "SourceBuffer is currently updating",
                    ));
                }
                if data.len() > sb.quota_bytes {
                    return Err(create_dom_exception(
                        vm,
                        "QuotaExceededError",
                        "Appended buffer exceeds SourceBuffer quota",
                    ));
                }
                let reopened = ms_entry.engine.reopen_if_ended();
                ms_entry.engine.source_buffers[idx].updating = true;
                let sb_ref = s.source_buffers.get_mut(&sb_h).unwrap();
                sb_ref.async_generation = sb_ref.async_generation.wrapping_add(1);
                (ms_h, sb_ref.async_generation, reopened)
            };

            if reopened {
                queue_object_event(&h_cl, &ev_cl, &el_cl, ms_h, "sourceopen");
            }
            queue_object_event(&h_cl, &ev_cl, &el_cl, sb_h, "updatestart");

            let sys_task = Rc::clone(&sys);
            let h_task = Rc::clone(&h_cl);
            let ev_task = Rc::clone(&ev_cl);
            let el_task = Rc::clone(&el_cl);
            el_cl
                .borrow_mut()
                .post_host_task(TaskPriority::Timer, move |vm| {
                    let outcome = {
                        let mut s = sys_task.borrow_mut();
                        let still_valid = s
                            .source_buffers
                            .get(&sb_h)
                            .is_some_and(|r| r.async_generation == gen);
                        if !still_valid {
                            return;
                        }
                        let Some((parent_ms, idx)) = s.locate_source_buffer(sb_h) else {
                            return;
                        };
                        let entry = s.media_sources.get_mut(&parent_ms).unwrap();
                        let play_t = entry.current_playback_time_s;
                        let res = entry.engine.source_buffers[idx].append_buffer(&data, play_t);
                        entry.engine.source_buffers[idx].updating = false;
                        match res {
                            Ok(app) => {
                                entry
                                    .engine
                                    .sync_duration_after_append(app.highest_end_timestamp);
                                let active_h = entry.active_source_buffers_list;
                                let active_handles: Vec<HeapHandle> = entry
                                    .buffer_handles
                                    .iter()
                                    .enumerate()
                                    .filter_map(|(i, &bh)| {
                                        let sb = &entry.engine.source_buffers[i];
                                        if !sb.tracks.is_empty() || !sb.buffered.is_empty() {
                                            Some(bh)
                                        } else {
                                            None
                                        }
                                    })
                                    .collect();
                                Some((Ok(()), active_h, active_handles))
                            }
                            Err(err) => {
                                Some((Err(err), entry.active_source_buffers_list, Vec::new()))
                            }
                        }
                    };

                    if let Some((res, active_list_h, active_handles)) = outcome {
                        if res.is_ok() {
                            sync_sbl_indices(vm, active_list_h, &active_handles);
                            fire_object_event(vm, &h_task, &ev_task, &el_task, sb_h, "update", &[]);
                        } else {
                            fire_object_event(vm, &h_task, &ev_task, &el_task, sb_h, "error", &[]);
                        }
                        fire_object_event(vm, &h_task, &ev_task, &el_task, sb_h, "updateend", &[]);
                    }
                });

            Ok(JsValue::UNDEFINED)
        });
    }

    // `remove(start, end)`
    {
        let sys = Rc::clone(ms_sys);
        let h_cl = Rc::clone(host);
        let ev_cl = Rc::clone(events);
        let el_cl = Rc::clone(event_loop);
        define_host_method(vm, proto, "remove", move |vm, this, args| {
            let sb_h = require_sb(vm, &sys, this)?;
            let start = vm.to_number(arg(args, 0))?;
            let end = vm.to_number(arg(args, 1))?;
            if !start.is_finite() || start < 0.0 || end.is_nan() || end <= start {
                return Err(
                    vm.type_error("SourceBuffer.remove requires finite start >= 0 and end > start")
                );
            }

            let (ms_h, gen, reopened) = {
                let mut s = sys.borrow_mut();
                let (ms_h, idx) = s.locate_source_buffer(sb_h).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "SourceBuffer removed")
                })?;
                let ms_entry = s.media_sources.get_mut(&ms_h).unwrap();
                if ms_entry.engine.ready_state == ReadyState::Closed {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "MediaSource is closed",
                    ));
                }
                if ms_entry.engine.source_buffers[idx].updating {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "SourceBuffer is currently updating",
                    ));
                }
                let reopened = ms_entry.engine.reopen_if_ended();
                ms_entry.engine.source_buffers[idx].updating = true;
                let sb_ref = s.source_buffers.get_mut(&sb_h).unwrap();
                sb_ref.async_generation = sb_ref.async_generation.wrapping_add(1);
                (ms_h, sb_ref.async_generation, reopened)
            };

            if reopened {
                queue_object_event(&h_cl, &ev_cl, &el_cl, ms_h, "sourceopen");
            }
            queue_object_event(&h_cl, &ev_cl, &el_cl, sb_h, "updatestart");

            let sys_task = Rc::clone(&sys);
            let h_task = Rc::clone(&h_cl);
            let ev_task = Rc::clone(&ev_cl);
            let el_task = Rc::clone(&el_cl);
            el_cl
                .borrow_mut()
                .post_host_task(TaskPriority::Timer, move |vm| {
                    let ran = {
                        let mut s = sys_task.borrow_mut();
                        let still_valid = s
                            .source_buffers
                            .get(&sb_h)
                            .is_some_and(|r| r.async_generation == gen);
                        if !still_valid {
                            return;
                        }
                        let Some((parent_ms, idx)) = s.locate_source_buffer(sb_h) else {
                            return;
                        };
                        let sb = &mut s
                            .media_sources
                            .get_mut(&parent_ms)
                            .unwrap()
                            .engine
                            .source_buffers[idx];
                        sb.updating = false;
                        sb.remove(start, end).is_ok()
                    };
                    if ran {
                        fire_object_event(vm, &h_task, &ev_task, &el_task, sb_h, "update", &[]);
                        fire_object_event(vm, &h_task, &ev_task, &el_task, sb_h, "updateend", &[]);
                    }
                });

            Ok(JsValue::UNDEFINED)
        });
    }

    // `abort()`
    {
        let sys = Rc::clone(ms_sys);
        let h_cl = Rc::clone(host);
        let ev_cl = Rc::clone(events);
        let el_cl = Rc::clone(event_loop);
        define_host_method(vm, proto, "abort", move |vm, this, _| {
            let sb_h = require_sb(vm, &sys, this)?;
            let was_updating = {
                let mut s = sys.borrow_mut();
                let (ms_h, idx) = s.locate_source_buffer(sb_h).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "SourceBuffer removed")
                })?;
                let ms_entry = s.media_sources.get_mut(&ms_h).unwrap();
                if ms_entry.engine.ready_state != ReadyState::Open {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "SourceBuffer.abort requires MediaSource.readyState === 'open'",
                    ));
                }
                let was_up = ms_entry.engine.source_buffers[idx].updating;
                ms_entry.engine.source_buffers[idx].abort();
                if let Some(r) = s.source_buffers.get_mut(&sb_h) {
                    r.async_generation = r.async_generation.wrapping_add(1);
                }
                was_up
            };
            if was_updating {
                queue_object_event(&h_cl, &ev_cl, &el_cl, sb_h, "abort");
                queue_object_event(&h_cl, &ev_cl, &el_cl, sb_h, "updateend");
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    // `changeType(type)`
    {
        let sys = Rc::clone(ms_sys);
        let h_cl = Rc::clone(host);
        let ev_cl = Rc::clone(events);
        let el_cl = Rc::clone(event_loop);
        define_host_method(vm, proto, "changeType", move |vm, this, args| {
            let sb_h = require_sb(vm, &sys, this)?;
            let mime = vm.value_to_rust_string(arg(args, 0));
            if mime.trim().is_empty() {
                return Err(vm.type_error("SourceBuffer.changeType requires a non-empty MIME type"));
            }
            let (ms_h, reopened) = {
                let mut s = sys.borrow_mut();
                let (ms_h, idx) = s.locate_source_buffer(sb_h).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "SourceBuffer removed")
                })?;
                let ms_entry = s.media_sources.get_mut(&ms_h).unwrap();
                if ms_entry.engine.ready_state == ReadyState::Closed {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "MediaSource is closed",
                    ));
                }
                ms_entry.engine.source_buffers[idx]
                    .change_type(&mime)
                    .map_err(|e| mse_err_to_js(vm, e))?;
                let reopened = ms_entry.engine.reopen_if_ended();
                (ms_h, reopened)
            };
            if reopened {
                queue_object_event(&h_cl, &ev_cl, &el_cl, ms_h, "sourceopen");
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    let ctor = vm.alloc_host_fn("SourceBuffer", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: SourceBuffer"))
    });
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "SourceBuffer", JsValue::from_object(ctor));
}

fn install_media_source_ctor(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    ms_sys: &SharedMediaSource,
) {
    let et_proto = host.borrow().prototypes.event_target_proto;
    let proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(et_proto)));
    vm.heap.get_object_mut(proto).unwrap().is_prototype = true;
    ms_sys.borrow_mut().protos.media_source_proto = Some(proto);

    // `readyState`
    {
        let sys = Rc::clone(ms_sys);
        define_host_getter(vm, proto, "readyState", move |vm, this, _| {
            let ms_h = require_ms(vm, &sys, this)?;
            let rs = sys.borrow().media_sources[&ms_h].engine.ready_state;
            Ok(JsValue::from_string(vm.heap.alloc_string(rs.as_str())))
        });
    }

    // `duration` get/set
    {
        let sys_g = Rc::clone(ms_sys);
        let sys_s = Rc::clone(ms_sys);
        define_host_accessor(
            vm,
            proto,
            "duration",
            move |vm, this, _| {
                let ms_h = require_ms(vm, &sys_g, this)?;
                Ok(JsValue::from_number(
                    sys_g.borrow().media_sources[&ms_h].engine.duration,
                ))
            },
            move |vm, this, args| {
                let ms_h = require_ms(vm, &sys_s, this)?;
                let d = vm.to_number(arg(args, 0))?;
                sys_s
                    .borrow_mut()
                    .media_sources
                    .get_mut(&ms_h)
                    .unwrap()
                    .engine
                    .set_duration(d)
                    .map_err(|e| mse_err_to_js(vm, e))?;
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // `sourceBuffers` & `activeSourceBuffers`
    {
        let sys = Rc::clone(ms_sys);
        define_host_getter(vm, proto, "sourceBuffers", move |vm, this, _| {
            let ms_h = require_ms(vm, &sys, this)?;
            Ok(JsValue::from_object(
                sys.borrow().media_sources[&ms_h].source_buffers_list,
            ))
        });
    }
    {
        let sys = Rc::clone(ms_sys);
        define_host_getter(vm, proto, "activeSourceBuffers", move |vm, this, _| {
            let ms_h = require_ms(vm, &sys, this)?;
            Ok(JsValue::from_object(
                sys.borrow().media_sources[&ms_h].active_source_buffers_list,
            ))
        });
    }

    // `addSourceBuffer(type)`
    {
        let sys = Rc::clone(ms_sys);
        let h_cl = Rc::clone(host);
        let ev_cl = Rc::clone(events);
        let el_cl = Rc::clone(event_loop);
        define_host_method(vm, proto, "addSourceBuffer", move |vm, this, args| {
            let ms_h = require_ms(vm, &sys, this)?;
            let mime = vm.value_to_rust_string(arg(args, 0));

            let sb_proto = sys
                .borrow()
                .protos
                .source_buffer_proto
                .unwrap_or(vm.object_proto);
            let sb_h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(sb_proto)));

            let (sbl_h, all_handles) = {
                let mut s = sys.borrow_mut();
                let entry = s.media_sources.get_mut(&ms_h).unwrap();
                entry
                    .engine
                    .add_source_buffer(&mime)
                    .map_err(|e| mse_err_to_js(vm, e))?;
                entry.buffer_handles.push(sb_h);
                let sbl_h = entry.source_buffers_list;
                let all_handles = entry.buffer_handles.clone();
                s.source_buffers.insert(
                    sb_h,
                    SourceBufferRef {
                        parent_ms: ms_h,
                        async_generation: 0,
                    },
                );
                (sbl_h, all_handles)
            };

            sync_sbl_indices(vm, sbl_h, &all_handles);
            queue_object_event(&h_cl, &ev_cl, &el_cl, sbl_h, "addsourcebuffer");
            Ok(JsValue::from_object(sb_h))
        });
    }

    // `removeSourceBuffer(sourceBuffer)`
    {
        let sys = Rc::clone(ms_sys);
        let h_cl = Rc::clone(host);
        let ev_cl = Rc::clone(events);
        let el_cl = Rc::clone(event_loop);
        define_host_method(vm, proto, "removeSourceBuffer", move |vm, this, args| {
            let ms_h = require_ms(vm, &sys, this)?;
            let Some(sb_h) = arg(args, 0).as_object() else {
                return Err(vm.type_error("removeSourceBuffer requires a SourceBuffer"));
            };

            let (sbl_h, active_h, remaining, active_rem) = {
                let mut s = sys.borrow_mut();
                let entry = s.media_sources.get_mut(&ms_h).unwrap();
                let Some(idx) = entry.buffer_handles.iter().position(|&h| h == sb_h) else {
                    return Err(create_dom_exception(
                        vm,
                        "NotFoundError",
                        "SourceBuffer does not belong to this MediaSource",
                    ));
                };
                entry.buffer_handles.remove(idx);
                let _ = entry.engine.remove_source_buffer(idx);
                let sbl_h = entry.source_buffers_list;
                let active_h = entry.active_source_buffers_list;
                let remaining = entry.buffer_handles.clone();
                let active_rem: Vec<HeapHandle> = entry
                    .buffer_handles
                    .iter()
                    .enumerate()
                    .filter_map(|(i, &bh)| {
                        let sb = &entry.engine.source_buffers[i];
                        if !sb.tracks.is_empty() || !sb.buffered.is_empty() {
                            Some(bh)
                        } else {
                            None
                        }
                    })
                    .collect();
                s.source_buffers.remove(&sb_h);
                (sbl_h, active_h, remaining, active_rem)
            };

            sync_sbl_indices(vm, sbl_h, &remaining);
            sync_sbl_indices(vm, active_h, &active_rem);
            queue_object_event(&h_cl, &ev_cl, &el_cl, sbl_h, "removesourcebuffer");
            Ok(JsValue::UNDEFINED)
        });
    }

    // `endOfStream(error?)`
    {
        let sys = Rc::clone(ms_sys);
        let h_cl = Rc::clone(host);
        let ev_cl = Rc::clone(events);
        let el_cl = Rc::clone(event_loop);
        define_host_method(vm, proto, "endOfStream", move |vm, this, args| {
            let ms_h = require_ms(vm, &sys, this)?;
            let err_arg = arg(args, 0);
            let eos_err = if err_arg.is_undefined() {
                None
            } else {
                let s = vm.value_to_rust_string(err_arg);
                let Some(parsed) = EndOfStreamError::parse(&s) else {
                    return Err(vm.type_error("Invalid endOfStream error parameter"));
                };
                Some(parsed)
            };
            sys.borrow_mut()
                .media_sources
                .get_mut(&ms_h)
                .unwrap()
                .engine
                .end_of_stream(eos_err)
                .map_err(|e| mse_err_to_js(vm, e))?;
            queue_object_event(&h_cl, &ev_cl, &el_cl, ms_h, "sourceended");
            Ok(JsValue::UNDEFINED)
        });
    }

    // `setLiveSeekableRange(start, end)` / `clearLiveSeekableRange()`
    {
        let sys = Rc::clone(ms_sys);
        define_host_method(vm, proto, "setLiveSeekableRange", move |vm, this, args| {
            let ms_h = require_ms(vm, &sys, this)?;
            let start = vm.to_number(arg(args, 0))?;
            let end = vm.to_number(arg(args, 1))?;
            sys.borrow_mut()
                .media_sources
                .get_mut(&ms_h)
                .unwrap()
                .engine
                .set_live_seekable_range(start, end)
                .map_err(|e| mse_err_to_js(vm, e))?;
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let sys = Rc::clone(ms_sys);
        define_host_method(vm, proto, "clearLiveSeekableRange", move |vm, this, _| {
            let ms_h = require_ms(vm, &sys, this)?;
            sys.borrow_mut()
                .media_sources
                .get_mut(&ms_h)
                .unwrap()
                .engine
                .clear_live_seekable_range()
                .map_err(|e| mse_err_to_js(vm, e))?;
            Ok(JsValue::UNDEFINED)
        });
    }

    // Constructor `new MediaSource()`
    let sys_ctor = Rc::clone(ms_sys);
    let ctor = vm.alloc_host_fn("MediaSource", move |vm, _, _| {
        let (ms_proto, sbl_proto) = {
            let s = sys_ctor.borrow();
            (
                s.protos.media_source_proto.unwrap_or(vm.object_proto),
                s.protos.source_buffer_list_proto.unwrap_or(vm.object_proto),
            )
        };
        let ms_h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(ms_proto)));
        let sbl_h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(sbl_proto)));
        let active_h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(sbl_proto)));

        let mut s = sys_ctor.borrow_mut();
        s.source_buffer_lists.insert(sbl_h, (ms_h, false));
        s.source_buffer_lists.insert(active_h, (ms_h, true));
        s.media_sources.insert(
            ms_h,
            MediaSourceEntry {
                engine: MediaSourceEngine::new(),
                source_buffers_list: sbl_h,
                active_source_buffers_list: active_h,
                buffer_handles: Vec::new(),
                attached_element: None,
                current_playback_time_s: 0.0,
            },
        );
        Ok(JsValue::from_object(ms_h))
    });

    // Static `MediaSource.isTypeSupported(type)`
    define_host_method(vm, ctor, "isTypeSupported", |vm, _, args| {
        let mime = vm.value_to_rust_string(arg(args, 0));
        Ok(JsValue::from_bool(is_type_supported(&mime)))
    });

    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "MediaSource", JsValue::from_object(ctor));
}

fn install_media_element_mse_hooks(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    ms_sys: &SharedMediaSource,
) {
    let media_proto = host.borrow().prototypes.html_media_element_proto;
    let video_proto = host.borrow().prototypes.html_video_element_proto;
    let audio_proto = host.borrow().prototypes.html_audio_element_proto;

    // `src` property on HTMLMediaElement.prototype
    {
        let h_g = Rc::clone(host);
        let h_s = Rc::clone(host);
        let ev_s = Rc::clone(events);
        let el_s = Rc::clone(event_loop);
        let wa_s = Rc::clone(web_apis);
        let ms_s = Rc::clone(ms_sys);
        define_host_accessor(
            vm,
            media_proto,
            "src",
            move |vm, this, _| {
                let Some(nid) = h_g.borrow().node_for_value(vm, this) else {
                    return Ok(JsValue::from_string(vm.heap.alloc_string("")));
                };
                let val = match h_g.borrow().document.borrow().get(nid) {
                    Some(NodeData::Element(el)) => {
                        el.get_attribute("src").unwrap_or("").to_string()
                    }
                    _ => String::new(),
                };
                Ok(JsValue::from_string(vm.heap.alloc_string(&val)))
            },
            move |vm, this, args| {
                let Some(nid) = h_s.borrow().node_for_value(vm, this) else {
                    return Ok(JsValue::UNDEFINED);
                };
                let url = vm.value_to_rust_string(arg(args, 0));
                if let Some(NodeData::Element(el)) = h_s.borrow().document.borrow_mut().get_mut(nid)
                {
                    el.set_attribute("src", &url);
                }
                let target_ms = wa_s.borrow().object_url_objects.get(&url).copied();
                if let Some(ms_h) = target_ms {
                    if ms_s.borrow().media_sources.contains_key(&ms_h) {
                        attach_media_source_to_element(nid, ms_h, &h_s, &ev_s, &el_s, &ms_s);
                        return Ok(JsValue::UNDEFINED);
                    }
                }
                detach_media_source_from_element(nid, &h_s, &ev_s, &el_s, &ms_s);
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // `srcObject` property on HTMLMediaElement.prototype
    {
        let h_g = Rc::clone(host);
        let ms_g = Rc::clone(ms_sys);
        let h_s = Rc::clone(host);
        let ev_s = Rc::clone(events);
        let el_s = Rc::clone(event_loop);
        let ms_s = Rc::clone(ms_sys);
        define_host_accessor(
            vm,
            media_proto,
            "srcObject",
            move |vm, this, _| {
                let Some(nid) = h_g.borrow().node_for_value(vm, this) else {
                    return Ok(JsValue::NULL);
                };
                let val = ms_g
                    .borrow()
                    .attached_sources
                    .get(&nid)
                    .copied()
                    .map(JsValue::from_object)
                    .unwrap_or(JsValue::NULL);
                Ok(val)
            },
            move |vm, this, args| {
                let Some(nid) = h_s.borrow().node_for_value(vm, this) else {
                    return Ok(JsValue::UNDEFINED);
                };
                let val = arg(args, 0);
                if val.is_null() || val.is_undefined() {
                    detach_media_source_from_element(nid, &h_s, &ev_s, &el_s, &ms_s);
                    return Ok(JsValue::UNDEFINED);
                }
                if let Some(obj_h) = val.as_object() {
                    if ms_s.borrow().media_sources.contains_key(&obj_h) {
                        attach_media_source_to_element(nid, obj_h, &h_s, &ev_s, &el_s, &ms_s);
                        return Ok(JsValue::UNDEFINED);
                    }
                }
                Err(vm.type_error("srcObject must be a MediaSource or null"))
            },
        );
    }

    // `buffered` getter on HTMLMediaElement.prototype
    {
        let h_g = Rc::clone(host);
        let ms_g = Rc::clone(ms_sys);
        define_host_getter(vm, media_proto, "buffered", move |vm, this, _| {
            let ranges = if let Some(nid) = h_g.borrow().node_for_value(vm, this) {
                let s = ms_g.borrow();
                s.attached_sources
                    .get(&nid)
                    .and_then(|ms_h| s.media_sources.get(ms_h))
                    .map(|entry| entry.engine.buffered())
                    .unwrap_or_default()
            } else {
                TimeRanges::new()
            };
            Ok(ms_g.borrow_mut().alloc_time_ranges(vm, ranges))
        });
    }

    // `duration` getter on HTMLMediaElement.prototype
    {
        let h_g = Rc::clone(host);
        let ms_g = Rc::clone(ms_sys);
        define_host_getter(vm, media_proto, "duration", move |vm, this, _| {
            let dur = if let Some(nid) = h_g.borrow().node_for_value(vm, this) {
                let s = ms_g.borrow();
                s.attached_sources
                    .get(&nid)
                    .and_then(|ms_h| s.media_sources.get(ms_h))
                    .map(|entry| entry.engine.duration)
                    .unwrap_or(f64::NAN)
            } else {
                f64::NAN
            };
            Ok(JsValue::from_number(dur))
        });
    }

    // `canPlayType(type)` on HTMLMediaElement.prototype
    define_host_method(vm, media_proto, "canPlayType", |vm, _, args| {
        let mime = vm.value_to_rust_string(arg(args, 0));
        let lower = mime.trim().to_ascii_lowercase();
        let ans = if is_type_supported(&mime) {
            if lower.contains("codecs=") {
                "probably"
            } else {
                "maybe"
            }
        } else if matches!(
            lower.as_str(),
            "audio/ogg" | "audio/wav" | "audio/mpeg" | "audio/mp3" | "audio/flac"
        ) {
            "maybe"
        } else {
            ""
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(ans)))
    });

    // Expose standard global constructors
    for (name, proto) in [
        ("HTMLMediaElement", media_proto),
        ("HTMLVideoElement", video_proto),
        ("HTMLAudioElement", audio_proto),
    ] {
        let ctor = vm.alloc_host_fn(name, move |vm, _, _| {
            Err(vm.type_error(format!("Illegal constructor: {name}")))
        });
        bind_ctor_proto(vm, ctor, proto);
        define_global(vm, name, JsValue::from_object(ctor));
    }
}

fn require_ms(
    vm: &mut JsVm,
    ms_sys: &SharedMediaSource,
    this: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = this.as_object() else {
        return Err(vm.type_error("Incompatible MediaSource receiver"));
    };
    if ms_sys.borrow().media_sources.contains_key(&h) {
        Ok(h)
    } else {
        Err(vm.type_error("Incompatible MediaSource receiver"))
    }
}

fn require_sb(
    vm: &mut JsVm,
    ms_sys: &SharedMediaSource,
    this: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = this.as_object() else {
        return Err(vm.type_error("Incompatible SourceBuffer receiver"));
    };
    if ms_sys.borrow().source_buffers.contains_key(&h) {
        Ok(h)
    } else {
        Err(vm.type_error("Incompatible SourceBuffer receiver"))
    }
}

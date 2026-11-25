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
    is_type_supported, AppendMode, EndOfStreamError, MediaSourceEngine, MseError, PendingBufferOp,
    ReadyState, TimeRanges, TrackKind,
};

use crate::dom_bridge::{wrap_node, SharedHost};
use crate::dom_interfaces::define_to_string_tag;
use crate::event_loop::{SharedEventLoop, TaskPriority};
use crate::events::{fire_object_event, fire_object_event_with_proto, SharedEvents};
use crate::media_elements::{
    time_is_buffered, SharedMediaElements, HAVE_ENOUGH_DATA, HAVE_FUTURE_DATA, HAVE_METADATA,
    HAVE_NOTHING, MEDIA_ERR_DECODE, MEDIA_ERR_NETWORK, MEDIA_ERR_SRC_NOT_SUPPORTED,
    NETWORK_LOADING, NETWORK_NO_SOURCE,
};
use crate::web_apis::{
    arg, bind_ctor_proto, create_dom_exception, define_global, define_host_accessor,
    define_host_getter, define_host_method, SharedWebApis,
};
use crate::webgpu_types::extract_gpu_buffer_source;

#[derive(Debug, Clone, Default)]
struct MediaSourceProtos {
    media_source_proto: Option<HeapHandle>,
    managed_media_source_proto: Option<HeapHandle>,
    source_buffer_proto: Option<HeapHandle>,
    managed_source_buffer_proto: Option<HeapHandle>,
    source_buffer_list_proto: Option<HeapHandle>,
    time_ranges_proto: Option<HeapHandle>,
    buffered_change_event_proto: Option<HeapHandle>,
    quota_exceeded_error_proto: Option<HeapHandle>,
}

/// Host entry backing one JS `MediaSource` (or `ManagedMediaSource`) instance.
pub struct MediaSourceEntry {
    pub engine: MediaSourceEngine,
    pub source_buffers_list: HeapHandle,
    pub active_source_buffers_list: HeapHandle,
    /// Ordered JS `SourceBuffer` handles matching `engine.source_buffers`.
    pub buffer_handles: Vec<HeapHandle>,
    /// Currently active `SourceBuffer` handles (for `addsourcebuffer`/`removesourcebuffer`).
    pub active_buffer_handles: Vec<HeapHandle>,
    /// Explicitly disabled/deselected `SourceBuffer` handles via `audioTracks`/`videoTracks`.
    pub disabled_buffers: std::collections::HashSet<HeapHandle>,
    pub attached_element: Option<NodeId>,
    /// Current playback head in seconds (updated by `HTMLMediaElement` playback).
    pub current_playback_time_s: f64,
    pub is_managed: bool,
    pub streaming: bool,
    pub sbl_synced_len: usize,
    pub active_sbl_synced_len: usize,
}

#[derive(Debug, Clone)]
struct SourceBufferRef {
    parent_ms: HeapHandle,
    /// Monotonic token so async tasks ignore aborted/removed buffers cleanly.
    async_generation: u64,
    is_managed: bool,
    cached_buffered: Option<(HeapHandle, TimeRanges)>,
    audio_tracks_list: HeapHandle,
    video_tracks_list: HeapHandle,
    audio_track_objs: Vec<HeapHandle>,
    video_track_objs: Vec<HeapHandle>,
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
    /// Persistent `HTMLMediaElement.{audioTracks,videoTracks}` lists `(list_h, track_handles, synced_len)`.
    elem_audio_lists: HashMap<NodeId, (HeapHandle, Vec<HeapHandle>, usize)>,
    elem_video_lists: HashMap<NodeId, (HeapHandle, Vec<HeapHandle>, usize)>,
    /// Track JS handle -> `(owning_sb, is_video, enabled_or_selected, attached_to_sb)`.
    track_states: HashMap<HeapHandle, (HeapHandle, bool, bool, bool)>,
    media_elements: Option<SharedMediaElements>,
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
            elem_audio_lists: HashMap::new(),
            elem_video_lists: HashMap::new(),
            track_states: HashMap::new(),
            media_elements: None,
        }
    }

    /// Computes active `SourceBuffer` handles for `entry` respecting track enablement.
    fn compute_active_handles(entry: &MediaSourceEntry) -> Vec<HeapHandle> {
        entry
            .buffer_handles
            .iter()
            .enumerate()
            .filter_map(|(i, &bh)| {
                if entry.disabled_buffers.contains(&bh) {
                    return None;
                }
                let sb = entry.engine.source_buffers.get(i)?;
                if sb.init_segment_received || !sb.tracks.is_empty() || !sb.buffered.is_empty() {
                    Some(bh)
                } else {
                    None
                }
            })
            .collect()
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
            self.protos.managed_media_source_proto,
            self.protos.source_buffer_proto,
            self.protos.managed_source_buffer_proto,
            self.protos.source_buffer_list_proto,
            self.protos.time_ranges_proto,
            self.protos.buffered_change_event_proto,
            self.protos.quota_exceeded_error_proto,
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
        for (&sb_h, sb_ref) in &self.source_buffers {
            if let Some((tr_h, _)) = &sb_ref.cached_buffered {
                out.push(JsValue::from_object(*tr_h));
            }
            out.push(JsValue::from_object(sb_ref.audio_tracks_list));
            out.push(JsValue::from_object(sb_ref.video_tracks_list));
            for &th in &sb_ref.audio_track_objs {
                out.push(JsValue::from_object(th));
            }
            for &th in &sb_ref.video_track_objs {
                out.push(JsValue::from_object(th));
            }
            if self.media_sources.contains_key(&sb_ref.parent_ms) {
                out.push(JsValue::from_object(sb_h));
            }
        }
        for (lh, tracks, _) in self
            .elem_audio_lists
            .values()
            .chain(self.elem_video_lists.values())
        {
            out.push(JsValue::from_object(*lh));
            for &th in tracks {
                out.push(JsValue::from_object(th));
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
        self.track_states.retain(|&h, _| heap.is_alive(h));
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

fn is_shared_buffer_source(vm: &JsVm, val: JsValue) -> bool {
    let Some(h) = val.as_object() else {
        return false;
    };
    let buf_h = match vm.heap.get_object(h) {
        Some(o) => match &o.kind {
            ObjectKind::ArrayBuffer(_) => h,
            ObjectKind::TypedArray(ta) => ta.buffer,
            ObjectKind::DataView(dv) => dv.buffer,
            _ => return false,
        },
        None => return false,
    };
    vm.heap
        .get_object(buf_h)
        .is_some_and(|o| o.rare.as_deref().is_some_and(|r| r.shared_mem.is_some()))
}

fn diff_time_ranges(old: &TimeRanges, new: &TimeRanges) -> (TimeRanges, TimeRanges) {
    let mut added = new.clone();
    for &(s, e) in old.as_slice() {
        added.remove_range(s, e);
    }
    let mut removed = old.clone();
    for &(s, e) in new.as_slice() {
        removed.remove_range(s, e);
    }
    (added, removed)
}

#[allow(clippy::too_many_arguments)]
fn fire_buffered_change_event(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    ms_sys: &SharedMediaSource,
    sb_h: HeapHandle,
    added: TimeRanges,
    removed: TimeRanges,
) {
    if added.is_empty() && removed.is_empty() {
        return;
    }
    let (ev_proto, added_v, removed_v) = {
        let mut s = ms_sys.borrow_mut();
        let p = s.protos.buffered_change_event_proto;
        let a = s.alloc_time_ranges(vm, added);
        let r = s.alloc_time_ranges(vm, removed);
        (p, a, r)
    };
    let Some(proto) = ev_proto else {
        return;
    };
    fire_object_event_with_proto(
        vm,
        host,
        events,
        event_loop,
        sb_h,
        "bufferedchange",
        proto,
        &[("addedRanges", added_v), ("removedRanges", removed_v)],
    );
}

fn sync_sbl_indices(
    vm: &mut JsVm,
    list_h: HeapHandle,
    handles: &[HeapHandle],
    prev_len: &mut usize,
) {
    let old_len = (*prev_len).max(handles.len());
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
    *prev_len = handles.len();
}

fn sync_track_list_props(
    vm: &mut JsVm,
    list_h: HeapHandle,
    handles: &[HeapHandle],
    prev_len: &mut usize,
) {
    sync_sbl_indices(vm, list_h, handles, prev_len);
    let lk = vm.heap.intern_key("length");
    vm.heap.object_define_data(
        list_h,
        lk,
        JsValue::from_number(handles.len() as f64),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
}

fn ensure_elem_track_list(
    vm: &mut JsVm,
    host: &SharedHost,
    ms_sys: &SharedMediaSource,
    nid: NodeId,
    is_video: bool,
) -> HeapHandle {
    {
        let s = ms_sys.borrow();
        let existing = if is_video {
            s.elem_video_lists.get(&nid).map(|t| t.0)
        } else {
            s.elem_audio_lists.get(&nid).map(|t| t.0)
        };
        if let Some(lh) = existing {
            return lh;
        }
    }
    let et_proto = host.borrow().prototypes.event_target_proto;
    let lh = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(et_proto)));
    let lk = vm.heap.intern_key("length");
    vm.heap.object_define_data(
        lh,
        lk,
        JsValue::from_number(0.0),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    let mut s = ms_sys.borrow_mut();
    if is_video {
        s.elem_video_lists.insert(nid, (lh, Vec::new(), 0));
    } else {
        s.elem_audio_lists.insert(nid, (lh, Vec::new(), 0));
    }
    lh
}

#[allow(clippy::too_many_arguments)]
fn populate_tracks_for_source_buffer(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    ms_sys: &SharedMediaSource,
    ms_h: HeapHandle,
    sb_h: HeapHandle,
    attached_nid: Option<NodeId>,
) {
    let (tracks_snapshot, already_populated, sb_audio_lh, sb_video_lh) = {
        let s = ms_sys.borrow();
        let Some((parent_ms, idx)) = s.locate_source_buffer(sb_h) else {
            return;
        };
        let Some(sb_ref) = s.source_buffers.get(&sb_h) else {
            return;
        };
        let already = !sb_ref.audio_track_objs.is_empty() || !sb_ref.video_track_objs.is_empty();
        let tracks = s.media_sources[&parent_ms].engine.source_buffers[idx]
            .tracks
            .clone();
        (
            tracks,
            already,
            sb_ref.audio_tracks_list,
            sb_ref.video_tracks_list,
        )
    };
    if already_populated || tracks_snapshot.is_empty() {
        return;
    }

    let elem_audio_lh =
        attached_nid.map(|nid| ensure_elem_track_list(vm, host, ms_sys, nid, false));
    let elem_video_lh = attached_nid.map(|nid| ensure_elem_track_list(vm, host, ms_sys, nid, true));

    let mut new_audio: Vec<HeapHandle> = Vec::new();
    let mut new_video: Vec<HeapHandle> = Vec::new();
    for track in &tracks_snapshot {
        let is_video = matches!(track.kind, TrackKind::Video(_));
        let tr_h = vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Ordinary,
            Some(vm.object_proto),
        ));
        let set_str = |vm: &mut JsVm, k: &str, v: &str| {
            let key = vm.heap.intern_key(k);
            let sv = vm.heap.alloc_string(v);
            vm.heap.object_define_data(
                tr_h,
                key,
                JsValue::from_string(sv),
                PropertyAttrs::METHOD,
                &mut vm.shapes,
            );
        };
        let id_str = track.id.to_string();
        set_str(vm, "id", &id_str);
        set_str(vm, "kind", "main");
        set_str(vm, "label", "");
        set_str(vm, "language", track.language.as_deref().unwrap_or(""));

        let ms_sb_g = Rc::clone(ms_sys);
        define_host_getter(vm, tr_h, "sourceBuffer", move |_, _, _| {
            let attached = ms_sb_g
                .borrow()
                .track_states
                .get(&tr_h)
                .is_some_and(|t| t.3);
            if attached {
                Ok(JsValue::from_object(sb_h))
            } else {
                Ok(JsValue::NULL)
            }
        });

        let toggle_prop = if is_video { "selected" } else { "enabled" };
        let ms_g2 = Rc::clone(ms_sys);
        let ms_s2 = Rc::clone(ms_sys);
        let h_s2 = Rc::clone(host);
        let ev_s2 = Rc::clone(events);
        let el_s2 = Rc::clone(event_loop);
        define_host_accessor(
            vm,
            tr_h,
            toggle_prop,
            move |_, _, _| {
                let on = ms_g2.borrow().track_states.get(&tr_h).is_none_or(|t| t.2);
                Ok(JsValue::from_bool(on))
            },
            move |vm, _, args| {
                let on = vm.to_boolean(arg(args, 0));
                let change_info = {
                    let mut s = ms_s2.borrow_mut();
                    let changed = if let Some(st) = s.track_states.get_mut(&tr_h) {
                        let c = st.2 != on;
                        st.2 = on;
                        c
                    } else {
                        false
                    };
                    if !changed {
                        return Ok(JsValue::UNDEFINED);
                    }
                    // For videoTracks, selecting one video track deselects other video tracks on the element.
                    let elem_nid = s.media_sources.get(&ms_h).and_then(|e| e.attached_element);
                    let elem_list_h = elem_nid.and_then(|nid| {
                        if is_video {
                            s.elem_video_lists.get(&nid).map(|x| x.0)
                        } else {
                            s.elem_audio_lists.get(&nid).map(|x| x.0)
                        }
                    });
                    if is_video && on {
                        if let Some(nid) = elem_nid {
                            let all_v: Vec<HeapHandle> = s
                                .elem_video_lists
                                .get(&nid)
                                .map(|x| x.1.clone())
                                .unwrap_or_default();
                            for other in all_v {
                                if other != tr_h {
                                    if let Some(st) = s.track_states.get_mut(&other) {
                                        st.2 = false;
                                    }
                                }
                            }
                        }
                    }
                    // Recompute which SourceBuffers have at least one enabled/selected track.
                    let mut active_set: std::collections::HashSet<HeapHandle> =
                        std::collections::HashSet::new();
                    let sb_handles: Vec<HeapHandle> = s
                        .media_sources
                        .get(&ms_h)
                        .map(|e| e.buffer_handles.clone())
                        .unwrap_or_default();
                    for &bh in &sb_handles {
                        let any_on = s.source_buffers.get(&bh).is_some_and(|r| {
                            r.audio_track_objs
                                .iter()
                                .chain(r.video_track_objs.iter())
                                .any(|th| s.track_states.get(th).is_some_and(|t| t.2))
                        });
                        if any_on {
                            active_set.insert(bh);
                        }
                    }
                    if let Some(entry) = s.media_sources.get_mut(&ms_h) {
                        let old_active = entry.active_buffer_handles.clone();
                        entry.disabled_buffers.clear();
                        for &bh in &entry.buffer_handles {
                            if !active_set.contains(&bh) {
                                entry.disabled_buffers.insert(bh);
                            }
                        }
                        let new_active = MediaSourceSystem::compute_active_handles(entry);
                        let added_any = new_active.iter().any(|h| !old_active.contains(h));
                        let removed_any = old_active.iter().any(|h| !new_active.contains(h));
                        entry.active_buffer_handles = new_active.clone();
                        let active_h = entry.active_source_buffers_list;
                        let prev_len = entry.active_sbl_synced_len;
                        entry.active_sbl_synced_len = new_active.len();
                        Some((
                            active_h,
                            new_active,
                            prev_len,
                            added_any,
                            removed_any,
                            elem_list_h,
                        ))
                    } else {
                        None
                    }
                };
                if let Some((active_h, new_active, mut prev_len, added, removed, elem_list_h)) =
                    change_info
                {
                    sync_sbl_indices(vm, active_h, &new_active, &mut prev_len);
                    if let Some(el_h) = elem_list_h {
                        queue_object_event(&h_s2, &ev_s2, &el_s2, el_h, "change");
                    }
                    if removed {
                        queue_object_event(&h_s2, &ev_s2, &el_s2, active_h, "removesourcebuffer");
                    }
                    if added {
                        queue_object_event(&h_s2, &ev_s2, &el_s2, active_h, "addsourcebuffer");
                    }
                }
                Ok(JsValue::UNDEFINED)
            },
        );

        // First video track is selected by default; first audio track is enabled by default.
        let initial_on = if is_video {
            attached_nid.is_none_or(|nid| {
                ms_sys
                    .borrow()
                    .elem_video_lists
                    .get(&nid)
                    .is_none_or(|x| x.1.is_empty())
            }) && new_video.is_empty()
        } else {
            true
        };
        ms_sys
            .borrow_mut()
            .track_states
            .insert(tr_h, (sb_h, is_video, initial_on, true));
        if is_video {
            new_video.push(tr_h);
        } else {
            new_audio.push(tr_h);
        }
    }

    let mut sb_a_prev = 0usize;
    let mut sb_v_prev = 0usize;
    if let Some(sb_ref) = ms_sys.borrow_mut().source_buffers.get_mut(&sb_h) {
        sb_a_prev = sb_ref.audio_track_objs.len();
        sb_v_prev = sb_ref.video_track_objs.len();
        sb_ref.audio_track_objs = new_audio.clone();
        sb_ref.video_track_objs = new_video.clone();
    }
    sync_track_list_props(vm, sb_audio_lh, &new_audio, &mut sb_a_prev);
    sync_track_list_props(vm, sb_video_lh, &new_video, &mut sb_v_prev);

    if let Some(nid) = attached_nid {
        if !new_audio.is_empty() {
            let (all_a, mut prev_a) = {
                let mut s = ms_sys.borrow_mut();
                let entry = s.elem_audio_lists.get_mut(&nid).unwrap();
                let p = entry.2;
                entry.1.extend_from_slice(&new_audio);
                entry.2 = entry.1.len();
                (entry.1.clone(), p)
            };
            if let Some(lh) = elem_audio_lh {
                sync_track_list_props(vm, lh, &all_a, &mut prev_a);
            }
        }
        if !new_video.is_empty() {
            let (all_v, mut prev_v) = {
                let mut s = ms_sys.borrow_mut();
                let entry = s.elem_video_lists.get_mut(&nid).unwrap();
                let p = entry.2;
                entry.1.extend_from_slice(&new_video);
                entry.2 = entry.1.len();
                (entry.1.clone(), p)
            };
            if let Some(lh) = elem_video_lh {
                sync_track_list_props(vm, lh, &all_v, &mut prev_v);
            }
        }
    }

    for _ in &new_audio {
        fire_object_event(vm, host, events, event_loop, sb_audio_lh, "addtrack", &[]);
        if let Some(lh) = elem_audio_lh {
            fire_object_event(vm, host, events, event_loop, lh, "addtrack", &[]);
        }
    }
    for _ in &new_video {
        fire_object_event(vm, host, events, event_loop, sb_video_lh, "addtrack", &[]);
        if let Some(lh) = elem_video_lh {
            fire_object_event(vm, host, events, event_loop, lh, "addtrack", &[]);
        }
    }
}

fn remove_tracks_for_source_buffer(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    ms_sys: &SharedMediaSource,
    sb_h: HeapHandle,
    attached_nid: Option<NodeId>,
) {
    let (sb_audio_lh, sb_video_lh, rem_a, rem_v) = {
        let mut s = ms_sys.borrow_mut();
        let Some(sb_ref) = s.source_buffers.get_mut(&sb_h) else {
            return;
        };
        let alh = sb_ref.audio_tracks_list;
        let vlh = sb_ref.video_tracks_list;
        let a = std::mem::take(&mut sb_ref.audio_track_objs);
        let v = std::mem::take(&mut sb_ref.video_track_objs);
        for &th in a.iter().chain(v.iter()) {
            if let Some(st) = s.track_states.get_mut(&th) {
                st.3 = false; // track.sourceBuffer becomes null!
            }
        }
        (alh, vlh, a, v)
    };
    if rem_a.is_empty() && rem_v.is_empty() {
        return;
    }

    let mut prev_sa = rem_a.len();
    let mut prev_sv = rem_v.len();
    sync_track_list_props(vm, sb_audio_lh, &[], &mut prev_sa);
    sync_track_list_props(vm, sb_video_lh, &[], &mut prev_sv);

    let mut elem_audio_lh = None;
    let mut elem_video_lh = None;
    if let Some(nid) = attached_nid {
        if !rem_a.is_empty() {
            let info = {
                let mut s = ms_sys.borrow_mut();
                s.elem_audio_lists.get_mut(&nid).map(|e| {
                    let p = e.2;
                    e.1.retain(|h| !rem_a.contains(h));
                    e.2 = e.1.len();
                    (e.0, e.1.clone(), p)
                })
            };
            if let Some((lh, keep, mut prev)) = info {
                sync_track_list_props(vm, lh, &keep, &mut prev);
                elem_audio_lh = Some(lh);
            }
        }
        if !rem_v.is_empty() {
            let info = {
                let mut s = ms_sys.borrow_mut();
                s.elem_video_lists.get_mut(&nid).map(|e| {
                    let p = e.2;
                    e.1.retain(|h| !rem_v.contains(h));
                    e.2 = e.1.len();
                    (e.0, e.1.clone(), p)
                })
            };
            if let Some((lh, keep, mut prev)) = info {
                sync_track_list_props(vm, lh, &keep, &mut prev);
                elem_video_lh = Some(lh);
            }
        }
    }

    for _ in &rem_a {
        queue_object_event(host, events, event_loop, sb_audio_lh, "removetrack");
        if let Some(lh) = elem_audio_lh {
            queue_object_event(host, events, event_loop, lh, "removetrack");
        }
    }
    if !rem_a.is_empty() {
        if let Some(lh) = elem_audio_lh {
            queue_object_event(host, events, event_loop, lh, "change");
        }
    }
    for _ in &rem_v {
        queue_object_event(host, events, event_loop, sb_video_lh, "removetrack");
        if let Some(lh) = elem_video_lh {
            queue_object_event(host, events, event_loop, lh, "removetrack");
        }
    }
    if !rem_v.is_empty() {
        if let Some(lh) = elem_video_lh {
            queue_object_event(host, events, event_loop, lh, "change");
        }
    }
}

fn close_media_source_now(
    vm: &mut JsVm,
    ms_h: HeapHandle,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    ms_sys: &SharedMediaSource,
) {
    let close_info = {
        let mut s = ms_sys.borrow_mut();
        if let Some(entry) = s.media_sources.get_mut(&ms_h) {
            let was_streaming = entry.is_managed && entry.streaming;
            entry.streaming = false;
            let attached_nid = entry.attached_element.take();
            let closed = entry.engine.close();
            if closed {
                let old_sbs = std::mem::take(&mut entry.buffer_handles);
                entry.active_buffer_handles.clear();
                entry.disabled_buffers.clear();
                let sbl_h = entry.source_buffers_list;
                let active_h = entry.active_source_buffers_list;
                let sbl_len = entry.sbl_synced_len;
                let act_len = entry.active_sbl_synced_len;
                entry.sbl_synced_len = 0;
                entry.active_sbl_synced_len = 0;
                Some((
                    was_streaming,
                    sbl_h,
                    active_h,
                    sbl_len,
                    act_len,
                    old_sbs,
                    attached_nid,
                ))
            } else {
                None
            }
        } else {
            None
        }
    };
    if let Some((was_streaming, sbl_h, active_h, mut sbl_len, mut act_len, old_sbs, attached_nid)) =
        close_info
    {
        for sb_h in old_sbs {
            remove_tracks_for_source_buffer(
                vm,
                host,
                events,
                event_loop,
                ms_sys,
                sb_h,
                attached_nid,
            );
        }
        sync_sbl_indices(vm, sbl_h, &[], &mut sbl_len);
        sync_sbl_indices(vm, active_h, &[], &mut act_len);
        if was_streaming {
            queue_object_event(host, events, event_loop, ms_h, "endstreaming");
        }
        queue_object_event(host, events, event_loop, ms_h, "sourceclose");
    }
}

/// Attaches a `MediaSource` handle to `media_node` asynchronously per HTML/MSE §2.4.1,
/// synchronously closing any previous attachment on `media_node`.
pub fn attach_media_source_to_element(
    vm: &mut JsVm,
    media_node: NodeId,
    ms_handle: HeapHandle,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    ms_sys: &SharedMediaSource,
) {
    // Synchronously detach & close any previous MediaSource on this element.
    detach_media_source_from_element(vm, media_node, host, events, event_loop, ms_sys);

    let me_opt = ms_sys.borrow().media_elements.clone();
    if let Some(me) = &me_opt {
        let mut me_b = me.borrow_mut();
        let p = me_b.ensure_player(media_node);
        p.reset_for_source_change();
        p.network_state = NETWORK_LOADING;
    }

    let h_cl = Rc::clone(host);
    let ev_cl = Rc::clone(events);
    let el_cl = Rc::clone(event_loop);
    let ms_cl = Rc::clone(ms_sys);
    event_loop
        .borrow_mut()
        .post_host_task(TaskPriority::Timer, move |vm| {
            let outcome = {
                let mut sys = ms_cl.borrow_mut();
                if let Some(entry) = sys.media_sources.get_mut(&ms_handle) {
                    if entry.engine.ready_state == ReadyState::Closed
                        && entry.attached_element.is_none()
                    {
                        entry.attached_element = Some(media_node);
                        entry.engine.open();
                        let is_managed = entry.is_managed;
                        if is_managed {
                            entry.streaming = true;
                        }
                        sys.attached_sources.insert(media_node, ms_handle);
                        Some(Ok(is_managed))
                    } else {
                        Some(Err("MediaSource is already attached to a media element"))
                    }
                } else {
                    Some(Err("MediaSource object no longer exists"))
                }
            };
            match outcome {
                Some(Ok(is_managed)) => {
                    if let Some(me) = ms_cl.borrow().media_elements.clone() {
                        me.borrow_mut().ensure_player(media_node).network_state = NETWORK_LOADING;
                    }
                    fire_object_event(vm, &h_cl, &ev_cl, &el_cl, ms_handle, "sourceopen", &[]);
                    if is_managed {
                        fire_object_event(
                            vm,
                            &h_cl,
                            &ev_cl,
                            &el_cl,
                            ms_handle,
                            "startstreaming",
                            &[],
                        );
                    }
                }
                Some(Err(msg)) => {
                    if let Some(me) = ms_cl.borrow().media_elements.clone() {
                        let mut me_b = me.borrow_mut();
                        let p = me_b.ensure_player(media_node);
                        p.error = Some((MEDIA_ERR_SRC_NOT_SUPPORTED, msg.to_string()));
                        p.network_state = NETWORK_NO_SOURCE;
                    }
                    let elem_v = wrap_node(vm, &h_cl, media_node);
                    if let Some(elem_h) = elem_v.as_object() {
                        fire_object_event(vm, &h_cl, &ev_cl, &el_cl, elem_h, "error", &[]);
                    }
                }
                None => {}
            }
        });
}

/// Detaches any `MediaSource` currently attached to `media_node`.
pub fn detach_media_source_from_element(
    vm: &mut JsVm,
    media_node: NodeId,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    ms_sys: &SharedMediaSource,
) {
    let old_ms = ms_sys.borrow_mut().attached_sources.remove(&media_node);
    if let Some(ms_h) = old_ms {
        close_media_source_now(vm, ms_h, host, events, event_loop, ms_sys);
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

fn install_null_event_handlers(vm: &mut JsVm, proto: HeapHandle, names: &[&str]) {
    for &name in names {
        let key = vm.heap.intern_key(name);
        vm.heap.object_define_data(
            proto,
            key,
            JsValue::NULL,
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
    }
}

/// Installs `MediaSource`, `ManagedMediaSource`, `SourceBuffer`, `ManagedSourceBuffer`,
/// `SourceBufferList`, `TimeRanges`, `BufferedChangeEvent`, and `HTMLMediaElement` MSE hooks.
pub fn install_media_source(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
) -> SharedMediaSource {
    let ms_sys = Rc::new(RefCell::new(MediaSourceSystem::new()));
    install_media_source_inner(vm, host, events, event_loop, web_apis, &ms_sys);
    ms_sys
}

/// Installs MSE bindings wired to `media_elements` so MSE updates drive `HTMLMediaElement`
/// `readyState`, `networkState`, `error`, `seeking`, `durationchange`, `loadedmetadata`, and playback.
pub fn install_media_source_with_elements(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    media_elements: &SharedMediaElements,
) -> SharedMediaSource {
    let ms_sys = Rc::new(RefCell::new(MediaSourceSystem::new()));
    ms_sys.borrow_mut().media_elements = Some(Rc::clone(media_elements));
    install_media_source_inner(vm, host, events, event_loop, web_apis, &ms_sys);
    ms_sys
}

fn install_media_source_inner(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    ms_sys: &SharedMediaSource,
) {
    // Register validator so `URL.createObjectURL(mediaSource)` recognizes `MediaSource` / `ManagedMediaSource`.
    {
        let sys_v = Rc::clone(ms_sys);
        web_apis
            .borrow_mut()
            .register_object_url_validator(move |h| sys_v.borrow().media_sources.contains_key(&h));
    }

    install_time_ranges_proto(vm, ms_sys);
    install_quota_exceeded_error_ctor(vm, ms_sys);
    install_buffered_change_event_ctor(vm, events, ms_sys);
    install_source_buffer_list_proto(vm, host, ms_sys);
    install_source_buffer_proto(vm, host, events, event_loop, ms_sys);
    install_media_source_ctor(vm, host, events, event_loop, ms_sys);
    install_managed_media_source_ctors(vm, ms_sys);
    install_media_element_mse_hooks(vm, host, events, event_loop, web_apis, ms_sys);
}

fn install_quota_exceeded_error_ctor(vm: &mut JsVm, ms_sys: &SharedMediaSource) {
    let dom_ex_proto = {
        let dk = vm.heap.intern_key("DOMException");
        let pk = vm.heap.intern_key("prototype");
        vm.get_property_by_key(vm.global, dk)
            .ok()
            .and_then(|v| v.as_object())
            .and_then(|ctor_h| vm.get_property_by_key(ctor_h, pk).ok())
            .and_then(|v| v.as_object())
            .unwrap_or(vm.error_proto)
    };
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(dom_ex_proto),
    ));
    vm.heap.get_object_mut(proto).unwrap().is_prototype = true;
    define_to_string_tag(vm, proto, "QuotaExceededError");
    ms_sys.borrow_mut().protos.quota_exceeded_error_proto = Some(proto);

    let ctor = vm.alloc_host_fn("QuotaExceededError", move |vm, _, args| {
        let msg = if args.is_empty() || arg(args, 0).is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(arg(args, 0))
        };
        Ok(alloc_quota_exceeded_error_obj(vm, proto, &msg))
    });
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "QuotaExceededError", JsValue::from_object(ctor));
}

fn alloc_quota_exceeded_error_obj(vm: &mut JsVm, proto: HeapHandle, message: &str) -> JsValue {
    let h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let nk = vm.heap.intern_key("name");
    let ns = vm.heap.alloc_string("QuotaExceededError");
    vm.heap.object_define_data(
        h,
        nk,
        JsValue::from_string(ns),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    let mk = vm.heap.intern_key("message");
    let ms = vm.heap.alloc_string(message);
    vm.heap.object_define_data(
        h,
        mk,
        JsValue::from_string(ms),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    let ck = vm.heap.intern_key("code");
    vm.heap.object_define_data(
        h,
        ck,
        JsValue::from_number(22.0),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    let qk = vm.heap.intern_key("quota");
    vm.heap
        .object_define_data(h, qk, JsValue::NULL, PropertyAttrs::METHOD, &mut vm.shapes);
    let rk = vm.heap.intern_key("requested");
    vm.heap
        .object_define_data(h, rk, JsValue::NULL, PropertyAttrs::METHOD, &mut vm.shapes);
    JsValue::from_object(h)
}

fn install_buffered_change_event_ctor(
    vm: &mut JsVm,
    events: &SharedEvents,
    ms_sys: &SharedMediaSource,
) {
    let ev_proto = events.borrow().event_proto.unwrap_or(vm.object_proto);
    let proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(ev_proto)));
    vm.heap.get_object_mut(proto).unwrap().is_prototype = true;
    define_to_string_tag(vm, proto, "BufferedChangeEvent");
    ms_sys.borrow_mut().protos.buffered_change_event_proto = Some(proto);

    let sys_c = Rc::clone(ms_sys);
    let ctor = vm.alloc_host_fn("BufferedChangeEvent", move |vm, _, args| {
        let ev_type = vm.to_js_string(arg(args, 0))?;
        let init = arg(args, 1);
        let h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        let tk = vm.heap.intern_key("type");
        let ts = vm.heap.alloc_string(&ev_type);
        vm.heap.object_define_data(
            h,
            tk,
            JsValue::from_string(ts),
            PropertyAttrs::METHOD,
            &mut vm.shapes,
        );
        let init_bool = |vm: &mut JsVm, prop: &str| -> bool {
            let Some(init_h) = init.as_object() else {
                return false;
            };
            let k = vm.heap.intern_key(prop);
            vm.get_property_by_key(init_h, k)
                .ok()
                .is_some_and(|v| vm.to_boolean(v))
        };
        let bubbles = init_bool(vm, "bubbles");
        let cancelable = init_bool(vm, "cancelable");
        let composed = init_bool(vm, "composed");
        for &(flag_name, flag_val) in &[
            ("isTrusted", false),
            ("bubbles", bubbles),
            ("cancelable", cancelable),
            ("composed", composed),
            ("defaultPrevented", false),
        ] {
            let fk = vm.heap.intern_key(flag_name);
            vm.heap.object_define_data(
                h,
                fk,
                JsValue::from_bool(flag_val),
                PropertyAttrs::METHOD,
                &mut vm.shapes,
            );
        }
        let extract_ranges = |vm: &mut JsVm, key_name: &str| -> Result<JsValue, JsException> {
            if let Some(init_h) = init.as_object() {
                let k = vm.heap.intern_key(key_name);
                let v = vm.get_property_by_key(init_h, k)?;
                if !v.is_undefined() && !v.is_null() {
                    if let Some(tr_h) = v.as_object() {
                        if sys_c.borrow().time_ranges.contains_key(&tr_h) {
                            return Ok(v);
                        }
                    }
                    return Err(vm.type_error("BufferedChangeEvent ranges must be a TimeRanges"));
                }
            }
            Ok(sys_c.borrow_mut().alloc_time_ranges(vm, TimeRanges::new()))
        };
        let added = extract_ranges(vm, "addedRanges")?;
        let removed = extract_ranges(vm, "removedRanges")?;
        let ak = vm.heap.intern_key("addedRanges");
        vm.heap
            .object_define_data(h, ak, added, PropertyAttrs::METHOD, &mut vm.shapes);
        let rk = vm.heap.intern_key("removedRanges");
        vm.heap
            .object_define_data(h, rk, removed, PropertyAttrs::METHOD, &mut vm.shapes);
        Ok(JsValue::from_object(h))
    });
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "BufferedChangeEvent", JsValue::from_object(ctor));
}

fn install_managed_media_source_ctors(vm: &mut JsVm, ms_sys: &SharedMediaSource) {
    let (ms_proto, sb_proto) = {
        let s = ms_sys.borrow();
        (
            s.protos.media_source_proto.unwrap_or(vm.object_proto),
            s.protos.source_buffer_proto.unwrap_or(vm.object_proto),
        )
    };

    // `ManagedSourceBuffer` inherits from `SourceBuffer`
    let msb_proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(sb_proto)));
    vm.heap.get_object_mut(msb_proto).unwrap().is_prototype = true;
    define_to_string_tag(vm, msb_proto, "ManagedSourceBuffer");
    install_null_event_handlers(vm, msb_proto, &["onbufferedchange"]);
    ms_sys.borrow_mut().protos.managed_source_buffer_proto = Some(msb_proto);
    let msb_ctor = vm.alloc_host_fn("ManagedSourceBuffer", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: ManagedSourceBuffer"))
    });
    bind_ctor_proto(vm, msb_ctor, msb_proto);
    define_global(vm, "ManagedSourceBuffer", JsValue::from_object(msb_ctor));

    // `ManagedMediaSource` inherits from `MediaSource`
    let mms_proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(ms_proto)));
    vm.heap.get_object_mut(mms_proto).unwrap().is_prototype = true;
    define_to_string_tag(vm, mms_proto, "ManagedMediaSource");
    install_null_event_handlers(vm, mms_proto, &["onstartstreaming", "onendstreaming"]);
    ms_sys.borrow_mut().protos.managed_media_source_proto = Some(mms_proto);

    {
        let sys = Rc::clone(ms_sys);
        define_host_getter(vm, mms_proto, "streaming", move |vm, this, _| {
            let ms_h = require_ms(vm, &sys, this)?;
            let streaming = sys
                .borrow()
                .media_sources
                .get(&ms_h)
                .is_some_and(|e| e.streaming);
            Ok(JsValue::from_bool(streaming))
        });
    }

    let sys_ctor = Rc::clone(ms_sys);
    let mms_ctor = vm.alloc_host_fn("ManagedMediaSource", move |vm, _, _| {
        let (proto, sbl_proto) = {
            let s = sys_ctor.borrow();
            (
                s.protos
                    .managed_media_source_proto
                    .unwrap_or(vm.object_proto),
                s.protos.source_buffer_list_proto.unwrap_or(vm.object_proto),
            )
        };
        let ms_h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
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
                active_buffer_handles: Vec::new(),
                disabled_buffers: std::collections::HashSet::new(),
                attached_element: None,
                current_playback_time_s: 0.0,
                is_managed: true,
                streaming: false,
                sbl_synced_len: 0,
                active_sbl_synced_len: 0,
            },
        );
        Ok(JsValue::from_object(ms_h))
    });
    define_host_method(vm, mms_ctor, "isTypeSupported", |vm, _, args| {
        let mime = vm.value_to_rust_string(arg(args, 0));
        Ok(JsValue::from_bool(is_type_supported(&mime)))
    });
    bind_ctor_proto(vm, mms_ctor, mms_proto);
    define_global(vm, "ManagedMediaSource", JsValue::from_object(mms_ctor));
}

fn install_time_ranges_proto(vm: &mut JsVm, ms_sys: &SharedMediaSource) {
    let proto = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    vm.heap.get_object_mut(proto).unwrap().is_prototype = true;
    define_to_string_tag(vm, proto, "TimeRanges");
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
    define_to_string_tag(vm, proto, "SourceBufferList");
    install_null_event_handlers(vm, proto, &["onaddsourcebuffer", "onremovesourcebuffer"]);
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
                        MediaSourceSystem::compute_active_handles(entry).len()
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

#[allow(clippy::too_many_arguments)]
fn complete_end_of_stream(
    vm: &mut JsVm,
    host: &SharedHost,
    events: &SharedEvents,
    event_loop: &SharedEventLoop,
    ms_sys: &SharedMediaSource,
    ms_h: HeapHandle,
    eos_err: Option<EndOfStreamError>,
    duration_changed: bool,
    managed_diffs: Vec<(HeapHandle, TimeRanges, TimeRanges)>,
) {
    fire_object_event(vm, host, events, event_loop, ms_h, "sourceended", &[]);

    if eos_err.is_none() {
        let should_end_streaming = {
            let mut s = ms_sys.borrow_mut();
            s.media_sources.get_mut(&ms_h).is_some_and(|e| {
                if e.is_managed && e.streaming {
                    e.streaming = false;
                    true
                } else {
                    false
                }
            })
        };
        if should_end_streaming {
            fire_object_event(vm, host, events, event_loop, ms_h, "endstreaming", &[]);
        }
    }

    for (sb_h, added, removed) in managed_diffs {
        fire_buffered_change_event(vm, host, events, event_loop, ms_sys, sb_h, added, removed);
    }

    let (attached_nid, me_opt) = {
        let s = ms_sys.borrow();
        (
            s.media_sources.get(&ms_h).and_then(|e| e.attached_element),
            s.media_elements.clone(),
        )
    };

    let Some(nid) = attached_nid else {
        return;
    };
    let elem_v = wrap_node(vm, host, nid);
    let Some(elem_h) = elem_v.as_object() else {
        return;
    };

    if let Some(err) = eos_err {
        let was_have_nothing = if let Some(me) = &me_opt {
            let mut me_b = me.borrow_mut();
            let p = me_b.ensure_player(nid);
            let nothing = p.ready_state == HAVE_NOTHING;
            if nothing {
                p.error = Some((
                    MEDIA_ERR_SRC_NOT_SUPPORTED,
                    "MSE decode/network error before metadata".to_string(),
                ));
                p.network_state = NETWORK_NO_SOURCE;
            } else {
                let code = match err {
                    EndOfStreamError::Network => MEDIA_ERR_NETWORK,
                    EndOfStreamError::Decode => MEDIA_ERR_DECODE,
                };
                p.error = Some((code, "MSE endOfStream error".to_string()));
            }
            nothing
        } else {
            false
        };
        fire_object_event(vm, host, events, event_loop, elem_h, "error", &[]);
        if was_have_nothing {
            ms_sys.borrow_mut().attached_sources.remove(&nid);
            close_media_source_now(vm, ms_h, host, events, event_loop, ms_sys);
        }
        return;
    }

    if duration_changed {
        fire_object_event(vm, host, events, event_loop, elem_h, "durationchange", &[]);
    }

    if let Some(me) = &me_opt {
        let (complete_seek, fire_canplaythrough, fire_playing) = {
            let mut me_b = me.borrow_mut();
            let p = me_b.ensure_player(nid);
            let mut did_seek = false;
            if p.seeking {
                p.seeking = false;
                p.seek_target_s = None;
                did_seek = true;
            }
            let fire_cpt = p.ready_state >= HAVE_METADATA && !p.has_fired_canplaythrough;
            if p.ready_state >= HAVE_METADATA {
                p.ready_state = HAVE_ENOUGH_DATA;
                p.has_fired_canplaythrough = true;
            }
            let fire_pl =
                p.clock.is_playing() && p.ready_state >= HAVE_FUTURE_DATA && !p.has_fired_playing;
            if fire_pl {
                p.has_fired_playing = true;
            }
            (did_seek, fire_cpt, fire_pl)
        };
        if complete_seek {
            fire_object_event(vm, host, events, event_loop, elem_h, "timeupdate", &[]);
            fire_object_event(vm, host, events, event_loop, elem_h, "seeked", &[]);
        }
        if fire_canplaythrough {
            fire_object_event(vm, host, events, event_loop, elem_h, "canplaythrough", &[]);
        }
        if fire_playing {
            fire_object_event(vm, host, events, event_loop, elem_h, "playing", &[]);
        }
    }
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
    define_to_string_tag(vm, proto, "SourceBuffer");
    install_null_event_handlers(
        vm,
        proto,
        &[
            "onupdatestart",
            "onupdate",
            "onupdateend",
            "onerror",
            "onabort",
        ],
    );
    ms_sys.borrow_mut().protos.source_buffer_proto = Some(proto);

    // `mode` getter/setter
    {
        let sys_g = Rc::clone(ms_sys);
        let sys_s = Rc::clone(ms_sys);
        let h_s = Rc::clone(host);
        let ev_s = Rc::clone(events);
        let el_s = Rc::clone(event_loop);
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
                // WebIDL enum attribute setter: ignore values outside the enum without throwing.
                let Some(mode) = AppendMode::parse(&mode_str) else {
                    return Ok(JsValue::UNDEFINED);
                };
                let (ms_h, reopened) = {
                    let mut sys = sys_s.borrow_mut();
                    let (ms_h, idx) = sys.locate_source_buffer(sb_h).ok_or_else(|| {
                        create_dom_exception(vm, "InvalidStateError", "SourceBuffer removed")
                    })?;
                    let entry = sys.media_sources.get_mut(&ms_h).unwrap();
                    entry.engine.source_buffers[idx]
                        .set_mode(mode)
                        .map_err(|e| mse_err_to_js(vm, e))?;
                    let reopened = entry.engine.reopen_if_ended();
                    (ms_h, reopened)
                };
                if reopened {
                    queue_object_event(&h_s, &ev_s, &el_s, ms_h, "sourceopen");
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // `audioTracks` & `videoTracks` getters on `SourceBuffer.prototype`
    {
        let sys = Rc::clone(ms_sys);
        define_host_getter(vm, proto, "audioTracks", move |vm, this, _| {
            let sb_h = require_sb(vm, &sys, this)?;
            let lh = sys.borrow().source_buffers[&sb_h].audio_tracks_list;
            Ok(JsValue::from_object(lh))
        });
    }
    {
        let sys = Rc::clone(ms_sys);
        define_host_getter(vm, proto, "videoTracks", move |vm, this, _| {
            let sb_h = require_sb(vm, &sys, this)?;
            let lh = sys.borrow().source_buffers[&sb_h].video_tracks_list;
            Ok(JsValue::from_object(lh))
        });
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

    // `buffered` getter -> TimeRanges (returns the same object while ranges are unchanged!)
    {
        let sys = Rc::clone(ms_sys);
        define_host_getter(vm, proto, "buffered", move |vm, this, _| {
            let sb_h = require_sb(vm, &sys, this)?;
            let (ranges, cached_hit) = {
                let s = sys.borrow();
                let (ms_h, idx) = s.locate_source_buffer(sb_h).ok_or_else(|| {
                    create_dom_exception(vm, "InvalidStateError", "SourceBuffer removed")
                })?;
                let entry = &s.media_sources[&ms_h];
                let is_ended = entry.engine.ready_state == ReadyState::Ended;
                let ranges = entry.engine.source_buffers[idx].buffered_ranges(is_ended);
                let hit = s.source_buffers.get(&sb_h).and_then(|r| {
                    r.cached_buffered
                        .as_ref()
                        .filter(|(_, prev)| *prev == ranges)
                        .map(|(h, _)| *h)
                });
                (ranges, hit)
            };
            if let Some(tr_h) = cached_hit {
                return Ok(JsValue::from_object(tr_h));
            }
            let tr_val = sys.borrow_mut().alloc_time_ranges(vm, ranges.clone());
            if let Some(tr_h) = tr_val.as_object() {
                if let Some(sb_ref) = sys.borrow_mut().source_buffers.get_mut(&sb_h) {
                    sb_ref.cached_buffered = Some((tr_h, ranges));
                }
            }
            Ok(tr_val)
        });
    }

    // `timestampOffset` getter/setter
    {
        let sys_g = Rc::clone(ms_sys);
        let sys_s = Rc::clone(ms_sys);
        let h_s = Rc::clone(host);
        let ev_s = Rc::clone(events);
        let el_s = Rc::clone(event_loop);
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
                let (ms_h, reopened) = {
                    let mut s = sys_s.borrow_mut();
                    let (ms_h, idx) = s.locate_source_buffer(sb_h).ok_or_else(|| {
                        create_dom_exception(vm, "InvalidStateError", "SourceBuffer removed")
                    })?;
                    let entry = s.media_sources.get_mut(&ms_h).unwrap();
                    entry.engine.source_buffers[idx]
                        .set_timestamp_offset(v)
                        .map_err(|e| mse_err_to_js(vm, e))?;
                    let reopened = entry.engine.reopen_if_ended();
                    (ms_h, reopened)
                };
                if reopened {
                    queue_object_event(&h_s, &ev_s, &el_s, ms_h, "sourceopen");
                }
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
            let raw_arg = arg(args, 0);
            if is_shared_buffer_source(vm, raw_arg) {
                return Err(vm.type_error("appendBuffer does not accept SharedArrayBuffer"));
            }
            let data = extract_gpu_buffer_source(vm, raw_arg, None, None).ok_or_else(|| {
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
                let play_t = ms_entry.current_playback_time_s;
                let sb = &mut ms_entry.engine.source_buffers[idx];
                if sb.updating {
                    return Err(create_dom_exception(
                        vm,
                        "InvalidStateError",
                        "SourceBuffer is currently updating",
                    ));
                }
                if let Err(err) = sb.prepare_append_quota(data.len(), play_t) {
                    let proto = s
                        .protos
                        .quota_exceeded_error_proto
                        .unwrap_or(vm.error_proto);
                    let obj = alloc_quota_exceeded_error_obj(vm, proto, err.message());
                    return Err(JsException {
                        value: obj,
                        message: err.message().to_string(),
                    });
                }
                let reopened = ms_entry.engine.reopen_if_ended();
                ms_entry.engine.source_buffers[idx].updating = true;
                ms_entry.engine.source_buffers[idx].pending_op = PendingBufferOp::Append;
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
                    enum AppendOutcome {
                        Success {
                            active_list_h: HeapHandle,
                            active_handles: Vec<HeapHandle>,
                            active_prev_len: usize,
                            active_added: bool,
                            attached_nid: Option<NodeId>,
                            duration_changed: bool,
                            all_inits_received: bool,
                            buffered: TimeRanges,
                            first_video_dims: Option<(u32, u32)>,
                            managed_diff: Option<(TimeRanges, TimeRanges)>,
                        },
                        DecodeError {
                            parent_ms: HeapHandle,
                        },
                    }

                    let outcome = {
                        let mut s = sys_task.borrow_mut();
                        let still_valid = s
                            .source_buffers
                            .get(&sb_h)
                            .is_some_and(|r| r.async_generation == gen);
                        if !still_valid {
                            return;
                        }
                        let is_managed_sb =
                            s.source_buffers.get(&sb_h).is_some_and(|r| r.is_managed);
                        let Some((parent_ms, idx)) = s.locate_source_buffer(sb_h) else {
                            return;
                        };
                        let entry = s.media_sources.get_mut(&parent_ms).unwrap();
                        let play_t = entry.current_playback_time_s;
                        let old_ranges = if is_managed_sb {
                            Some(entry.engine.source_buffers[idx].buffered_ranges(false))
                        } else {
                            None
                        };
                        let res = entry.engine.source_buffers[idx].append_buffer(&data, play_t);
                        entry.engine.source_buffers[idx].updating = false;
                        entry.engine.source_buffers[idx].pending_op = PendingBufferOp::None;
                        match res {
                            Ok(app) => {
                                let duration_changed =
                                    entry.engine.sync_duration_after_append_with_init(
                                        app.init_duration_s,
                                        app.highest_end_timestamp,
                                    );
                                let active_h = entry.active_source_buffers_list;
                                let active_handles =
                                    MediaSourceSystem::compute_active_handles(entry);
                                let active_added =
                                    active_handles.len() > entry.active_buffer_handles.len();
                                entry.active_buffer_handles = active_handles.clone();
                                let active_prev_len = entry.active_sbl_synced_len;
                                entry.active_sbl_synced_len = active_handles.len();

                                let all_inits_received = !entry.engine.source_buffers.is_empty()
                                    && entry
                                        .engine
                                        .source_buffers
                                        .iter()
                                        .all(|b| b.init_segment_received);
                                let buffered = entry.engine.buffered();
                                let first_video_dims = entry
                                    .engine
                                    .source_buffers
                                    .iter()
                                    .flat_map(|b| b.tracks.iter())
                                    .find_map(|t| match &t.kind {
                                        TrackKind::Video(v) if v.width > 0 && v.height > 0 => {
                                            Some((v.width, v.height))
                                        }
                                        _ => None,
                                    });
                                let managed_diff = old_ranges.map(|old_r| {
                                    let new_r =
                                        entry.engine.source_buffers[idx].buffered_ranges(false);
                                    diff_time_ranges(&old_r, &new_r)
                                });
                                let attached_nid = entry.attached_element;
                                Some(AppendOutcome::Success {
                                    active_list_h: active_h,
                                    active_handles,
                                    active_prev_len,
                                    active_added,
                                    attached_nid,
                                    duration_changed,
                                    all_inits_received,
                                    buffered,
                                    first_video_dims,
                                    managed_diff,
                                })
                            }
                            Err(_) => Some(AppendOutcome::DecodeError { parent_ms }),
                        }
                    };

                    match outcome {
                        Some(AppendOutcome::Success {
                            active_list_h,
                            active_handles,
                            mut active_prev_len,
                            active_added,
                            attached_nid,
                            duration_changed,
                            all_inits_received,
                            buffered,
                            first_video_dims,
                            managed_diff,
                        }) => {
                            let parent_ms_opt = sys_task
                                .borrow()
                                .source_buffers
                                .get(&sb_h)
                                .map(|r| r.parent_ms);
                            if let Some(pms) = parent_ms_opt {
                                populate_tracks_for_source_buffer(
                                    vm,
                                    &h_task,
                                    &ev_task,
                                    &el_task,
                                    &sys_task,
                                    pms,
                                    sb_h,
                                    attached_nid,
                                );
                            }
                            sync_sbl_indices(
                                vm,
                                active_list_h,
                                &active_handles,
                                &mut active_prev_len,
                            );
                            if active_added {
                                fire_object_event(
                                    vm,
                                    &h_task,
                                    &ev_task,
                                    &el_task,
                                    active_list_h,
                                    "addsourcebuffer",
                                    &[],
                                );
                            }

                            // Update attached HTMLMediaElement readyState & fire media lifecycle events.
                            let me_opt = sys_task.borrow().media_elements.clone();
                            if let (Some(nid), Some(me)) = (attached_nid, me_opt) {
                                let elem_v = wrap_node(vm, &h_task, nid);
                                if let Some(elem_h) = elem_v.as_object() {
                                    if duration_changed {
                                        fire_object_event(
                                            vm,
                                            &h_task,
                                            &ev_task,
                                            &el_task,
                                            elem_h,
                                            "durationchange",
                                            &[],
                                        );
                                    }
                                    let (
                                        fire_meta,
                                        fire_loadeddata,
                                        fire_canplay,
                                        fire_playing,
                                        fire_seeked,
                                    ) = {
                                        let mut me_b = me.borrow_mut();
                                        let p = me_b.ensure_player(nid);
                                        if let Some((w, h)) = first_video_dims {
                                            if p.video_width == 0 && p.video_height == 0 {
                                                p.video_width = w;
                                                p.video_height = h;
                                            }
                                        }
                                        let mut f_meta = false;
                                        if all_inits_received && !p.has_fired_loadedmetadata {
                                            p.has_fired_loadedmetadata = true;
                                            if p.ready_state < HAVE_METADATA {
                                                p.ready_state = HAVE_METADATA;
                                            }
                                            f_meta = true;
                                        }
                                        let cur_t = p.clock.current_time_s();
                                        let covered = !buffered.is_empty()
                                            && time_is_buffered(&buffered, cur_t);
                                        let mut f_seeked = false;
                                        if p.seeking {
                                            let tgt = p.seek_target_s.unwrap_or(cur_t);
                                            if !buffered.is_empty()
                                                && time_is_buffered(&buffered, tgt)
                                            {
                                                p.seeking = false;
                                                p.seek_target_s = None;
                                                f_seeked = true;
                                            }
                                        }
                                        let mut f_ld = false;
                                        let mut f_cp = false;
                                        let mut f_pl = false;
                                        if p.has_fired_loadedmetadata && covered {
                                            if p.ready_state < HAVE_ENOUGH_DATA {
                                                p.ready_state = HAVE_ENOUGH_DATA;
                                            }
                                            if !p.has_fired_loadeddata {
                                                p.has_fired_loadeddata = true;
                                                f_ld = true;
                                            }
                                            if !p.has_fired_canplay {
                                                p.has_fired_canplay = true;
                                                f_cp = true;
                                            }
                                            if p.clock.is_playing()
                                                && !p.seeking
                                                && !p.has_fired_playing
                                            {
                                                p.has_fired_playing = true;
                                                f_pl = true;
                                            }
                                        }
                                        (f_meta, f_ld, f_cp, f_pl, f_seeked)
                                    };
                                    if fire_meta {
                                        fire_object_event(
                                            vm,
                                            &h_task,
                                            &ev_task,
                                            &el_task,
                                            elem_h,
                                            "loadedmetadata",
                                            &[],
                                        );
                                    }
                                    if fire_loadeddata {
                                        fire_object_event(
                                            vm,
                                            &h_task,
                                            &ev_task,
                                            &el_task,
                                            elem_h,
                                            "loadeddata",
                                            &[],
                                        );
                                    }
                                    if fire_canplay {
                                        fire_object_event(
                                            vm,
                                            &h_task,
                                            &ev_task,
                                            &el_task,
                                            elem_h,
                                            "canplay",
                                            &[],
                                        );
                                    }
                                    if fire_seeked {
                                        fire_object_event(
                                            vm,
                                            &h_task,
                                            &ev_task,
                                            &el_task,
                                            elem_h,
                                            "timeupdate",
                                            &[],
                                        );
                                        fire_object_event(
                                            vm,
                                            &h_task,
                                            &ev_task,
                                            &el_task,
                                            elem_h,
                                            "seeked",
                                            &[],
                                        );
                                    }
                                    if fire_playing {
                                        fire_object_event(
                                            vm,
                                            &h_task,
                                            &ev_task,
                                            &el_task,
                                            elem_h,
                                            "playing",
                                            &[],
                                        );
                                    }
                                }
                            }

                            if let Some((added, removed)) = managed_diff {
                                fire_buffered_change_event(
                                    vm, &h_task, &ev_task, &el_task, &sys_task, sb_h, added,
                                    removed,
                                );
                            }
                            fire_object_event(vm, &h_task, &ev_task, &el_task, sb_h, "update", &[]);
                            fire_object_event(
                                vm,
                                &h_task,
                                &ev_task,
                                &el_task,
                                sb_h,
                                "updateend",
                                &[],
                            );
                        }
                        Some(AppendOutcome::DecodeError { parent_ms }) => {
                            fire_object_event(vm, &h_task, &ev_task, &el_task, sb_h, "error", &[]);
                            fire_object_event(
                                vm,
                                &h_task,
                                &ev_task,
                                &el_task,
                                sb_h,
                                "updateend",
                                &[],
                            );
                            // W3C MSE §3.5.3 Append Error Algorithm -> endOfStream("decode")!
                            let eos_ok = sys_task
                                .borrow_mut()
                                .media_sources
                                .get_mut(&parent_ms)
                                .and_then(|e| {
                                    e.engine
                                        .end_of_stream_spec(Some(EndOfStreamError::Decode))
                                        .ok()
                                });
                            if let Some(dur_chg) = eos_ok {
                                complete_end_of_stream(
                                    vm,
                                    &h_task,
                                    &ev_task,
                                    &el_task,
                                    &sys_task,
                                    parent_ms,
                                    Some(EndOfStreamError::Decode),
                                    dur_chg,
                                    Vec::new(),
                                );
                            }
                        }
                        None => {}
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
                let dur = ms_entry.engine.duration;
                if dur.is_nan()
                    || !start.is_finite()
                    || start < 0.0
                    || start > dur
                    || end.is_nan()
                    || end <= start
                {
                    return Err(vm.type_error(
                        "SourceBuffer.remove requires finite 0 <= start <= duration and end > start",
                    ));
                }
                let reopened = ms_entry.engine.reopen_if_ended();
                ms_entry.engine.source_buffers[idx].updating = true;
                ms_entry.engine.source_buffers[idx].pending_op = PendingBufferOp::Remove;
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
                    let result = {
                        let mut s = sys_task.borrow_mut();
                        let still_valid = s
                            .source_buffers
                            .get(&sb_h)
                            .is_some_and(|r| r.async_generation == gen);
                        if !still_valid {
                            return;
                        }
                        let is_managed_sb =
                            s.source_buffers.get(&sb_h).is_some_and(|r| r.is_managed);
                        let Some((parent_ms, idx)) = s.locate_source_buffer(sb_h) else {
                            return;
                        };
                        let entry = s.media_sources.get_mut(&parent_ms).unwrap();
                        let sb = &mut entry.engine.source_buffers[idx];
                        let old_ranges = if is_managed_sb {
                            Some(sb.buffered_ranges(false))
                        } else {
                            None
                        };
                        sb.updating = false;
                        sb.pending_op = PendingBufferOp::None;
                        let ok = sb.remove(start, end).is_ok();
                        let managed_diff = if ok {
                            old_ranges.map(|old_r| {
                                let new_r = sb.buffered_ranges(false);
                                diff_time_ranges(&old_r, &new_r)
                            })
                        } else {
                            None
                        };
                        let should_restart_streaming = if ok && entry.is_managed && !entry.streaming
                        {
                            let buf_empty = entry.engine.buffered().is_empty();
                            if buf_empty {
                                entry.streaming = true;
                                Some(parent_ms)
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        (ok, managed_diff, should_restart_streaming)
                    };
                    if result.0 {
                        if let Some((added, removed)) = result.1 {
                            fire_buffered_change_event(
                                vm, &h_task, &ev_task, &el_task, &sys_task, sb_h, added, removed,
                            );
                        }
                        fire_object_event(vm, &h_task, &ev_task, &el_task, sb_h, "update", &[]);
                        fire_object_event(vm, &h_task, &ev_task, &el_task, sb_h, "updateend", &[]);
                        if let Some(ms_h) = result.2 {
                            fire_object_event(
                                vm,
                                &h_task,
                                &ev_task,
                                &el_task,
                                ms_h,
                                "startstreaming",
                                &[],
                            );
                        }
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
                let was_up = ms_entry.engine.source_buffers[idx]
                    .abort_spec()
                    .map_err(|e| mse_err_to_js(vm, e))?;
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
    define_to_string_tag(vm, proto, "MediaSource");
    install_null_event_handlers(
        vm,
        proto,
        &["onsourceopen", "onsourceended", "onsourceclose"],
    );
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
        let h_s = Rc::clone(host);
        let ev_s = Rc::clone(events);
        let el_s = Rc::clone(event_loop);
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
                let (changed, actual_dur, attached_nid, me_opt) = {
                    let mut s = sys_s.borrow_mut();
                    let entry = s.media_sources.get_mut(&ms_h).unwrap();
                    let changed = entry
                        .engine
                        .set_duration_spec(d)
                        .map_err(|e| mse_err_to_js(vm, e))?;
                    (
                        changed,
                        entry.engine.duration,
                        entry.attached_element,
                        s.media_elements.clone(),
                    )
                };
                if changed {
                    if let Some(nid) = attached_nid {
                        let elem_v = wrap_node(vm, &h_s, nid);
                        if let Some(elem_h) = elem_v.as_object() {
                            queue_object_event(&h_s, &ev_s, &el_s, elem_h, "durationchange");
                            // W3C MSE §2.4.6: if currentTime > new duration, seek to new duration!
                            if let Some(me) = &me_opt {
                                let should_seek = {
                                    let mut me_b = me.borrow_mut();
                                    let p = me_b.ensure_player(nid);
                                    p.duration_s = actual_dur;
                                    if actual_dur.is_finite()
                                        && p.clock.current_time_s() > actual_dur
                                    {
                                        p.clock.seek_seconds(actual_dur);
                                        p.seeking = true;
                                        p.seek_target_s = Some(actual_dur);
                                        true
                                    } else {
                                        false
                                    }
                                };
                                if should_seek {
                                    queue_object_event(&h_s, &ev_s, &el_s, elem_h, "seeking");
                                }
                            }
                        }
                    }
                }
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

            let (is_managed, sb_proto) = {
                let s = sys.borrow();
                let managed = s.media_sources.get(&ms_h).is_some_and(|e| e.is_managed);
                let p = if managed {
                    s.protos
                        .managed_source_buffer_proto
                        .or(s.protos.source_buffer_proto)
                        .unwrap_or(vm.object_proto)
                } else {
                    s.protos.source_buffer_proto.unwrap_or(vm.object_proto)
                };
                (managed, p)
            };
            let sb_h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(sb_proto)));
            let et_proto = h_cl.borrow().prototypes.event_target_proto;
            let sb_audio_lh = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(et_proto)));
            let sb_video_lh = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(et_proto)));
            let lk = vm.heap.intern_key("length");
            vm.heap.object_define_data(
                sb_audio_lh,
                lk,
                JsValue::from_number(0.0),
                PropertyAttrs::METHOD,
                &mut vm.shapes,
            );
            vm.heap.object_define_data(
                sb_video_lh,
                lk,
                JsValue::from_number(0.0),
                PropertyAttrs::METHOD,
                &mut vm.shapes,
            );

            let (sbl_h, all_handles, mut prev_len) = {
                let mut s = sys.borrow_mut();
                let entry = s.media_sources.get_mut(&ms_h).unwrap();
                entry
                    .engine
                    .add_source_buffer(&mime)
                    .map_err(|e| mse_err_to_js(vm, e))?;
                entry.buffer_handles.push(sb_h);
                let sbl_h = entry.source_buffers_list;
                let all_handles = entry.buffer_handles.clone();
                let prev_len = entry.sbl_synced_len;
                entry.sbl_synced_len = all_handles.len();
                s.source_buffers.insert(
                    sb_h,
                    SourceBufferRef {
                        parent_ms: ms_h,
                        async_generation: 0,
                        is_managed,
                        cached_buffered: None,
                        audio_tracks_list: sb_audio_lh,
                        video_tracks_list: sb_video_lh,
                        audio_track_objs: Vec::new(),
                        video_track_objs: Vec::new(),
                    },
                );
                (sbl_h, all_handles, prev_len)
            };

            sync_sbl_indices(vm, sbl_h, &all_handles, &mut prev_len);
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

            let (
                sbl_h,
                active_h,
                remaining,
                active_rem,
                mut sbl_prev,
                mut active_prev,
                was_active,
                attached_nid,
            ) = {
                let mut s = sys.borrow_mut();
                let entry = s.media_sources.get_mut(&ms_h).unwrap();
                let Some(idx) = entry.buffer_handles.iter().position(|&h| h == sb_h) else {
                    return Err(create_dom_exception(
                        vm,
                        "NotFoundError",
                        "SourceBuffer does not belong to this MediaSource",
                    ));
                };
                let attached_nid = entry.attached_element;
                let was_active = entry.active_buffer_handles.contains(&sb_h);
                entry.buffer_handles.remove(idx);
                entry.disabled_buffers.remove(&sb_h);
                let _ = entry.engine.remove_source_buffer(idx);
                let sbl_h = entry.source_buffers_list;
                let active_h = entry.active_source_buffers_list;
                let remaining = entry.buffer_handles.clone();
                let active_rem = MediaSourceSystem::compute_active_handles(entry);
                entry.active_buffer_handles = active_rem.clone();
                let sbl_prev = entry.sbl_synced_len;
                let active_prev = entry.active_sbl_synced_len;
                entry.sbl_synced_len = remaining.len();
                entry.active_sbl_synced_len = active_rem.len();
                if let Some(sb_ref) = s.source_buffers.get_mut(&sb_h) {
                    sb_ref.async_generation = sb_ref.async_generation.wrapping_add(1);
                }
                (
                    sbl_h,
                    active_h,
                    remaining,
                    active_rem,
                    sbl_prev,
                    active_prev,
                    was_active,
                    attached_nid,
                )
            };

            remove_tracks_for_source_buffer(vm, &h_cl, &ev_cl, &el_cl, &sys, sb_h, attached_nid);
            sync_sbl_indices(vm, sbl_h, &remaining, &mut sbl_prev);
            sync_sbl_indices(vm, active_h, &active_rem, &mut active_prev);
            if was_active {
                queue_object_event(&h_cl, &ev_cl, &el_cl, active_h, "removesourcebuffer");
            }
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
            let (dur_changed, managed_diffs) = {
                let mut s = sys.borrow_mut();
                let entry = s.media_sources.get_mut(&ms_h).unwrap();
                let old_ranges: Vec<(HeapHandle, TimeRanges)> = if entry.is_managed {
                    entry
                        .buffer_handles
                        .iter()
                        .enumerate()
                        .map(|(i, &bh)| (bh, entry.engine.source_buffers[i].buffered_ranges(false)))
                        .collect()
                } else {
                    Vec::new()
                };
                let dur_changed = entry
                    .engine
                    .end_of_stream_spec(eos_err)
                    .map_err(|e| mse_err_to_js(vm, e))?;
                let diffs: Vec<(HeapHandle, TimeRanges, TimeRanges)> = if entry.is_managed {
                    old_ranges
                        .into_iter()
                        .enumerate()
                        .filter_map(|(i, (bh, old_r))| {
                            let new_r = entry.engine.source_buffers.get(i)?.buffered_ranges(true);
                            let (a, r) = diff_time_ranges(&old_r, &new_r);
                            if a.is_empty() && r.is_empty() {
                                None
                            } else {
                                Some((bh, a, r))
                            }
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                (dur_changed, diffs)
            };
            let h_t = Rc::clone(&h_cl);
            let ev_t = Rc::clone(&ev_cl);
            let el_t = Rc::clone(&el_cl);
            let sys_t = Rc::clone(&sys);
            el_cl
                .borrow_mut()
                .post_host_task(TaskPriority::Timer, move |vm| {
                    complete_end_of_stream(
                        vm,
                        &h_t,
                        &ev_t,
                        &el_t,
                        &sys_t,
                        ms_h,
                        eos_err,
                        dur_changed,
                        managed_diffs,
                    );
                });
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
                active_buffer_handles: Vec::new(),
                disabled_buffers: std::collections::HashSet::new(),
                attached_element: None,
                current_playback_time_s: 0.0,
                is_managed: false,
                streaming: false,
                sbl_synced_len: 0,
                active_sbl_synced_len: 0,
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
                        attach_media_source_to_element(vm, nid, ms_h, &h_s, &ev_s, &el_s, &ms_s);
                        return Ok(JsValue::UNDEFINED);
                    }
                }
                detach_media_source_from_element(vm, nid, &h_s, &ev_s, &el_s, &ms_s);
                // If setting a revoked or invalid blob: URL (not in object_urls), fire MEDIA_ERR_SRC_NOT_SUPPORTED.
                if url.starts_with("blob:") && !wa_s.borrow().object_urls.contains_key(&url) {
                    let me_opt = ms_s.borrow().media_elements.clone();
                    let h_t = Rc::clone(&h_s);
                    let ev_t = Rc::clone(&ev_s);
                    let el_t = Rc::clone(&el_s);
                    el_s.borrow_mut()
                        .post_host_task(TaskPriority::Timer, move |vm| {
                            if let Some(me) = &me_opt {
                                let mut me_b = me.borrow_mut();
                                let p = me_b.ensure_player(nid);
                                p.error = Some((
                                    MEDIA_ERR_SRC_NOT_SUPPORTED,
                                    "Revoked or unknown blob URL".to_string(),
                                ));
                                p.network_state = NETWORK_NO_SOURCE;
                            }
                            let elem_v = wrap_node(vm, &h_t, nid);
                            if let Some(elem_h) = elem_v.as_object() {
                                fire_object_event(vm, &h_t, &ev_t, &el_t, elem_h, "error", &[]);
                            }
                        });
                }
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
                    detach_media_source_from_element(vm, nid, &h_s, &ev_s, &el_s, &ms_s);
                    return Ok(JsValue::UNDEFINED);
                }
                if let Some(obj_h) = val.as_object() {
                    if ms_s.borrow().media_sources.contains_key(&obj_h) {
                        attach_media_source_to_element(vm, nid, obj_h, &h_s, &ev_s, &el_s, &ms_s);
                        return Ok(JsValue::UNDEFINED);
                    }
                }
                Err(vm.type_error("srcObject must be a MediaSource or null"))
            },
        );
    }

    // `audioTracks` & `videoTracks` persistent live track lists on `HTMLMediaElement.prototype`
    for &(prop_name, want_video) in &[("audioTracks", false), ("videoTracks", true)] {
        let h_g = Rc::clone(host);
        let ms_g = Rc::clone(ms_sys);
        define_host_getter(vm, media_proto, prop_name, move |vm, this, _| {
            let Some(nid) = h_g.borrow().node_for_value(vm, this) else {
                let et_proto = h_g.borrow().prototypes.event_target_proto;
                let lh = vm
                    .heap
                    .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(et_proto)));
                return Ok(JsValue::from_object(lh));
            };
            let lh = ensure_elem_track_list(vm, &h_g, &ms_g, nid, want_video);
            Ok(JsValue::from_object(lh))
        });
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

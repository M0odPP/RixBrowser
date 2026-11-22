//! W3C WebAudio API JS bindings for `ContentRuntime`:
//! - `AudioContext` / `webkitAudioContext` & `OfflineAudioContext`
//! - `AudioParam` (sample-accurate timeline automation)
//! - `AudioNode`, `GainNode`, `BiquadFilterNode`, `AnalyserNode`, `OscillatorNode`,
//!   `AudioBuffer`, `AudioBufferSourceNode`, `MediaElementAudioSourceNode`, `AudioWorkletNode`
//! - `decodeAudioData(ArrayBuffer)` backed by pure-Rust `rix_media::decode_container_audio`

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use rix_js::heap::{TypedArrayKind, TypedArrayObject};
use rix_js::promise::{alloc_promise, promise_resolve_value, reject_promise};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};
use rix_media::{
    decode_container_audio, AudioBuffer, AudioGraph, AudioNodeId, AudioNodeKind, AudioOutputDevice,
    AudioParam, BiquadFilterType, MediaClock, OscillatorType,
};

use crate::dom_bridge::SharedHost;
use crate::media_elements::SharedMediaElements;
use crate::web_apis::{
    alloc_array_buffer, arg, bind_ctor_proto, create_dom_exception, define_global,
    define_host_accessor, define_host_getter, define_host_method,
};
use crate::webgpu_types::extract_gpu_buffer_source;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParamTarget {
    GainValue,
    BiquadFrequency,
    BiquadDetune,
    BiquadQ,
    BiquadGain,
    OscFrequency,
    OscDetune,
    BufferPlaybackRate,
}

#[derive(Debug, Clone, Default)]
struct WebAudioProtos {
    base_ctx_proto: Option<HeapHandle>,
    audio_ctx_proto: Option<HeapHandle>,
    offline_ctx_proto: Option<HeapHandle>,
    audio_node_proto: Option<HeapHandle>,
    gain_proto: Option<HeapHandle>,
    biquad_proto: Option<HeapHandle>,
    analyser_proto: Option<HeapHandle>,
    osc_proto: Option<HeapHandle>,
    buffer_source_proto: Option<HeapHandle>,
    media_source_proto: Option<HeapHandle>,
    worklet_proto: Option<HeapHandle>,
    param_proto: Option<HeapHandle>,
    buffer_proto: Option<HeapHandle>,
}

/// Host entry backing one JS `AudioContext` or `OfflineAudioContext`.
pub struct AudioContextEntry {
    pub graph: Arc<Mutex<AudioGraph>>,
    pub state: &'static str,
    pub destination_handle: HeapHandle,
    pub worklet_handle: HeapHandle,
    pub offline_length: Option<usize>,
    pub output_device: Option<AudioOutputDevice>,
}

#[derive(Debug, Clone, Copy)]
struct AudioNodeRef {
    ctx: HeapHandle,
    id: AudioNodeId,
}

#[derive(Debug, Clone, Copy)]
struct AudioParamRef {
    ctx: HeapHandle,
    node_id: AudioNodeId,
    target: ParamTarget,
}

/// Per-`ContentRuntime` WebAudio registry.
pub struct WebAudioSystem {
    protos: WebAudioProtos,
    pub contexts: HashMap<HeapHandle, AudioContextEntry>,
    nodes: HashMap<HeapHandle, AudioNodeRef>,
    params: HashMap<HeapHandle, AudioParamRef>,
    buffers: HashMap<HeapHandle, Arc<AudioBuffer>>,
}

/// Shared handle to [`WebAudioSystem`].
pub type SharedWebAudio = Rc<RefCell<WebAudioSystem>>;

impl Default for WebAudioSystem {
    fn default() -> Self {
        Self::new()
    }
}

impl WebAudioSystem {
    pub fn new() -> Self {
        Self {
            protos: WebAudioProtos::default(),
            contexts: HashMap::new(),
            nodes: HashMap::new(),
            params: HashMap::new(),
            buffers: HashMap::new(),
        }
    }

    /// Steps all running real-time `AudioContext`s by `frames` samples (useful for deterministic
    /// headless rendering / testing when OS audio hardware is absent).
    pub fn step_realtime_contexts(&mut self, frames: usize) {
        if frames == 0 {
            return;
        }
        for ctx in self.contexts.values() {
            if ctx.offline_length.is_none() && ctx.state == "running" {
                let mut g = ctx.graph.lock().unwrap_or_else(|e| e.into_inner());
                let ch = g.channels();
                let mut scratch = vec![0.0f32; frames * ch];
                g.render_interleaved(&mut scratch);
            }
        }
    }

    /// Collects all GC roots retained by live WebAudio objects.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for p in [
            self.protos.base_ctx_proto,
            self.protos.audio_ctx_proto,
            self.protos.offline_ctx_proto,
            self.protos.audio_node_proto,
            self.protos.gain_proto,
            self.protos.biquad_proto,
            self.protos.analyser_proto,
            self.protos.osc_proto,
            self.protos.buffer_source_proto,
            self.protos.media_source_proto,
            self.protos.worklet_proto,
            self.protos.param_proto,
            self.protos.buffer_proto,
        ]
        .into_iter()
        .flatten()
        {
            out.push(JsValue::from_object(p));
        }
        for entry in self.contexts.values() {
            out.push(JsValue::from_object(entry.destination_handle));
            out.push(JsValue::from_object(entry.worklet_handle));
        }
    }

    /// Sweeps dead JS handles after Major GC.
    pub fn sweep_dead(&mut self, heap: &rix_js::GcHeap) {
        self.buffers.retain(|&h, _| heap.is_alive(h));
        self.params.retain(|&h, _| heap.is_alive(h));
        self.nodes.retain(|&h, _| heap.is_alive(h));
        self.contexts.retain(|&h, _| heap.is_alive(h));
    }

    fn alloc_param(
        &mut self,
        vm: &mut JsVm,
        ctx: HeapHandle,
        node_id: AudioNodeId,
        target: ParamTarget,
    ) -> HeapHandle {
        let proto = self.protos.param_proto.unwrap_or(vm.object_proto);
        let h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        self.params.insert(
            h,
            AudioParamRef {
                ctx,
                node_id,
                target,
            },
        );
        h
    }

    fn alloc_audio_buffer(&mut self, vm: &mut JsVm, buf: Arc<AudioBuffer>) -> HeapHandle {
        let proto = self.protos.buffer_proto.unwrap_or(vm.object_proto);
        let h = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        self.buffers.insert(h, buf);
        h
    }
}

fn with_param<R>(
    sys: &SharedWebAudio,
    param_h: HeapHandle,
    f: impl FnOnce(&AudioParam) -> R,
) -> Option<R> {
    let pref = *sys.borrow().params.get(&param_h)?;
    let graph = Arc::clone(&sys.borrow().contexts.get(&pref.ctx)?.graph);
    let g = graph.lock().unwrap_or_else(|e| e.into_inner());
    let node = g.node(pref.node_id)?;
    let p = match (node, pref.target) {
        (AudioNodeKind::Gain { gain }, ParamTarget::GainValue) => gain,
        (AudioNodeKind::BiquadFilter(bf), ParamTarget::BiquadFrequency) => &bf.frequency,
        (AudioNodeKind::BiquadFilter(bf), ParamTarget::BiquadDetune) => &bf.detune,
        (AudioNodeKind::BiquadFilter(bf), ParamTarget::BiquadQ) => &bf.q,
        (AudioNodeKind::BiquadFilter(bf), ParamTarget::BiquadGain) => &bf.gain,
        (AudioNodeKind::Oscillator { frequency, .. }, ParamTarget::OscFrequency) => frequency,
        (AudioNodeKind::Oscillator { detune, .. }, ParamTarget::OscDetune) => detune,
        (AudioNodeKind::BufferSource { playback_rate, .. }, ParamTarget::BufferPlaybackRate) => {
            playback_rate
        }
        _ => return None,
    };
    Some(f(p))
}

fn with_param_mut<R>(
    sys: &SharedWebAudio,
    param_h: HeapHandle,
    f: impl FnOnce(&mut AudioParam) -> R,
) -> Option<R> {
    let pref = *sys.borrow().params.get(&param_h)?;
    let graph = Arc::clone(&sys.borrow().contexts.get(&pref.ctx)?.graph);
    let mut g = graph.lock().unwrap_or_else(|e| e.into_inner());
    let node = g.node_mut(pref.node_id)?;
    let p = match (node, pref.target) {
        (AudioNodeKind::Gain { gain }, ParamTarget::GainValue) => gain,
        (AudioNodeKind::BiquadFilter(bf), ParamTarget::BiquadFrequency) => &mut bf.frequency,
        (AudioNodeKind::BiquadFilter(bf), ParamTarget::BiquadDetune) => &mut bf.detune,
        (AudioNodeKind::BiquadFilter(bf), ParamTarget::BiquadQ) => &mut bf.q,
        (AudioNodeKind::BiquadFilter(bf), ParamTarget::BiquadGain) => &mut bf.gain,
        (AudioNodeKind::Oscillator { frequency, .. }, ParamTarget::OscFrequency) => frequency,
        (AudioNodeKind::Oscillator { detune, .. }, ParamTarget::OscDetune) => detune,
        (AudioNodeKind::BufferSource { playback_rate, .. }, ParamTarget::BufferPlaybackRate) => {
            playback_rate
        }
        _ => return None,
    };
    Some(f(p))
}

fn alloc_float32_array(vm: &mut JsVm, data: &[f32]) -> HeapHandle {
    let mut bytes = Vec::with_capacity(data.len() * 4);
    for &s in data {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    let buf = alloc_array_buffer(vm, bytes);
    vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::TypedArray(TypedArrayObject {
            kind: TypedArrayKind::Float32,
            buffer: buf,
            byte_offset: 0,
            length: data.len() as u32,
        }),
        Some(vm.typed_array_protos[TypedArrayKind::Float32 as usize]),
    ))
}

fn write_f32_into_typed_array(vm: &mut JsVm, val: JsValue, src: &[f32]) -> Result<(), JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Expected Float32Array argument"));
    };
    let ta = match vm.heap.get_object(h).map(|o| &o.kind) {
        Some(ObjectKind::TypedArray(ta)) if ta.kind == TypedArrayKind::Float32 => *ta,
        _ => return Err(vm.type_error("Expected Float32Array argument")),
    };
    let count = (ta.length as usize).min(src.len());
    let off = ta.byte_offset as usize;
    if let Some(ObjectKind::ArrayBuffer(buf)) =
        vm.heap.get_object_mut(ta.buffer).map(|o| &mut o.kind)
    {
        for (i, &sample) in src.iter().take(count).enumerate() {
            let pos = off + i * 4;
            if pos + 4 <= buf.len() {
                buf[pos..pos + 4].copy_from_slice(&sample.to_le_bytes());
            }
        }
    }
    Ok(())
}

fn write_u8_into_typed_array(vm: &mut JsVm, val: JsValue, src: &[u8]) -> Result<(), JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Expected Uint8Array argument"));
    };
    let ta = match vm.heap.get_object(h).map(|o| &o.kind) {
        Some(ObjectKind::TypedArray(ta)) if ta.kind == TypedArrayKind::Uint8 => *ta,
        _ => return Err(vm.type_error("Expected Uint8Array argument")),
    };
    let count = (ta.length as usize).min(src.len());
    let off = ta.byte_offset as usize;
    if let Some(ObjectKind::ArrayBuffer(buf)) =
        vm.heap.get_object_mut(ta.buffer).map(|o| &mut o.kind)
    {
        let end = (off + count).min(buf.len());
        if off < end {
            buf[off..end].copy_from_slice(&src[..end - off]);
        }
    }
    Ok(())
}

fn define_ro_prop(vm: &mut JsVm, obj: HeapHandle, name: &str, val: JsValue) {
    let k = vm.heap.intern_key(name);
    vm.heap
        .object_define_data(obj, k, val, PropertyAttrs::METHOD, &mut vm.shapes);
}

fn make_proto(vm: &mut JsVm, parent: HeapHandle) -> HeapHandle {
    let h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(parent)));
    vm.heap.get_object_mut(h).unwrap().is_prototype = true;
    h
}

/// Installs all WebAudio APIs (`AudioContext`, `webkitAudioContext`, `OfflineAudioContext`,
/// `AudioNode`, `GainNode`, `BiquadFilterNode`, `AnalyserNode`, `OscillatorNode`,
/// `AudioBuffer`, `AudioBufferSourceNode`, `MediaElementAudioSourceNode`, `AudioWorkletNode`,
/// `AudioParam`) onto `vm`.
pub fn install_webaudio(
    vm: &mut JsVm,
    host: &SharedHost,
    media_elements: &SharedMediaElements,
) -> SharedWebAudio {
    let wa = Rc::new(RefCell::new(WebAudioSystem::new()));
    let et_proto = host.borrow().prototypes.event_target_proto;

    install_audio_param_proto(vm, &wa);
    install_audio_buffer_proto(vm, &wa);
    install_audio_node_protos(vm, et_proto, &wa);
    install_audio_context_ctors(vm, et_proto, host, media_elements, &wa);

    wa
}

fn install_audio_param_proto(vm: &mut JsVm, wa: &SharedWebAudio) {
    let proto = make_proto(vm, vm.object_proto);
    wa.borrow_mut().protos.param_proto = Some(proto);

    {
        let sys_g = Rc::clone(wa);
        let sys_s = Rc::clone(wa);
        define_host_accessor(
            vm,
            proto,
            "value",
            move |vm, this, _| {
                let h = this
                    .as_object()
                    .ok_or_else(|| vm.type_error("Invalid AudioParam"))?;
                let v = with_param(&sys_g, h, |p| p.value).unwrap_or(0.0);
                Ok(JsValue::from_number(f64::from(v)))
            },
            move |vm, this, args| {
                let h = this
                    .as_object()
                    .ok_or_else(|| vm.type_error("Invalid AudioParam"))?;
                let v = vm.to_number(arg(args, 0))? as f32;
                with_param_mut(&sys_s, h, |p| p.set_value(v));
                Ok(JsValue::UNDEFINED)
            },
        );
    }
    for (name, extractor) in [
        (
            "defaultValue",
            (|p: &AudioParam| p.default_value) as fn(&AudioParam) -> f32,
        ),
        ("minValue", |p: &AudioParam| p.min_value),
        ("maxValue", |p: &AudioParam| p.max_value),
    ] {
        let sys = Rc::clone(wa);
        define_host_getter(vm, proto, name, move |vm, this, _| {
            let h = this
                .as_object()
                .ok_or_else(|| vm.type_error("Invalid AudioParam"))?;
            let v = with_param(&sys, h, extractor).unwrap_or(0.0);
            Ok(JsValue::from_number(f64::from(v)))
        });
    }

    {
        let sys = Rc::clone(wa);
        define_host_method(vm, proto, "setValueAtTime", move |vm, this, args| {
            let h = this
                .as_object()
                .ok_or_else(|| vm.type_error("Invalid AudioParam"))?;
            let v = vm.to_number(arg(args, 0))? as f32;
            let t = vm.to_number(arg(args, 1))?;
            with_param_mut(&sys, h, |p| p.set_value_at_time(v, t));
            Ok(this)
        });
    }
    {
        let sys = Rc::clone(wa);
        define_host_method(
            vm,
            proto,
            "linearRampToValueAtTime",
            move |vm, this, args| {
                let h = this
                    .as_object()
                    .ok_or_else(|| vm.type_error("Invalid AudioParam"))?;
                let v = vm.to_number(arg(args, 0))? as f32;
                let t = vm.to_number(arg(args, 1))?;
                with_param_mut(&sys, h, |p| p.linear_ramp_to_value_at_time(v, t));
                Ok(this)
            },
        );
    }
    {
        let sys = Rc::clone(wa);
        define_host_method(
            vm,
            proto,
            "exponentialRampToValueAtTime",
            move |vm, this, args| {
                let h = this
                    .as_object()
                    .ok_or_else(|| vm.type_error("Invalid AudioParam"))?;
                let v = vm.to_number(arg(args, 0))? as f32;
                let t = vm.to_number(arg(args, 1))?;
                with_param_mut(&sys, h, |p| p.exponential_ramp_to_value_at_time(v, t));
                Ok(this)
            },
        );
    }
    {
        let sys = Rc::clone(wa);
        define_host_method(vm, proto, "setTargetAtTime", move |vm, this, args| {
            let h = this
                .as_object()
                .ok_or_else(|| vm.type_error("Invalid AudioParam"))?;
            let v = vm.to_number(arg(args, 0))? as f32;
            let t = vm.to_number(arg(args, 1))?;
            let tc = vm.to_number(arg(args, 2))?;
            with_param_mut(&sys, h, |p| p.set_target_at_time(v, t, tc));
            Ok(this)
        });
    }
    {
        let sys = Rc::clone(wa);
        define_host_method(vm, proto, "cancelScheduledValues", move |vm, this, args| {
            let h = this
                .as_object()
                .ok_or_else(|| vm.type_error("Invalid AudioParam"))?;
            let t = vm.to_number(arg(args, 0))?;
            with_param_mut(&sys, h, |p| p.cancel_scheduled_values(t));
            Ok(this)
        });
    }

    let ctor = vm.alloc_host_fn("AudioParam", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: AudioParam"))
    });
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "AudioParam", JsValue::from_object(ctor));
}

fn install_audio_buffer_proto(vm: &mut JsVm, wa: &SharedWebAudio) {
    let proto = make_proto(vm, vm.object_proto);
    wa.borrow_mut().protos.buffer_proto = Some(proto);

    for (name, extractor) in [
        (
            "sampleRate",
            (|b: &AudioBuffer| f64::from(b.sample_rate)) as fn(&AudioBuffer) -> f64,
        ),
        ("length", |b: &AudioBuffer| b.length() as f64),
        ("duration", |b: &AudioBuffer| b.duration()),
        ("numberOfChannels", |b: &AudioBuffer| {
            b.number_of_channels() as f64
        }),
    ] {
        let sys = Rc::clone(wa);
        define_host_getter(vm, proto, name, move |vm, this, _| {
            let h = this
                .as_object()
                .ok_or_else(|| vm.type_error("Invalid AudioBuffer"))?;
            let v = sys
                .borrow()
                .buffers
                .get(&h)
                .map(|b| extractor(b))
                .ok_or_else(|| vm.type_error("Invalid AudioBuffer"))?;
            Ok(JsValue::from_number(v))
        });
    }

    {
        let sys = Rc::clone(wa);
        define_host_method(vm, proto, "getChannelData", move |vm, this, args| {
            let h = this
                .as_object()
                .ok_or_else(|| vm.type_error("Invalid AudioBuffer"))?;
            let ch = vm.to_number(arg(args, 0))?.max(0.0) as usize;
            let data = {
                let s = sys.borrow();
                let buf = s
                    .buffers
                    .get(&h)
                    .ok_or_else(|| vm.type_error("Invalid AudioBuffer"))?;
                let Some(slice) = buf.channels.get(ch) else {
                    return Err(create_dom_exception(
                        vm,
                        "IndexSizeError",
                        "AudioBuffer channel index out of bounds",
                    ));
                };
                slice.clone()
            };
            Ok(JsValue::from_object(alloc_float32_array(vm, &data)))
        });
    }

    {
        let sys = Rc::clone(wa);
        define_host_method(vm, proto, "copyFromChannel", move |vm, this, args| {
            let h = this
                .as_object()
                .ok_or_else(|| vm.type_error("Invalid AudioBuffer"))?;
            let ch = vm.to_number(arg(args, 1))?.max(0.0) as usize;
            let start = if args.len() > 2 && !arg(args, 2).is_undefined() {
                vm.to_number(arg(args, 2))?.max(0.0) as usize
            } else {
                0
            };
            let slice = {
                let s = sys.borrow();
                let buf = s
                    .buffers
                    .get(&h)
                    .ok_or_else(|| vm.type_error("Invalid AudioBuffer"))?;
                let Some(c) = buf.channels.get(ch) else {
                    return Err(create_dom_exception(
                        vm,
                        "IndexSizeError",
                        "AudioBuffer channel index out of bounds",
                    ));
                };
                c[start.min(c.len())..].to_vec()
            };
            write_f32_into_typed_array(vm, arg(args, 0), &slice)?;
            Ok(JsValue::UNDEFINED)
        });
    }

    {
        let sys = Rc::clone(wa);
        define_host_method(vm, proto, "copyToChannel", move |vm, this, args| {
            let h = this
                .as_object()
                .ok_or_else(|| vm.type_error("Invalid AudioBuffer"))?;
            let src_bytes = extract_gpu_buffer_source(vm, arg(args, 0), None, None)
                .ok_or_else(|| vm.type_error("copyToChannel requires Float32Array"))?;
            let ch = vm.to_number(arg(args, 1))?.max(0.0) as usize;
            let start = if args.len() > 2 && !arg(args, 2).is_undefined() {
                vm.to_number(arg(args, 2))?.max(0.0) as usize
            } else {
                0
            };
            let floats: Vec<f32> = src_bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|&b| f32::from_le_bytes(b))
                .collect();
            let mut s = sys.borrow_mut();
            let buf_arc = s
                .buffers
                .get_mut(&h)
                .ok_or_else(|| vm.type_error("Invalid AudioBuffer"))?;
            let mut cloned = (**buf_arc).clone();
            if ch >= cloned.channels.len() {
                return Err(create_dom_exception(
                    vm,
                    "IndexSizeError",
                    "AudioBuffer channel index out of bounds",
                ));
            }
            let target = &mut cloned.channels[ch];
            for (i, &val) in floats.iter().enumerate() {
                let pos = start + i;
                if pos < target.len() {
                    target[pos] = val;
                }
            }
            *buf_arc = Arc::new(cloned);
            Ok(JsValue::UNDEFINED)
        });
    }

    let ctor = vm.alloc_host_fn("AudioBuffer", |vm, _, _| {
        Err(vm.type_error("Use AudioContext.createBuffer()"))
    });
    bind_ctor_proto(vm, ctor, proto);
    define_global(vm, "AudioBuffer", JsValue::from_object(ctor));
}

fn install_audio_node_protos(vm: &mut JsVm, et_proto: HeapHandle, wa: &SharedWebAudio) {
    let node_proto = make_proto(vm, et_proto);
    let gain_proto = make_proto(vm, node_proto);
    let biquad_proto = make_proto(vm, node_proto);
    let analyser_proto = make_proto(vm, node_proto);
    let osc_proto = make_proto(vm, node_proto);
    let bs_proto = make_proto(vm, node_proto);
    let ms_proto = make_proto(vm, node_proto);
    let worklet_proto = make_proto(vm, node_proto);

    {
        let mut s = wa.borrow_mut();
        s.protos.audio_node_proto = Some(node_proto);
        s.protos.gain_proto = Some(gain_proto);
        s.protos.biquad_proto = Some(biquad_proto);
        s.protos.analyser_proto = Some(analyser_proto);
        s.protos.osc_proto = Some(osc_proto);
        s.protos.buffer_source_proto = Some(bs_proto);
        s.protos.media_source_proto = Some(ms_proto);
        s.protos.worklet_proto = Some(worklet_proto);
    }

    // `AudioNode.prototype.connect(destination)` & `disconnect(destination?)`
    {
        let sys = Rc::clone(wa);
        define_host_method(vm, node_proto, "connect", move |vm, this, args| {
            let src_h = this
                .as_object()
                .ok_or_else(|| vm.type_error("Invalid AudioNode"))?;
            let dst_val = arg(args, 0);
            let dst_h = dst_val
                .as_object()
                .ok_or_else(|| vm.type_error("AudioNode.connect requires a target AudioNode"))?;
            let (src_ref, dst_ref) = {
                let s = sys.borrow();
                (s.nodes.get(&src_h).copied(), s.nodes.get(&dst_h).copied())
            };
            let (Some(sr), Some(dr)) = (src_ref, dst_ref) else {
                return Err(vm.type_error("Invalid AudioNode connect target"));
            };
            if sr.ctx != dr.ctx {
                return Err(create_dom_exception(
                    vm,
                    "InvalidAccessError",
                    "Cannot connect AudioNodes belonging to different AudioContexts",
                ));
            }
            if let Some(ctx) = sys.borrow().contexts.get(&sr.ctx) {
                ctx.graph
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .connect(sr.id, dr.id);
            }
            Ok(dst_val)
        });
    }
    {
        let sys = Rc::clone(wa);
        define_host_method(vm, node_proto, "disconnect", move |vm, this, args| {
            let src_h = this
                .as_object()
                .ok_or_else(|| vm.type_error("Invalid AudioNode"))?;
            let dst_id = arg(args, 0)
                .as_object()
                .and_then(|h| sys.borrow().nodes.get(&h).map(|r| r.id));
            if let Some(sr) = sys.borrow().nodes.get(&src_h).copied() {
                if let Some(ctx) = sys.borrow().contexts.get(&sr.ctx) {
                    ctx.graph
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .disconnect(sr.id, dst_id);
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    // `BiquadFilterNode.prototype.type`
    {
        let sys_g = Rc::clone(wa);
        let sys_s = Rc::clone(wa);
        define_host_accessor(
            vm,
            biquad_proto,
            "type",
            move |vm, this, _| {
                let h = this
                    .as_object()
                    .ok_or_else(|| vm.type_error("Invalid BiquadFilterNode"))?;
                let ty = {
                    let s = sys_g.borrow();
                    let nr = s
                        .nodes
                        .get(&h)
                        .ok_or_else(|| vm.type_error("Invalid BiquadFilterNode"))?;
                    let g = s.contexts[&nr.ctx]
                        .graph
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    match g.node(nr.id) {
                        Some(AudioNodeKind::BiquadFilter(bf)) => bf.filter_type.as_str(),
                        _ => "lowpass",
                    }
                };
                Ok(JsValue::from_string(vm.heap.alloc_string(ty)))
            },
            move |vm, this, args| {
                let h = this
                    .as_object()
                    .ok_or_else(|| vm.type_error("Invalid BiquadFilterNode"))?;
                let raw = vm.value_to_rust_string(arg(args, 0));
                if let Some(parsed) = BiquadFilterType::parse(&raw) {
                    let s = sys_s.borrow();
                    if let Some(nr) = s.nodes.get(&h) {
                        let mut g = s.contexts[&nr.ctx]
                            .graph
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        if let Some(AudioNodeKind::BiquadFilter(bf)) = g.node_mut(nr.id) {
                            bf.filter_type = parsed;
                        }
                    }
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // `AnalyserNode` properties & FFT/waveform methods
    {
        let sys_g = Rc::clone(wa);
        let sys_s = Rc::clone(wa);
        define_host_accessor(
            vm,
            analyser_proto,
            "fftSize",
            move |vm, this, _| {
                let h = this
                    .as_object()
                    .ok_or_else(|| vm.type_error("Invalid AnalyserNode"))?;
                let sz = {
                    let s = sys_g.borrow();
                    let nr = s
                        .nodes
                        .get(&h)
                        .ok_or_else(|| vm.type_error("Invalid AnalyserNode"))?;
                    let g = s.contexts[&nr.ctx]
                        .graph
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    match g.node(nr.id) {
                        Some(AudioNodeKind::Analyser(an)) => an.fft_size,
                        _ => 2048,
                    }
                };
                Ok(JsValue::from_number(sz as f64))
            },
            move |vm, this, args| {
                let h = this
                    .as_object()
                    .ok_or_else(|| vm.type_error("Invalid AnalyserNode"))?;
                let sz = vm.to_number(arg(args, 0))?.max(0.0) as usize;
                let res = {
                    let s = sys_s.borrow();
                    let nr = s
                        .nodes
                        .get(&h)
                        .ok_or_else(|| vm.type_error("Invalid AnalyserNode"))?;
                    let mut g = s.contexts[&nr.ctx]
                        .graph
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    match g.node_mut(nr.id) {
                        Some(AudioNodeKind::Analyser(an)) => an.set_fft_size(sz),
                        _ => Ok(()),
                    }
                };
                res.map_err(|msg| create_dom_exception(vm, "IndexSizeError", msg))?;
                Ok(JsValue::UNDEFINED)
            },
        );
    }
    {
        let sys = Rc::clone(wa);
        define_host_getter(
            vm,
            analyser_proto,
            "frequencyBinCount",
            move |vm, this, _| {
                let h = this
                    .as_object()
                    .ok_or_else(|| vm.type_error("Invalid AnalyserNode"))?;
                let bins = {
                    let s = sys.borrow();
                    let nr = s
                        .nodes
                        .get(&h)
                        .ok_or_else(|| vm.type_error("Invalid AnalyserNode"))?;
                    let g = s.contexts[&nr.ctx]
                        .graph
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    match g.node(nr.id) {
                        Some(AudioNodeKind::Analyser(an)) => an.frequency_bin_count(),
                        _ => 1024,
                    }
                };
                Ok(JsValue::from_number(bins as f64))
            },
        );
    }
    {
        let sys_g = Rc::clone(wa);
        let sys_s = Rc::clone(wa);
        define_host_accessor(
            vm,
            analyser_proto,
            "smoothingTimeConstant",
            move |vm, this, _| {
                let h = this
                    .as_object()
                    .ok_or_else(|| vm.type_error("Invalid AnalyserNode"))?;
                let v = {
                    let s = sys_g.borrow();
                    let nr = s
                        .nodes
                        .get(&h)
                        .ok_or_else(|| vm.type_error("Invalid AnalyserNode"))?;
                    let g = s.contexts[&nr.ctx]
                        .graph
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    match g.node(nr.id) {
                        Some(AudioNodeKind::Analyser(an)) => an.smoothing_time_constant,
                        _ => 0.8,
                    }
                };
                Ok(JsValue::from_number(f64::from(v)))
            },
            move |vm, this, args| {
                let h = this
                    .as_object()
                    .ok_or_else(|| vm.type_error("Invalid AnalyserNode"))?;
                let v = vm.to_number(arg(args, 0))? as f32;
                let s = sys_s.borrow();
                if let Some(nr) = s.nodes.get(&h) {
                    let mut g = s.contexts[&nr.ctx]
                        .graph
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some(AudioNodeKind::Analyser(an)) = g.node_mut(nr.id) {
                        an.smoothing_time_constant = v.clamp(0.0, 1.0);
                    }
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }
    {
        let sys = Rc::clone(wa);
        define_host_method(
            vm,
            analyser_proto,
            "getFloatFrequencyData",
            move |vm, this, args| {
                let h = this
                    .as_object()
                    .ok_or_else(|| vm.type_error("Invalid AnalyserNode"))?;
                let data = {
                    let s = sys.borrow();
                    let nr = s
                        .nodes
                        .get(&h)
                        .ok_or_else(|| vm.type_error("Invalid AnalyserNode"))?;
                    let mut g = s.contexts[&nr.ctx]
                        .graph
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    match g.node_mut(nr.id) {
                        Some(AudioNodeKind::Analyser(an)) => {
                            let mut out = vec![-160.0f32; an.frequency_bin_count()];
                            an.get_float_frequency_data(&mut out);
                            out
                        }
                        _ => Vec::new(),
                    }
                };
                write_f32_into_typed_array(vm, arg(args, 0), &data)?;
                Ok(JsValue::UNDEFINED)
            },
        );
    }
    {
        let sys = Rc::clone(wa);
        define_host_method(
            vm,
            analyser_proto,
            "getByteFrequencyData",
            move |vm, this, args| {
                let h = this
                    .as_object()
                    .ok_or_else(|| vm.type_error("Invalid AnalyserNode"))?;
                let data = {
                    let s = sys.borrow();
                    let nr = s
                        .nodes
                        .get(&h)
                        .ok_or_else(|| vm.type_error("Invalid AnalyserNode"))?;
                    let mut g = s.contexts[&nr.ctx]
                        .graph
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    match g.node_mut(nr.id) {
                        Some(AudioNodeKind::Analyser(an)) => {
                            let mut out = vec![0u8; an.frequency_bin_count()];
                            an.get_byte_frequency_data(&mut out);
                            out
                        }
                        _ => Vec::new(),
                    }
                };
                write_u8_into_typed_array(vm, arg(args, 0), &data)?;
                Ok(JsValue::UNDEFINED)
            },
        );
    }
    {
        let sys = Rc::clone(wa);
        define_host_method(
            vm,
            analyser_proto,
            "getFloatTimeDomainData",
            move |vm, this, args| {
                let h = this
                    .as_object()
                    .ok_or_else(|| vm.type_error("Invalid AnalyserNode"))?;
                let data = {
                    let s = sys.borrow();
                    let nr = s
                        .nodes
                        .get(&h)
                        .ok_or_else(|| vm.type_error("Invalid AnalyserNode"))?;
                    let g = s.contexts[&nr.ctx]
                        .graph
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    match g.node(nr.id) {
                        Some(AudioNodeKind::Analyser(an)) => {
                            let mut out = vec![0.0f32; an.fft_size];
                            an.get_float_time_domain_data(&mut out);
                            out
                        }
                        _ => Vec::new(),
                    }
                };
                write_f32_into_typed_array(vm, arg(args, 0), &data)?;
                Ok(JsValue::UNDEFINED)
            },
        );
    }
    {
        let sys = Rc::clone(wa);
        define_host_method(
            vm,
            analyser_proto,
            "getByteTimeDomainData",
            move |vm, this, args| {
                let h = this
                    .as_object()
                    .ok_or_else(|| vm.type_error("Invalid AnalyserNode"))?;
                let data = {
                    let s = sys.borrow();
                    let nr = s
                        .nodes
                        .get(&h)
                        .ok_or_else(|| vm.type_error("Invalid AnalyserNode"))?;
                    let g = s.contexts[&nr.ctx]
                        .graph
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    match g.node(nr.id) {
                        Some(AudioNodeKind::Analyser(an)) => {
                            let mut out = vec![128u8; an.fft_size];
                            an.get_byte_time_domain_data(&mut out);
                            out
                        }
                        _ => Vec::new(),
                    }
                };
                write_u8_into_typed_array(vm, arg(args, 0), &data)?;
                Ok(JsValue::UNDEFINED)
            },
        );
    }

    // `OscillatorNode`: `type`, `start(when?)`, `stop(when?)`
    {
        let sys_g = Rc::clone(wa);
        let sys_s = Rc::clone(wa);
        define_host_accessor(
            vm,
            osc_proto,
            "type",
            move |vm, this, _| {
                let h = this
                    .as_object()
                    .ok_or_else(|| vm.type_error("Invalid OscillatorNode"))?;
                let ty = {
                    let s = sys_g.borrow();
                    let nr = s
                        .nodes
                        .get(&h)
                        .ok_or_else(|| vm.type_error("Invalid OscillatorNode"))?;
                    let g = s.contexts[&nr.ctx]
                        .graph
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    match g.node(nr.id) {
                        Some(AudioNodeKind::Oscillator { osc_type, .. }) => osc_type.as_str(),
                        _ => "sine",
                    }
                };
                Ok(JsValue::from_string(vm.heap.alloc_string(ty)))
            },
            move |vm, this, args| {
                let h = this
                    .as_object()
                    .ok_or_else(|| vm.type_error("Invalid OscillatorNode"))?;
                let raw = vm.value_to_rust_string(arg(args, 0));
                if let Some(parsed) = OscillatorType::parse(&raw) {
                    let s = sys_s.borrow();
                    if let Some(nr) = s.nodes.get(&h) {
                        let mut g = s.contexts[&nr.ctx]
                            .graph
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        if let Some(AudioNodeKind::Oscillator { osc_type, .. }) = g.node_mut(nr.id)
                        {
                            *osc_type = parsed;
                        }
                    }
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }
    {
        let sys = Rc::clone(wa);
        define_host_method(vm, osc_proto, "start", move |vm, this, args| {
            let h = this
                .as_object()
                .ok_or_else(|| vm.type_error("Invalid OscillatorNode"))?;
            let when = if args.is_empty() || arg(args, 0).is_undefined() {
                0.0
            } else {
                vm.to_number(arg(args, 0))?.max(0.0)
            };
            let s = sys.borrow();
            if let Some(nr) = s.nodes.get(&h) {
                let mut g = s.contexts[&nr.ctx]
                    .graph
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(AudioNodeKind::Oscillator { start_time, .. }) = g.node_mut(nr.id) {
                    *start_time = Some(when);
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let sys = Rc::clone(wa);
        define_host_method(vm, osc_proto, "stop", move |vm, this, args| {
            let h = this
                .as_object()
                .ok_or_else(|| vm.type_error("Invalid OscillatorNode"))?;
            let when = if args.is_empty() || arg(args, 0).is_undefined() {
                0.0
            } else {
                vm.to_number(arg(args, 0))?.max(0.0)
            };
            let s = sys.borrow();
            if let Some(nr) = s.nodes.get(&h) {
                let mut g = s.contexts[&nr.ctx]
                    .graph
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(AudioNodeKind::Oscillator { stop_time, .. }) = g.node_mut(nr.id) {
                    *stop_time = Some(when);
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    // `AudioBufferSourceNode`: `buffer`, `loop`, `start(when?, offset?, duration?)`, `stop(when?)`
    {
        let sys_s = Rc::clone(wa);
        define_host_accessor(
            vm,
            bs_proto,
            "buffer",
            |_, _, _| Ok(JsValue::NULL),
            move |vm, this, args| {
                let h = this
                    .as_object()
                    .ok_or_else(|| vm.type_error("Invalid AudioBufferSourceNode"))?;
                let val = arg(args, 0);
                let buf_opt = if val.is_null() || val.is_undefined() {
                    None
                } else {
                    let bh = val
                        .as_object()
                        .ok_or_else(|| vm.type_error("buffer must be an AudioBuffer or null"))?;
                    let b =
                        sys_s.borrow().buffers.get(&bh).cloned().ok_or_else(|| {
                            vm.type_error("buffer must be an AudioBuffer or null")
                        })?;
                    Some(b)
                };
                let s = sys_s.borrow();
                if let Some(nr) = s.nodes.get(&h) {
                    let mut g = s.contexts[&nr.ctx]
                        .graph
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some(AudioNodeKind::BufferSource { buffer, .. }) = g.node_mut(nr.id) {
                        *buffer = buf_opt;
                    }
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }
    {
        let sys_g = Rc::clone(wa);
        let sys_s = Rc::clone(wa);
        define_host_accessor(
            vm,
            bs_proto,
            "loop",
            move |vm, this, _| {
                let h = this
                    .as_object()
                    .ok_or_else(|| vm.type_error("Invalid AudioBufferSourceNode"))?;
                let v = {
                    let s = sys_g.borrow();
                    let nr = s
                        .nodes
                        .get(&h)
                        .ok_or_else(|| vm.type_error("Invalid node"))?;
                    let g = s.contexts[&nr.ctx]
                        .graph
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    match g.node(nr.id) {
                        Some(AudioNodeKind::BufferSource { loop_enabled, .. }) => *loop_enabled,
                        _ => false,
                    }
                };
                Ok(JsValue::from_bool(v))
            },
            move |vm, this, args| {
                let h = this
                    .as_object()
                    .ok_or_else(|| vm.type_error("Invalid AudioBufferSourceNode"))?;
                let v = vm.to_boolean(arg(args, 0));
                let s = sys_s.borrow();
                if let Some(nr) = s.nodes.get(&h) {
                    let mut g = s.contexts[&nr.ctx]
                        .graph
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some(AudioNodeKind::BufferSource { loop_enabled, .. }) =
                        g.node_mut(nr.id)
                    {
                        *loop_enabled = v;
                    }
                }
                Ok(JsValue::UNDEFINED)
            },
        );
    }
    {
        let sys = Rc::clone(wa);
        define_host_method(vm, bs_proto, "start", move |vm, this, args| {
            let h = this
                .as_object()
                .ok_or_else(|| vm.type_error("Invalid AudioBufferSourceNode"))?;
            let when = if args.is_empty() || arg(args, 0).is_undefined() {
                0.0
            } else {
                vm.to_number(arg(args, 0))?.max(0.0)
            };
            let offset = if args.len() > 1 && !arg(args, 1).is_undefined() {
                vm.to_number(arg(args, 1))?.max(0.0)
            } else {
                0.0
            };
            let dur = if args.len() > 2 && !arg(args, 2).is_undefined() {
                Some(vm.to_number(arg(args, 2))?.max(0.0))
            } else {
                None
            };
            let s = sys.borrow();
            if let Some(nr) = s.nodes.get(&h) {
                let mut g = s.contexts[&nr.ctx]
                    .graph
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(AudioNodeKind::BufferSource {
                    start_time,
                    offset_s,
                    duration_s,
                    ..
                }) = g.node_mut(nr.id)
                {
                    *start_time = Some(when);
                    *offset_s = offset;
                    *duration_s = dur;
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let sys = Rc::clone(wa);
        define_host_method(vm, bs_proto, "stop", move |vm, this, args| {
            let h = this
                .as_object()
                .ok_or_else(|| vm.type_error("Invalid AudioBufferSourceNode"))?;
            let when = if args.is_empty() || arg(args, 0).is_undefined() {
                0.0
            } else {
                vm.to_number(arg(args, 0))?.max(0.0)
            };
            let s = sys.borrow();
            if let Some(nr) = s.nodes.get(&h) {
                let mut g = s.contexts[&nr.ctx]
                    .graph
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Some(AudioNodeKind::BufferSource { stop_time, .. }) = g.node_mut(nr.id) {
                    *stop_time = Some(when);
                }
            }
            Ok(JsValue::UNDEFINED)
        });
    }

    // `new AudioWorkletNode(ctx, name)` constructor
    {
        let sys = Rc::clone(wa);
        let worklet_ctor = vm.alloc_host_fn("AudioWorkletNode", move |vm, _, args| {
            let ctx_h = arg(args, 0)
                .as_object()
                .ok_or_else(|| vm.type_error("AudioWorkletNode requires a BaseAudioContext"))?;
            let name = vm.value_to_rust_string(arg(args, 1));
            let node_id = {
                let s = sys.borrow();
                let ctx = s
                    .contexts
                    .get(&ctx_h)
                    .ok_or_else(|| vm.type_error("Invalid BaseAudioContext"))?;
                let id = ctx
                    .graph
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .create_worklet(
                        name,
                        Box::new(|inputs, outputs, _, _| {
                            for (i, o) in inputs.iter().zip(outputs.iter_mut()) {
                                o.copy_from_slice(i);
                            }
                        }),
                    );
                id
            };
            let proto = sys.borrow().protos.worklet_proto.unwrap_or(vm.object_proto);
            let h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
            sys.borrow_mut().nodes.insert(
                h,
                AudioNodeRef {
                    ctx: ctx_h,
                    id: node_id,
                },
            );
            Ok(JsValue::from_object(h))
        });
        bind_ctor_proto(vm, worklet_ctor, worklet_proto);
        define_global(vm, "AudioWorkletNode", JsValue::from_object(worklet_ctor));
    }

    for (name, p) in [
        ("AudioNode", node_proto),
        ("GainNode", gain_proto),
        ("BiquadFilterNode", biquad_proto),
        ("AnalyserNode", analyser_proto),
        ("OscillatorNode", osc_proto),
        ("AudioBufferSourceNode", bs_proto),
        ("MediaElementAudioSourceNode", ms_proto),
    ] {
        let ctor = vm.alloc_host_fn(name, move |vm, _, _| {
            Err(vm.type_error(format!("Illegal constructor: {name}")))
        });
        bind_ctor_proto(vm, ctor, p);
        define_global(vm, name, JsValue::from_object(ctor));
    }
}

fn install_audio_context_ctors(
    vm: &mut JsVm,
    et_proto: HeapHandle,
    host: &SharedHost,
    media_elements: &SharedMediaElements,
    wa: &SharedWebAudio,
) {
    let base_proto = make_proto(vm, et_proto);
    let audio_proto = make_proto(vm, base_proto);
    let offline_proto = make_proto(vm, base_proto);
    {
        let mut s = wa.borrow_mut();
        s.protos.base_ctx_proto = Some(base_proto);
        s.protos.audio_ctx_proto = Some(audio_proto);
        s.protos.offline_ctx_proto = Some(offline_proto);
    }

    // `sampleRate`, `currentTime`, `state`, `destination`, `audioWorklet`
    {
        let sys = Rc::clone(wa);
        define_host_getter(vm, base_proto, "sampleRate", move |vm, this, _| {
            let ctx_h = require_ctx(vm, &sys, this)?;
            let sr = sys.borrow().contexts[&ctx_h]
                .graph
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .sample_rate();
            Ok(JsValue::from_number(f64::from(sr)))
        });
    }
    {
        let sys = Rc::clone(wa);
        define_host_getter(vm, base_proto, "currentTime", move |vm, this, _| {
            let ctx_h = require_ctx(vm, &sys, this)?;
            let t = sys.borrow().contexts[&ctx_h]
                .graph
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .current_time();
            Ok(JsValue::from_number(t))
        });
    }
    {
        let sys = Rc::clone(wa);
        define_host_getter(vm, base_proto, "state", move |vm, this, _| {
            let ctx_h = require_ctx(vm, &sys, this)?;
            let st = sys.borrow().contexts[&ctx_h].state;
            Ok(JsValue::from_string(vm.heap.alloc_string(st)))
        });
    }
    {
        let sys = Rc::clone(wa);
        define_host_getter(vm, base_proto, "destination", move |vm, this, _| {
            let ctx_h = require_ctx(vm, &sys, this)?;
            Ok(JsValue::from_object(
                sys.borrow().contexts[&ctx_h].destination_handle,
            ))
        });
    }
    {
        let sys = Rc::clone(wa);
        define_host_getter(vm, base_proto, "audioWorklet", move |vm, this, _| {
            let ctx_h = require_ctx(vm, &sys, this)?;
            Ok(JsValue::from_object(
                sys.borrow().contexts[&ctx_h].worklet_handle,
            ))
        });
    }

    // `createGain()`
    {
        let sys = Rc::clone(wa);
        define_host_method(vm, base_proto, "createGain", move |vm, this, _| {
            let ctx_h = require_ctx(vm, &sys, this)?;
            let node_id = sys.borrow().contexts[&ctx_h]
                .graph
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .create_gain();
            let proto = sys.borrow().protos.gain_proto.unwrap_or(vm.object_proto);
            let h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
            let gain_param =
                sys.borrow_mut()
                    .alloc_param(vm, ctx_h, node_id, ParamTarget::GainValue);
            define_ro_prop(vm, h, "gain", JsValue::from_object(gain_param));
            sys.borrow_mut().nodes.insert(
                h,
                AudioNodeRef {
                    ctx: ctx_h,
                    id: node_id,
                },
            );
            Ok(JsValue::from_object(h))
        });
    }

    // `createBiquadFilter()`
    {
        let sys = Rc::clone(wa);
        define_host_method(vm, base_proto, "createBiquadFilter", move |vm, this, _| {
            let ctx_h = require_ctx(vm, &sys, this)?;
            let node_id = sys.borrow().contexts[&ctx_h]
                .graph
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .create_biquad_filter();
            let proto = sys.borrow().protos.biquad_proto.unwrap_or(vm.object_proto);
            let h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
            let p_freq =
                sys.borrow_mut()
                    .alloc_param(vm, ctx_h, node_id, ParamTarget::BiquadFrequency);
            let p_det = sys
                .borrow_mut()
                .alloc_param(vm, ctx_h, node_id, ParamTarget::BiquadDetune);
            let p_q = sys
                .borrow_mut()
                .alloc_param(vm, ctx_h, node_id, ParamTarget::BiquadQ);
            let p_gain = sys
                .borrow_mut()
                .alloc_param(vm, ctx_h, node_id, ParamTarget::BiquadGain);
            define_ro_prop(vm, h, "frequency", JsValue::from_object(p_freq));
            define_ro_prop(vm, h, "detune", JsValue::from_object(p_det));
            define_ro_prop(vm, h, "Q", JsValue::from_object(p_q));
            define_ro_prop(vm, h, "gain", JsValue::from_object(p_gain));
            sys.borrow_mut().nodes.insert(
                h,
                AudioNodeRef {
                    ctx: ctx_h,
                    id: node_id,
                },
            );
            Ok(JsValue::from_object(h))
        });
    }

    // `createAnalyser()`
    {
        let sys = Rc::clone(wa);
        define_host_method(vm, base_proto, "createAnalyser", move |vm, this, _| {
            let ctx_h = require_ctx(vm, &sys, this)?;
            let node_id = sys.borrow().contexts[&ctx_h]
                .graph
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .create_analyser();
            let proto = sys
                .borrow()
                .protos
                .analyser_proto
                .unwrap_or(vm.object_proto);
            let h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
            sys.borrow_mut().nodes.insert(
                h,
                AudioNodeRef {
                    ctx: ctx_h,
                    id: node_id,
                },
            );
            Ok(JsValue::from_object(h))
        });
    }

    // `createOscillator()`
    {
        let sys = Rc::clone(wa);
        define_host_method(vm, base_proto, "createOscillator", move |vm, this, _| {
            let ctx_h = require_ctx(vm, &sys, this)?;
            let node_id = sys.borrow().contexts[&ctx_h]
                .graph
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .create_oscillator();
            let proto = sys.borrow().protos.osc_proto.unwrap_or(vm.object_proto);
            let h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
            let p_freq =
                sys.borrow_mut()
                    .alloc_param(vm, ctx_h, node_id, ParamTarget::OscFrequency);
            let p_det = sys
                .borrow_mut()
                .alloc_param(vm, ctx_h, node_id, ParamTarget::OscDetune);
            define_ro_prop(vm, h, "frequency", JsValue::from_object(p_freq));
            define_ro_prop(vm, h, "detune", JsValue::from_object(p_det));
            sys.borrow_mut().nodes.insert(
                h,
                AudioNodeRef {
                    ctx: ctx_h,
                    id: node_id,
                },
            );
            Ok(JsValue::from_object(h))
        });
    }

    // `createBuffer(numberOfChannels, length, sampleRate)`
    {
        let sys = Rc::clone(wa);
        define_host_method(vm, base_proto, "createBuffer", move |vm, this, args| {
            let _ = require_ctx(vm, &sys, this)?;
            let channels = vm.to_number(arg(args, 0))?.max(0.0) as usize;
            let length = vm.to_number(arg(args, 1))?.max(0.0) as usize;
            let sr = vm.to_number(arg(args, 2))?.max(0.0) as u32;
            if channels == 0 || length == 0 || sr < 8_000 {
                return Err(create_dom_exception(
                    vm,
                    "NotSupportedError",
                    "Invalid AudioBuffer dimensions or sample rate",
                ));
            }
            let buf = Arc::new(AudioBuffer::new(channels, length, sr));
            let h = sys.borrow_mut().alloc_audio_buffer(vm, buf);
            Ok(JsValue::from_object(h))
        });
    }

    // `createBufferSource()`
    {
        let sys = Rc::clone(wa);
        define_host_method(vm, base_proto, "createBufferSource", move |vm, this, _| {
            let ctx_h = require_ctx(vm, &sys, this)?;
            let node_id = sys.borrow().contexts[&ctx_h]
                .graph
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .create_buffer_source();
            let proto = sys
                .borrow()
                .protos
                .buffer_source_proto
                .unwrap_or(vm.object_proto);
            let h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
            let p_rate =
                sys.borrow_mut()
                    .alloc_param(vm, ctx_h, node_id, ParamTarget::BufferPlaybackRate);
            define_ro_prop(vm, h, "playbackRate", JsValue::from_object(p_rate));
            sys.borrow_mut().nodes.insert(
                h,
                AudioNodeRef {
                    ctx: ctx_h,
                    id: node_id,
                },
            );
            Ok(JsValue::from_object(h))
        });
    }

    // `createMediaElementSource(mediaElement)`
    {
        let sys = Rc::clone(wa);
        let h_cl = Rc::clone(host);
        let me_cl = Rc::clone(media_elements);
        define_host_method(
            vm,
            base_proto,
            "createMediaElementSource",
            move |vm, this, args| {
                let ctx_h = require_ctx(vm, &sys, this)?;
                let Some(nid) = h_cl.borrow().node_for_value(vm, arg(args, 0)) else {
                    return Err(
                        vm.type_error("createMediaElementSource requires an HTMLMediaElement")
                    );
                };
                let queue = me_cl.borrow_mut().audio_queue_for(nid);
                let node_id = sys.borrow().contexts[&ctx_h]
                    .graph
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .create_media_element_source(queue, 2);
                let proto = sys
                    .borrow()
                    .protos
                    .media_source_proto
                    .unwrap_or(vm.object_proto);
                let h = vm
                    .heap
                    .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
                sys.borrow_mut().nodes.insert(
                    h,
                    AudioNodeRef {
                        ctx: ctx_h,
                        id: node_id,
                    },
                );
                Ok(JsValue::from_object(h))
            },
        );
    }

    // `decodeAudioData(ArrayBuffer) -> Promise<AudioBuffer>`
    {
        let sys = Rc::clone(wa);
        define_host_method(vm, base_proto, "decodeAudioData", move |vm, this, args| {
            let _ = require_ctx(vm, &sys, this)?;
            let bytes = extract_gpu_buffer_source(vm, arg(args, 0), None, None)
                .ok_or_else(|| vm.type_error("decodeAudioData requires an ArrayBuffer"))?;
            match decode_container_audio(&bytes, None) {
                Ok(frames) if !frames.is_empty() => {
                    let sr = frames[0].sample_rate;
                    let ch = usize::from(frames[0].channels.max(1));
                    let mut all_samples = Vec::new();
                    for f in frames {
                        all_samples.extend_from_slice(&f.samples);
                    }
                    let buf = Arc::new(AudioBuffer::from_interleaved(&all_samples, ch, sr));
                    let buf_h = sys.borrow_mut().alloc_audio_buffer(vm, buf);
                    Ok(JsValue::from_object(promise_resolve_value(
                        vm,
                        JsValue::from_object(buf_h),
                    )))
                }
                Ok(_) => {
                    let ex = create_dom_exception(vm, "EncodingError", "Decoded zero audio frames");
                    let p = alloc_promise(vm);
                    reject_promise(vm, p, ex.value);
                    Ok(JsValue::from_object(p))
                }
                Err(err) => {
                    let ex = create_dom_exception(vm, "EncodingError", &err.to_string());
                    let p = alloc_promise(vm);
                    reject_promise(vm, p, ex.value);
                    Ok(JsValue::from_object(p))
                }
            }
        });
    }

    // `AudioContext.prototype`: `resume()`, `suspend()`, `close()`
    {
        let sys = Rc::clone(wa);
        define_host_method(vm, audio_proto, "resume", move |vm, this, _| {
            let ctx_h = require_ctx(vm, &sys, this)?;
            if let Some(entry) = sys.borrow_mut().contexts.get_mut(&ctx_h) {
                if entry.state != "closed" {
                    entry.state = "running";
                    if let Some(dev) = &entry.output_device {
                        dev.clock.play();
                    }
                }
            }
            Ok(JsValue::from_object(promise_resolve_value(
                vm,
                JsValue::UNDEFINED,
            )))
        });
    }
    {
        let sys = Rc::clone(wa);
        define_host_method(vm, audio_proto, "suspend", move |vm, this, _| {
            let ctx_h = require_ctx(vm, &sys, this)?;
            if let Some(entry) = sys.borrow_mut().contexts.get_mut(&ctx_h) {
                if entry.state != "closed" {
                    entry.state = "suspended";
                    if let Some(dev) = &entry.output_device {
                        dev.clock.pause();
                    }
                }
            }
            Ok(JsValue::from_object(promise_resolve_value(
                vm,
                JsValue::UNDEFINED,
            )))
        });
    }
    {
        let sys = Rc::clone(wa);
        define_host_method(vm, audio_proto, "close", move |vm, this, _| {
            let ctx_h = require_ctx(vm, &sys, this)?;
            if let Some(entry) = sys.borrow_mut().contexts.get_mut(&ctx_h) {
                entry.state = "closed";
                entry.output_device = None;
            }
            Ok(JsValue::from_object(promise_resolve_value(
                vm,
                JsValue::UNDEFINED,
            )))
        });
    }

    // `OfflineAudioContext.prototype.startRendering() -> Promise<AudioBuffer>`
    {
        let sys = Rc::clone(wa);
        define_host_method(vm, offline_proto, "startRendering", move |vm, this, _| {
            let ctx_h = require_ctx(vm, &sys, this)?;
            let (graph, total_frames) = {
                let mut s = sys.borrow_mut();
                let entry = s.contexts.get_mut(&ctx_h).unwrap();
                entry.state = "running";
                (Arc::clone(&entry.graph), entry.offline_length.unwrap_or(0))
            };
            let rendered = Arc::new(
                graph
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .render_offline(total_frames),
            );
            if let Some(entry) = sys.borrow_mut().contexts.get_mut(&ctx_h) {
                entry.state = "closed";
            }
            let buf_h = sys.borrow_mut().alloc_audio_buffer(vm, rendered);
            Ok(JsValue::from_object(promise_resolve_value(
                vm,
                JsValue::from_object(buf_h),
            )))
        });
    }

    // `new AudioContext(options?)`
    {
        let sys = Rc::clone(wa);
        let ctor = vm.alloc_host_fn("AudioContext", move |vm, _, args| {
            let sr = args
                .first()
                .and_then(|v| v.as_object())
                .and_then(|o| crate::webgpu_types::js_prop_u32(vm, o, "sampleRate"))
                .unwrap_or(48_000)
                .max(8_000);
            let ctx_h = alloc_context_instance(vm, &sys, sr, 2, None);
            Ok(JsValue::from_object(ctx_h))
        });
        bind_ctor_proto(vm, ctor, audio_proto);
        define_global(vm, "AudioContext", JsValue::from_object(ctor));
        define_global(vm, "webkitAudioContext", JsValue::from_object(ctor));
    }

    // `new OfflineAudioContext(numberOfChannels, length, sampleRate)`
    {
        let sys = Rc::clone(wa);
        let ctor = vm.alloc_host_fn("OfflineAudioContext", move |vm, _, args| {
            let channels = vm.to_number(arg(args, 0))?.max(1.0) as usize;
            let length = vm.to_number(arg(args, 1))?.max(1.0) as usize;
            let sr = vm.to_number(arg(args, 2))?.max(8_000.0) as u32;
            let ctx_h = alloc_context_instance(vm, &sys, sr, channels.min(2), Some(length));
            Ok(JsValue::from_object(ctx_h))
        });
        bind_ctor_proto(vm, ctor, offline_proto);
        define_global(vm, "OfflineAudioContext", JsValue::from_object(ctor));
    }

    let base_ctor = vm.alloc_host_fn("BaseAudioContext", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: BaseAudioContext"))
    });
    bind_ctor_proto(vm, base_ctor, base_proto);
    define_global(vm, "BaseAudioContext", JsValue::from_object(base_ctor));
}

fn alloc_context_instance(
    vm: &mut JsVm,
    sys: &SharedWebAudio,
    sample_rate: u32,
    channels: usize,
    offline_length: Option<usize>,
) -> HeapHandle {
    let (ctx_proto, node_proto) = {
        let s = sys.borrow();
        let cp = if offline_length.is_some() {
            s.protos.offline_ctx_proto.unwrap_or(vm.object_proto)
        } else {
            s.protos.audio_ctx_proto.unwrap_or(vm.object_proto)
        };
        (cp, s.protos.audio_node_proto.unwrap_or(vm.object_proto))
    };

    let ctx_h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(ctx_proto)));
    let dest_h = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(node_proto)));
    let worklet_h = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.object_proto),
    ));
    define_host_method(vm, worklet_h, "addModule", |vm, _, _| {
        Ok(JsValue::from_object(promise_resolve_value(
            vm,
            JsValue::UNDEFINED,
        )))
    });

    let graph = Arc::new(Mutex::new(AudioGraph::new(sample_rate, channels)));
    let dest_id = graph
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .destination();

    let output_device = if offline_length.is_none() {
        let clock = Arc::new(MediaClock::new(sample_rate));
        clock.play();
        if std::env::var_os("RIX_ENABLE_HARDWARE_AUDIO").is_some() {
            let g_cb = Arc::clone(&graph);
            Some(AudioOutputDevice::start_or_headless(
                sample_rate,
                channels as u16,
                clock,
                move |out, _ch| {
                    if let Ok(mut g) = g_cb.try_lock() {
                        g.render_interleaved(out);
                    } else {
                        out.fill(0.0);
                    }
                },
            ))
        } else {
            Some(AudioOutputDevice::headless(
                sample_rate,
                channels as u16,
                clock,
            ))
        }
    } else {
        None
    };

    let mut s = sys.borrow_mut();
    s.nodes.insert(
        dest_h,
        AudioNodeRef {
            ctx: ctx_h,
            id: dest_id,
        },
    );
    s.contexts.insert(
        ctx_h,
        AudioContextEntry {
            graph,
            state: if offline_length.is_some() {
                "suspended"
            } else {
                "running"
            },
            destination_handle: dest_h,
            worklet_handle: worklet_h,
            offline_length,
            output_device,
        },
    );
    ctx_h
}

fn require_ctx(
    vm: &mut JsVm,
    wa: &SharedWebAudio,
    this: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = this.as_object() else {
        return Err(vm.type_error("Incompatible BaseAudioContext receiver"));
    };
    if wa.borrow().contexts.contains_key(&h) {
        Ok(h)
    } else {
        Err(vm.type_error("Incompatible BaseAudioContext receiver"))
    }
}

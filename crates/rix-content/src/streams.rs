//! WHATWG Streams Standard (`ReadableStream`, `WritableStream`, `TransformStream`)
//! supporting `getReader`, `getWriter`, `pipeThrough`, `pipeTo`, `tee`, `cancel`,
//! and `Blob.prototype.stream()`.
//!
//! Reference: Spec 06 §3 & Issue #17.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use rix_js::promise::{
    alloc_promise, promise_resolve_value, promise_then, reject_promise, resolve_promise,
};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};

pub(crate) use crate::streams_writable::create_writable_stream_from_callbacks;
use crate::streams_writable::{
    install_writable_stream_types, writable_close_internal, writable_write_internal,
    WritableDefaultWriterState, WritableStreamState,
};
use crate::web_apis::{
    alloc_uint8_array, arg, bind_ctor_proto, define_global, define_host_getter, define_host_method,
    SharedWebApis,
};

#[derive(Debug, Clone)]
struct ReadableStreamState {
    queue: VecDeque<JsValue>,
    closed: bool,
    errored: Option<JsValue>,
    locked: bool,
    disturbed: bool,
    pull_fn: Option<HeapHandle>,
    cancel_fn: Option<HeapHandle>,
    source_obj: Option<HeapHandle>,
    pending_reads: VecDeque<HeapHandle>,
    closed_promises: Vec<HeapHandle>,
}

impl ReadableStreamState {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            closed: false,
            errored: None,
            locked: false,
            disturbed: false,
            pull_fn: None,
            cancel_fn: None,
            source_obj: None,
            pending_reads: VecDeque::new(),
            closed_promises: Vec::new(),
        }
    }
}

/// Shared registry for WHATWG `ReadableStream`, `WritableStream`, and `TransformStream`.
#[derive(Default)]
pub struct StreamSystem {
    readable: HashMap<HeapHandle, ReadableStreamState>,
    pub(crate) writable: HashMap<HeapHandle, WritableStreamState>,
    pub(crate) writable_controllers: HashMap<HeapHandle, HeapHandle>,
    pub(crate) writable_writers: HashMap<HeapHandle, WritableDefaultWriterState>,
    pub readable_proto: Option<HeapHandle>,
    pub writable_proto: Option<HeapHandle>,
    pub writable_writer_proto: Option<HeapHandle>,
    pub writable_controller_proto: Option<HeapHandle>,
    pub transform_proto: Option<HeapHandle>,
}

/// Shared handle to [`StreamSystem`].
pub type SharedStreams = Rc<RefCell<StreamSystem>>;

impl StreamSystem {
    /// Collects all GC roots retained in stream queues, controllers, and pending read promises.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for p in [
            self.readable_proto,
            self.writable_proto,
            self.writable_writer_proto,
            self.writable_controller_proto,
            self.transform_proto,
        ]
        .into_iter()
        .flatten()
        {
            out.push(JsValue::from_object(p));
        }
        for rs in self.readable.values() {
            for v in &rs.queue {
                out.push(*v);
            }
            if let Some(e) = rs.errored {
                out.push(e);
            }
            for h in [rs.pull_fn, rs.cancel_fn, rs.source_obj]
                .into_iter()
                .flatten()
            {
                out.push(JsValue::from_object(h));
            }
            for p in &rs.pending_reads {
                out.push(JsValue::from_object(*p));
            }
            for p in &rs.closed_promises {
                out.push(JsValue::from_object(*p));
            }
        }
        for ws in self.writable.values() {
            ws.collect_roots(out);
        }
        for w in self.writable_writers.values() {
            if let Some(s) = w.stream {
                out.push(JsValue::from_object(s));
            }
            out.push(JsValue::from_object(w.closed_promise));
            out.push(JsValue::from_object(w.ready_promise));
        }
    }

    /// Returns whether the given `ReadableStream` has been read from or canceled (disturbed).
    pub(crate) fn is_readable_disturbed(&self, h: HeapHandle) -> bool {
        self.readable.get(&h).is_some_and(|rs| rs.disturbed)
    }

    /// Sweeps dead native entries for GC-collected stream handles.
    pub fn sweep_dead(&mut self, heap: &rix_js::GcHeap) {
        self.readable.retain(|&h, _| heap.is_alive(h));
        self.writable.retain(|&h, _| heap.is_alive(h));
        self.writable_controllers.retain(|&h, _| heap.is_alive(h));
        self.writable_writers.retain(|&h, _| heap.is_alive(h));
    }
}

/// Creates a `ReadableStream` pre-populated with a single `Uint8Array` chunk of `bytes`
/// and immediately closed (used by `Response.body` and `Blob.prototype.stream()`).
pub fn create_readable_stream_from_bytes(
    vm: &mut JsVm,
    streams: &SharedStreams,
    bytes: Vec<u8>,
) -> HeapHandle {
    let proto = streams.borrow().readable_proto.unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let mut st = ReadableStreamState::new();
    if !bytes.is_empty() {
        let u8a = alloc_uint8_array(vm, bytes);
        st.queue.push_back(JsValue::from_object(u8a));
    }
    st.closed = true;
    streams.borrow_mut().readable.insert(obj, st);
    obj
}

fn make_read_result(vm: &mut JsVm, value: JsValue, done: bool) -> JsValue {
    let obj = vm.alloc_ordinary_object();
    let k_val = vm.heap.intern_key("value");
    let k_done = vm.heap.intern_key("done");
    vm.heap
        .object_define_data(obj, k_val, value, PropertyAttrs::DEFAULT, &mut vm.shapes);
    vm.heap.object_define_data(
        obj,
        k_done,
        JsValue::from_bool(done),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
    JsValue::from_object(obj)
}

fn readable_enqueue(vm: &mut JsVm, streams: &SharedStreams, stream_h: HeapHandle, chunk: JsValue) {
    let pending_p = {
        let mut st = streams.borrow_mut();
        let Some(rs) = st.readable.get_mut(&stream_h) else {
            return;
        };
        if rs.closed || rs.errored.is_some() {
            return;
        }
        if let Some(p) = rs.pending_reads.pop_front() {
            Some(p)
        } else {
            rs.queue.push_back(chunk);
            None
        }
    };
    if let Some(p) = pending_p {
        let res = make_read_result(vm, chunk, false);
        resolve_promise(vm, p, res);
    }
}

fn readable_close(vm: &mut JsVm, streams: &SharedStreams, stream_h: HeapHandle) {
    let (pending, closed_ps) = {
        let mut st = streams.borrow_mut();
        let Some(rs) = st.readable.get_mut(&stream_h) else {
            return;
        };
        rs.closed = true;
        (
            std::mem::take(&mut rs.pending_reads),
            std::mem::take(&mut rs.closed_promises),
        )
    };
    for p in pending {
        let res = make_read_result(vm, JsValue::UNDEFINED, true);
        resolve_promise(vm, p, res);
    }
    for p in closed_ps {
        resolve_promise(vm, p, JsValue::UNDEFINED);
    }
}

fn readable_error(vm: &mut JsVm, streams: &SharedStreams, stream_h: HeapHandle, reason: JsValue) {
    let (pending, closed_ps) = {
        let mut st = streams.borrow_mut();
        let Some(rs) = st.readable.get_mut(&stream_h) else {
            return;
        };
        rs.errored = Some(reason);
        rs.queue.clear();
        (
            std::mem::take(&mut rs.pending_reads),
            std::mem::take(&mut rs.closed_promises),
        )
    };
    for p in pending {
        reject_promise(vm, p, reason);
    }
    for p in closed_ps {
        reject_promise(vm, p, reason);
    }
}

fn create_readable_controller(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
) -> HeapHandle {
    let ctrl = vm.alloc_ordinary_object();
    let ss = Rc::clone(streams);
    define_host_method(vm, ctrl, "enqueue", move |vm, _, args| {
        readable_enqueue(vm, &ss, stream_h, arg(args, 0));
        Ok(JsValue::UNDEFINED)
    });
    let ss = Rc::clone(streams);
    define_host_method(vm, ctrl, "close", move |vm, _, _| {
        readable_close(vm, &ss, stream_h);
        Ok(JsValue::UNDEFINED)
    });
    let ss = Rc::clone(streams);
    define_host_method(vm, ctrl, "error", move |vm, _, args| {
        readable_error(vm, &ss, stream_h, arg(args, 0));
        Ok(JsValue::UNDEFINED)
    });
    ctrl
}

fn readable_read_next(vm: &mut JsVm, streams: &SharedStreams, stream_h: HeapHandle) -> HeapHandle {
    let immediate = {
        let mut st = streams.borrow_mut();
        let Some(rs) = st.readable.get_mut(&stream_h) else {
            let res = make_read_result(vm, JsValue::UNDEFINED, true);
            return promise_resolve_value(vm, res);
        };
        rs.disturbed = true;
        if let Some(chunk) = rs.queue.pop_front() {
            Some(Ok(Some(chunk)))
        } else if let Some(err) = rs.errored {
            Some(Err(err))
        } else if rs.closed {
            Some(Ok(None))
        } else {
            None
        }
    };
    match immediate {
        Some(Ok(Some(chunk))) => {
            let res = make_read_result(vm, chunk, false);
            return promise_resolve_value(vm, res);
        }
        Some(Ok(None)) => {
            let res = make_read_result(vm, JsValue::UNDEFINED, true);
            return promise_resolve_value(vm, res);
        }
        Some(Err(e)) => {
            let p = alloc_promise(vm);
            reject_promise(vm, p, e);
            return p;
        }
        None => {}
    }

    let pull_info = streams
        .borrow()
        .readable
        .get(&stream_h)
        .and_then(|rs| rs.pull_fn.map(|f| (f, rs.source_obj)));
    if let Some((pull_fn, src_obj)) = pull_info {
        let ctrl = create_readable_controller(vm, streams, stream_h);
        let this_v = src_obj.map_or(JsValue::UNDEFINED, JsValue::from_object);
        if let Err(e) = vm.call_value(
            JsValue::from_object(pull_fn),
            this_v,
            &[JsValue::from_object(ctrl)],
        ) {
            readable_error(vm, streams, stream_h, e.value);
        }
        let post = {
            let mut st = streams.borrow_mut();
            let Some(rs) = st.readable.get_mut(&stream_h) else {
                let res = make_read_result(vm, JsValue::UNDEFINED, true);
                return promise_resolve_value(vm, res);
            };
            if let Some(chunk) = rs.queue.pop_front() {
                Some(Ok(Some(chunk)))
            } else if let Some(err) = rs.errored {
                Some(Err(err))
            } else if rs.closed {
                Some(Ok(None))
            } else {
                None
            }
        };
        match post {
            Some(Ok(Some(chunk))) => {
                let res = make_read_result(vm, chunk, false);
                return promise_resolve_value(vm, res);
            }
            Some(Ok(None)) => {
                let res = make_read_result(vm, JsValue::UNDEFINED, true);
                return promise_resolve_value(vm, res);
            }
            Some(Err(e)) => {
                let p = alloc_promise(vm);
                reject_promise(vm, p, e);
                return p;
            }
            None => {}
        }
    }

    let p = alloc_promise(vm);
    if let Some(rs) = streams.borrow_mut().readable.get_mut(&stream_h) {
        rs.pending_reads.push_back(p);
    }
    p
}

/// Installs `ReadableStream`, `WritableStream`, `TransformStream`, and `Blob.prototype.stream()`.
pub fn install_streams(vm: &mut JsVm, web_apis: &SharedWebApis) -> SharedStreams {
    let streams = Rc::new(RefCell::new(StreamSystem::default()));

    // --- ReadableStream ---
    let rsp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(rsp).unwrap().is_prototype = true;
    streams.borrow_mut().readable_proto = Some(rsp);

    let ss = Rc::clone(&streams);
    define_host_getter(vm, rsp, "locked", move |vm, this_val, _| {
        let h = require_readable(vm, &ss, this_val)?;
        let locked = ss.borrow().readable.get(&h).is_some_and(|rs| rs.locked);
        Ok(JsValue::from_bool(locked))
    });

    let ss = Rc::clone(&streams);
    define_host_method(vm, rsp, "getReader", move |vm, this_val, _| {
        let h = require_readable(vm, &ss, this_val)?;
        {
            let mut st = ss.borrow_mut();
            let rs = st
                .readable
                .get_mut(&h)
                .ok_or_else(|| vm.type_error("Invalid ReadableStream"))?;
            if rs.locked {
                return Err(vm.type_error("ReadableStream is already locked"));
            }
            rs.locked = true;
        }
        let reader = vm.alloc_ordinary_object();
        let closed_p = alloc_promise(vm);
        {
            let mut st = ss.borrow_mut();
            if let Some(rs) = st.readable.get_mut(&h) {
                if let Some(err) = rs.errored {
                    reject_promise(vm, closed_p, err);
                } else if rs.closed {
                    resolve_promise(vm, closed_p, JsValue::UNDEFINED);
                } else {
                    rs.closed_promises.push(closed_p);
                }
            }
        }
        let k_closed = vm.heap.intern_key("closed");
        vm.heap.object_define_data(
            reader,
            k_closed,
            JsValue::from_object(closed_p),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );

        let ss_r = Rc::clone(&ss);
        define_host_method(vm, reader, "read", move |vm, _, _| {
            Ok(JsValue::from_object(readable_read_next(vm, &ss_r, h)))
        });
        let ss_rel = Rc::clone(&ss);
        define_host_method(vm, reader, "releaseLock", move |_, _, _| {
            if let Some(rs) = ss_rel.borrow_mut().readable.get_mut(&h) {
                rs.locked = false;
            }
            Ok(JsValue::UNDEFINED)
        });
        let ss_c = Rc::clone(&ss);
        define_host_method(vm, reader, "cancel", move |vm, _, args| {
            if let Some(rs) = ss_c.borrow_mut().readable.get_mut(&h) {
                rs.disturbed = true;
            }
            readable_close(vm, &ss_c, h);
            let cancel_info = ss_c
                .borrow()
                .readable
                .get(&h)
                .and_then(|rs| rs.cancel_fn.map(|f| (f, rs.source_obj)));
            if let Some((cf, src)) = cancel_info {
                let this_v = src.map_or(JsValue::UNDEFINED, JsValue::from_object);
                let _ = vm.call_value(JsValue::from_object(cf), this_v, &[arg(args, 0)]);
            }
            Ok(JsValue::from_object(promise_resolve_value(
                vm,
                JsValue::UNDEFINED,
            )))
        });
        Ok(JsValue::from_object(reader))
    });

    let ss = Rc::clone(&streams);
    define_host_method(vm, rsp, "cancel", move |vm, this_val, args| {
        let h = require_readable(vm, &ss, this_val)?;
        if let Some(rs) = ss.borrow_mut().readable.get_mut(&h) {
            rs.disturbed = true;
        }
        readable_close(vm, &ss, h);
        let cancel_info = ss
            .borrow()
            .readable
            .get(&h)
            .and_then(|rs| rs.cancel_fn.map(|f| (f, rs.source_obj)));
        if let Some((cf, src)) = cancel_info {
            let this_v = src.map_or(JsValue::UNDEFINED, JsValue::from_object);
            let _ = vm.call_value(JsValue::from_object(cf), this_v, &[arg(args, 0)]);
        }
        Ok(JsValue::from_object(promise_resolve_value(
            vm,
            JsValue::UNDEFINED,
        )))
    });

    let ss = Rc::clone(&streams);
    define_host_method(vm, rsp, "pipeTo", move |vm, this_val, args| {
        let src_h = require_readable(vm, &ss, this_val)?;
        let dst_h = require_writable(vm, &ss, arg(args, 0))?;
        Ok(JsValue::from_object(pipe_readable_to_writable(
            vm, &ss, src_h, dst_h,
        )))
    });

    let ss = Rc::clone(&streams);
    define_host_method(vm, rsp, "pipeThrough", move |vm, this_val, args| {
        let src_h = require_readable(vm, &ss, this_val)?;
        let Some(tr_h) = arg(args, 0).as_object() else {
            return Err(vm.type_error("pipeThrough requires a { readable, writable } object"));
        };
        let kw = vm.heap.intern_key("writable");
        let kr = vm.heap.intern_key("readable");
        let writable = vm.get_property_by_key(tr_h, kw)?;
        let readable = vm.get_property_by_key(tr_h, kr)?;
        let dst_h = require_writable(vm, &ss, writable)?;
        let _ = pipe_readable_to_writable(vm, &ss, src_h, dst_h);
        Ok(readable)
    });

    let ss = Rc::clone(&streams);
    define_host_method(vm, rsp, "tee", move |vm, this_val, _| {
        let src_h = require_readable(vm, &ss, this_val)?;
        let mut chunks = Vec::new();
        loop {
            let item = {
                let mut st = ss.borrow_mut();
                let rs = st
                    .readable
                    .get_mut(&src_h)
                    .ok_or_else(|| vm.type_error("Invalid ReadableStream"))?;
                if let Some(c) = rs.queue.pop_front() {
                    Some(c)
                } else {
                    rs.closed = true;
                    None
                }
            };
            if let Some(c) = item {
                chunks.push(c);
            } else {
                break;
            }
        }
        let make_branch = |vm: &mut JsVm, items: &[JsValue]| -> HeapHandle {
            let b = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(rsp)));
            let mut st = ReadableStreamState::new();
            for &v in items {
                st.queue.push_back(v);
            }
            st.closed = true;
            ss.borrow_mut().readable.insert(b, st);
            b
        };
        let b1 = make_branch(vm, &chunks);
        let b2 = make_branch(vm, &chunks);
        let arr = vm.alloc_array();
        vm.heap.object_push_element(arr, JsValue::from_object(b1));
        vm.heap.object_push_element(arr, JsValue::from_object(b2));
        Ok(JsValue::from_object(arr))
    });

    let ss = Rc::clone(&streams);
    let rs_ctor = vm.alloc_host_fn("ReadableStream", move |vm, _, args| {
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(rsp)));
        let mut st = ReadableStreamState::new();
        let src_val = arg(args, 0);
        let mut start_fn = None;
        if let Some(sh) = src_val.as_object() {
            st.source_obj = Some(sh);
            let k_start = vm.heap.intern_key("start");
            let k_pull = vm.heap.intern_key("pull");
            let k_cancel = vm.heap.intern_key("cancel");
            if let Ok(v) = vm.get_property_by_key(sh, k_start) {
                start_fn = v.as_object().filter(|&fh| vm.is_callable_handle(fh));
            }
            if let Ok(v) = vm.get_property_by_key(sh, k_pull) {
                st.pull_fn = v.as_object().filter(|&fh| vm.is_callable_handle(fh));
            }
            if let Ok(v) = vm.get_property_by_key(sh, k_cancel) {
                st.cancel_fn = v.as_object().filter(|&fh| vm.is_callable_handle(fh));
            }
        }
        ss.borrow_mut().readable.insert(obj, st);
        if let Some(sf) = start_fn {
            let ctrl = create_readable_controller(vm, &ss, obj);
            vm.call_value(
                JsValue::from_object(sf),
                src_val,
                &[JsValue::from_object(ctrl)],
            )?;
        }
        Ok(JsValue::from_object(obj))
    });
    bind_ctor_proto(vm, rs_ctor, rsp);
    define_global(vm, "ReadableStream", JsValue::from_object(rs_ctor));

    // --- WritableStream + WritableStreamDefaultWriter + WritableStreamDefaultController ---
    let _wsp = install_writable_stream_types(vm, &streams, web_apis);

    // --- TransformStream ---
    let tsp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(tsp).unwrap().is_prototype = true;
    streams.borrow_mut().transform_proto = Some(tsp);

    let ss = Rc::clone(&streams);
    let ts_ctor = vm.alloc_host_fn("TransformStream", move |vm, _, args| {
        let transformer_val = arg(args, 0);
        let readable = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(rsp)));
        ss.borrow_mut()
            .readable
            .insert(readable, ReadableStreamState::new());

        let mut start_fn = None;
        let mut transform_fn = None;
        let mut flush_fn = None;
        if let Some(th) = transformer_val.as_object() {
            let ks = vm.heap.intern_key("start");
            let kt = vm.heap.intern_key("transform");
            let kf = vm.heap.intern_key("flush");
            if let Ok(v) = vm.get_property_by_key(th, ks) {
                start_fn = v.as_object().filter(|&fh| vm.is_callable_handle(fh));
            }
            if let Ok(v) = vm.get_property_by_key(th, kt) {
                transform_fn = v.as_object().filter(|&fh| vm.is_callable_handle(fh));
            }
            if let Ok(v) = vm.get_property_by_key(th, kf) {
                flush_fn = v.as_object().filter(|&fh| vm.is_callable_handle(fh));
            }
        }
        let ctrl = create_readable_controller(vm, &ss, readable);
        if let Some(sf) = start_fn {
            vm.call_value(
                JsValue::from_object(sf),
                transformer_val,
                &[JsValue::from_object(ctrl)],
            )?;
        }

        let ss_w = Rc::clone(&ss);
        let write_cb = vm.alloc_host_fn("transform_write", move |vm, _, wargs| {
            let chunk = arg(wargs, 0);
            if let Some(tf) = transform_fn {
                vm.call_value(
                    JsValue::from_object(tf),
                    transformer_val,
                    &[chunk, JsValue::from_object(ctrl)],
                )?;
            } else {
                readable_enqueue(vm, &ss_w, readable, chunk);
            }
            Ok(JsValue::UNDEFINED)
        });
        let ss_cl = Rc::clone(&ss);
        let close_cb = vm.alloc_host_fn("transform_close", move |vm, _, _| {
            if let Some(ff) = flush_fn {
                vm.call_value(
                    JsValue::from_object(ff),
                    transformer_val,
                    &[JsValue::from_object(ctrl)],
                )?;
            }
            readable_close(vm, &ss_cl, readable);
            Ok(JsValue::UNDEFINED)
        });

        let ss_ab = Rc::clone(&ss);
        let abort_cb = vm.alloc_host_fn("transform_abort", move |vm, _, a| {
            readable_error(vm, &ss_ab, readable, arg(a, 0));
            Ok(JsValue::UNDEFINED)
        });
        let writable = create_writable_stream_from_callbacks(vm, &ss, write_cb, close_cb, abort_cb);

        let ts_obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(tsp)));
        let kr = vm.heap.intern_key("readable");
        let kw = vm.heap.intern_key("writable");
        vm.heap.object_define_data(
            ts_obj,
            kr,
            JsValue::from_object(readable),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        vm.heap.object_define_data(
            ts_obj,
            kw,
            JsValue::from_object(writable),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        Ok(JsValue::from_object(ts_obj))
    });
    bind_ctor_proto(vm, ts_ctor, tsp);
    define_global(vm, "TransformStream", JsValue::from_object(ts_ctor));

    // --- TextDecoderStream & TextEncoderStream (WHATWG Encoding §8.5) ---
    install_text_encoding_streams(vm, &streams, web_apis, rsp);

    // Attach `Blob.prototype.stream()` cleanly once streams are installed.
    if let Some(bp) = web_apis.borrow().protos.blob_proto {
        let wa = Rc::clone(web_apis);
        let ss = Rc::clone(&streams);
        define_host_method(vm, bp, "stream", move |vm, this_val, _| {
            let Some(h) = this_val.as_object() else {
                return Err(vm.type_error("Incompatible Blob receiver"));
            };
            let bytes = wa
                .borrow()
                .blobs
                .get(&h)
                .map(|b| b.bytes.clone())
                .ok_or_else(|| vm.type_error("Incompatible Blob receiver"))?;
            Ok(JsValue::from_object(create_readable_stream_from_bytes(
                vm, &ss, bytes,
            )))
        });
    }

    streams
}

fn require_readable(
    vm: &mut JsVm,
    streams: &SharedStreams,
    val: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible ReadableStream receiver"));
    };
    if streams.borrow().readable.contains_key(&h) {
        Ok(h)
    } else {
        Err(vm.type_error("Incompatible ReadableStream receiver"))
    }
}

pub(crate) fn require_writable(
    vm: &mut JsVm,
    streams: &SharedStreams,
    val: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible WritableStream receiver"));
    };
    if streams.borrow().writable.contains_key(&h) {
        Ok(h)
    } else {
        Err(vm.type_error("Incompatible WritableStream receiver"))
    }
}

fn pipe_readable_to_writable(
    vm: &mut JsVm,
    streams: &SharedStreams,
    src_h: HeapHandle,
    dst_h: HeapHandle,
) -> HeapHandle {
    let done_p = alloc_promise(vm);
    pump_pipe_step(vm, streams, src_h, dst_h, done_p);
    done_p
}

fn pump_pipe_step(
    vm: &mut JsVm,
    streams: &SharedStreams,
    src_h: HeapHandle,
    dst_h: HeapHandle,
    done_p: HeapHandle,
) {
    let read_p = readable_read_next(vm, streams, src_h);
    let ss_ok = Rc::clone(streams);
    let on_read = vm.alloc_host_fn("pipe_step", move |vm, _, args| {
        let res = arg(args, 0);
        let Some(rh) = res.as_object() else {
            resolve_promise(vm, done_p, JsValue::UNDEFINED);
            return Ok(JsValue::UNDEFINED);
        };
        let k_done = vm.heap.intern_key("done");
        let k_val = vm.heap.intern_key("value");
        let is_done = vm
            .get_property_by_key(rh, k_done)
            .is_ok_and(|v| vm.to_boolean(v));
        if is_done {
            let close_p = writable_close_internal(vm, &ss_ok, dst_h);
            let on_cl = vm.alloc_host_fn("pipe_close_ok", move |vm, _, _| {
                resolve_promise(vm, done_p, JsValue::UNDEFINED);
                Ok(JsValue::UNDEFINED)
            });
            let _ = promise_then(vm, close_p, Some(on_cl), None);
        } else {
            let val = vm
                .get_property_by_key(rh, k_val)
                .unwrap_or(JsValue::UNDEFINED);
            let wp = writable_write_internal(vm, &ss_ok, dst_h, val);
            let ss_next = Rc::clone(&ss_ok);
            let on_wr = vm.alloc_host_fn("pipe_write_ok", move |vm, _, _| {
                pump_pipe_step(vm, &ss_next, src_h, dst_h, done_p);
                Ok(JsValue::UNDEFINED)
            });
            let on_werr = vm.alloc_host_fn("pipe_write_err", move |vm, _, args| {
                reject_promise(vm, done_p, arg(args, 0));
                Ok(JsValue::UNDEFINED)
            });
            let _ = promise_then(vm, wp, Some(on_wr), Some(on_werr));
        }
        Ok(JsValue::UNDEFINED)
    });
    let on_err = vm.alloc_host_fn("pipe_read_err", move |vm, _, args| {
        reject_promise(vm, done_p, arg(args, 0));
        Ok(JsValue::UNDEFINED)
    });
    let _ = promise_then(vm, read_p, Some(on_read), Some(on_err));
}

fn install_text_encoding_streams(
    vm: &mut JsVm,
    streams: &SharedStreams,
    web_apis: &SharedWebApis,
    rsp: HeapHandle,
) {
    // TextDecoderStream
    let tdsp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(tdsp).unwrap().is_prototype = true;
    let ss_d = Rc::clone(streams);
    let wa_d = Rc::clone(web_apis);
    let tds_ctor = vm.alloc_host_fn("TextDecoderStream", move |vm, _, args| {
        let label = if args.is_empty() || arg(args, 0).is_undefined() {
            "utf-8".to_string()
        } else {
            vm.value_to_rust_string(arg(args, 0))
        };
        let enc = encoding_rs::Encoding::for_label(label.trim().as_bytes())
            .ok_or_else(|| vm.range_error(format!("Unsupported encoding label: {label}")))?;
        let mut fatal = false;
        let mut ignore_bom = false;
        if let Some(oh) = arg(args, 1).as_object() {
            let kf = vm.heap.intern_key("fatal");
            if let Ok(v) = vm.get_property_by_key(oh, kf) {
                fatal = vm.to_boolean(v);
            }
            let kb = vm.heap.intern_key("ignoreBOM");
            if let Ok(v) = vm.get_property_by_key(oh, kb) {
                ignore_bom = vm.to_boolean(v);
            }
        }
        let decoder = Rc::new(RefCell::new(if ignore_bom {
            enc.new_decoder_without_bom_handling()
        } else {
            enc.new_decoder_with_bom_removal()
        }));
        let readable = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(rsp)));
        ss_d.borrow_mut()
            .readable
            .insert(readable, ReadableStreamState::new());
        let ss_w = Rc::clone(&ss_d);
        let wa_w = Rc::clone(&wa_d);
        let dec_w = Rc::clone(&decoder);
        let write_fn = vm.alloc_host_fn("tds_write", move |vm, _, args| {
            let chunk = arg(args, 0);
            let bytes = crate::web_apis::extract_bytes_from_js(vm, &wa_w, chunk)?;
            let mut dec = dec_w.borrow_mut();
            let mut out = String::with_capacity(
                dec.max_utf8_buffer_length(bytes.len())
                    .unwrap_or(bytes.len() * 4),
            );
            let (_res, _read, had_err) = dec.decode_to_string(&bytes, &mut out, false);
            if fatal && had_err {
                return Err(vm.type_error("The encoded data was not valid for encoding"));
            }
            if !out.is_empty() {
                let s = JsValue::from_string(vm.heap.alloc_string(out));
                readable_enqueue(vm, &ss_w, readable, s);
            }
            Ok(JsValue::UNDEFINED)
        });

        let ss_c = Rc::clone(&ss_d);
        let dec_c = Rc::clone(&decoder);
        let close_fn = vm.alloc_host_fn("tds_close", move |vm, _, _| {
            let mut dec = dec_c.borrow_mut();
            let mut out = String::with_capacity(16);
            let (_res, _read, had_err) = dec.decode_to_string(&[], &mut out, true);
            if fatal && had_err {
                return Err(vm.type_error("The encoded data was not valid for encoding"));
            }
            if !out.is_empty() {
                let s = JsValue::from_string(vm.heap.alloc_string(out));
                readable_enqueue(vm, &ss_c, readable, s);
            }
            readable_close(vm, &ss_c, readable);
            Ok(JsValue::UNDEFINED)
        });
        let ss_ab = Rc::clone(&ss_d);
        let abort_fn = vm.alloc_host_fn("tds_abort", move |vm, _, a| {
            readable_error(vm, &ss_ab, readable, arg(a, 0));
            Ok(JsValue::UNDEFINED)
        });
        let writable =
            create_writable_stream_from_callbacks(vm, &ss_d, write_fn, close_fn, abort_fn);
        let inst = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(tdsp)));
        let kr = vm.heap.intern_key("readable");
        let kw = vm.heap.intern_key("writable");
        let ke = vm.heap.intern_key("encoding");
        let kf = vm.heap.intern_key("fatal");
        let kb = vm.heap.intern_key("ignoreBOM");
        let enc_name = JsValue::from_string(vm.heap.alloc_string(enc.name().to_ascii_lowercase()));
        for (k, v) in [
            (kr, JsValue::from_object(readable)),
            (kw, JsValue::from_object(writable)),
            (ke, enc_name),
            (kf, JsValue::from_bool(fatal)),
            (kb, JsValue::from_bool(ignore_bom)),
        ] {
            vm.heap
                .object_define_data(inst, k, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
        }
        Ok(JsValue::from_object(inst))
    });
    bind_ctor_proto(vm, tds_ctor, tdsp);
    define_global(vm, "TextDecoderStream", JsValue::from_object(tds_ctor));

    // TextEncoderStream
    let tesp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(tesp).unwrap().is_prototype = true;
    let ss_e = Rc::clone(streams);
    let tes_ctor = vm.alloc_host_fn("TextEncoderStream", move |vm, _, _| {
        let readable = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(rsp)));
        ss_e.borrow_mut()
            .readable
            .insert(readable, ReadableStreamState::new());
        let ss_w = Rc::clone(&ss_e);
        let write_fn = vm.alloc_host_fn("tes_write", move |vm, _, args| {
            let text = vm.to_js_string(arg(args, 0))?;
            let bytes = text.into_bytes();
            if !bytes.is_empty() {
                let u8arr = alloc_uint8_array(vm, bytes);
                readable_enqueue(vm, &ss_w, readable, JsValue::from_object(u8arr));
            }
            Ok(JsValue::UNDEFINED)
        });
        let ss_c = Rc::clone(&ss_e);
        let close_fn = vm.alloc_host_fn("tes_close", move |vm, _, _| {
            readable_close(vm, &ss_c, readable);
            Ok(JsValue::UNDEFINED)
        });
        let ss_ab = Rc::clone(&ss_e);
        let abort_fn = vm.alloc_host_fn("tes_abort", move |vm, _, a| {
            readable_error(vm, &ss_ab, readable, arg(a, 0));
            Ok(JsValue::UNDEFINED)
        });
        let writable =
            create_writable_stream_from_callbacks(vm, &ss_e, write_fn, close_fn, abort_fn);
        let inst = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(tesp)));
        let kr = vm.heap.intern_key("readable");
        let kw = vm.heap.intern_key("writable");
        let ke = vm.heap.intern_key("encoding");
        let enc_name = JsValue::from_string(vm.heap.alloc_string("utf-8"));
        for (k, v) in [
            (kr, JsValue::from_object(readable)),
            (kw, JsValue::from_object(writable)),
            (ke, enc_name),
        ] {
            vm.heap
                .object_define_data(inst, k, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
        }
        Ok(JsValue::from_object(inst))
    });
    bind_ctor_proto(vm, tes_ctor, tesp);
    define_global(vm, "TextEncoderStream", JsValue::from_object(tes_ctor));
}

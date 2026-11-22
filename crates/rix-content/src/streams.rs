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
            pull_fn: None,
            cancel_fn: None,
            source_obj: None,
            pending_reads: VecDeque::new(),
            closed_promises: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct WritableStreamState {
    closed: bool,
    errored: Option<JsValue>,
    locked: bool,
    write_fn: Option<HeapHandle>,
    close_fn: Option<HeapHandle>,
    abort_fn: Option<HeapHandle>,
    sink_obj: Option<HeapHandle>,
}

/// Shared registry for WHATWG `ReadableStream`, `WritableStream`, and `TransformStream`.
#[derive(Default)]
pub struct StreamSystem {
    readable: HashMap<HeapHandle, ReadableStreamState>,
    writable: HashMap<HeapHandle, WritableStreamState>,
    pub readable_proto: Option<HeapHandle>,
    pub writable_proto: Option<HeapHandle>,
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
            if let Some(e) = ws.errored {
                out.push(e);
            }
            for h in [ws.write_fn, ws.close_fn, ws.abort_fn, ws.sink_obj]
                .into_iter()
                .flatten()
            {
                out.push(JsValue::from_object(h));
            }
        }
    }

    /// Sweeps dead native entries for GC-collected stream handles.
    pub fn sweep_dead(&mut self, heap: &rix_js::GcHeap) {
        self.readable.retain(|&h, _| heap.is_alive(h));
        self.writable.retain(|&h, _| heap.is_alive(h));
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

    // --- WritableStream ---
    let wsp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(wsp).unwrap().is_prototype = true;
    streams.borrow_mut().writable_proto = Some(wsp);

    let ss = Rc::clone(&streams);
    define_host_getter(vm, wsp, "locked", move |vm, this_val, _| {
        let h = require_writable(vm, &ss, this_val)?;
        let locked = ss.borrow().writable.get(&h).is_some_and(|ws| ws.locked);
        Ok(JsValue::from_bool(locked))
    });

    let ss = Rc::clone(&streams);
    define_host_method(vm, wsp, "getWriter", move |vm, this_val, _| {
        let h = require_writable(vm, &ss, this_val)?;
        {
            let mut st = ss.borrow_mut();
            let ws = st
                .writable
                .get_mut(&h)
                .ok_or_else(|| vm.type_error("Invalid WritableStream"))?;
            if ws.locked {
                return Err(vm.type_error("WritableStream is already locked"));
            }
            ws.locked = true;
        }
        let writer = vm.alloc_ordinary_object();
        let ready_p = promise_resolve_value(vm, JsValue::UNDEFINED);
        let closed_p = alloc_promise(vm);
        let k_ready = vm.heap.intern_key("ready");
        let k_closed = vm.heap.intern_key("closed");
        vm.heap.object_define_data(
            writer,
            k_ready,
            JsValue::from_object(ready_p),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        vm.heap.object_define_data(
            writer,
            k_closed,
            JsValue::from_object(closed_p),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        let ss_w = Rc::clone(&ss);
        define_host_method(vm, writer, "write", move |vm, _, args| {
            Ok(JsValue::from_object(writable_write_chunk(
                vm,
                &ss_w,
                h,
                arg(args, 0),
            )))
        });
        let ss_cl = Rc::clone(&ss);
        define_host_method(vm, writer, "close", move |vm, _, _| {
            let p = writable_close(vm, &ss_cl, h);
            resolve_promise(vm, closed_p, JsValue::UNDEFINED);
            Ok(JsValue::from_object(p))
        });
        let ss_ab = Rc::clone(&ss);
        define_host_method(vm, writer, "abort", move |vm, _, args| {
            let reason = arg(args, 0);
            let info = ss_ab
                .borrow()
                .writable
                .get(&h)
                .and_then(|ws| ws.abort_fn.map(|f| (f, ws.sink_obj)));
            if let Some(ws) = ss_ab.borrow_mut().writable.get_mut(&h) {
                ws.errored = Some(reason);
                ws.closed = true;
            }
            if let Some((af, sink)) = info {
                let this_v = sink.map_or(JsValue::UNDEFINED, JsValue::from_object);
                let _ = vm.call_value(JsValue::from_object(af), this_v, &[reason]);
            }
            reject_promise(vm, closed_p, reason);
            Ok(JsValue::from_object(promise_resolve_value(
                vm,
                JsValue::UNDEFINED,
            )))
        });
        let ss_rel = Rc::clone(&ss);
        define_host_method(vm, writer, "releaseLock", move |_, _, _| {
            if let Some(ws) = ss_rel.borrow_mut().writable.get_mut(&h) {
                ws.locked = false;
            }
            Ok(JsValue::UNDEFINED)
        });
        Ok(JsValue::from_object(writer))
    });

    let ss = Rc::clone(&streams);
    let ws_ctor = vm.alloc_host_fn("WritableStream", move |vm, _, args| {
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(wsp)));
        let sink_val = arg(args, 0);
        let mut ws = WritableStreamState {
            closed: false,
            errored: None,
            locked: false,
            write_fn: None,
            close_fn: None,
            abort_fn: None,
            sink_obj: sink_val.as_object(),
        };
        let mut start_fn = None;
        if let Some(sh) = sink_val.as_object() {
            for (name, slot) in [
                ("write", &mut ws.write_fn),
                ("close", &mut ws.close_fn),
                ("abort", &mut ws.abort_fn),
            ] {
                let k = vm.heap.intern_key(name);
                if let Ok(v) = vm.get_property_by_key(sh, k) {
                    *slot = v.as_object().filter(|&fh| vm.is_callable_handle(fh));
                }
            }
            let ks = vm.heap.intern_key("start");
            if let Ok(v) = vm.get_property_by_key(sh, ks) {
                start_fn = v.as_object().filter(|&fh| vm.is_callable_handle(fh));
            }
        }
        ss.borrow_mut().writable.insert(obj, ws);
        if let Some(sf) = start_fn {
            let ctrl = vm.alloc_ordinary_object();
            vm.call_value(
                JsValue::from_object(sf),
                sink_val,
                &[JsValue::from_object(ctrl)],
            )?;
        }
        Ok(JsValue::from_object(obj))
    });
    bind_ctor_proto(vm, ws_ctor, wsp);
    define_global(vm, "WritableStream", JsValue::from_object(ws_ctor));

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

        let writable = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(wsp)));
        ss.borrow_mut().writable.insert(
            writable,
            WritableStreamState {
                closed: false,
                errored: None,
                locked: false,
                write_fn: Some(write_cb),
                close_fn: Some(close_cb),
                abort_fn: None,
                sink_obj: None,
            },
        );

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

fn require_writable(
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

fn writable_write_chunk(
    vm: &mut JsVm,
    streams: &SharedStreams,
    ws_h: HeapHandle,
    chunk: JsValue,
) -> HeapHandle {
    let info = {
        let st = streams.borrow();
        let Some(ws) = st.writable.get(&ws_h) else {
            return promise_resolve_value(vm, JsValue::UNDEFINED);
        };
        if let Some(err) = ws.errored {
            let p = alloc_promise(vm);
            reject_promise(vm, p, err);
            return p;
        }
        (ws.write_fn, ws.sink_obj)
    };
    if let Some(wf) = info.0 {
        let this_v = info.1.map_or(JsValue::UNDEFINED, JsValue::from_object);
        match vm.call_value(JsValue::from_object(wf), this_v, &[chunk]) {
            Ok(v) => promise_resolve_value(vm, v),
            Err(e) => {
                let p = alloc_promise(vm);
                reject_promise(vm, p, e.value);
                p
            }
        }
    } else {
        promise_resolve_value(vm, JsValue::UNDEFINED)
    }
}

fn writable_close(vm: &mut JsVm, streams: &SharedStreams, ws_h: HeapHandle) -> HeapHandle {
    let info = {
        let mut st = streams.borrow_mut();
        let Some(ws) = st.writable.get_mut(&ws_h) else {
            return promise_resolve_value(vm, JsValue::UNDEFINED);
        };
        ws.closed = true;
        (ws.close_fn, ws.sink_obj)
    };
    if let Some(cf) = info.0 {
        let this_v = info.1.map_or(JsValue::UNDEFINED, JsValue::from_object);
        match vm.call_value(JsValue::from_object(cf), this_v, &[]) {
            Ok(v) => promise_resolve_value(vm, v),
            Err(e) => {
                let p = alloc_promise(vm);
                reject_promise(vm, p, e.value);
                p
            }
        }
    } else {
        promise_resolve_value(vm, JsValue::UNDEFINED)
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
            let close_p = writable_close(vm, &ss_ok, dst_h);
            let on_cl = vm.alloc_host_fn("pipe_close_ok", move |vm, _, _| {
                resolve_promise(vm, done_p, JsValue::UNDEFINED);
                Ok(JsValue::UNDEFINED)
            });
            let _ = promise_then(vm, close_p, Some(on_cl), None);
        } else {
            let val = vm
                .get_property_by_key(rh, k_val)
                .unwrap_or(JsValue::UNDEFINED);
            let wp = writable_write_chunk(vm, &ss_ok, dst_h, val);
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

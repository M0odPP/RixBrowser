//! WHATWG Streams Standard (`ReadableStream`, `ReadableStreamDefaultReader`,
//! `ReadableStreamDefaultController`, `WritableStream`, `WritableStreamDefaultWriter`,
//! `WritableStreamDefaultController`, `TransformStream`, `CountQueuingStrategy`,
//! `ByteLengthQueuingStrategy`, `TextDecoderStream`, `TextEncoderStream`,
//! and `Blob.prototype.stream()`).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use rix_js::promise::{
    alloc_promise, promise_resolve_value, promise_then, reject_promise, resolve_promise,
};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};

use crate::streams_bytes::{
    acquire_byob_reader, alloc_byte_readable_stream, byte_call_pull_if_needed,
    byte_cancel_internal, byte_controller_error, byte_default_read_with_request,
    byte_on_default_reader_released, install_byte_stream_types, parse_positive_u64_enforce_range,
    readable_byte_stream_tee, ByobRequestState, ByteControllerState, ReadableByobReaderState,
};
use crate::streams_pipe::{readable_pipe_through, readable_pipe_to_method};
use crate::streams_transform::{install_transform_stream_types, TransformStreamState};
pub(crate) use crate::streams_writable::create_writable_stream_from_callbacks;
use crate::streams_writable::{
    install_writable_stream_types, WritableDefaultWriterState, WritableStreamState,
};
use crate::web_apis::{
    alloc_uint8_array, arg, bind_ctor_proto, define_global, define_host_getter, define_host_method,
    SharedWebApis,
};

use crate::streams_common::{
    attach_gc_anchor, call_returning_promise, call_size_algorithm, extract_strategy,
    get_method_checked, promise_is_pending, promise_map_to_undefined, promise_mark_handled,
    promise_reject_value, set_to_string_tag, validate_and_normalize_hwm, QueueWithSizes,
};

pub(crate) type ChunkCallback = Rc<RefCell<Option<Box<dyn FnOnce(&mut JsVm, JsValue)>>>>;
pub(crate) type CloseCallback = Rc<RefCell<Option<Box<dyn FnOnce(&mut JsVm)>>>>;
pub(crate) type ErrorCallback = Rc<RefCell<Option<Box<dyn FnOnce(&mut JsVm, JsValue)>>>>;

#[derive(Clone)]
pub(crate) enum ReadRequest {
    Promise(HeapHandle),
    Callback {
        on_chunk: ChunkCallback,
        on_close: CloseCallback,
        on_error: ErrorCallback,
    },
}

pub(crate) fn fulfill_read_request(vm: &mut JsVm, req: ReadRequest, chunk: JsValue) {
    match req {
        ReadRequest::Promise(p) => {
            let res = make_read_result(vm, chunk, false);
            resolve_promise(vm, p, res);
        }
        ReadRequest::Callback { on_chunk, .. } => {
            if let Some(cb) = on_chunk.borrow_mut().take() {
                cb(vm, chunk);
            }
        }
    }
}

pub(crate) fn close_read_request(vm: &mut JsVm, req: ReadRequest) {
    match req {
        ReadRequest::Promise(p) => {
            let res = make_read_result(vm, JsValue::UNDEFINED, true);
            resolve_promise(vm, p, res);
        }
        ReadRequest::Callback { on_close, .. } => {
            if let Some(cb) = on_close.borrow_mut().take() {
                cb(vm);
            }
        }
    }
}

pub(crate) fn error_read_request(vm: &mut JsVm, req: ReadRequest, err: JsValue) {
    match req {
        ReadRequest::Promise(p) => reject_promise(vm, p, err),
        ReadRequest::Callback { on_error, .. } => {
            if let Some(cb) = on_error.borrow_mut().take() {
                cb(vm, err);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ReadableStream State Machine Structures
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReadableState {
    Readable,
    Closed,
    Errored,
}

#[derive(Clone)]
pub(crate) struct ReadableStreamState {
    pub(crate) state: ReadableState,
    pub(crate) stored_error: JsValue,
    pub(crate) disturbed: bool,
    pub(crate) reader: Option<HeapHandle>,
    pub(crate) reader_is_byob: bool,
    pub(crate) controller: Option<HeapHandle>,
    pub(crate) queue: QueueWithSizes,
    pub(crate) hwm: f64,
    pub(crate) size_fn: Option<HeapHandle>,
    pub(crate) started: bool,
    pub(crate) close_requested: bool,
    pub(crate) pull_again: bool,
    pub(crate) pulling: bool,
    pub(crate) is_byte_stream: bool,
    pub(crate) byte_controller: Option<ByteControllerState>,
    pub(crate) pull_fn: Option<HeapHandle>,
    pub(crate) cancel_fn: Option<HeapHandle>,
    pub(crate) source_obj: Option<HeapHandle>,
    pub(crate) read_requests: VecDeque<ReadRequest>,
}

impl ReadableStreamState {
    pub(crate) fn new(hwm: f64, size_fn: Option<HeapHandle>) -> Self {
        Self {
            state: ReadableState::Readable,
            stored_error: JsValue::UNDEFINED,
            disturbed: false,
            reader: None,
            reader_is_byob: false,
            controller: None,
            queue: QueueWithSizes::new(),
            hwm,
            size_fn,
            started: false,
            close_requested: false,
            pull_again: false,
            pulling: false,
            is_byte_stream: false,
            byte_controller: None,
            pull_fn: None,
            cancel_fn: None,
            source_obj: None,
            read_requests: VecDeque::new(),
        }
    }

    pub(crate) fn desired_size(&self) -> Option<f64> {
        match self.state {
            ReadableState::Errored => None,
            ReadableState::Closed => Some(0.0),
            ReadableState::Readable => Some(self.hwm - self.queue.total_size),
        }
    }

    pub(crate) fn can_close_or_enqueue(&self) -> bool {
        !self.close_requested && self.state == ReadableState::Readable
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ReadableDefaultReaderState {
    pub(crate) stream: Option<HeapHandle>,
    pub(crate) closed_promise: HeapHandle,
}

#[derive(Debug, Clone)]
struct AsyncIteratorState {
    reader: HeapHandle,
    prevent_cancel: bool,
    finished: bool,
    ongoing_promise: Option<HeapHandle>,
}

/// Shared registry for WHATWG `ReadableStream`, `WritableStream`, `TransformStream`,
/// readers, writers, controllers, and queuing strategies.
#[derive(Default)]
pub struct StreamSystem {
    pub(crate) readable: HashMap<HeapHandle, ReadableStreamState>,
    pub(crate) readable_controllers: HashMap<HeapHandle, HeapHandle>,
    pub(crate) byte_controllers: HashMap<HeapHandle, HeapHandle>,
    pub(crate) readable_readers: HashMap<HeapHandle, ReadableDefaultReaderState>,
    pub(crate) byob_readers: HashMap<HeapHandle, ReadableByobReaderState>,
    pub(crate) byob_requests: HashMap<HeapHandle, ByobRequestState>,
    async_iterators: HashMap<HeapHandle, AsyncIteratorState>,
    pub(crate) writable: HashMap<HeapHandle, WritableStreamState>,
    pub(crate) writable_controllers: HashMap<HeapHandle, HeapHandle>,
    pub(crate) writable_writers: HashMap<HeapHandle, WritableDefaultWriterState>,
    pub(crate) transforms: HashMap<HeapHandle, TransformStreamState>,
    pub(crate) transform_controllers: HashMap<HeapHandle, HeapHandle>,
    pub(crate) active_pipes: HashMap<HeapHandle, Vec<HeapHandle>>,
    count_strategies: HashMap<HeapHandle, f64>,
    byte_length_strategies: HashMap<HeapHandle, f64>,
    pub readable_proto: Option<HeapHandle>,
    pub readable_reader_proto: Option<HeapHandle>,
    pub readable_controller_proto: Option<HeapHandle>,
    pub byte_controller_proto: Option<HeapHandle>,
    pub byob_reader_proto: Option<HeapHandle>,
    pub byob_request_proto: Option<HeapHandle>,
    pub async_iterator_proto: Option<HeapHandle>,
    pub writable_proto: Option<HeapHandle>,
    pub writable_writer_proto: Option<HeapHandle>,
    pub writable_controller_proto: Option<HeapHandle>,
    pub transform_proto: Option<HeapHandle>,
    pub transform_controller_proto: Option<HeapHandle>,
    pub(crate) et_add_listener_fn: Option<HeapHandle>,
    pub(crate) et_remove_listener_fn: Option<HeapHandle>,
    count_size_fn: Option<HeapHandle>,
    byte_length_size_fn: Option<HeapHandle>,
}

/// Shared handle to [`StreamSystem`].
pub type SharedStreams = Rc<RefCell<StreamSystem>>;

impl StreamSystem {
    /// Collects all GC roots retained in stream queues, controllers, readers, writers, and prototypes.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for p in [
            self.readable_proto,
            self.readable_reader_proto,
            self.readable_controller_proto,
            self.byte_controller_proto,
            self.byob_reader_proto,
            self.byob_request_proto,
            self.async_iterator_proto,
            self.writable_proto,
            self.writable_writer_proto,
            self.writable_controller_proto,
            self.transform_proto,
            self.transform_controller_proto,
            self.et_add_listener_fn,
            self.et_remove_listener_fn,
            self.count_size_fn,
            self.byte_length_size_fn,
        ]
        .into_iter()
        .flatten()
        {
            out.push(JsValue::from_object(p));
        }
        for rs in self.readable.values() {
            for e in &rs.queue.entries {
                out.push(e.value);
            }
            out.push(rs.stored_error);
            for h in [
                rs.reader,
                rs.controller,
                rs.size_fn,
                rs.pull_fn,
                rs.cancel_fn,
                rs.source_obj,
            ]
            .into_iter()
            .flatten()
            {
                out.push(JsValue::from_object(h));
            }
            for req in &rs.read_requests {
                if let ReadRequest::Promise(p) = req {
                    out.push(JsValue::from_object(*p));
                }
            }
            if let Some(ref bc) = rs.byte_controller {
                bc.collect_roots(out);
            }
        }
        for r in self.readable_readers.values() {
            if let Some(s) = r.stream {
                out.push(JsValue::from_object(s));
            }
            out.push(JsValue::from_object(r.closed_promise));
        }
        for r in self.byob_readers.values() {
            if let Some(s) = r.stream {
                out.push(JsValue::from_object(s));
            }
            out.push(JsValue::from_object(r.closed_promise));
        }
        for br in self.byob_requests.values() {
            if let Some(s) = br.stream {
                out.push(JsValue::from_object(s));
            }
            if let Some(v) = br.view {
                out.push(JsValue::from_object(v));
            }
        }
        for it in self.async_iterators.values() {
            out.push(JsValue::from_object(it.reader));
            if let Some(p) = it.ongoing_promise {
                out.push(JsValue::from_object(p));
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
        for ts in self.transforms.values() {
            ts.collect_roots(out);
        }
        for (&pipe_p, handles) in &self.active_pipes {
            out.push(JsValue::from_object(pipe_p));
            for &h in handles {
                out.push(JsValue::from_object(h));
            }
        }
    }

    /// Returns whether the given `ReadableStream` has been read from or canceled (disturbed).
    pub(crate) fn is_readable_disturbed(&self, h: HeapHandle) -> bool {
        self.readable.get(&h).is_some_and(|rs| rs.disturbed)
    }

    /// Returns whether the given `ReadableStream` is currently locked to a reader.
    pub(crate) fn is_readable_locked(&self, h: HeapHandle) -> bool {
        self.readable.get(&h).is_some_and(|rs| rs.reader.is_some())
    }

    /// Sweeps dead native entries for GC-collected stream handles.
    pub fn sweep_dead(&mut self, heap: &rix_js::GcHeap) {
        self.readable.retain(|&h, _| heap.is_alive(h));
        self.readable_controllers.retain(|&h, _| heap.is_alive(h));
        self.byte_controllers.retain(|&h, _| heap.is_alive(h));
        self.readable_readers.retain(|&h, _| heap.is_alive(h));
        self.byob_readers.retain(|&h, _| heap.is_alive(h));
        self.byob_requests.retain(|&h, _| heap.is_alive(h));
        self.async_iterators.retain(|&h, _| heap.is_alive(h));
        self.writable.retain(|&h, _| heap.is_alive(h));
        self.writable_controllers.retain(|&h, _| heap.is_alive(h));
        self.writable_writers.retain(|&h, _| heap.is_alive(h));
        self.transforms.retain(|&h, _| heap.is_alive(h));
        self.transform_controllers.retain(|&h, _| heap.is_alive(h));
        self.count_strategies.retain(|&h, _| heap.is_alive(h));
        self.byte_length_strategies.retain(|&h, _| heap.is_alive(h));
    }
}

// ---------------------------------------------------------------------------
// ReadableStream Core Algorithms (WHATWG Streams §4)
// ---------------------------------------------------------------------------

pub(crate) fn make_read_result(vm: &mut JsVm, value: JsValue, done: bool) -> JsValue {
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

fn readable_stream_should_call_pull(rs: &ReadableStreamState) -> bool {
    if !rs.can_close_or_enqueue() || !rs.started {
        return false;
    }
    if rs.reader.is_some() && !rs.read_requests.is_empty() {
        return true;
    }
    rs.desired_size().is_some_and(|d| d > 0.0)
}

pub(crate) fn readable_call_pull_if_needed(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
) {
    let pull_call = {
        let mut st = streams.borrow_mut();
        let Some(rs) = st.readable.get_mut(&stream_h) else {
            return;
        };
        if !readable_stream_should_call_pull(rs) {
            return;
        }
        if rs.pulling {
            rs.pull_again = true;
            return;
        }
        rs.pulling = true;
        (rs.pull_fn, rs.source_obj, rs.controller)
    };

    let Some(pull_fn) = pull_call.0 else {
        if let Some(rs) = streams.borrow_mut().readable.get_mut(&stream_h) {
            rs.pulling = false;
            rs.pull_again = false;
        }
        return;
    };
    let this_v = pull_call.1.map_or(JsValue::UNDEFINED, JsValue::from_object);
    let ctrl_v = pull_call.2.map_or(JsValue::UNDEFINED, JsValue::from_object);
    let pull_p = call_returning_promise(vm, pull_fn, this_v, &[ctrl_v]);

    let ss_ok = Rc::clone(streams);
    let on_ok = vm.alloc_host_fn("rs_pull_ok", move |vm, _, _| {
        let again = {
            let mut st = ss_ok.borrow_mut();
            if let Some(rs) = st.readable.get_mut(&stream_h) {
                rs.pulling = false;
                let a = rs.pull_again;
                rs.pull_again = false;
                a
            } else {
                false
            }
        };
        if again {
            readable_call_pull_if_needed(vm, &ss_ok, stream_h);
        }
        Ok(JsValue::UNDEFINED)
    });
    let ss_err = Rc::clone(streams);
    let on_err = vm.alloc_host_fn("rs_pull_err", move |vm, _, args| {
        readable_error_internal(vm, &ss_err, stream_h, arg(args, 0));
        Ok(JsValue::UNDEFINED)
    });
    let _ = promise_then(vm, pull_p, Some(on_ok), Some(on_err));
}

pub(crate) fn readable_close_stream(vm: &mut JsVm, streams: &SharedStreams, stream_h: HeapHandle) {
    let (pending, closed_p) = {
        let mut st = streams.borrow_mut();
        let Some(rs) = st.readable.get_mut(&stream_h) else {
            return;
        };
        if rs.state != ReadableState::Readable {
            return;
        }
        rs.state = ReadableState::Closed;
        let reader_h = rs.reader;
        let pending = std::mem::take(&mut rs.read_requests);
        let closed_p = reader_h
            .and_then(|rh| st.readable_readers.get(&rh))
            .map(|r| r.closed_promise);
        (pending, closed_p)
    };
    if let Some(cp) = closed_p {
        resolve_promise(vm, cp, JsValue::UNDEFINED);
    }
    for req in pending {
        close_read_request(vm, req);
    }
}

pub(crate) fn readable_controller_close(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
) -> Result<(), JsException> {
    let close_now = {
        let mut st = streams.borrow_mut();
        let Some(rs) = st.readable.get_mut(&stream_h) else {
            return Err(vm.type_error("Invalid ReadableStreamDefaultController"));
        };
        if !rs.can_close_or_enqueue() {
            return Err(vm.type_error("Cannot close a closed or errored ReadableStream"));
        }
        rs.close_requested = true;
        rs.queue.is_empty()
    };
    if close_now {
        if let Some(rs) = streams.borrow_mut().readable.get_mut(&stream_h) {
            rs.pull_fn = None;
            rs.cancel_fn = None;
            rs.size_fn = None;
        }
        readable_close_stream(vm, streams, stream_h);
    }
    Ok(())
}

pub(crate) fn readable_controller_enqueue(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
    chunk: JsValue,
) -> Result<(), JsException> {
    let step = {
        let mut st = streams.borrow_mut();
        let Some(rs) = st.readable.get_mut(&stream_h) else {
            return Err(vm.type_error("Invalid ReadableStreamDefaultController"));
        };
        if !rs.can_close_or_enqueue() {
            return Err(vm.type_error("Cannot enqueue into a closed or errored ReadableStream"));
        }
        if rs.reader.is_some() {
            if let Some(req_p) = rs.read_requests.pop_front() {
                (Some(req_p), None)
            } else {
                (None, Some(rs.size_fn))
            }
        } else {
            (None, Some(rs.size_fn))
        }
    };

    if let Some(req) = step.0 {
        fulfill_read_request(vm, req, chunk);
        readable_call_pull_if_needed(vm, streams, stream_h);
        return Ok(());
    }

    let size_fn = step.1.flatten();
    let chunk_size = match call_size_algorithm(vm, size_fn, chunk) {
        Ok(sz) => sz,
        Err(e) => {
            readable_error_internal(vm, streams, stream_h, e.value);
            return Err(e);
        }
    };

    // Re-check stream state after user `size()` callback (re-entrancy safe!).
    let enqueue_res = {
        let mut st = streams.borrow_mut();
        let Some(rs) = st.readable.get_mut(&stream_h) else {
            return Ok(());
        };
        if rs.state != ReadableState::Readable {
            return Ok(());
        }
        rs.queue.enqueue_value_with_size(vm, chunk, chunk_size)
    };
    if let Err(e) = enqueue_res {
        readable_error_internal(vm, streams, stream_h, e.value);
        return Err(e);
    }
    readable_call_pull_if_needed(vm, streams, stream_h);
    Ok(())
}

pub(crate) fn readable_error_internal(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
    reason: JsValue,
) {
    if streams
        .borrow()
        .readable
        .get(&stream_h)
        .is_some_and(|rs| rs.is_byte_stream)
    {
        byte_controller_error(vm, streams, stream_h, reason);
        return;
    }
    let (pending, closed_p) = {
        let mut st = streams.borrow_mut();
        let Some(rs) = st.readable.get_mut(&stream_h) else {
            return;
        };
        if rs.state != ReadableState::Readable {
            return;
        }
        rs.queue.reset();
        rs.pull_fn = None;
        rs.cancel_fn = None;
        rs.size_fn = None;
        rs.state = ReadableState::Errored;
        rs.stored_error = reason;
        let reader_h = rs.reader;
        let pending = std::mem::take(&mut rs.read_requests);
        let closed_p = reader_h
            .and_then(|rh| st.readable_readers.get(&rh))
            .map(|r| r.closed_promise);
        (pending, closed_p)
    };
    if let Some(cp) = closed_p {
        reject_promise(vm, cp, reason);
        promise_mark_handled(vm, cp);
    }
    for req in pending {
        error_read_request(vm, req, reason);
    }
}

pub(crate) fn readable_cancel_internal(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
    reason: JsValue,
) -> HeapHandle {
    if streams
        .borrow()
        .readable
        .get(&stream_h)
        .is_some_and(|rs| rs.is_byte_stream)
    {
        return byte_cancel_internal(vm, streams, stream_h, reason);
    }
    let action = {
        let mut st = streams.borrow_mut();
        let Some(rs) = st.readable.get_mut(&stream_h) else {
            return promise_resolve_value(vm, JsValue::UNDEFINED);
        };
        rs.disturbed = true;
        match rs.state {
            ReadableState::Closed => return promise_resolve_value(vm, JsValue::UNDEFINED),
            ReadableState::Errored => return promise_reject_value(vm, rs.stored_error),
            ReadableState::Readable => {
                let cf = rs.cancel_fn.take();
                let src = rs.source_obj;
                rs.queue.reset();
                rs.pull_fn = None;
                rs.size_fn = None;
                (cf, src)
            }
        }
    };
    readable_close_stream(vm, streams, stream_h);
    if let Some(cf) = action.0 {
        let this_v = action.1.map_or(JsValue::UNDEFINED, JsValue::from_object);
        let p = call_returning_promise(vm, cf, this_v, &[reason]);
        promise_map_to_undefined(vm, p)
    } else {
        promise_resolve_value(vm, JsValue::UNDEFINED)
    }
}

pub(crate) fn readable_read_with_request(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
    req: ReadRequest,
) {
    if streams
        .borrow()
        .readable
        .get(&stream_h)
        .is_some_and(|rs| rs.is_byte_stream)
    {
        byte_default_read_with_request(vm, streams, stream_h, req);
        return;
    }
    enum ReadOutcome {
        Chunk(JsValue, bool, ReadRequest),
        Closed(ReadRequest),
        Errored(JsValue, ReadRequest),
        Pending,
    }
    let outcome = {
        let mut st = streams.borrow_mut();
        let Some(rs) = st.readable.get_mut(&stream_h) else {
            drop(st);
            close_read_request(vm, req);
            return;
        };
        rs.disturbed = true;
        match rs.state {
            ReadableState::Closed => ReadOutcome::Closed(req),
            ReadableState::Errored => ReadOutcome::Errored(rs.stored_error, req),
            ReadableState::Readable => {
                if let Some(chunk) = rs.queue.dequeue_value() {
                    let close_after = rs.close_requested && rs.queue.is_empty();
                    if close_after {
                        rs.pull_fn = None;
                        rs.cancel_fn = None;
                        rs.size_fn = None;
                    }
                    ReadOutcome::Chunk(chunk, close_after, req)
                } else {
                    rs.read_requests.push_back(req);
                    ReadOutcome::Pending
                }
            }
        }
    };
    match outcome {
        ReadOutcome::Chunk(chunk, close_after, r) => {
            if close_after {
                readable_close_stream(vm, streams, stream_h);
            } else {
                readable_call_pull_if_needed(vm, streams, stream_h);
            }
            fulfill_read_request(vm, r, chunk);
        }
        ReadOutcome::Closed(r) => close_read_request(vm, r),
        ReadOutcome::Errored(err, r) => error_read_request(vm, r, err),
        ReadOutcome::Pending => readable_call_pull_if_needed(vm, streams, stream_h),
    }
}

pub(crate) fn readable_reader_read_internal(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
) -> HeapHandle {
    let p = alloc_promise(vm);
    readable_read_with_request(vm, streams, stream_h, ReadRequest::Promise(p));
    p
}

pub(crate) fn acquire_default_reader(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
) -> Result<HeapHandle, JsException> {
    let reader_proto = streams
        .borrow()
        .readable_reader_proto
        .unwrap_or(vm.object_proto);
    let closed_p = alloc_promise(vm);
    let reader = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(reader_proto),
    ));
    let init_state = {
        let mut st = streams.borrow_mut();
        let Some(rs) = st.readable.get_mut(&stream_h) else {
            return Err(vm.type_error("Invalid ReadableStream"));
        };
        if rs.reader.is_some() {
            return Err(vm.type_error("ReadableStream is locked to a reader"));
        }
        rs.reader = Some(reader);
        let state = (rs.state, rs.stored_error);
        st.readable_readers.insert(
            reader,
            ReadableDefaultReaderState {
                stream: Some(stream_h),
                closed_promise: closed_p,
            },
        );
        state
    };
    attach_gc_anchor(vm, reader, stream_h);
    match init_state.0 {
        ReadableState::Readable => {}
        ReadableState::Closed => resolve_promise(vm, closed_p, JsValue::UNDEFINED),
        ReadableState::Errored => {
            reject_promise(vm, closed_p, init_state.1);
            promise_mark_handled(vm, closed_p);
        }
    }
    Ok(reader)
}

pub(crate) fn release_default_reader(vm: &mut JsVm, streams: &SharedStreams, reader_h: HeapHandle) {
    let info = {
        let mut st = streams.borrow_mut();
        let Some(r) = st.readable_readers.get_mut(&reader_h) else {
            return;
        };
        let Some(stream_h) = r.stream.take() else {
            return;
        };
        let closed_p = r.closed_promise;
        let (pending, byte_stream_h) = if let Some(rs) = st.readable.get_mut(&stream_h) {
            rs.reader = None;
            rs.reader_is_byob = false;
            (
                std::mem::take(&mut rs.read_requests),
                rs.is_byte_stream.then_some(stream_h),
            )
        } else {
            (VecDeque::new(), None)
        };
        (closed_p, pending, byte_stream_h)
    };
    if let Some(sh) = info.2 {
        byte_on_default_reader_released(streams, sh);
    }
    let rel_err = vm.type_error("Reader lock was released").value;
    if promise_is_pending(vm, info.0) {
        reject_promise(vm, info.0, rel_err);
        promise_mark_handled(vm, info.0);
    } else {
        let new_cp = promise_reject_value(vm, rel_err);
        promise_mark_handled(vm, new_cp);
        if let Some(r) = streams.borrow_mut().readable_readers.get_mut(&reader_h) {
            r.closed_promise = new_cp;
        }
    }
    for req in info.1 {
        error_read_request(vm, req, rel_err);
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
    let ctrl_proto = streams
        .borrow()
        .readable_controller_proto
        .unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let ctrl = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(ctrl_proto)));
    let mut st = ReadableStreamState::new(1.0, None);
    st.started = true;
    st.controller = Some(ctrl);
    if !bytes.is_empty() {
        let sz = bytes.len() as f64;
        let u8a = alloc_uint8_array(vm, bytes);
        let _ = st
            .queue
            .enqueue_value_with_size(vm, JsValue::from_object(u8a), sz);
        st.close_requested = true;
    } else {
        st.state = ReadableState::Closed;
    }
    attach_gc_anchor(vm, ctrl, obj);
    let mut reg = streams.borrow_mut();
    reg.readable.insert(obj, st);
    reg.readable_controllers.insert(ctrl, obj);
    obj
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn alloc_readable_stream_internal(
    vm: &mut JsVm,
    streams: &SharedStreams,
    hwm: f64,
    size_fn: Option<HeapHandle>,
    pull_fn: Option<HeapHandle>,
    cancel_fn: Option<HeapHandle>,
    source_obj: Option<HeapHandle>,
    started: bool,
) -> (HeapHandle, HeapHandle) {
    let (rsp, ctrl_proto) = {
        let s = streams.borrow();
        (
            s.readable_proto.unwrap_or(vm.object_proto),
            s.readable_controller_proto.unwrap_or(vm.object_proto),
        )
    };
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(rsp)));
    let ctrl = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(ctrl_proto)));
    let mut st = ReadableStreamState::new(hwm, size_fn);
    st.started = started;
    st.pull_fn = pull_fn;
    st.cancel_fn = cancel_fn;
    st.source_obj = source_obj;
    st.controller = Some(ctrl);
    attach_gc_anchor(vm, ctrl, obj);
    let mut reg = streams.borrow_mut();
    reg.readable.insert(obj, st);
    reg.readable_controllers.insert(ctrl, obj);
    (obj, ctrl)
}

// ---------------------------------------------------------------------------
// Live Pull-on-Demand ReadableStreamDefaultTee (WHATWG Streams §4.2.4)
// ---------------------------------------------------------------------------

/// Tees a Fetch body `ReadableStream` with `cloneForBranch2 = true` (WHATWG Fetch §5.2).
pub(crate) fn tee_readable_stream_for_fetch(
    vm: &mut JsVm,
    streams: &SharedStreams,
    web_apis: &SharedWebApis,
    src_h: HeapHandle,
) -> Result<(HeapHandle, HeapHandle), JsException> {
    let is_byte = streams
        .borrow()
        .readable
        .get(&src_h)
        .is_some_and(|rs| rs.is_byte_stream);
    let arr = if is_byte {
        readable_byte_stream_tee(vm, streams, src_h)?
    } else {
        readable_stream_default_tee(vm, streams, Some(web_apis), src_h)?
    };
    let b1 = vm
        .heap
        .get_object(arr)
        .and_then(|o| o.elements.get(0))
        .and_then(|v| v.as_object())
        .ok_or_else(|| vm.type_error("tee failed to create branch 1"))?;
    let b2 = vm
        .heap
        .get_object(arr)
        .and_then(|o| o.elements.get(1))
        .and_then(|v| v.as_object())
        .ok_or_else(|| vm.type_error("tee failed to create branch 2"))?;
    Ok((b1, b2))
}

fn readable_stream_default_tee(
    vm: &mut JsVm,
    streams: &SharedStreams,
    clone_web_apis: Option<&SharedWebApis>,
    src_h: HeapHandle,
) -> Result<HeapHandle, JsException> {
    let clone_wa = clone_web_apis.map(Rc::clone);
    let reader_h = acquire_default_reader(vm, streams, src_h)?;
    let reader_closed_p = streams
        .borrow()
        .readable_readers
        .get(&reader_h)
        .map(|r| r.closed_promise)
        .unwrap_or_else(|| alloc_promise(vm));

    struct TeeShared {
        reading: bool,
        read_again: bool,
        canceled1: bool,
        canceled2: bool,
        reason1: JsValue,
        reason2: JsValue,
        cancel_promise: HeapHandle,
        branch1: Option<HeapHandle>,
        branch2: Option<HeapHandle>,
    }

    let cancel_promise = alloc_promise(vm);
    attach_gc_anchor(vm, reader_h, cancel_promise);
    let shared = Rc::new(RefCell::new(TeeShared {
        reading: false,
        read_again: false,
        canceled1: false,
        canceled2: false,
        reason1: JsValue::UNDEFINED,
        reason2: JsValue::UNDEFINED,
        cancel_promise,
        branch1: None,
        branch2: None,
    }));

    fn tee_pull(
        vm: &mut JsVm,
        streams: &SharedStreams,
        clone_wa: &Option<SharedWebApis>,
        src_h: HeapHandle,
        shared: &Rc<RefCell<TeeShared>>,
    ) {
        {
            let mut sh = shared.borrow_mut();
            if sh.reading {
                sh.read_again = true;
                return;
            }
            sh.reading = true;
        }
        let ss_chunk = Rc::clone(streams);
        let sh_chunk = Rc::clone(shared);
        let wa_chunk = clone_wa.clone();
        let ss_close = Rc::clone(streams);
        let sh_close = Rc::clone(shared);
        let sh_err = Rc::clone(shared);
        let req = ReadRequest::Callback {
            on_chunk: Rc::new(RefCell::new(Some(Box::new(
                move |vm: &mut JsVm, val: JsValue| {
                    let ss_m = Rc::clone(&ss_chunk);
                    let sh_m = Rc::clone(&sh_chunk);
                    let wa_m = wa_chunk.clone();
                    let micro =
                        vm.alloc_host_fn_rooted("tee_chunk_micro", &[val], move |vm, _, _| {
                            let (c1, c2, b1, b2) = {
                                let mut sh = sh_m.borrow_mut();
                                sh.read_again = false;
                                (sh.canceled1, sh.canceled2, sh.branch1, sh.branch2)
                            };
                            let mut val2 = val;
                            if !c2 {
                                if let Some(ref wa) = wa_m {
                                    let mut memo = HashMap::new();
                                    match crate::web_apis::structured_clone_inner(
                                        vm, wa, val, &mut memo,
                                    ) {
                                        Ok(cloned) => val2 = cloned,
                                        Err(e) => {
                                            if let Some(h1) = b1 {
                                                readable_error_internal(vm, &ss_m, h1, e.value);
                                            }
                                            if let Some(h2) = b2 {
                                                readable_error_internal(vm, &ss_m, h2, e.value);
                                            }
                                            let cp = sh_m.borrow().cancel_promise;
                                            let res_p =
                                                readable_cancel_internal(vm, &ss_m, src_h, e.value);
                                            let on_ok =
                                                vm.alloc_host_fn("tee_sc_ok", move |vm, _, _| {
                                                    resolve_promise(vm, cp, JsValue::UNDEFINED);
                                                    Ok(JsValue::UNDEFINED)
                                                });
                                            let on_err =
                                                vm.alloc_host_fn("tee_sc_err", move |vm, _, a| {
                                                    reject_promise(vm, cp, arg(a, 0));
                                                    Ok(JsValue::UNDEFINED)
                                                });
                                            let _ =
                                                promise_then(vm, res_p, Some(on_ok), Some(on_err));
                                            return Ok(JsValue::UNDEFINED);
                                        }
                                    }
                                }
                            }
                            if !c1 {
                                if let Some(h1) = b1 {
                                    let _ = readable_controller_enqueue(vm, &ss_m, h1, val);
                                }
                            }
                            if !c2 {
                                if let Some(h2) = b2 {
                                    let _ = readable_controller_enqueue(vm, &ss_m, h2, val2);
                                }
                            }
                            let again = {
                                let mut sh = sh_m.borrow_mut();
                                sh.reading = false;
                                sh.read_again
                            };
                            if again {
                                tee_pull(vm, &ss_m, &wa_m, src_h, &sh_m);
                            }
                            Ok(JsValue::UNDEFINED)
                        });
                    let resolved = promise_resolve_value(vm, JsValue::UNDEFINED);
                    let _ = promise_then(vm, resolved, Some(micro), None);
                },
            )))),
            on_close: Rc::new(RefCell::new(Some(Box::new(move |vm: &mut JsVm| {
                let (c1, c2, b1, b2, cp) = {
                    let mut sh = sh_close.borrow_mut();
                    sh.reading = false;
                    (
                        sh.canceled1,
                        sh.canceled2,
                        sh.branch1,
                        sh.branch2,
                        sh.cancel_promise,
                    )
                };
                if !c1 {
                    if let Some(h1) = b1 {
                        let _ = readable_controller_close(vm, &ss_close, h1);
                    }
                }
                if !c2 {
                    if let Some(h2) = b2 {
                        let _ = readable_controller_close(vm, &ss_close, h2);
                    }
                }
                if !c1 || !c2 {
                    resolve_promise(vm, cp, JsValue::UNDEFINED);
                }
            })))),
            on_error: Rc::new(RefCell::new(Some(Box::new(
                move |_vm: &mut JsVm, _err: JsValue| {
                    sh_err.borrow_mut().reading = false;
                },
            )))),
        };
        readable_read_with_request(vm, streams, src_h, req);
    }

    let ss_p1 = Rc::clone(streams);
    let sh_p1 = Rc::clone(&shared);
    let wa_p1 = clone_wa.clone();
    let pull_cb1 = vm.alloc_host_fn("tee_pull1", move |vm, _, _| {
        tee_pull(vm, &ss_p1, &wa_p1, src_h, &sh_p1);
        Ok(JsValue::UNDEFINED)
    });
    let ss_p2 = Rc::clone(streams);
    let sh_p2 = Rc::clone(&shared);
    let wa_p2 = clone_wa;
    let pull_cb2 = vm.alloc_host_fn("tee_pull2", move |vm, _, _| {
        tee_pull(vm, &ss_p2, &wa_p2, src_h, &sh_p2);
        Ok(JsValue::UNDEFINED)
    });

    let ss_c1 = Rc::clone(streams);
    let sh_c1 = Rc::clone(&shared);
    let cancel_cb1 = vm.alloc_host_fn("tee_cancel1", move |vm, _, args| {
        let reason = arg(args, 0);
        let (both, r1, r2, cp) = {
            let mut sh = sh_c1.borrow_mut();
            sh.canceled1 = true;
            sh.reason1 = reason;
            (sh.canceled2, sh.reason1, sh.reason2, sh.cancel_promise)
        };
        if both {
            let composite = vm.alloc_array();
            vm.heap.object_push_element(composite, r1);
            vm.heap.object_push_element(composite, r2);
            let res_p =
                readable_cancel_internal(vm, &ss_c1, src_h, JsValue::from_object(composite));
            let on_ok = vm.alloc_host_fn("tee_c_ok", move |vm, _, _| {
                resolve_promise(vm, cp, JsValue::UNDEFINED);
                Ok(JsValue::UNDEFINED)
            });
            let on_err = vm.alloc_host_fn("tee_c_err", move |vm, _, a| {
                reject_promise(vm, cp, arg(a, 0));
                Ok(JsValue::UNDEFINED)
            });
            let _ = promise_then(vm, res_p, Some(on_ok), Some(on_err));
        }
        Ok(JsValue::from_object(cp))
    });

    let ss_c2 = Rc::clone(streams);
    let sh_c2 = Rc::clone(&shared);
    let cancel_cb2 = vm.alloc_host_fn("tee_cancel2", move |vm, _, args| {
        let reason = arg(args, 0);
        let (both, r1, r2, cp) = {
            let mut sh = sh_c2.borrow_mut();
            sh.canceled2 = true;
            sh.reason2 = reason;
            (sh.canceled1, sh.reason1, sh.reason2, sh.cancel_promise)
        };
        if both {
            let composite = vm.alloc_array();
            vm.heap.object_push_element(composite, r1);
            vm.heap.object_push_element(composite, r2);
            let res_p =
                readable_cancel_internal(vm, &ss_c2, src_h, JsValue::from_object(composite));
            let on_ok = vm.alloc_host_fn("tee_c_ok2", move |vm, _, _| {
                resolve_promise(vm, cp, JsValue::UNDEFINED);
                Ok(JsValue::UNDEFINED)
            });
            let on_err = vm.alloc_host_fn("tee_c_err2", move |vm, _, a| {
                reject_promise(vm, cp, arg(a, 0));
                Ok(JsValue::UNDEFINED)
            });
            let _ = promise_then(vm, res_p, Some(on_ok), Some(on_err));
        }
        Ok(JsValue::from_object(cp))
    });

    let (b1, _) = alloc_readable_stream_internal(
        vm,
        streams,
        1.0,
        None,
        Some(pull_cb1),
        Some(cancel_cb1),
        None,
        false,
    );
    let (b2, _) = alloc_readable_stream_internal(
        vm,
        streams,
        1.0,
        None,
        Some(pull_cb2),
        Some(cancel_cb2),
        None,
        false,
    );
    {
        let mut sh = shared.borrow_mut();
        sh.branch1 = Some(b1);
        sh.branch2 = Some(b2);
    }
    attach_gc_anchor(vm, b1, reader_h);
    attach_gc_anchor(vm, b2, reader_h);

    // Schedule standard startAlgorithm completion microtask on both tee branches (WHATWG §4.2.4)
    // so initial pull runs against the source stream (resolving closed/cancel states cleanly).
    for b in [b1, b2] {
        let start_p = promise_resolve_value(vm, JsValue::UNDEFINED);
        let ss_s = Rc::clone(streams);
        let on_start = vm.alloc_host_fn("tee_branch_start", move |vm, _, _| {
            if let Some(rs) = ss_s.borrow_mut().readable.get_mut(&b) {
                rs.started = true;
            }
            readable_call_pull_if_needed(vm, &ss_s, b);
            Ok(JsValue::UNDEFINED)
        });
        let _ = promise_then(vm, start_p, Some(on_start), None);
    }

    // Propagate source reader `closed` rejection to both branches.
    let ss_cl = Rc::clone(streams);
    let sh_cl = Rc::clone(&shared);
    let on_closed_err = vm.alloc_host_fn("tee_closed_err", move |vm, _, args| {
        let err = arg(args, 0);
        let (c1, c2, cp) = {
            let sh = sh_cl.borrow();
            (sh.canceled1, sh.canceled2, sh.cancel_promise)
        };
        readable_error_internal(vm, &ss_cl, b1, err);
        readable_error_internal(vm, &ss_cl, b2, err);
        if !c1 || !c2 {
            resolve_promise(vm, cp, JsValue::UNDEFINED);
        }
        Ok(JsValue::UNDEFINED)
    });
    let _ = promise_then(vm, reader_closed_p, None, Some(on_closed_err));

    let arr = vm.alloc_array();
    vm.heap.object_push_element(arr, JsValue::from_object(b1));
    vm.heap.object_push_element(arr, JsValue::from_object(b2));
    Ok(arr)
}

// ---------------------------------------------------------------------------
// ReadableStream.from(asyncIterable) (WHATWG Streams §4.2.5)
// ---------------------------------------------------------------------------

fn readable_stream_from(
    vm: &mut JsVm,
    streams: &SharedStreams,
    iterable: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(iter_obj) = iterable.as_object() else {
        return Err(vm.type_error("ReadableStream.from argument is not iterable"));
    };
    let k_async = vm.key_symbol_async_iterator;
    let k_sync = vm.key_symbol_iterator;
    let async_method = vm.get_property_by_key(iter_obj, k_async)?;
    let (iterator_val, is_async) = if !async_method.is_undefined() && !async_method.is_null() {
        let Some(fh) = async_method
            .as_object()
            .filter(|&h| vm.is_callable_handle(h))
        else {
            return Err(vm.type_error("@@asyncIterator is not callable"));
        };
        let it = vm.call_value(JsValue::from_object(fh), iterable, &[])?;
        (it, true)
    } else {
        let sync_method = vm.get_property_by_key(iter_obj, k_sync)?;
        let Some(fh) = sync_method
            .as_object()
            .filter(|&h| vm.is_callable_handle(h))
        else {
            return Err(vm.type_error("ReadableStream.from argument is not iterable"));
        };
        let it = vm.call_value(JsValue::from_object(fh), iterable, &[])?;
        (it, false)
    };
    let Some(it_h) = iterator_val.as_object() else {
        return Err(vm.type_error("Iterator must be an object"));
    };
    let k_next = vm.heap.intern_key("next");
    let next_val = vm.get_property_by_key(it_h, k_next)?;
    let Some(next_fn) = next_val.as_object().filter(|&h| vm.is_callable_handle(h)) else {
        return Err(vm.type_error("Iterator next is not callable"));
    };

    let ss_pull = Rc::clone(streams);
    let stream_slot: Rc<Cell<Option<HeapHandle>>> = Rc::new(Cell::new(None));
    let slot_pull = Rc::clone(&stream_slot);
    let pull_fn =
        vm.alloc_host_fn_rooted("from_pull", &[iterator_val, next_val], move |vm, _, _| {
            let Some(stream_h) = slot_pull.get() else {
                return Ok(JsValue::UNDEFINED);
            };
            let next_res_p = call_returning_promise(vm, next_fn, iterator_val, &[]);
            let ss_ok = Rc::clone(&ss_pull);
            let on_next = vm.alloc_host_fn("from_next_ok", move |vm, _, args| {
                let res = arg(args, 0);
                let Some(rh) = res.as_object() else {
                    return Err(vm.type_error("Iterator result is not an object"));
                };
                let k_done = vm.heap.intern_key("done");
                let done_v = vm.get_property_by_key(rh, k_done)?;
                let done = vm.to_boolean(done_v);
                if done {
                    let _ = readable_controller_close(vm, &ss_ok, stream_h);
                    return Ok(JsValue::UNDEFINED);
                }
                let k_val = vm.heap.intern_key("value");
                let val = vm.get_property_by_key(rh, k_val)?;
                if is_async {
                    let _ = readable_controller_enqueue(vm, &ss_ok, stream_h, val);
                    Ok(JsValue::UNDEFINED)
                } else {
                    // Sync iterable wrapping: unwrap promise values per AsyncFromSyncIterator!
                    let vp = promise_resolve_value(vm, val);
                    let ss_e = Rc::clone(&ss_ok);
                    let on_val = vm.alloc_host_fn("from_sync_val", move |vm, _, a| {
                        let _ = readable_controller_enqueue(vm, &ss_e, stream_h, arg(a, 0));
                        Ok(JsValue::UNDEFINED)
                    });
                    let out_p = promise_then(vm, vp, Some(on_val), None);
                    Ok(JsValue::from_object(out_p))
                }
            });
            let out = promise_then(vm, next_res_p, Some(on_next), None);
            Ok(JsValue::from_object(out))
        });

    let cancel_fn = vm.alloc_host_fn_rooted("from_cancel", &[iterator_val], move |vm, _, args| {
        let reason = arg(args, 0);
        let ret_fn = get_method_checked(vm, it_h, "return")?;
        let Some(rf) = ret_fn else {
            return Ok(JsValue::UNDEFINED);
        };
        let p = call_returning_promise(vm, rf, iterator_val, &[reason]);
        let on_ok = vm.alloc_host_fn("from_ret_check", |vm, _, a| {
            if !arg(a, 0).is_object() {
                return Err(vm.type_error("Iterator return() result is not an object"));
            }
            Ok(JsValue::UNDEFINED)
        });
        let out = promise_then(vm, p, Some(on_ok), None);
        Ok(JsValue::from_object(out))
    });

    // Per WHATWG §4.2.5: highWaterMark = 0 so pull() runs lazily on first read!
    let (stream_h, _) = alloc_readable_stream_internal(
        vm,
        streams,
        0.0,
        None,
        Some(pull_fn),
        Some(cancel_fn),
        None,
        true,
    );
    stream_slot.set(Some(stream_h));
    Ok(stream_h)
}

// ---------------------------------------------------------------------------
// Queuing Strategies (WHATWG Streams §7)
// ---------------------------------------------------------------------------

fn install_queuing_strategies(vm: &mut JsVm, streams: &SharedStreams) {
    // CountQueuingStrategy
    let cqs_proto = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(cqs_proto).unwrap().is_prototype = true;
    set_to_string_tag(vm, cqs_proto, "CountQueuingStrategy");

    let count_size_fn = vm.alloc_host_fn("size", |_, _, _| Ok(JsValue::from_f64(1.0)));
    vm.set_fn_length(count_size_fn, 0);
    streams.borrow_mut().count_size_fn = Some(count_size_fn);

    let ss_h = Rc::clone(streams);
    define_host_getter(vm, cqs_proto, "highWaterMark", move |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Incompatible CountQueuingStrategy receiver"));
        };
        let hwm = ss_h
            .borrow()
            .count_strategies
            .get(&h)
            .copied()
            .ok_or_else(|| vm.type_error("Incompatible CountQueuingStrategy receiver"))?;
        Ok(JsValue::from_f64(hwm))
    });
    let ss_s = Rc::clone(streams);
    define_host_getter(vm, cqs_proto, "size", move |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Incompatible CountQueuingStrategy receiver"));
        };
        if !ss_s.borrow().count_strategies.contains_key(&h) {
            return Err(vm.type_error("Incompatible CountQueuingStrategy receiver"));
        }
        Ok(JsValue::from_object(count_size_fn))
    });

    let ss_c = Rc::clone(streams);
    let cqs_ctor = vm.alloc_host_fn("CountQueuingStrategy", move |vm, _, args| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("CountQueuingStrategy constructor requires 'new'"));
        }
        let init = arg(args, 0);
        let Some(ih) = init.as_object() else {
            return Err(vm.type_error("CountQueuingStrategy requires an init dictionary"));
        };
        let kh = vm.heap.intern_key("highWaterMark");
        let hv = vm.get_property_by_key(ih, kh)?;
        if hv.is_undefined() {
            return Err(vm.type_error("highWaterMark is required"));
        }
        let hwm = vm.to_number(hv)?;
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(cqs_proto)));
        ss_c.borrow_mut().count_strategies.insert(obj, hwm);
        Ok(JsValue::from_object(obj))
    });
    vm.set_fn_length(cqs_ctor, 1);
    bind_ctor_proto(vm, cqs_ctor, cqs_proto);
    define_global(vm, "CountQueuingStrategy", JsValue::from_object(cqs_ctor));

    // ByteLengthQueuingStrategy
    let blqs_proto = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(blqs_proto).unwrap().is_prototype = true;
    set_to_string_tag(vm, blqs_proto, "ByteLengthQueuingStrategy");

    let bl_size_fn = vm.alloc_host_fn("size", |vm, _, args| {
        let chunk = arg(args, 0);
        if chunk.is_null() || chunk.is_undefined() {
            return Err(vm.type_error("Cannot read property 'byteLength' of null or undefined"));
        }
        let Some(ch) = chunk.as_object() else {
            return Ok(JsValue::UNDEFINED);
        };
        let k = vm.heap.intern_key("byteLength");
        vm.get_property_by_key(ch, k)
    });
    vm.set_fn_length(bl_size_fn, 1);
    streams.borrow_mut().byte_length_size_fn = Some(bl_size_fn);

    let ss_bh = Rc::clone(streams);
    define_host_getter(vm, blqs_proto, "highWaterMark", move |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Incompatible ByteLengthQueuingStrategy receiver"));
        };
        let hwm = ss_bh
            .borrow()
            .byte_length_strategies
            .get(&h)
            .copied()
            .ok_or_else(|| vm.type_error("Incompatible ByteLengthQueuingStrategy receiver"))?;
        Ok(JsValue::from_f64(hwm))
    });
    let ss_bs = Rc::clone(streams);
    define_host_getter(vm, blqs_proto, "size", move |vm, this_val, _| {
        let Some(h) = this_val.as_object() else {
            return Err(vm.type_error("Incompatible ByteLengthQueuingStrategy receiver"));
        };
        if !ss_bs.borrow().byte_length_strategies.contains_key(&h) {
            return Err(vm.type_error("Incompatible ByteLengthQueuingStrategy receiver"));
        }
        Ok(JsValue::from_object(bl_size_fn))
    });

    let ss_bc = Rc::clone(streams);
    let blqs_ctor = vm.alloc_host_fn("ByteLengthQueuingStrategy", move |vm, _, args| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("ByteLengthQueuingStrategy constructor requires 'new'"));
        }
        let init = arg(args, 0);
        let Some(ih) = init.as_object() else {
            return Err(vm.type_error("ByteLengthQueuingStrategy requires an init dictionary"));
        };
        let kh = vm.heap.intern_key("highWaterMark");
        let hv = vm.get_property_by_key(ih, kh)?;
        if hv.is_undefined() {
            return Err(vm.type_error("highWaterMark is required"));
        }
        let hwm = vm.to_number(hv)?;
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(blqs_proto)));
        ss_bc.borrow_mut().byte_length_strategies.insert(obj, hwm);
        Ok(JsValue::from_object(obj))
    });
    vm.set_fn_length(blqs_ctor, 1);
    bind_ctor_proto(vm, blqs_ctor, blqs_proto);
    define_global(
        vm,
        "ByteLengthQueuingStrategy",
        JsValue::from_object(blqs_ctor),
    );
}

// ---------------------------------------------------------------------------
// Public Installation Entrypoint
// ---------------------------------------------------------------------------

/// Installs WHATWG Streams (`ReadableStream`, `ReadableStreamDefaultReader`,
/// `ReadableStreamDefaultController`, `WritableStream`, `WritableStreamDefaultWriter`,
/// `WritableStreamDefaultController`, `TransformStream`, `CountQueuingStrategy`,
/// `ByteLengthQueuingStrategy`, `TextDecoderStream`, `TextEncoderStream`,
/// and `Blob.prototype.stream()`).
pub fn install_streams(vm: &mut JsVm, web_apis: &SharedWebApis) -> SharedStreams {
    let streams = Rc::new(RefCell::new(StreamSystem::default()));

    // Save tamper-proof native `EventTarget.prototype.{add,remove}EventListener` for `pipeTo({ signal })`.
    if let Some(sig_proto) = web_apis.borrow().protos.abort_signal_proto {
        let k_add = vm.heap.intern_key("addEventListener");
        let k_rm = vm.heap.intern_key("removeEventListener");
        let add_fn = vm
            .get_property_by_key(sig_proto, k_add)
            .ok()
            .and_then(|v| v.as_object());
        let rm_fn = vm
            .get_property_by_key(sig_proto, k_rm)
            .ok()
            .and_then(|v| v.as_object());
        let mut st = streams.borrow_mut();
        st.et_add_listener_fn = add_fn;
        st.et_remove_listener_fn = rm_fn;
    }

    install_queuing_strategies(vm, &streams);

    // --- ReadableStreamDefaultController ---
    let rcp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(rcp).unwrap().is_prototype = true;
    set_to_string_tag(vm, rcp, "ReadableStreamDefaultController");
    streams.borrow_mut().readable_controller_proto = Some(rcp);

    let ss = Rc::clone(&streams);
    define_host_getter(vm, rcp, "desiredSize", move |vm, this_val, _| {
        let stream_h = require_readable_controller(vm, &ss, this_val)?;
        let ds = ss
            .borrow()
            .readable
            .get(&stream_h)
            .and_then(ReadableStreamState::desired_size);
        Ok(ds.map_or(JsValue::NULL, JsValue::from_f64))
    });
    let ss = Rc::clone(&streams);
    define_host_method(vm, rcp, "close", move |vm, this_val, _| {
        let stream_h = require_readable_controller(vm, &ss, this_val)?;
        readable_controller_close(vm, &ss, stream_h)?;
        Ok(JsValue::UNDEFINED)
    });
    let ss = Rc::clone(&streams);
    define_host_method(vm, rcp, "enqueue", move |vm, this_val, args| {
        let stream_h = require_readable_controller(vm, &ss, this_val)?;
        readable_controller_enqueue(vm, &ss, stream_h, arg(args, 0))?;
        Ok(JsValue::UNDEFINED)
    });
    let ss = Rc::clone(&streams);
    define_host_method(vm, rcp, "error", move |vm, this_val, args| {
        let stream_h = require_readable_controller(vm, &ss, this_val)?;
        readable_error_internal(vm, &ss, stream_h, arg(args, 0));
        Ok(JsValue::UNDEFINED)
    });
    let rc_ctor = vm.alloc_host_fn("ReadableStreamDefaultController", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: ReadableStreamDefaultController"))
    });
    bind_ctor_proto(vm, rc_ctor, rcp);
    define_global(
        vm,
        "ReadableStreamDefaultController",
        JsValue::from_object(rc_ctor),
    );

    // --- ReadableStreamDefaultReader ---
    let rrp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(rrp).unwrap().is_prototype = true;
    set_to_string_tag(vm, rrp, "ReadableStreamDefaultReader");
    streams.borrow_mut().readable_reader_proto = Some(rrp);

    let ss = Rc::clone(&streams);
    define_host_getter(vm, rrp, "closed", move |vm, this_val, _| {
        let Some(rh) = this_val.as_object() else {
            let err = vm.type_error("Incompatible ReadableStreamDefaultReader receiver");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        };
        let cp = ss
            .borrow()
            .readable_readers
            .get(&rh)
            .map(|r| r.closed_promise);
        match cp {
            Some(p) => Ok(JsValue::from_object(p)),
            None => {
                let err = vm.type_error("Incompatible ReadableStreamDefaultReader receiver");
                Ok(JsValue::from_object(promise_reject_value(vm, err.value)))
            }
        }
    });
    let ss = Rc::clone(&streams);
    define_host_method(vm, rrp, "read", move |vm, this_val, _| {
        let Some(rh) = this_val.as_object() else {
            let err = vm.type_error("Incompatible ReadableStreamDefaultReader receiver");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        };
        let stream_opt = match ss.borrow().readable_readers.get(&rh) {
            Some(r) => r.stream,
            None => {
                let err = vm.type_error("Incompatible ReadableStreamDefaultReader receiver");
                return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
            }
        };
        let Some(stream_h) = stream_opt else {
            let err = vm.type_error("Cannot read from a released ReadableStreamDefaultReader");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        };
        Ok(JsValue::from_object(readable_reader_read_internal(
            vm, &ss, stream_h,
        )))
    });
    let ss = Rc::clone(&streams);
    define_host_method(vm, rrp, "cancel", move |vm, this_val, args| {
        let Some(rh) = this_val.as_object() else {
            let err = vm.type_error("Incompatible ReadableStreamDefaultReader receiver");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        };
        let stream_opt = match ss.borrow().readable_readers.get(&rh) {
            Some(r) => r.stream,
            None => {
                let err = vm.type_error("Incompatible ReadableStreamDefaultReader receiver");
                return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
            }
        };
        let Some(stream_h) = stream_opt else {
            let err = vm.type_error("Cannot cancel a released ReadableStreamDefaultReader");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        };
        Ok(JsValue::from_object(readable_cancel_internal(
            vm,
            &ss,
            stream_h,
            arg(args, 0),
        )))
    });
    let ss = Rc::clone(&streams);
    define_host_method(vm, rrp, "releaseLock", move |vm, this_val, _| {
        let rh = require_readable_reader(vm, &ss, this_val)?;
        release_default_reader(vm, &ss, rh);
        Ok(JsValue::UNDEFINED)
    });

    let ss = Rc::clone(&streams);
    let rr_ctor = vm.alloc_host_fn("ReadableStreamDefaultReader", move |vm, _, args| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("ReadableStreamDefaultReader requires 'new'"));
        }
        let stream_h = require_readable(vm, &ss, arg(args, 0))?;
        let reader = acquire_default_reader(vm, &ss, stream_h)?;
        Ok(JsValue::from_object(reader))
    });
    vm.set_fn_length(rr_ctor, 1);
    bind_ctor_proto(vm, rr_ctor, rrp);
    define_global(
        vm,
        "ReadableStreamDefaultReader",
        JsValue::from_object(rr_ctor),
    );

    // --- ReadableStreamAsyncIterator Prototype ---
    let aip = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.async_iterator_proto),
    ));
    vm.heap.get_object_mut(aip).unwrap().is_prototype = true;
    streams.borrow_mut().async_iterator_proto = Some(aip);

    let ss = Rc::clone(&streams);
    let aiter_next_fn = vm.alloc_host_fn("next", move |vm, this_val, _| {
        let Some(ih) = this_val.as_object() else {
            let err = vm.type_error("Incompatible ReadableStreamAsyncIterator receiver");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        };
        let (finished, reader_h, ongoing) = {
            let st = ss.borrow();
            let Some(it) = st.async_iterators.get(&ih) else {
                let err = vm.type_error("Incompatible ReadableStreamAsyncIterator receiver");
                return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
            };
            (it.finished, it.reader, it.ongoing_promise)
        };
        let has_pending_ongoing = ongoing.filter(|&p| promise_is_pending(vm, p));
        if finished && has_pending_ongoing.is_none() {
            let res = make_read_result(vm, JsValue::UNDEFINED, true);
            return Ok(JsValue::from_object(promise_resolve_value(vm, res)));
        }
        let ss_step = Rc::clone(&ss);
        let run_step = move |vm: &mut JsVm| -> HeapHandle {
            let (is_fin, stream_opt) = {
                let st = ss_step.borrow();
                let fin = st.async_iterators.get(&ih).is_none_or(|i| i.finished);
                let s_opt = st.readable_readers.get(&reader_h).and_then(|r| r.stream);
                (fin, s_opt)
            };
            if is_fin {
                let res = make_read_result(vm, JsValue::UNDEFINED, true);
                return promise_resolve_value(vm, res);
            }
            let Some(stream_h) = stream_opt else {
                if let Some(it) = ss_step.borrow_mut().async_iterators.get_mut(&ih) {
                    it.finished = true;
                }
                let res = make_read_result(vm, JsValue::UNDEFINED, true);
                return promise_resolve_value(vm, res);
            };
            let rp = readable_reader_read_internal(vm, &ss_step, stream_h);
            let ss_done = Rc::clone(&ss_step);
            let on_ok = vm.alloc_host_fn("rs_aiter_ok", move |vm, _, args| {
                let res = arg(args, 0);
                let done = res.as_object().is_some_and(|rh| {
                    let k = vm.heap.intern_key("done");
                    vm.get_property_by_key(rh, k)
                        .is_ok_and(|v| vm.to_boolean(v))
                });
                if done {
                    if let Some(it) = ss_done.borrow_mut().async_iterators.get_mut(&ih) {
                        it.finished = true;
                        it.ongoing_promise = None;
                    }
                    release_default_reader(vm, &ss_done, reader_h);
                }
                Ok(res)
            });
            let ss_err = Rc::clone(&ss_step);
            let on_err = vm.alloc_host_fn("rs_aiter_err", move |vm, _, args| {
                if let Some(it) = ss_err.borrow_mut().async_iterators.get_mut(&ih) {
                    it.finished = true;
                    it.ongoing_promise = None;
                }
                release_default_reader(vm, &ss_err, reader_h);
                let err = arg(args, 0);
                let msg = vm.value_to_rust_string(err);
                Err(JsException {
                    value: err,
                    message: msg,
                })
            });
            promise_then(vm, rp, Some(on_ok), Some(on_err))
        };
        let next_p = if let Some(prev) = has_pending_ongoing {
            let on_prev = vm.alloc_host_fn("rs_aiter_chain", move |vm, _, _| {
                Ok(JsValue::from_object(run_step(vm)))
            });
            promise_then(vm, prev, Some(on_prev), Some(on_prev))
        } else {
            run_step(vm)
        };
        if promise_is_pending(vm, next_p) {
            if let Some(it) = ss.borrow_mut().async_iterators.get_mut(&ih) {
                it.ongoing_promise = Some(next_p);
            }
        }
        Ok(JsValue::from_object(next_p))
    });
    vm.set_fn_length(aiter_next_fn, 0);
    let k_next = vm.heap.intern_key("next");
    vm.heap.object_define_data(
        aip,
        k_next,
        JsValue::from_object(aiter_next_fn),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );

    let ss = Rc::clone(&streams);
    let aiter_ret_fn = vm.alloc_host_fn("return", move |vm, this_val, args| {
        let value = arg(args, 0);
        let Some(ih) = this_val.as_object() else {
            let err = vm.type_error("Incompatible ReadableStreamAsyncIterator receiver");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        };
        let (finished, reader_h, prevent_cancel, ongoing) = {
            let st = ss.borrow();
            let Some(it) = st.async_iterators.get(&ih) else {
                let err = vm.type_error("Incompatible ReadableStreamAsyncIterator receiver");
                return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
            };
            (
                it.finished,
                it.reader,
                it.prevent_cancel,
                it.ongoing_promise,
            )
        };
        let has_pending_ongoing = ongoing.filter(|&p| promise_is_pending(vm, p));
        if finished && has_pending_ongoing.is_none() {
            let res = make_read_result(vm, value, true);
            return Ok(JsValue::from_object(promise_resolve_value(vm, res)));
        }
        let ss_ret = Rc::clone(&ss);
        let do_return = move |vm: &mut JsVm| -> HeapHandle {
            let already = ss_ret
                .borrow()
                .async_iterators
                .get(&ih)
                .is_none_or(|i| i.finished);
            if already {
                let res = make_read_result(vm, value, true);
                return promise_resolve_value(vm, res);
            }
            if let Some(it) = ss_ret.borrow_mut().async_iterators.get_mut(&ih) {
                it.finished = true;
                it.ongoing_promise = None;
            }
            let stream_opt = ss_ret
                .borrow()
                .readable_readers
                .get(&reader_h)
                .and_then(|r| r.stream);
            if !prevent_cancel {
                if let Some(stream_h) = stream_opt {
                    let cp = readable_cancel_internal(vm, &ss_ret, stream_h, value);
                    release_default_reader(vm, &ss_ret, reader_h);
                    let on_ok = vm.alloc_host_fn("rs_aiter_ret_ok", move |vm, _, _| {
                        Ok(make_read_result(vm, value, true))
                    });
                    return promise_then(vm, cp, Some(on_ok), None);
                }
            }
            release_default_reader(vm, &ss_ret, reader_h);
            let res = make_read_result(vm, value, true);
            promise_resolve_value(vm, res)
        };
        let ret_p = if let Some(prev) = has_pending_ongoing {
            let on_prev = vm.alloc_host_fn("rs_aiter_ret_chain", move |vm, _, _| {
                Ok(JsValue::from_object(do_return(vm)))
            });
            promise_then(vm, prev, Some(on_prev), Some(on_prev))
        } else {
            do_return(vm)
        };
        if promise_is_pending(vm, ret_p) {
            if let Some(it) = ss.borrow_mut().async_iterators.get_mut(&ih) {
                it.ongoing_promise = Some(ret_p);
            }
        }
        Ok(JsValue::from_object(ret_p))
    });
    vm.set_fn_length(aiter_ret_fn, 1);
    let k_ret = vm.heap.intern_key("return");
    vm.heap.object_define_data(
        aip,
        k_ret,
        JsValue::from_object(aiter_ret_fn),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );

    // --- ReadableStream ---
    let rsp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(rsp).unwrap().is_prototype = true;
    set_to_string_tag(vm, rsp, "ReadableStream");
    streams.borrow_mut().readable_proto = Some(rsp);

    let ss = Rc::clone(&streams);
    define_host_getter(vm, rsp, "locked", move |vm, this_val, _| {
        let h = require_readable(vm, &ss, this_val)?;
        let locked = ss
            .borrow()
            .readable
            .get(&h)
            .is_some_and(|rs| rs.reader.is_some());
        Ok(JsValue::from_bool(locked))
    });

    let ss = Rc::clone(&streams);
    define_host_method(vm, rsp, "getReader", move |vm, this_val, args| {
        let h = require_readable(vm, &ss, this_val)?;
        let opts = arg(args, 0);
        if !opts.is_undefined() && !opts.is_null() {
            let Some(oh) = opts.as_object() else {
                return Err(vm.type_error("getReader options must be an object"));
            };
            let km = vm.heap.intern_key("mode");
            let mv = vm.get_property_by_key(oh, km)?;
            if !mv.is_undefined() {
                let s = vm.to_js_string(mv)?;
                if s == "byob" {
                    let reader = acquire_byob_reader(vm, &ss, h)?;
                    return Ok(JsValue::from_object(reader));
                } else {
                    return Err(vm.type_error(format!("Invalid ReadableStreamReaderMode '{s}'")));
                }
            }
        }
        let reader = acquire_default_reader(vm, &ss, h)?;
        Ok(JsValue::from_object(reader))
    });

    let ss = Rc::clone(&streams);
    define_host_method(vm, rsp, "cancel", move |vm, this_val, args| {
        let h = match require_readable(vm, &ss, this_val) {
            Ok(h) => h,
            Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
        };
        let is_locked = ss
            .borrow()
            .readable
            .get(&h)
            .is_some_and(|rs| rs.reader.is_some());
        if is_locked {
            let err = vm.type_error("Cannot cancel a locked ReadableStream");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        }
        Ok(JsValue::from_object(readable_cancel_internal(
            vm,
            &ss,
            h,
            arg(args, 0),
        )))
    });

    let ss = Rc::clone(&streams);
    let wa_pt = Rc::clone(web_apis);
    define_host_method(vm, rsp, "pipeTo", move |vm, this_val, args| {
        Ok(JsValue::from_object(readable_pipe_to_method(
            vm,
            &ss,
            &wa_pt,
            this_val,
            arg(args, 0),
            arg(args, 1),
        )))
    });

    let ss = Rc::clone(&streams);
    let wa_pth = Rc::clone(web_apis);
    define_host_method(vm, rsp, "pipeThrough", move |vm, this_val, args| {
        readable_pipe_through(vm, &ss, &wa_pth, this_val, arg(args, 0), arg(args, 1))
    });

    let ss = Rc::clone(&streams);
    define_host_method(vm, rsp, "tee", move |vm, this_val, _| {
        let src_h = require_readable(vm, &ss, this_val)?;
        let is_byte = ss
            .borrow()
            .readable
            .get(&src_h)
            .is_some_and(|rs| rs.is_byte_stream);
        let arr = if is_byte {
            readable_byte_stream_tee(vm, &ss, src_h)?
        } else {
            readable_stream_default_tee(vm, &ss, None, src_h)?
        };
        Ok(JsValue::from_object(arr))
    });

    let ss = Rc::clone(&streams);
    let values_fn = vm.alloc_host_fn("values", move |vm, this_val, args| {
        let stream_h = require_readable(vm, &ss, this_val)?;
        let mut prevent_cancel = false;
        let opts = arg(args, 0);
        if !opts.is_undefined() && !opts.is_null() {
            let Some(oh) = opts.as_object() else {
                return Err(vm.type_error("values() options must be an object"));
            };
            let kp = vm.heap.intern_key("preventCancel");
            let pv = vm.get_property_by_key(oh, kp)?;
            prevent_cancel = vm.to_boolean(pv);
        }
        let reader_h = acquire_default_reader(vm, &ss, stream_h)?;
        let iter = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(aip)));
        ss.borrow_mut().async_iterators.insert(
            iter,
            AsyncIteratorState {
                reader: reader_h,
                prevent_cancel,
                finished: false,
                ongoing_promise: None,
            },
        );
        attach_gc_anchor(vm, iter, reader_h);
        Ok(JsValue::from_object(iter))
    });
    let k_values = vm.heap.intern_key("values");
    vm.heap.object_define_data(
        rsp,
        k_values,
        JsValue::from_object(values_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    let k_sym_aiter = vm.key_symbol_async_iterator;
    vm.heap.object_define_data(
        rsp,
        k_sym_aiter,
        JsValue::from_object(values_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );

    let ss = Rc::clone(&streams);
    let rs_ctor = vm.alloc_host_fn("ReadableStream", move |vm, _, args| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("ReadableStream constructor requires 'new'"));
        }
        let src_val = arg(args, 0);
        if !src_val.is_undefined() && !src_val.is_object() {
            return Err(vm.type_error("ReadableStream underlyingSource must be an object"));
        }
        // WebIDL converts `strategy` argument BEFORE `underlyingSource` dictionary conversion.
        let (hwm_val_opt, size_fn) = extract_strategy(vm, arg(args, 1))?;

        let mut start_fn = None;
        let mut pull_fn = None;
        let mut cancel_fn = None;
        let mut is_byte_stream = false;
        let mut auto_alloc_size = None;
        let source_obj = src_val.as_object();
        if let Some(sh) = source_obj {
            // Read `UnderlyingSource` dictionary members.
            let k_type = vm.heap.intern_key("type");
            let type_v = vm.get_property_by_key(sh, k_type)?;
            if !type_v.is_undefined() {
                let s = vm.to_js_string(type_v)?;
                if s == "bytes" {
                    is_byte_stream = true;
                } else {
                    return Err(vm.type_error(format!("Invalid ReadableStream type '{s}'")));
                }
            }
            if is_byte_stream {
                let k_aac = vm.heap.intern_key("autoAllocateChunkSize");
                let aac_v = vm.get_property_by_key(sh, k_aac)?;
                if !aac_v.is_undefined() {
                    auto_alloc_size = Some(parse_positive_u64_enforce_range(vm, aac_v)?);
                }
            }
            start_fn = get_method_checked(vm, sh, "start")?;
            pull_fn = get_method_checked(vm, sh, "pull")?;
            cancel_fn = get_method_checked(vm, sh, "cancel")?;
        }

        if is_byte_stream && size_fn.is_some() {
            return Err(
                vm.range_error("ReadableStream with type 'bytes' cannot specify a size algorithm")
            );
        }
        let default_hwm = if is_byte_stream { 0.0 } else { 1.0 };
        let hwm = match hwm_val_opt {
            Some(v) => validate_and_normalize_hwm(vm, v)?,
            None => default_hwm,
        };

        let (obj, ctrl) = if is_byte_stream {
            alloc_byte_readable_stream(
                vm,
                &ss,
                hwm,
                auto_alloc_size,
                pull_fn,
                cancel_fn,
                source_obj,
                false,
            )
        } else {
            alloc_readable_stream_internal(
                vm, &ss, hwm, size_fn, pull_fn, cancel_fn, source_obj, false,
            )
        };

        let start_res = if let Some(sf) = start_fn {
            vm.call_value(
                JsValue::from_object(sf),
                src_val,
                &[JsValue::from_object(ctrl)],
            )?
        } else {
            JsValue::UNDEFINED
        };
        let start_p = promise_resolve_value(vm, start_res);
        let ss_ok = Rc::clone(&ss);
        let on_ok = vm.alloc_host_fn("rs_start_ok", move |vm, _, _| {
            if let Some(rs) = ss_ok.borrow_mut().readable.get_mut(&obj) {
                rs.started = true;
            }
            if is_byte_stream {
                byte_call_pull_if_needed(vm, &ss_ok, obj);
            } else {
                readable_call_pull_if_needed(vm, &ss_ok, obj);
            }
            Ok(JsValue::UNDEFINED)
        });
        let ss_err = Rc::clone(&ss);
        let on_err = vm.alloc_host_fn("rs_start_err", move |vm, _, a| {
            readable_error_internal(vm, &ss_err, obj, arg(a, 0));
            Ok(JsValue::UNDEFINED)
        });
        let _ = promise_then(vm, start_p, Some(on_ok), Some(on_err));
        Ok(JsValue::from_object(obj))
    });
    let ss_from = Rc::clone(&streams);
    define_host_method(vm, rs_ctor, "from", move |vm, _, args| {
        let stream_h = readable_stream_from(vm, &ss_from, arg(args, 0))?;
        Ok(JsValue::from_object(stream_h))
    });
    bind_ctor_proto(vm, rs_ctor, rsp);
    define_global(vm, "ReadableStream", JsValue::from_object(rs_ctor));

    // --- ReadableByteStreamController + ReadableStreamBYOBReader + ReadableStreamBYOBRequest ---
    install_byte_stream_types(vm, &streams);

    // --- WritableStream + WritableStreamDefaultWriter + WritableStreamDefaultController ---
    let wsp = install_writable_stream_types(vm, &streams, web_apis);

    // --- TransformStream + TransformStreamDefaultController ---
    install_transform_stream_types(vm, &streams, web_apis);

    // --- TextDecoderStream & TextEncoderStream (WHATWG Encoding §8.5) ---
    install_text_encoding_streams(vm, &streams, web_apis, rsp, wsp);

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

// ---------------------------------------------------------------------------
// Brand Checks & Pipe Helpers
// ---------------------------------------------------------------------------

pub(crate) fn require_readable(
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

fn require_readable_controller(
    vm: &mut JsVm,
    streams: &SharedStreams,
    val: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible ReadableStreamDefaultController receiver"));
    };
    streams
        .borrow()
        .readable_controllers
        .get(&h)
        .copied()
        .ok_or_else(|| vm.type_error("Incompatible ReadableStreamDefaultController receiver"))
}

fn require_readable_reader(
    vm: &mut JsVm,
    streams: &SharedStreams,
    val: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible ReadableStreamDefaultReader receiver"));
    };
    if streams.borrow().readable_readers.contains_key(&h) {
        Ok(h)
    } else {
        Err(vm.type_error("Incompatible ReadableStreamDefaultReader receiver"))
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

// ---------------------------------------------------------------------------
// TextDecoderStream & TextEncoderStream (WHATWG Encoding §8.5)
// ---------------------------------------------------------------------------

fn install_text_encoding_streams(
    vm: &mut JsVm,
    streams: &SharedStreams,
    web_apis: &SharedWebApis,
    _rsp: HeapHandle,
    _wsp: HeapHandle,
) {
    // TextDecoderStream
    let tdsp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(tdsp).unwrap().is_prototype = true;
    set_to_string_tag(vm, tdsp, "TextDecoderStream");
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
        let (readable, _) =
            alloc_readable_stream_internal(vm, &ss_d, 1.0, None, None, None, None, true);

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
                let _ = readable_controller_enqueue(vm, &ss_w, readable, s);
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
                let _ = readable_controller_enqueue(vm, &ss_c, readable, s);
            }
            let _ = readable_controller_close(vm, &ss_c, readable);
            Ok(JsValue::UNDEFINED)
        });
        let ss_ab = Rc::clone(&ss_d);
        let abort_fn = vm.alloc_host_fn("tds_abort", move |vm, _, a| {
            readable_error_internal(vm, &ss_ab, readable, arg(a, 0));
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
    set_to_string_tag(vm, tesp, "TextEncoderStream");
    let ss_e = Rc::clone(streams);
    let tes_ctor = vm.alloc_host_fn("TextEncoderStream", move |vm, _, _| {
        let (readable, _) =
            alloc_readable_stream_internal(vm, &ss_e, 1.0, None, None, None, None, true);

        let ss_w = Rc::clone(&ss_e);
        let write_fn = vm.alloc_host_fn("tes_write", move |vm, _, args| {
            let text = vm.to_js_string(arg(args, 0))?;
            let bytes = text.into_bytes();
            if !bytes.is_empty() {
                let u8arr = alloc_uint8_array(vm, bytes);
                let _ =
                    readable_controller_enqueue(vm, &ss_w, readable, JsValue::from_object(u8arr));
            }
            Ok(JsValue::UNDEFINED)
        });
        let ss_c = Rc::clone(&ss_e);
        let close_fn = vm.alloc_host_fn("tes_close", move |vm, _, _| {
            let _ = readable_controller_close(vm, &ss_c, readable);
            Ok(JsValue::UNDEFINED)
        });
        let ss_ab = Rc::clone(&ss_e);
        let abort_fn = vm.alloc_host_fn("tes_abort", move |vm, _, a| {
            readable_error_internal(vm, &ss_ab, readable, arg(a, 0));
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

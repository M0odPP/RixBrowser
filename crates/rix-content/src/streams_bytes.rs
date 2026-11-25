//! WHATWG Streams Standard §4.3 / §4.4 / §4.6 / §4.7:
//! `ReadableByteStreamController`, `ReadableStreamBYOBReader`,
//! `ReadableStreamBYOBRequest`, `autoAllocateChunkSize`, `read(view, { min })`,
//! `TransferArrayBuffer` detachment, and `ReadableByteStreamTee`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use rix_js::heap::{DataViewObject, TypedArrayKind, TypedArrayObject};
use rix_js::promise::{
    alloc_promise, promise_resolve_value, promise_then, reject_promise, resolve_promise,
};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind};

use crate::streams::{
    close_read_request, error_read_request, fulfill_read_request, make_read_result,
    require_readable, ChunkCallback, ErrorCallback, ReadRequest, ReadableState,
    ReadableStreamState, SharedStreams,
};
use crate::streams_common::{
    attach_gc_anchor, call_returning_promise, promise_is_pending, promise_mark_handled,
    promise_reject_value, set_to_string_tag,
};
use crate::web_apis::{
    arg, bind_ctor_proto, define_global, define_host_getter, define_host_method,
};

// ---------------------------------------------------------------------------
// Byte Stream State Structures (WHATWG Streams §4.4 / §4.6 / §4.7)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PullIntoViewKind {
    TypedArray(TypedArrayKind),
    DataView,
}

impl PullIntoViewKind {
    fn element_size(self) -> usize {
        match self {
            Self::TypedArray(k) => k.bytes_per_element(),
            Self::DataView => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PullIntoReaderType {
    Default,
    Byob,
    None,
}

#[derive(Debug, Clone)]
pub(crate) struct ByteQueueEntry {
    pub(crate) buffer: HeapHandle,
    pub(crate) byte_offset: usize,
    pub(crate) byte_length: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct PullIntoDescriptor {
    pub(crate) buffer: HeapHandle,
    pub(crate) buffer_byte_length: usize,
    pub(crate) byte_offset: usize,
    pub(crate) byte_length: usize,
    pub(crate) bytes_filled: usize,
    pub(crate) minimum_fill: usize,
    pub(crate) element_size: usize,
    pub(crate) view_kind: PullIntoViewKind,
    pub(crate) reader_type: PullIntoReaderType,
}

#[derive(Clone)]
pub(crate) enum ReadIntoRequest {
    Promise(HeapHandle),
    Callback {
        on_chunk: ChunkCallback,
        on_close: ChunkCallback,
        on_error: ErrorCallback,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct ByobRequestState {
    pub(crate) stream: Option<HeapHandle>,
    pub(crate) view: Option<HeapHandle>,
}

#[derive(Debug, Clone)]
pub(crate) struct ReadableByobReaderState {
    pub(crate) stream: Option<HeapHandle>,
    pub(crate) closed_promise: HeapHandle,
}

#[derive(Clone, Default)]
pub(crate) struct ByteControllerState {
    pub(crate) auto_allocate_chunk_size: Option<usize>,
    pub(crate) byob_request: Option<HeapHandle>,
    pub(crate) queue: VecDeque<ByteQueueEntry>,
    pub(crate) queue_total_size: usize,
    pub(crate) pending_pull_intos: VecDeque<PullIntoDescriptor>,
    pub(crate) read_into_requests: VecDeque<ReadIntoRequest>,
}

impl ByteControllerState {
    pub(crate) fn new(auto_allocate_chunk_size: Option<usize>) -> Self {
        Self {
            auto_allocate_chunk_size,
            byob_request: None,
            queue: VecDeque::new(),
            queue_total_size: 0,
            pending_pull_intos: VecDeque::new(),
            read_into_requests: VecDeque::new(),
        }
    }

    pub(crate) fn reset_queue(&mut self) {
        self.queue.clear();
        self.queue_total_size = 0;
    }

    pub(crate) fn collect_roots(&self, out: &mut Vec<JsValue>) {
        if let Some(br) = self.byob_request {
            out.push(JsValue::from_object(br));
        }
        for e in &self.queue {
            out.push(JsValue::from_object(e.buffer));
        }
        for d in &self.pending_pull_intos {
            out.push(JsValue::from_object(d.buffer));
        }
        for r in &self.read_into_requests {
            if let ReadIntoRequest::Promise(p) = r {
                out.push(JsValue::from_object(*p));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// WebIDL Integer & ArrayBuffer / ArrayBufferView Helpers
// ---------------------------------------------------------------------------

/// WebIDL `[EnforceRange] unsigned long long` (allowing 0).
pub(crate) fn parse_non_negative_u64_enforce_range(
    vm: &mut JsVm,
    val: JsValue,
) -> Result<usize, JsException> {
    let n = vm.to_number(val)?;
    if !n.is_finite() {
        return Err(vm.type_error("Value must be a finite non-negative integer"));
    }
    let trunc = n.trunc();
    if !(0.0..=9_007_199_254_740_991.0).contains(&trunc) {
        return Err(vm.type_error("Value is outside [EnforceRange] unsigned long long bounds"));
    }
    Ok(trunc as usize)
}

/// WebIDL `[EnforceRange] unsigned long long` requiring strictly positive (`>= 1`).
pub(crate) fn parse_positive_u64_enforce_range(
    vm: &mut JsVm,
    val: JsValue,
) -> Result<usize, JsException> {
    let n = parse_non_negative_u64_enforce_range(vm, val)?;
    if n == 0 {
        return Err(vm.type_error("Value must be a positive integer"));
    }
    Ok(n)
}

pub(crate) struct ViewInfo {
    pub(crate) buffer: HeapHandle,
    pub(crate) buffer_byte_length: usize,
    pub(crate) byte_offset: usize,
    pub(crate) byte_length: usize,
    pub(crate) element_size: usize,
    pub(crate) view_kind: PullIntoViewKind,
    pub(crate) detached: bool,
}

fn ab_byte_length_and_detached(vm: &JsVm, ab_h: HeapHandle) -> (usize, bool) {
    let Some(o) = vm.heap.get_object(ab_h) else {
        return (0, true);
    };
    if let Some(r) = o.rare.as_deref() {
        if r.ab_detached {
            return (0, true);
        }
        if let Some(ref sh) = r.shared_mem {
            let len = r.ab_fixed_byte_len.unwrap_or_else(|| sh.byte_len());
            return (len, false);
        }
    }
    if let ObjectKind::ArrayBuffer(buf) = &o.kind {
        (buf.len(), false)
    } else {
        (0, true)
    }
}

fn ab_is_non_transferable(vm: &JsVm, ab_h: HeapHandle) -> bool {
    vm.heap.get_object(ab_h).is_none_or(|o| {
        !matches!(o.kind, ObjectKind::ArrayBuffer(_))
            || o.rare.as_deref().is_some_and(|r| {
                r.ab_detached || r.ab_immutable || r.ab_wasm_memory || r.shared_mem.is_some()
            })
    })
}

fn extract_view_info(vm: &mut JsVm, val: JsValue) -> Result<ViewInfo, JsException> {
    let Some(vh) = val.as_object() else {
        return Err(vm.type_error("Expected an ArrayBufferView"));
    };
    let raw = vm.heap.get_object(vh).map(|o| {
        let len_tracking = o.rare.as_deref().is_some_and(|r| r.view_auto_length);
        match &o.kind {
            ObjectKind::TypedArray(ta) => Some((
                ta.buffer,
                ta.byte_offset as usize,
                (ta.length as usize) * ta.kind.bytes_per_element(),
                PullIntoViewKind::TypedArray(ta.kind),
                len_tracking,
            )),
            ObjectKind::DataView(dv) => Some((
                dv.buffer,
                dv.byte_offset as usize,
                dv.byte_length as usize,
                PullIntoViewKind::DataView,
                len_tracking,
            )),
            _ => None,
        }
    });
    let Some(Some((buffer, byte_offset, stored_byte_len, view_kind, len_tracking))) = raw else {
        return Err(vm.type_error("Expected an ArrayBufferView"));
    };
    let (buf_len, detached) = ab_byte_length_and_detached(vm, buffer);
    let element_size = view_kind.element_size();
    let byte_length = if detached {
        0
    } else if len_tracking {
        if byte_offset > buf_len {
            0
        } else {
            let rem = buf_len - byte_offset;
            rem - (rem % element_size)
        }
    } else if byte_offset.saturating_add(stored_byte_len) > buf_len {
        0
    } else {
        stored_byte_len
    };
    Ok(ViewInfo {
        buffer,
        buffer_byte_length: buf_len,
        byte_offset: if detached { 0 } else { byte_offset },
        byte_length,
        element_size,
        view_kind,
        detached,
    })
}

/// Transfers an `ArrayBuffer` per WHATWG Streams `TransferArrayBuffer`:
/// detaches `ab_h` and returns a fresh `ArrayBuffer` owning the moved bytes.
pub(crate) fn transfer_array_buffer(
    vm: &mut JsVm,
    ab_h: HeapHandle,
) -> Result<HeapHandle, JsException> {
    if ab_is_non_transferable(vm, ab_h) {
        return Err(vm.type_error("ArrayBuffer is detached or non-transferable"));
    }
    let (bytes, max_len) = {
        let Some(o) = vm.heap.get_object_mut(ab_h) else {
            return Err(vm.type_error("Invalid ArrayBuffer"));
        };
        let max_len = o.rare.as_deref().and_then(|r| r.ab_max_byte_length);
        let bytes = if let ObjectKind::ArrayBuffer(buf) = &mut o.kind {
            std::mem::take(buf.as_mut())
        } else {
            return Err(vm.type_error("Invalid ArrayBuffer"));
        };
        o.rare_mut().ab_detached = true;
        (bytes, max_len)
    };
    let new_ab = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::ArrayBuffer(Box::new(bytes)),
        Some(vm.array_buffer_proto),
    ));
    if let Some(ml) = max_len {
        if let Some(o) = vm.heap.get_object_mut(new_ab) {
            o.rare_mut().ab_max_byte_length = Some(ml);
        }
    }
    Ok(new_ab)
}

fn alloc_array_buffer(vm: &mut JsVm, bytes: Vec<u8>) -> HeapHandle {
    vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::ArrayBuffer(Box::new(bytes)),
        Some(vm.array_buffer_proto),
    ))
}

fn alloc_view(
    vm: &mut JsVm,
    kind: PullIntoViewKind,
    buf_h: HeapHandle,
    byte_offset: usize,
    byte_length: usize,
) -> HeapHandle {
    match kind {
        PullIntoViewKind::TypedArray(tk) => {
            let elem = tk.bytes_per_element();
            let length = (byte_length / elem) as u32;
            vm.heap.alloc_object(JsObject::with_kind(
                ObjectKind::TypedArray(TypedArrayObject {
                    kind: tk,
                    buffer: buf_h,
                    byte_offset: byte_offset as u32,
                    length,
                }),
                Some(vm.typed_array_protos[tk as usize]),
            ))
        }
        PullIntoViewKind::DataView => vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::DataView(DataViewObject {
                buffer: buf_h,
                byte_offset: byte_offset as u32,
                byte_length: byte_length as u32,
            }),
            Some(vm.data_view_proto),
        )),
    }
}

fn copy_ab_slice(vm: &JsVm, ab_h: HeapHandle, offset: usize, len: usize) -> Vec<u8> {
    if len == 0 {
        return Vec::new();
    }
    let Some(o) = vm.heap.get_object(ab_h) else {
        return vec![0u8; len];
    };
    if let ObjectKind::ArrayBuffer(buf) = &o.kind {
        let end = offset.saturating_add(len).min(buf.len());
        if offset < end {
            return buf[offset..end].to_vec();
        }
    }
    vec![0u8; len]
}

fn write_ab_slice(vm: &mut JsVm, dst_ab: HeapHandle, dst_offset: usize, src: &[u8]) {
    if src.is_empty() {
        return;
    }
    if let Some(o) = vm.heap.get_object_mut(dst_ab) {
        if let ObjectKind::ArrayBuffer(buf) = &mut o.kind {
            let end = dst_offset.saturating_add(src.len()).min(buf.len());
            if dst_offset < end {
                let n = end - dst_offset;
                buf[dst_offset..end].copy_from_slice(&src[..n]);
            }
        }
    }
}

fn clone_as_uint8_array(vm: &mut JsVm, buf_h: HeapHandle, offset: usize, len: usize) -> HeapHandle {
    let bytes = copy_ab_slice(vm, buf_h, offset, len);
    let n = bytes.len();
    let new_ab = alloc_array_buffer(vm, bytes);
    alloc_view(
        vm,
        PullIntoViewKind::TypedArray(TypedArrayKind::Uint8),
        new_ab,
        0,
        n,
    )
}

// ---------------------------------------------------------------------------
// ReadIntoRequest Settlement Helpers
// ---------------------------------------------------------------------------

fn fulfill_read_into_request(vm: &mut JsVm, req: ReadIntoRequest, chunk: JsValue) {
    match req {
        ReadIntoRequest::Promise(p) => {
            let res = make_read_result(vm, chunk, false);
            resolve_promise(vm, p, res);
        }
        ReadIntoRequest::Callback { on_chunk, .. } => {
            if let Some(cb) = on_chunk.borrow_mut().take() {
                cb(vm, chunk);
            }
        }
    }
}

fn close_read_into_request(vm: &mut JsVm, req: ReadIntoRequest, chunk: JsValue) {
    match req {
        ReadIntoRequest::Promise(p) => {
            let res = make_read_result(vm, chunk, true);
            resolve_promise(vm, p, res);
        }
        ReadIntoRequest::Callback { on_close, .. } => {
            if let Some(cb) = on_close.borrow_mut().take() {
                cb(vm, chunk);
            }
        }
    }
}

fn error_read_into_request(vm: &mut JsVm, req: ReadIntoRequest, err: JsValue) {
    match req {
        ReadIntoRequest::Promise(p) => reject_promise(vm, p, err),
        ReadIntoRequest::Callback { on_error, .. } => {
            if let Some(cb) = on_error.borrow_mut().take() {
                cb(vm, err);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Core Byte Stream Controller Operations (WHATWG Streams §4.6.4)
// ---------------------------------------------------------------------------

pub(crate) fn byte_desired_size(rs: &ReadableStreamState) -> Option<f64> {
    match rs.state {
        ReadableState::Errored => None,
        ReadableState::Closed => Some(0.0),
        ReadableState::Readable => {
            let q = rs
                .byte_controller
                .as_ref()
                .map_or(0, |bc| bc.queue_total_size);
            Some(rs.hwm - (q as f64))
        }
    }
}

fn byte_should_call_pull(rs: &ReadableStreamState) -> bool {
    if rs.state != ReadableState::Readable {
        return false;
    }
    if rs.close_requested || !rs.started {
        return false;
    }
    let Some(ref bc) = rs.byte_controller else {
        return false;
    };
    if rs.reader.is_some() && !rs.reader_is_byob && !rs.read_requests.is_empty() {
        return true;
    }
    if rs.reader.is_some() && rs.reader_is_byob && !bc.read_into_requests.is_empty() {
        return true;
    }
    byte_desired_size(rs).is_some_and(|d| d > 0.0)
}

pub(crate) fn byte_call_pull_if_needed(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
) {
    let pull_call = {
        let mut st = streams.borrow_mut();
        let Some(rs) = st.readable.get_mut(&stream_h) else {
            return;
        };
        if !byte_should_call_pull(rs) {
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
    let on_ok = vm.alloc_host_fn("rbs_pull_ok", move |vm, _, _| {
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
            byte_call_pull_if_needed(vm, &ss_ok, stream_h);
        }
        Ok(JsValue::UNDEFINED)
    });
    let ss_err = Rc::clone(streams);
    let on_err = vm.alloc_host_fn("rbs_pull_err", move |vm, _, a| {
        byte_controller_error(vm, &ss_err, stream_h, arg(a, 0));
        Ok(JsValue::UNDEFINED)
    });
    let _ = promise_then(vm, pull_p, Some(on_ok), Some(on_err));
}

/// WHATWG §4.6.4 `ReadableByteStreamControllerInvalidateBYOBRequest`.
fn invalidate_byob_request(streams: &SharedStreams, stream_h: HeapHandle) {
    let mut reg = streams.borrow_mut();
    let br_opt = reg
        .readable
        .get_mut(&stream_h)
        .and_then(|rs| rs.byte_controller.as_mut())
        .and_then(|bc| bc.byob_request.take());
    if let Some(br_h) = br_opt {
        if let Some(br) = reg.byob_requests.get_mut(&br_h) {
            br.stream = None;
            br.view = None;
        }
    }
}

/// WHATWG §4.6.4 `ReadableByteStreamControllerShiftPendingPullInto`.
fn shift_pending_pull_into(
    streams: &SharedStreams,
    stream_h: HeapHandle,
) -> Option<PullIntoDescriptor> {
    invalidate_byob_request(streams, stream_h);
    streams
        .borrow_mut()
        .readable
        .get_mut(&stream_h)
        .and_then(|rs| rs.byte_controller.as_mut())
        .and_then(|bc| bc.pending_pull_intos.pop_front())
}

fn enqueue_chunk_to_queue(
    streams: &SharedStreams,
    stream_h: HeapHandle,
    buffer: HeapHandle,
    byte_offset: usize,
    byte_length: usize,
) {
    if let Some(bc) = streams
        .borrow_mut()
        .readable
        .get_mut(&stream_h)
        .and_then(|rs| rs.byte_controller.as_mut())
    {
        bc.queue.push_back(ByteQueueEntry {
            buffer,
            byte_offset,
            byte_length,
        });
        bc.queue_total_size = bc.queue_total_size.saturating_add(byte_length);
    }
}

fn enqueue_cloned_chunk_to_queue(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
    buffer: HeapHandle,
    byte_offset: usize,
    byte_length: usize,
) {
    let bytes = copy_ab_slice(vm, buffer, byte_offset, byte_length);
    let len = bytes.len();
    let ab = alloc_array_buffer(vm, bytes);
    enqueue_chunk_to_queue(streams, stream_h, ab, 0, len);
}

fn enqueue_detached_pull_into_to_queue(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
    desc: &PullIntoDescriptor,
) -> Result<(), JsException> {
    if desc.bytes_filled > 0 {
        let transferred = transfer_array_buffer(vm, desc.buffer)?;
        enqueue_chunk_to_queue(
            streams,
            stream_h,
            transferred,
            desc.byte_offset,
            desc.bytes_filled,
        );
    }
    Ok(())
}

/// WHATWG §4.6.4 `ReadableByteStreamControllerFillPullIntoDescriptorFromQueue`.
/// Returns `true` when `desc` now holds at least `minimum_fill` element-aligned bytes.
fn fill_pull_into_descriptor_from_queue(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
    desc: &mut PullIntoDescriptor,
) -> bool {
    let queue_total = streams
        .borrow()
        .readable
        .get(&stream_h)
        .and_then(|rs| rs.byte_controller.as_ref())
        .map_or(0, |bc| bc.queue_total_size);
    let max_bytes_to_copy = queue_total.min(desc.byte_length.saturating_sub(desc.bytes_filled));
    let max_bytes_filled = desc.bytes_filled + max_bytes_to_copy;
    let mut total_to_copy = max_bytes_to_copy;
    let mut ready = false;
    let max_aligned = max_bytes_filled - (max_bytes_filled % desc.element_size);
    if max_aligned >= desc.minimum_fill {
        total_to_copy = max_aligned - desc.bytes_filled;
        ready = true;
    }

    while total_to_copy > 0 {
        let copy_step = {
            let mut reg = streams.borrow_mut();
            let Some(bc) = reg
                .readable
                .get_mut(&stream_h)
                .and_then(|rs| rs.byte_controller.as_mut())
            else {
                break;
            };
            let Some(head) = bc.queue.front_mut() else {
                break;
            };
            let n = total_to_copy.min(head.byte_length);
            let src_buf = head.buffer;
            let src_off = head.byte_offset;
            if head.byte_length == n {
                bc.queue.pop_front();
            } else {
                head.byte_offset += n;
                head.byte_length -= n;
            }
            bc.queue_total_size = bc.queue_total_size.saturating_sub(n);
            (src_buf, src_off, n)
        };
        let bytes = copy_ab_slice(vm, copy_step.0, copy_step.1, copy_step.2);
        write_ab_slice(
            vm,
            desc.buffer,
            desc.byte_offset + desc.bytes_filled,
            &bytes,
        );
        desc.bytes_filled += copy_step.2;
        total_to_copy -= copy_step.2;
    }
    ready
}

/// WHATWG §4.6.4 `ReadableByteStreamControllerConvertPullIntoDescriptor`.
fn convert_pull_into_descriptor(
    vm: &mut JsVm,
    desc: &PullIntoDescriptor,
) -> Result<HeapHandle, JsException> {
    let transferred = transfer_array_buffer(vm, desc.buffer)?;
    Ok(alloc_view(
        vm,
        desc.view_kind,
        transferred,
        desc.byte_offset,
        desc.bytes_filled,
    ))
}

/// WHATWG §4.6.4 `ReadableByteStreamControllerCommitPullIntoDescriptor`.
fn commit_pull_into_descriptor(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
    desc: PullIntoDescriptor,
) {
    let (state, req_default, req_byob) = {
        let mut reg = streams.borrow_mut();
        let Some(rs) = reg.readable.get_mut(&stream_h) else {
            return;
        };
        let state = rs.state;
        match desc.reader_type {
            PullIntoReaderType::Default => (state, rs.read_requests.pop_front(), None),
            PullIntoReaderType::Byob => (
                state,
                None,
                rs.byte_controller
                    .as_mut()
                    .and_then(|bc| bc.read_into_requests.pop_front()),
            ),
            PullIntoReaderType::None => (state, None, None),
        }
    };
    if desc.reader_type == PullIntoReaderType::None {
        return;
    }
    let view = match convert_pull_into_descriptor(vm, &desc) {
        Ok(v) => v,
        Err(e) => {
            byte_controller_error(vm, streams, stream_h, e.value);
            return;
        }
    };
    let done = state == ReadableState::Closed;
    if let Some(req) = req_default {
        if done {
            close_read_request(vm, req);
        } else {
            fulfill_read_request(vm, req, JsValue::from_object(view));
        }
    } else if let Some(req) = req_byob {
        if done {
            close_read_into_request(vm, req, JsValue::from_object(view));
        } else {
            fulfill_read_into_request(vm, req, JsValue::from_object(view));
        }
    }
}

/// WHATWG §4.6.4 `ReadableByteStreamControllerProcessReadRequestsUsingQueue`.
fn process_read_requests_using_queue(vm: &mut JsVm, streams: &SharedStreams, stream_h: HeapHandle) {
    loop {
        let step = {
            let mut reg = streams.borrow_mut();
            let Some(rs) = reg.readable.get_mut(&stream_h) else {
                return;
            };
            if rs.reader.is_none() || rs.reader_is_byob || rs.read_requests.is_empty() {
                return;
            }
            let Some(ref mut bc) = rs.byte_controller else {
                return;
            };
            if bc.queue_total_size == 0 {
                return;
            }
            let Some(entry) = bc.queue.pop_front() else {
                return;
            };
            bc.queue_total_size = bc.queue_total_size.saturating_sub(entry.byte_length);
            let close_after = bc.queue_total_size == 0 && rs.close_requested;
            if close_after {
                rs.pull_fn = None;
                rs.cancel_fn = None;
            }
            let req = rs.read_requests.pop_front().unwrap();
            let popped_default_desc = bc
                .pending_pull_intos
                .front()
                .is_some_and(|d| d.reader_type == PullIntoReaderType::Default);
            if popped_default_desc {
                bc.pending_pull_intos.pop_front();
            }
            (entry, req, close_after, popped_default_desc)
        };
        if step.3 {
            invalidate_byob_request(streams, stream_h);
        }
        let view = alloc_view(
            vm,
            PullIntoViewKind::TypedArray(TypedArrayKind::Uint8),
            step.0.buffer,
            step.0.byte_offset,
            step.0.byte_length,
        );
        if step.2 {
            byte_close_stream(vm, streams, stream_h);
        }
        fulfill_read_request(vm, step.1, JsValue::from_object(view));
    }
}

/// WHATWG §4.6.4 `ReadableByteStreamControllerProcessPullIntoDescriptorsUsingQueue`.
fn process_pull_into_descriptors_using_queue(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
) {
    // Collect committed descriptors first (after invalidating byobRequest & popping them)
    // so patched `Object.prototype.then` getters always observe updated `byobRequest`!
    let mut ready_to_commit: Vec<PullIntoDescriptor> = Vec::new();
    let mut close_after = false;

    loop {
        let mut head = {
            let mut reg = streams.borrow_mut();
            let Some(rs) = reg.readable.get_mut(&stream_h) else {
                break;
            };
            let Some(ref mut bc) = rs.byte_controller else {
                break;
            };
            if bc.queue_total_size == 0 || bc.pending_pull_intos.is_empty() {
                break;
            }
            bc.pending_pull_intos.pop_front().unwrap()
        };
        let ready = fill_pull_into_descriptor_from_queue(vm, streams, stream_h, &mut head);
        if ready {
            invalidate_byob_request(streams, stream_h);
            ready_to_commit.push(head);
            let should_close = streams.borrow().readable.get(&stream_h).is_some_and(|rs| {
                rs.close_requested
                    && rs
                        .byte_controller
                        .as_ref()
                        .is_some_and(|bc| bc.queue_total_size == 0)
            });
            if should_close {
                close_after = true;
            }
        } else {
            // Put partially filled descriptor back at front and stop.
            invalidate_byob_request(streams, stream_h);
            if let Some(bc) = streams
                .borrow_mut()
                .readable
                .get_mut(&stream_h)
                .and_then(|rs| rs.byte_controller.as_mut())
            {
                bc.pending_pull_intos.push_front(head);
            }
            break;
        }
    }

    if close_after {
        if let Some(rs) = streams.borrow_mut().readable.get_mut(&stream_h) {
            rs.pull_fn = None;
            rs.cancel_fn = None;
        }
        byte_close_stream(vm, streams, stream_h);
    }
    for desc in ready_to_commit {
        commit_pull_into_descriptor(vm, streams, stream_h, desc);
    }
}

/// Closes a byte stream (`rs.state = Closed`, resolves reader `closed`, closes default reads).
pub(crate) fn byte_close_stream(vm: &mut JsVm, streams: &SharedStreams, stream_h: HeapHandle) {
    let (pending_default, closed_p) = {
        let mut reg = streams.borrow_mut();
        let Some(rs) = reg.readable.get_mut(&stream_h) else {
            return;
        };
        if rs.state != ReadableState::Readable {
            return;
        }
        rs.state = ReadableState::Closed;
        let reader_h = rs.reader;
        let is_byob = rs.reader_is_byob;
        let pending_default = if !is_byob {
            std::mem::take(&mut rs.read_requests)
        } else {
            VecDeque::new()
        };
        let closed_p = reader_h.and_then(|rh| {
            if is_byob {
                reg.byob_readers.get(&rh).map(|r| r.closed_promise)
            } else {
                reg.readable_readers.get(&rh).map(|r| r.closed_promise)
            }
        });
        (pending_default, closed_p)
    };
    if let Some(cp) = closed_p {
        resolve_promise(vm, cp, JsValue::UNDEFINED);
    }
    for req in pending_default {
        close_read_request(vm, req);
    }
}

/// WHATWG §4.6.4 `ReadableByteStreamControllerClose`.
pub(crate) fn byte_controller_close(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
) -> Result<(), JsException> {
    let action = {
        let mut reg = streams.borrow_mut();
        let Some(rs) = reg.readable.get_mut(&stream_h) else {
            return Err(vm.type_error("Invalid ReadableByteStreamController"));
        };
        if rs.close_requested || rs.state != ReadableState::Readable {
            return Err(vm.type_error("Cannot close a closed or errored ReadableStream"));
        }
        let Some(ref bc) = rs.byte_controller else {
            return Err(vm.type_error("Invalid ReadableByteStreamController"));
        };
        if bc.queue_total_size > 0 {
            rs.close_requested = true;
            return Ok(());
        }
        let misaligned = bc
            .pending_pull_intos
            .front()
            .is_some_and(|d| d.bytes_filled % d.element_size != 0);
        if misaligned {
            true
        } else {
            rs.pull_fn = None;
            rs.cancel_fn = None;
            false
        }
    };
    if action {
        let err = vm.type_error("Insufficient bytes to fill elements in the given buffer");
        byte_controller_error(vm, streams, stream_h, err.value);
        return Err(err);
    }
    byte_close_stream(vm, streams, stream_h);
    Ok(())
}

/// WHATWG §4.6.4 `ReadableByteStreamControllerError`.
pub(crate) fn byte_controller_error(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
    reason: JsValue,
) {
    invalidate_byob_request(streams, stream_h);
    let (pending_default, pending_byob, closed_p) = {
        let mut reg = streams.borrow_mut();
        let Some(rs) = reg.readable.get_mut(&stream_h) else {
            return;
        };
        if rs.state != ReadableState::Readable {
            return;
        }
        rs.state = ReadableState::Errored;
        rs.stored_error = reason;
        rs.pull_fn = None;
        rs.cancel_fn = None;
        let reader_h = rs.reader;
        let is_byob = rs.reader_is_byob;
        let pending_default = std::mem::take(&mut rs.read_requests);
        let pending_byob = if let Some(ref mut bc) = rs.byte_controller {
            bc.reset_queue();
            bc.pending_pull_intos.clear();
            std::mem::take(&mut bc.read_into_requests)
        } else {
            VecDeque::new()
        };
        let closed_p = reader_h.and_then(|rh| {
            if is_byob {
                reg.byob_readers.get(&rh).map(|r| r.closed_promise)
            } else {
                reg.readable_readers.get(&rh).map(|r| r.closed_promise)
            }
        });
        (pending_default, pending_byob, closed_p)
    };
    if let Some(cp) = closed_p {
        reject_promise(vm, cp, reason);
        promise_mark_handled(vm, cp);
    }
    for req in pending_default {
        error_read_request(vm, req, reason);
    }
    for req in pending_byob {
        error_read_into_request(vm, req, reason);
    }
}

/// WHATWG §4.6.4 `ReadableByteStreamControllerCancelSteps`.
pub(crate) fn byte_cancel_internal(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
    reason: JsValue,
) -> HeapHandle {
    let action = {
        let mut reg = streams.borrow_mut();
        let Some(rs) = reg.readable.get_mut(&stream_h) else {
            return promise_resolve_value(vm, JsValue::UNDEFINED);
        };
        rs.disturbed = true;
        match rs.state {
            ReadableState::Closed => return promise_resolve_value(vm, JsValue::UNDEFINED),
            ReadableState::Errored => return promise_reject_value(vm, rs.stored_error),
            ReadableState::Readable => {
                let cf = rs.cancel_fn.take();
                let src = rs.source_obj;
                rs.pull_fn = None;
                let (pending_byob, pending_descs) = if let Some(ref mut bc) = rs.byte_controller {
                    bc.reset_queue();
                    let descs = std::mem::take(&mut bc.pending_pull_intos);
                    let reqs = std::mem::take(&mut bc.read_into_requests);
                    (reqs, descs)
                } else {
                    (VecDeque::new(), VecDeque::new())
                };
                (cf, src, pending_byob, pending_descs)
            }
        }
    };
    invalidate_byob_request(streams, stream_h);
    byte_close_stream(vm, streams, stream_h);
    // Resolve any pending BYOB read-into requests with `{ value: undefined, done: true }`
    // (or empty view if descriptor present — WHATWG §4.2.4 ReadableStreamCancel closes read-into requests with undefined).
    drop(action.3);
    for req in action.2 {
        close_read_into_request(vm, req, JsValue::UNDEFINED);
    }
    if let Some(cf) = action.0 {
        let this_v = action.1.map_or(JsValue::UNDEFINED, JsValue::from_object);
        let p = call_returning_promise(vm, cf, this_v, &[reason]);
        crate::streams_common::promise_map_to_undefined(vm, p)
    } else {
        promise_resolve_value(vm, JsValue::UNDEFINED)
    }
}

/// WHATWG §4.6.4 `ReadableByteStreamControllerEnqueue`.
pub(crate) fn byte_controller_enqueue(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
    chunk_val: JsValue,
) -> Result<(), JsException> {
    let info = extract_view_info(vm, chunk_val)?;
    if info.byte_length == 0 || info.buffer_byte_length == 0 || info.detached {
        return Err(vm.type_error("Cannot enqueue a zero-length or detached ArrayBufferView"));
    }
    let can_enq = streams
        .borrow()
        .readable
        .get(&stream_h)
        .is_some_and(|rs| !rs.close_requested && rs.state == ReadableState::Readable);
    if !can_enq {
        return Err(vm.type_error("Cannot enqueue into a closed or errored ReadableStream"));
    }

    // Step 5: Transfer caller's buffer (throws TypeError if detached / non-transferable).
    let transferred_buf = transfer_array_buffer(vm, info.buffer)?;

    // Step 6: Validate and transfer head pending pull-into descriptor's buffer (if any).
    let head_buf_opt = streams
        .borrow()
        .readable
        .get(&stream_h)
        .and_then(|rs| rs.byte_controller.as_ref())
        .and_then(|bc| bc.pending_pull_intos.front())
        .map(|d| (d.buffer, d.reader_type));

    if let Some((head_buf, head_rt)) = head_buf_opt {
        let (_, head_detached) = ab_byte_length_and_detached(vm, head_buf);
        if head_detached {
            return Err(vm.type_error("The pending BYOB request's ArrayBuffer has been detached"));
        }
        invalidate_byob_request(streams, stream_h);
        let new_head_buf = transfer_array_buffer(vm, head_buf)?;
        let popped_none = {
            let mut reg = streams.borrow_mut();
            if let Some(bc) = reg
                .readable
                .get_mut(&stream_h)
                .and_then(|rs| rs.byte_controller.as_mut())
            {
                if let Some(first) = bc.pending_pull_intos.front_mut() {
                    first.buffer = new_head_buf;
                }
                if head_rt == PullIntoReaderType::None {
                    bc.pending_pull_intos.pop_front()
                } else {
                    None
                }
            } else {
                None
            }
        };
        // WHATWG §4.6.4 ReadableByteStreamControllerEnqueue step 6e:
        // preserve any bytes already written into a released ("none") pull-into descriptor!
        if let Some(none_desc) = popped_none {
            if none_desc.bytes_filled > 0 {
                enqueue_chunk_to_queue(
                    streams,
                    stream_h,
                    none_desc.buffer,
                    none_desc.byte_offset,
                    none_desc.bytes_filled,
                );
            }
        }
    }

    let (has_default_reader, has_byob_reader) =
        streams
            .borrow()
            .readable
            .get(&stream_h)
            .map_or((false, false), |rs| {
                (
                    rs.reader.is_some() && !rs.reader_is_byob,
                    rs.reader.is_some() && rs.reader_is_byob,
                )
            });

    if has_default_reader {
        process_read_requests_using_queue(vm, streams, stream_h);
        let pending_req = {
            let mut reg = streams.borrow_mut();
            let rs = reg.readable.get_mut(&stream_h);
            if let Some(rs) = rs {
                if let Some(req) = rs.read_requests.pop_front() {
                    // Pop corresponding auto-allocated pull-into descriptor if present.
                    if let Some(ref mut bc) = rs.byte_controller {
                        if !bc.pending_pull_intos.is_empty() {
                            bc.pending_pull_intos.pop_front();
                        }
                    }
                    Some(req)
                } else {
                    None
                }
            } else {
                None
            }
        };
        if let Some(req) = pending_req {
            invalidate_byob_request(streams, stream_h);
            let view = alloc_view(
                vm,
                PullIntoViewKind::TypedArray(TypedArrayKind::Uint8),
                transferred_buf,
                info.byte_offset,
                info.byte_length,
            );
            fulfill_read_request(vm, req, JsValue::from_object(view));
        } else {
            enqueue_chunk_to_queue(
                streams,
                stream_h,
                transferred_buf,
                info.byte_offset,
                info.byte_length,
            );
        }
    } else if has_byob_reader {
        enqueue_chunk_to_queue(
            streams,
            stream_h,
            transferred_buf,
            info.byte_offset,
            info.byte_length,
        );
        process_pull_into_descriptors_using_queue(vm, streams, stream_h);
    } else {
        enqueue_chunk_to_queue(
            streams,
            stream_h,
            transferred_buf,
            info.byte_offset,
            info.byte_length,
        );
    }

    byte_call_pull_if_needed(vm, streams, stream_h);
    Ok(())
}

/// WHATWG §4.6.4 `ReadableByteStreamControllerRespondInternal`.
fn byte_respond_internal(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
    bytes_written: usize,
) -> Result<(), JsException> {
    let (state, head_buf, bytes_filled, byte_length) = {
        let reg = streams.borrow();
        let Some(rs) = reg.readable.get(&stream_h) else {
            return Err(vm.type_error("Invalid ReadableByteStreamController"));
        };
        let Some(first) = rs
            .byte_controller
            .as_ref()
            .and_then(|bc| bc.pending_pull_intos.front())
        else {
            return Err(vm.type_error("No pending BYOB pull-into descriptor"));
        };
        (
            rs.state,
            first.buffer,
            first.bytes_filled,
            first.byte_length,
        )
    };
    let (_, head_detached) = ab_byte_length_and_detached(vm, head_buf);
    if head_detached {
        return Err(vm.type_error("BYOB request's ArrayBuffer has been detached"));
    }

    if state == ReadableState::Closed {
        if bytes_written != 0 {
            return Err(vm.type_error("bytesWritten must be 0 when byte stream is closed"));
        }
        invalidate_byob_request(streams, stream_h);
        // Pop leading None descriptor (e.g. from released lock or closed default reader).
        {
            let mut reg = streams.borrow_mut();
            if let Some(bc) = reg
                .readable
                .get_mut(&stream_h)
                .and_then(|rs| rs.byte_controller.as_mut())
            {
                if bc
                    .pending_pull_intos
                    .front()
                    .is_some_and(|d| d.reader_type != PullIntoReaderType::Byob)
                {
                    bc.pending_pull_intos.pop_front();
                }
            }
        }
        // Commit all pending BYOB read-into requests in Closed state!
        let mut to_commit = Vec::new();
        {
            let mut reg = streams.borrow_mut();
            if let Some(rs) = reg.readable.get_mut(&stream_h) {
                if rs.reader.is_some() && rs.reader_is_byob {
                    if let Some(ref mut bc) = rs.byte_controller {
                        while !bc.read_into_requests.is_empty() && !bc.pending_pull_intos.is_empty()
                        {
                            if let Some(d) = bc.pending_pull_intos.pop_front() {
                                to_commit.push(d);
                            }
                        }
                    }
                }
            }
        }
        for desc in to_commit {
            commit_pull_into_descriptor(vm, streams, stream_h, desc);
        }
        return Ok(());
    }

    // Readable state:
    if bytes_written == 0 {
        return Err(vm.type_error("bytesWritten must be > 0 when byte stream is readable"));
    }
    if bytes_filled.saturating_add(bytes_written) > byte_length {
        return Err(vm.range_error("bytesWritten exceeds remaining view byteLength"));
    }

    // WHATWG §4.6.4 ReadableByteStreamControllerRespondInternal:
    // Invalidate current BYOB request and transfer the head descriptor's buffer immediately
    // (detaching the caller's `byobRequest.view.buffer` even before `minimum_fill` is reached).
    invalidate_byob_request(streams, stream_h);
    let transferred_head = transfer_array_buffer(vm, head_buf)?;

    let (reader_type, new_filled, min_fill) = {
        let mut reg = streams.borrow_mut();
        let bc = reg
            .readable
            .get_mut(&stream_h)
            .and_then(|rs| rs.byte_controller.as_mut())
            .unwrap();
        let first = bc.pending_pull_intos.front_mut().unwrap();
        first.buffer = transferred_head;
        first.bytes_filled += bytes_written;
        (first.reader_type, first.bytes_filled, first.minimum_fill)
    };

    if reader_type == PullIntoReaderType::None {
        if let Some(desc) = shift_pending_pull_into(streams, stream_h) {
            enqueue_detached_pull_into_to_queue(vm, streams, stream_h, &desc)?;
        }
        let has_default = streams
            .borrow()
            .readable
            .get(&stream_h)
            .is_some_and(|rs| rs.reader.is_some() && !rs.reader_is_byob);
        if has_default {
            process_read_requests_using_queue(vm, streams, stream_h);
        } else {
            process_pull_into_descriptors_using_queue(vm, streams, stream_h);
        }
    } else if new_filled >= min_fill {
        if let Some(mut desc) = shift_pending_pull_into(streams, stream_h) {
            let remainder = desc.bytes_filled % desc.element_size;
            if remainder > 0 {
                let end = desc.byte_offset + desc.bytes_filled;
                enqueue_cloned_chunk_to_queue(
                    vm,
                    streams,
                    stream_h,
                    desc.buffer,
                    end - remainder,
                    remainder,
                );
                desc.bytes_filled -= remainder;
            }
            commit_pull_into_descriptor(vm, streams, stream_h, desc);
            process_pull_into_descriptors_using_queue(vm, streams, stream_h);
        }
    }

    byte_call_pull_if_needed(vm, streams, stream_h);
    Ok(())
}

/// WHATWG §4.6.4 `ReadableByteStreamControllerRespondWithNewView`.
fn byte_respond_with_new_view(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
    view_val: JsValue,
) -> Result<(), JsException> {
    let info = extract_view_info(vm, view_val)?;
    if info.detached {
        return Err(vm.type_error("Cannot respondWithNewView with a detached ArrayBuffer"));
    }
    let (state, expected_offset, expected_buf_len, max_remaining) = {
        let reg = streams.borrow();
        let Some(rs) = reg.readable.get(&stream_h) else {
            return Err(vm.type_error("Invalid ReadableByteStreamController"));
        };
        let Some(first) = rs
            .byte_controller
            .as_ref()
            .and_then(|bc| bc.pending_pull_intos.front())
        else {
            return Err(vm.type_error("No pending BYOB pull-into descriptor"));
        };
        (
            rs.state,
            first.byte_offset + first.bytes_filled,
            first.buffer_byte_length,
            first.byte_length.saturating_sub(first.bytes_filled),
        )
    };
    if state == ReadableState::Readable && info.byte_length == 0 {
        return Err(vm.type_error("view.byteLength must be > 0 when byte stream is readable"));
    }
    if info.byte_offset != expected_offset {
        return Err(vm.range_error("view.byteOffset does not match BYOB request offset"));
    }
    if info.buffer_byte_length != expected_buf_len {
        return Err(vm.range_error("view.buffer.byteLength does not match BYOB request buffer"));
    }
    if info.byte_length > max_remaining {
        return Err(vm.range_error("view.byteLength exceeds remaining BYOB request byteLength"));
    }
    if state == ReadableState::Closed && info.byte_length != 0 {
        return Err(vm.type_error("view.byteLength must be 0 when byte stream is closed"));
    }

    let transferred = transfer_array_buffer(vm, info.buffer)?;
    if let Some(first) = streams
        .borrow_mut()
        .readable
        .get_mut(&stream_h)
        .and_then(|rs| rs.byte_controller.as_mut())
        .and_then(|bc| bc.pending_pull_intos.front_mut())
    {
        first.buffer = transferred;
    }
    byte_respond_internal(vm, streams, stream_h, info.byte_length)
}

/// Default reader `read()` on a byte stream (WHATWG §4.6.4 `[[PullSteps]]`).
pub(crate) fn byte_default_read_with_request(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
    req: ReadRequest,
) {
    enum Step {
        Chunk(ByteQueueEntry, bool),
        Closed,
        Errored(JsValue),
        Pending(Option<usize>),
    }
    let step = {
        let mut reg = streams.borrow_mut();
        let Some(rs) = reg.readable.get_mut(&stream_h) else {
            drop(reg);
            close_read_request(vm, req);
            return;
        };
        rs.disturbed = true;
        match rs.state {
            ReadableState::Closed => Step::Closed,
            ReadableState::Errored => Step::Errored(rs.stored_error),
            ReadableState::Readable => {
                if let Some(ref mut bc) = rs.byte_controller {
                    if let Some(entry) = bc.queue.pop_front() {
                        bc.queue_total_size = bc.queue_total_size.saturating_sub(entry.byte_length);
                        let close_after = bc.queue_total_size == 0 && rs.close_requested;
                        if close_after {
                            rs.pull_fn = None;
                            rs.cancel_fn = None;
                        }
                        Step::Chunk(entry, close_after)
                    } else {
                        Step::Pending(bc.auto_allocate_chunk_size)
                    }
                } else {
                    Step::Closed
                }
            }
        }
    };
    match step {
        Step::Chunk(entry, close_after) => {
            if close_after {
                byte_close_stream(vm, streams, stream_h);
            } else {
                byte_call_pull_if_needed(vm, streams, stream_h);
            }
            let view = alloc_view(
                vm,
                PullIntoViewKind::TypedArray(TypedArrayKind::Uint8),
                entry.buffer,
                entry.byte_offset,
                entry.byte_length,
            );
            fulfill_read_request(vm, req, JsValue::from_object(view));
        }
        Step::Closed => close_read_request(vm, req),
        Step::Errored(err) => error_read_request(vm, req, err),
        Step::Pending(auto_sz) => {
            let alloc_desc = auto_sz.map(|sz| {
                let ab = alloc_array_buffer(vm, vec![0u8; sz]);
                PullIntoDescriptor {
                    buffer: ab,
                    buffer_byte_length: sz,
                    byte_offset: 0,
                    byte_length: sz,
                    bytes_filled: 0,
                    minimum_fill: 1,
                    element_size: 1,
                    view_kind: PullIntoViewKind::TypedArray(TypedArrayKind::Uint8),
                    reader_type: PullIntoReaderType::Default,
                }
            });
            if let Some(rs) = streams.borrow_mut().readable.get_mut(&stream_h) {
                if let (Some(desc), Some(ref mut bc)) = (alloc_desc, rs.byte_controller.as_mut()) {
                    bc.pending_pull_intos.push_back(desc);
                }
                rs.read_requests.push_back(req);
            }
            byte_call_pull_if_needed(vm, streams, stream_h);
        }
    }
}

/// WHATWG §4.6.4 `ReadableByteStreamControllerPullInto` (for `ReadableStreamBYOBReader.read`).
pub(crate) fn byte_byob_read_with_request(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
    view_val: JsValue,
    min: usize,
    req: ReadIntoRequest,
) {
    let info = match extract_view_info(vm, view_val) {
        Ok(i) => i,
        Err(e) => {
            error_read_into_request(vm, req, e.value);
            return;
        }
    };
    if info.detached || info.byte_length == 0 || info.buffer_byte_length == 0 {
        let e = vm.type_error("Cannot read into a detached or zero-length ArrayBufferView");
        error_read_into_request(vm, req, e.value);
        return;
    }
    let view_elements = info.byte_length / info.element_size;
    if min > view_elements {
        let e = vm.range_error("min exceeds the number of elements in view");
        error_read_into_request(vm, req, e.value);
        return;
    }

    let (state, stored_err) = {
        let mut reg = streams.borrow_mut();
        let Some(rs) = reg.readable.get_mut(&stream_h) else {
            let e = vm.type_error("Invalid ReadableStream");
            drop(reg);
            error_read_into_request(vm, req, e.value);
            return;
        };
        rs.disturbed = true;
        (rs.state, rs.stored_error)
    };
    if state == ReadableState::Errored {
        error_read_into_request(vm, req, stored_err);
        return;
    }

    let transferred_buf = match transfer_array_buffer(vm, info.buffer) {
        Ok(b) => b,
        Err(e) => {
            error_read_into_request(vm, req, e.value);
            return;
        }
    };
    let minimum_fill = min.saturating_mul(info.element_size);
    let mut desc = PullIntoDescriptor {
        buffer: transferred_buf,
        buffer_byte_length: info.buffer_byte_length,
        byte_offset: info.byte_offset,
        byte_length: info.byte_length,
        bytes_filled: 0,
        minimum_fill,
        element_size: info.element_size,
        view_kind: info.view_kind,
        reader_type: PullIntoReaderType::Byob,
    };

    let (has_pending_intos, queue_total, close_requested) = streams
        .borrow()
        .readable
        .get(&stream_h)
        .map_or((false, 0, false), |rs| {
            let (p, q) = rs.byte_controller.as_ref().map_or((false, 0), |bc| {
                (!bc.pending_pull_intos.is_empty(), bc.queue_total_size)
            });
            (p, q, rs.close_requested)
        });

    if has_pending_intos {
        if let Some(bc) = streams
            .borrow_mut()
            .readable
            .get_mut(&stream_h)
            .and_then(|rs| rs.byte_controller.as_mut())
        {
            bc.pending_pull_intos.push_back(desc);
            bc.read_into_requests.push_back(req);
        }
        return;
    }

    if state == ReadableState::Closed {
        let empty_view = alloc_view(vm, desc.view_kind, desc.buffer, desc.byte_offset, 0);
        close_read_into_request(vm, req, JsValue::from_object(empty_view));
        return;
    }

    if queue_total > 0 {
        let ready = fill_pull_into_descriptor_from_queue(vm, streams, stream_h, &mut desc);
        if ready {
            let rem_zero_closing = streams.borrow().readable.get(&stream_h).is_some_and(|rs| {
                rs.close_requested
                    && rs
                        .byte_controller
                        .as_ref()
                        .is_some_and(|bc| bc.queue_total_size == 0)
            });
            if rem_zero_closing {
                if let Some(rs) = streams.borrow_mut().readable.get_mut(&stream_h) {
                    rs.pull_fn = None;
                    rs.cancel_fn = None;
                }
                byte_close_stream(vm, streams, stream_h);
            } else {
                byte_call_pull_if_needed(vm, streams, stream_h);
            }
            match convert_pull_into_descriptor(vm, &desc) {
                Ok(v) => fulfill_read_into_request(vm, req, JsValue::from_object(v)),
                Err(e) => {
                    byte_controller_error(vm, streams, stream_h, e.value);
                    error_read_into_request(vm, req, e.value);
                }
            }
            return;
        }
        if close_requested {
            let err = vm.type_error("Insufficient bytes to fill elements in the given buffer");
            byte_controller_error(vm, streams, stream_h, err.value);
            error_read_into_request(vm, req, err.value);
            return;
        }
    }

    if let Some(bc) = streams
        .borrow_mut()
        .readable
        .get_mut(&stream_h)
        .and_then(|rs| rs.byte_controller.as_mut())
    {
        bc.pending_pull_intos.push_back(desc);
        bc.read_into_requests.push_back(req);
    }
    byte_call_pull_if_needed(vm, streams, stream_h);
}

// ---------------------------------------------------------------------------
// BYOB Reader Acquire / Release (WHATWG Streams §4.4)
// ---------------------------------------------------------------------------

pub(crate) fn acquire_byob_reader(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
) -> Result<HeapHandle, JsException> {
    let reader_proto = streams
        .borrow()
        .byob_reader_proto
        .unwrap_or(vm.object_proto);
    let closed_p = alloc_promise(vm);
    let reader = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(reader_proto),
    ));
    let init = {
        let mut st = streams.borrow_mut();
        let Some(rs) = st.readable.get_mut(&stream_h) else {
            return Err(vm.type_error("Invalid ReadableStream"));
        };
        if !rs.is_byte_stream {
            return Err(vm.type_error("Cannot acquire BYOB reader on a non-byte ReadableStream"));
        }
        if rs.reader.is_some() {
            return Err(vm.type_error("ReadableStream is locked to a reader"));
        }
        rs.reader = Some(reader);
        rs.reader_is_byob = true;
        let state = (rs.state, rs.stored_error);
        st.byob_readers.insert(
            reader,
            ReadableByobReaderState {
                stream: Some(stream_h),
                closed_promise: closed_p,
            },
        );
        state
    };
    attach_gc_anchor(vm, reader, stream_h);
    match init.0 {
        ReadableState::Readable => {}
        ReadableState::Closed => resolve_promise(vm, closed_p, JsValue::UNDEFINED),
        ReadableState::Errored => {
            reject_promise(vm, closed_p, init.1);
            promise_mark_handled(vm, closed_p);
        }
    }
    Ok(reader)
}

pub(crate) fn release_byob_reader(vm: &mut JsVm, streams: &SharedStreams, reader_h: HeapHandle) {
    let info = {
        let mut st = streams.borrow_mut();
        let Some(r) = st.byob_readers.get_mut(&reader_h) else {
            return;
        };
        let Some(stream_h) = r.stream.take() else {
            return;
        };
        let closed_p = r.closed_promise;
        let pending = if let Some(rs) = st.readable.get_mut(&stream_h) {
            rs.reader = None;
            rs.reader_is_byob = false;
            if let Some(ref mut bc) = rs.byte_controller {
                // WHATWG §4.6.4 ReleaseSteps: keep head pending pull-into descriptor alive
                // with `reader_type = None` so an active `byobRequest` is NOT invalidated,
                // and drop any queued descriptors behind it!
                if let Some(first) = bc.pending_pull_intos.front_mut() {
                    first.reader_type = PullIntoReaderType::None;
                    bc.pending_pull_intos.truncate(1);
                }
                std::mem::take(&mut bc.read_into_requests)
            } else {
                VecDeque::new()
            }
        } else {
            VecDeque::new()
        };
        (closed_p, pending)
    };
    let rel_err = vm.type_error("Reader lock was released").value;
    if promise_is_pending(vm, info.0) {
        reject_promise(vm, info.0, rel_err);
        promise_mark_handled(vm, info.0);
    } else {
        let new_cp = promise_reject_value(vm, rel_err);
        promise_mark_handled(vm, new_cp);
        if let Some(r) = streams.borrow_mut().byob_readers.get_mut(&reader_h) {
            r.closed_promise = new_cp;
        }
    }
    for req in info.1 {
        error_read_into_request(vm, req, rel_err);
    }
}

/// Also called when a default reader releases its lock on a byte stream.
pub(crate) fn byte_on_default_reader_released(streams: &SharedStreams, stream_h: HeapHandle) {
    if let Some(bc) = streams
        .borrow_mut()
        .readable
        .get_mut(&stream_h)
        .and_then(|rs| rs.byte_controller.as_mut())
    {
        if let Some(first) = bc.pending_pull_intos.front_mut() {
            first.reader_type = PullIntoReaderType::None;
            bc.pending_pull_intos.truncate(1);
        }
    }
}

// ---------------------------------------------------------------------------
// Allocation & Constructor Helper for Byte Streams
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
pub(crate) fn alloc_byte_readable_stream(
    vm: &mut JsVm,
    streams: &SharedStreams,
    hwm: f64,
    auto_allocate_chunk_size: Option<usize>,
    pull_fn: Option<HeapHandle>,
    cancel_fn: Option<HeapHandle>,
    source_obj: Option<HeapHandle>,
    started: bool,
) -> (HeapHandle, HeapHandle) {
    let (rsp, bcp) = {
        let s = streams.borrow();
        (
            s.readable_proto.unwrap_or(vm.object_proto),
            s.byte_controller_proto.unwrap_or(vm.object_proto),
        )
    };
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(rsp)));
    let ctrl = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(bcp)));
    let mut st = ReadableStreamState::new(hwm, None);
    st.started = started;
    st.is_byte_stream = true;
    st.pull_fn = pull_fn;
    st.cancel_fn = cancel_fn;
    st.source_obj = source_obj;
    st.controller = Some(ctrl);
    st.byte_controller = Some(ByteControllerState::new(auto_allocate_chunk_size));
    attach_gc_anchor(vm, ctrl, obj);
    let mut reg = streams.borrow_mut();
    reg.readable.insert(obj, st);
    reg.byte_controllers.insert(ctrl, obj);
    (obj, ctrl)
}

/// Creates a WHATWG Fetch byte-oriented `ReadableStream` (`type: "bytes"`) backed by an
/// owned byte buffer, supporting both default readers and BYOB readers (including partial
/// offset reads and `tee()`).
pub(crate) fn create_readable_byte_stream_from_bytes(
    vm: &mut JsVm,
    streams: &SharedStreams,
    bytes: Vec<u8>,
) -> HeapHandle {
    if bytes.is_empty() {
        let (obj, _) = alloc_byte_readable_stream(vm, streams, 0.0, None, None, None, None, true);
        byte_close_stream(vm, streams, obj);
        return obj;
    }

    let state = Rc::new(RefCell::new((bytes, 0usize)));
    let ss_pull = Rc::clone(streams);
    let stream_slot: Rc<RefCell<Option<HeapHandle>>> = Rc::new(RefCell::new(None));
    let stream_slot_pull = Rc::clone(&stream_slot);
    let pull_fn = vm.alloc_host_fn("fetch_byte_body_pull", move |vm, _, _| {
        let Some(stream_h) = *stream_slot_pull.borrow() else {
            return Ok(JsValue::UNDEFINED);
        };
        let (rem_len, pos) = {
            let st = state.borrow();
            (st.0.len().saturating_sub(st.1), st.1)
        };
        if rem_len == 0 {
            let _ = byte_controller_close(vm, &ss_pull, stream_h);
            return Ok(JsValue::UNDEFINED);
        }

        // If a BYOB pull-into descriptor is waiting, fill directly into its buffer view!
        let byob_target = ss_pull
            .borrow()
            .readable
            .get(&stream_h)
            .and_then(|rs| rs.byte_controller.as_ref())
            .and_then(|bc| bc.pending_pull_intos.front())
            .map(|d| {
                (
                    d.buffer,
                    d.byte_offset + d.bytes_filled,
                    d.byte_length - d.bytes_filled,
                )
            });

        if let Some((buf_h, write_off, avail)) = byob_target {
            let n = rem_len.min(avail);
            if n > 0 {
                let chunk_slice = state.borrow().0[pos..pos + n].to_vec();
                if let Some(buf_obj) = vm.heap.get_object_mut(buf_h) {
                    if let ObjectKind::ArrayBuffer(ref mut ab) = buf_obj.kind {
                        if write_off + n <= ab.len() {
                            ab[write_off..write_off + n].copy_from_slice(&chunk_slice);
                        }
                    }
                }
                state.borrow_mut().1 += n;
                let done = {
                    let st = state.borrow();
                    st.1 >= st.0.len()
                };
                byte_respond_internal(vm, &ss_pull, stream_h, n)?;
                if done {
                    let _ = byte_controller_close(vm, &ss_pull, stream_h);
                }
            }
            return Ok(JsValue::UNDEFINED);
        }

        // Default reader: enqueue all remaining bytes as a Uint8Array and close!
        let remaining = {
            let mut st = state.borrow_mut();
            let slice = st.0[st.1..].to_vec();
            st.1 = st.0.len();
            slice
        };
        let u8_h = crate::web_apis::alloc_uint8_array(vm, remaining);
        let _ = byte_controller_enqueue(vm, &ss_pull, stream_h, JsValue::from_object(u8_h));
        let _ = byte_controller_close(vm, &ss_pull, stream_h);
        Ok(JsValue::UNDEFINED)
    });

    let (obj, _) =
        alloc_byte_readable_stream(vm, streams, 0.0, None, Some(pull_fn), None, None, true);
    *stream_slot.borrow_mut() = Some(obj);
    obj
}

// ---------------------------------------------------------------------------
// ReadableByteStreamTee (WHATWG Streams §4.2.4)
// ---------------------------------------------------------------------------

/// Implements live pull-on-demand `ReadableByteStreamTee` supporting both
/// default and BYOB reads on each branch with independent buffer cloning.
pub(crate) fn readable_byte_stream_tee(
    vm: &mut JsVm,
    streams: &SharedStreams,
    src_h: HeapHandle,
) -> Result<HeapHandle, JsException> {
    let init_reader = acquire_default_reader_for_tee(vm, streams, src_h)?;

    struct ByteTeeShared {
        reader: HeapHandle,
        reader_is_byob: bool,
        reading: bool,
        read_again_for_branch1: bool,
        read_again_for_branch2: bool,
        canceled1: bool,
        canceled2: bool,
        reason1: JsValue,
        reason2: JsValue,
        cancel_promise: HeapHandle,
        branch1: Option<HeapHandle>,
        branch2: Option<HeapHandle>,
    }

    let cancel_promise = alloc_promise(vm);
    let shared = Rc::new(RefCell::new(ByteTeeShared {
        reader: init_reader,
        reader_is_byob: false,
        reading: false,
        read_again_for_branch1: false,
        read_again_for_branch2: false,
        canceled1: false,
        canceled2: false,
        reason1: JsValue::UNDEFINED,
        reason2: JsValue::UNDEFINED,
        cancel_promise,
        branch1: None,
        branch2: None,
    }));

    fn forward_reader_error(
        vm: &mut JsVm,
        streams: &SharedStreams,
        shared: &Rc<RefCell<ByteTeeShared>>,
        reader_h: HeapHandle,
        is_byob: bool,
    ) {
        let closed_p = {
            let reg = streams.borrow();
            if is_byob {
                reg.byob_readers.get(&reader_h).map(|r| r.closed_promise)
            } else {
                reg.readable_readers
                    .get(&reader_h)
                    .map(|r| r.closed_promise)
            }
        };
        let Some(cp) = closed_p else {
            return;
        };
        let ss_err = Rc::clone(streams);
        let sh_err = Rc::clone(shared);
        let on_err = vm.alloc_host_fn("btee_err", move |vm, _, a| {
            let r = arg(a, 0);
            let (is_cur, c1, c2, b1, b2, cancel_p) = {
                let sh = sh_err.borrow();
                (
                    sh.reader == reader_h,
                    sh.canceled1,
                    sh.canceled2,
                    sh.branch1,
                    sh.branch2,
                    sh.cancel_promise,
                )
            };
            if !is_cur {
                return Ok(JsValue::UNDEFINED);
            }
            if let Some(h1) = b1 {
                byte_controller_error(vm, &ss_err, h1, r);
            }
            if let Some(h2) = b2 {
                byte_controller_error(vm, &ss_err, h2, r);
            }
            if !c1 || !c2 {
                resolve_promise(vm, cancel_p, JsValue::UNDEFINED);
            }
            Ok(JsValue::UNDEFINED)
        });
        let _ = promise_then(vm, cp, None, Some(on_err));
    }

    fn pull_with_default_reader(
        vm: &mut JsVm,
        streams: &SharedStreams,
        src_h: HeapHandle,
        shared: &Rc<RefCell<ByteTeeShared>>,
    ) {
        let need_switch = shared.borrow().reader_is_byob;
        if need_switch {
            let old = shared.borrow().reader;
            release_byob_reader(vm, streams, old);
            match acquire_default_reader_for_tee(vm, streams, src_h) {
                Ok(new_r) => {
                    {
                        let mut sh = shared.borrow_mut();
                        sh.reader = new_r;
                        sh.reader_is_byob = false;
                    }
                    forward_reader_error(vm, streams, shared, new_r, false);
                }
                Err(_) => return,
            }
        }
        let ss_c = Rc::clone(streams);
        let sh_c = Rc::clone(shared);
        let ss_cl = Rc::clone(streams);
        let sh_cl = Rc::clone(shared);
        let sh_er = Rc::clone(shared);
        let req = ReadRequest::Callback {
            on_chunk: Rc::new(RefCell::new(Some(Box::new(
                move |vm: &mut JsVm, chunk: JsValue| {
                    let ss_m = Rc::clone(&ss_c);
                    let sh_m = Rc::clone(&sh_c);
                    let micro =
                        vm.alloc_host_fn_rooted("btee_def_chunk", &[chunk], move |vm, _, _| {
                            let (c1, c2, b1, b2) = {
                                let mut sh = sh_m.borrow_mut();
                                sh.read_again_for_branch1 = false;
                                sh.read_again_for_branch2 = false;
                                (sh.canceled1, sh.canceled2, sh.branch1, sh.branch2)
                            };
                            let mut chunk1 = chunk;
                            let mut chunk2 = chunk;
                            if !c1 && !c2 {
                                if let Ok(info) = extract_view_info(vm, chunk) {
                                    let cloned = clone_as_uint8_array(
                                        vm,
                                        info.buffer,
                                        info.byte_offset,
                                        info.byte_length,
                                    );
                                    chunk2 = JsValue::from_object(cloned);
                                }
                            }
                            if !c1 {
                                if let Some(h1) = b1 {
                                    let _ = byte_controller_enqueue(vm, &ss_m, h1, chunk1);
                                    chunk1 = JsValue::UNDEFINED;
                                }
                            }
                            let _ = chunk1;
                            if !c2 {
                                if let Some(h2) = b2 {
                                    let _ = byte_controller_enqueue(vm, &ss_m, h2, chunk2);
                                }
                            }
                            let (again1, again2) = {
                                let mut sh = sh_m.borrow_mut();
                                sh.reading = false;
                                (sh.read_again_for_branch1, sh.read_again_for_branch2)
                            };
                            if again1 {
                                pull1_impl(vm, &ss_m, src_h, &sh_m);
                            } else if again2 {
                                pull2_impl(vm, &ss_m, src_h, &sh_m);
                            }
                            Ok(JsValue::UNDEFINED)
                        });
                    let resolved = promise_resolve_value(vm, JsValue::UNDEFINED);
                    let _ = promise_then(vm, resolved, Some(micro), None);
                },
            )))),
            on_close: Rc::new(RefCell::new(Some(Box::new(move |vm: &mut JsVm| {
                let (c1, c2, b1, b2, cp) = {
                    let mut sh = sh_cl.borrow_mut();
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
                        let _ = byte_controller_close(vm, &ss_cl, h1);
                        let has_br = ss_cl
                            .borrow()
                            .readable
                            .get(&h1)
                            .and_then(|rs| rs.byte_controller.as_ref())
                            .is_some_and(|bc| !bc.pending_pull_intos.is_empty());
                        if has_br {
                            let _ = byte_respond_internal(vm, &ss_cl, h1, 0);
                        }
                    }
                }
                if !c2 {
                    if let Some(h2) = b2 {
                        let _ = byte_controller_close(vm, &ss_cl, h2);
                        let has_br = ss_cl
                            .borrow()
                            .readable
                            .get(&h2)
                            .and_then(|rs| rs.byte_controller.as_ref())
                            .is_some_and(|bc| !bc.pending_pull_intos.is_empty());
                        if has_br {
                            let _ = byte_respond_internal(vm, &ss_cl, h2, 0);
                        }
                    }
                }
                if !c1 || !c2 {
                    resolve_promise(vm, cp, JsValue::UNDEFINED);
                }
            })))),
            on_error: Rc::new(RefCell::new(Some(Box::new(move |_, _| {
                sh_er.borrow_mut().reading = false;
            })))),
        };
        byte_default_read_with_request(vm, streams, src_h, req);
    }

    fn pull_with_byob_reader(
        vm: &mut JsVm,
        streams: &SharedStreams,
        src_h: HeapHandle,
        shared: &Rc<RefCell<ByteTeeShared>>,
        view: HeapHandle,
        for_branch2: bool,
    ) {
        let need_switch = !shared.borrow().reader_is_byob;
        if need_switch {
            let old = shared.borrow().reader;
            crate::streams::release_default_reader(vm, streams, old);
            match acquire_byob_reader(vm, streams, src_h) {
                Ok(new_r) => {
                    {
                        let mut sh = shared.borrow_mut();
                        sh.reader = new_r;
                        sh.reader_is_byob = true;
                    }
                    forward_reader_error(vm, streams, shared, new_r, true);
                }
                Err(_) => return,
            }
        }
        let (this_branch, other_branch) = {
            let sh = shared.borrow();
            if for_branch2 {
                (sh.branch2, sh.branch1)
            } else {
                (sh.branch1, sh.branch2)
            }
        };
        let ss_c = Rc::clone(streams);
        let sh_c = Rc::clone(shared);
        let ss_cl = Rc::clone(streams);
        let sh_cl = Rc::clone(shared);
        let sh_er = Rc::clone(shared);
        let req = ReadIntoRequest::Callback {
            on_chunk: Rc::new(RefCell::new(Some(Box::new(
                move |vm: &mut JsVm, chunk: JsValue| {
                    let ss_m = Rc::clone(&ss_c);
                    let sh_m = Rc::clone(&sh_c);
                    let micro =
                        vm.alloc_host_fn_rooted("btee_byob_chunk", &[chunk], move |vm, _, _| {
                            let (this_canceled, other_canceled) = {
                                let mut sh = sh_m.borrow_mut();
                                sh.read_again_for_branch1 = false;
                                sh.read_again_for_branch2 = false;
                                if for_branch2 {
                                    (sh.canceled2, sh.canceled1)
                                } else {
                                    (sh.canceled1, sh.canceled2)
                                }
                            };
                            if !other_canceled {
                                if let (Ok(info), Some(ob)) =
                                    (extract_view_info(vm, chunk), other_branch)
                                {
                                    let cloned = clone_as_uint8_array(
                                        vm,
                                        info.buffer,
                                        info.byte_offset,
                                        info.byte_length,
                                    );
                                    if !this_canceled {
                                        if let Some(tb) = this_branch {
                                            let _ =
                                                byte_respond_with_new_view(vm, &ss_m, tb, chunk);
                                        }
                                    }
                                    let _ = byte_controller_enqueue(
                                        vm,
                                        &ss_m,
                                        ob,
                                        JsValue::from_object(cloned),
                                    );
                                }
                            } else if !this_canceled {
                                if let Some(tb) = this_branch {
                                    let _ = byte_respond_with_new_view(vm, &ss_m, tb, chunk);
                                }
                            }
                            let (again1, again2) = {
                                let mut sh = sh_m.borrow_mut();
                                sh.reading = false;
                                (sh.read_again_for_branch1, sh.read_again_for_branch2)
                            };
                            if again1 {
                                pull1_impl(vm, &ss_m, src_h, &sh_m);
                            } else if again2 {
                                pull2_impl(vm, &ss_m, src_h, &sh_m);
                            }
                            Ok(JsValue::UNDEFINED)
                        });
                    let resolved = promise_resolve_value(vm, JsValue::UNDEFINED);
                    let _ = promise_then(vm, resolved, Some(micro), None);
                },
            )))),
            on_close: Rc::new(RefCell::new(Some(Box::new(
                move |vm: &mut JsVm, chunk: JsValue| {
                    let (this_canceled, other_canceled, cp) = {
                        let mut sh = sh_cl.borrow_mut();
                        sh.reading = false;
                        if for_branch2 {
                            (sh.canceled2, sh.canceled1, sh.cancel_promise)
                        } else {
                            (sh.canceled1, sh.canceled2, sh.cancel_promise)
                        }
                    };
                    if !this_canceled {
                        if let Some(tb) = this_branch {
                            let _ = byte_controller_close(vm, &ss_cl, tb);
                        }
                    }
                    if !other_canceled {
                        if let Some(ob) = other_branch {
                            let _ = byte_controller_close(vm, &ss_cl, ob);
                        }
                    }
                    if !chunk.is_undefined() {
                        if !this_canceled {
                            if let Some(tb) = this_branch {
                                let _ = byte_respond_with_new_view(vm, &ss_cl, tb, chunk);
                            }
                        }
                        if !other_canceled {
                            if let Some(ob) = other_branch {
                                let has_br = ss_cl
                                    .borrow()
                                    .readable
                                    .get(&ob)
                                    .and_then(|rs| rs.byte_controller.as_ref())
                                    .is_some_and(|bc| !bc.pending_pull_intos.is_empty());
                                if has_br {
                                    let _ = byte_respond_internal(vm, &ss_cl, ob, 0);
                                }
                            }
                        }
                    }
                    if !this_canceled || !other_canceled {
                        resolve_promise(vm, cp, JsValue::UNDEFINED);
                    }
                },
            )))),
            on_error: Rc::new(RefCell::new(Some(Box::new(move |_, _| {
                sh_er.borrow_mut().reading = false;
            })))),
        };
        byte_byob_read_with_request(vm, streams, src_h, JsValue::from_object(view), 1, req);
    }

    fn pull1_impl(
        vm: &mut JsVm,
        streams: &SharedStreams,
        src_h: HeapHandle,
        shared: &Rc<RefCell<ByteTeeShared>>,
    ) {
        {
            let mut sh = shared.borrow_mut();
            if sh.reading {
                sh.read_again_for_branch1 = true;
                return;
            }
            sh.reading = true;
        }
        let b1 = shared.borrow().branch1;
        if let Some(h1) = b1 {
            if let Some(view) = byte_get_or_create_byob_view(vm, streams, h1) {
                pull_with_byob_reader(vm, streams, src_h, shared, view, false);
                return;
            }
        }
        pull_with_default_reader(vm, streams, src_h, shared);
    }

    fn pull2_impl(
        vm: &mut JsVm,
        streams: &SharedStreams,
        src_h: HeapHandle,
        shared: &Rc<RefCell<ByteTeeShared>>,
    ) {
        {
            let mut sh = shared.borrow_mut();
            if sh.reading {
                sh.read_again_for_branch2 = true;
                return;
            }
            sh.reading = true;
        }
        let b2 = shared.borrow().branch2;
        if let Some(h2) = b2 {
            if let Some(view) = byte_get_or_create_byob_view(vm, streams, h2) {
                pull_with_byob_reader(vm, streams, src_h, shared, view, true);
                return;
            }
        }
        pull_with_default_reader(vm, streams, src_h, shared);
    }

    let ss_p1 = Rc::clone(streams);
    let sh_p1 = Rc::clone(&shared);
    let pull1 = vm.alloc_host_fn("btee_pull1", move |vm, _, _| {
        pull1_impl(vm, &ss_p1, src_h, &sh_p1);
        Ok(JsValue::UNDEFINED)
    });
    let ss_p2 = Rc::clone(streams);
    let sh_p2 = Rc::clone(&shared);
    let pull2 = vm.alloc_host_fn("btee_pull2", move |vm, _, _| {
        pull2_impl(vm, &ss_p2, src_h, &sh_p2);
        Ok(JsValue::UNDEFINED)
    });

    let ss_c1 = Rc::clone(streams);
    let sh_c1 = Rc::clone(&shared);
    let cancel1 = vm.alloc_host_fn("btee_cancel1", move |vm, _, a| {
        let reason = arg(a, 0);
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
            let res_p = byte_cancel_internal(vm, &ss_c1, src_h, JsValue::from_object(composite));
            let on_ok = vm.alloc_host_fn_rooted(
                "btee_c1_ok",
                &[JsValue::from_object(cp)],
                move |vm, _, _| {
                    resolve_promise(vm, cp, JsValue::UNDEFINED);
                    Ok(JsValue::UNDEFINED)
                },
            );
            let on_err = vm.alloc_host_fn_rooted(
                "btee_c1_err",
                &[JsValue::from_object(cp)],
                move |vm, _, ea| {
                    reject_promise(vm, cp, arg(ea, 0));
                    Ok(JsValue::UNDEFINED)
                },
            );
            let _ = promise_then(vm, res_p, Some(on_ok), Some(on_err));
        }
        Ok(JsValue::from_object(cp))
    });

    let ss_c2 = Rc::clone(streams);
    let sh_c2 = Rc::clone(&shared);
    let cancel2 = vm.alloc_host_fn("btee_cancel2", move |vm, _, a| {
        let reason = arg(a, 0);
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
            let res_p = byte_cancel_internal(vm, &ss_c2, src_h, JsValue::from_object(composite));
            let on_ok = vm.alloc_host_fn_rooted(
                "btee_c2_ok",
                &[JsValue::from_object(cp)],
                move |vm, _, _| {
                    resolve_promise(vm, cp, JsValue::UNDEFINED);
                    Ok(JsValue::UNDEFINED)
                },
            );
            let on_err = vm.alloc_host_fn_rooted(
                "btee_c2_err",
                &[JsValue::from_object(cp)],
                move |vm, _, ea| {
                    reject_promise(vm, cp, arg(ea, 0));
                    Ok(JsValue::UNDEFINED)
                },
            );
            let _ = promise_then(vm, res_p, Some(on_ok), Some(on_err));
        }
        Ok(JsValue::from_object(cp))
    });

    let (b1, _) = alloc_byte_readable_stream(
        vm,
        streams,
        0.0,
        None,
        Some(pull1),
        Some(cancel1),
        None,
        false,
    );
    let (b2, _) = alloc_byte_readable_stream(
        vm,
        streams,
        0.0,
        None,
        Some(pull2),
        Some(cancel2),
        None,
        false,
    );
    {
        let mut sh = shared.borrow_mut();
        sh.branch1 = Some(b1);
        sh.branch2 = Some(b2);
    }
    attach_gc_anchor(vm, b1, init_reader);
    attach_gc_anchor(vm, b1, cancel_promise);
    attach_gc_anchor(vm, b2, init_reader);
    attach_gc_anchor(vm, b2, cancel_promise);

    // Schedule initial start microtasks on both branches (HWM = 0).
    for bh in [b1, b2] {
        let ss_s = Rc::clone(streams);
        let on_st = vm.alloc_host_fn("btee_branch_start", move |vm, _, _| {
            if let Some(rs) = ss_s.borrow_mut().readable.get_mut(&bh) {
                rs.started = true;
            }
            byte_call_pull_if_needed(vm, &ss_s, bh);
            Ok(JsValue::UNDEFINED)
        });
        let p = promise_resolve_value(vm, JsValue::UNDEFINED);
        let _ = promise_then(vm, p, Some(on_st), None);
    }

    forward_reader_error(vm, streams, &shared, init_reader, false);

    let arr = vm.alloc_array();
    vm.heap.object_push_element(arr, JsValue::from_object(b1));
    vm.heap.object_push_element(arr, JsValue::from_object(b2));
    Ok(arr)
}

fn acquire_default_reader_for_tee(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
) -> Result<HeapHandle, JsException> {
    crate::streams::acquire_default_reader(vm, streams, stream_h)
}

fn byte_get_or_create_byob_request(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
) -> Option<HeapHandle> {
    let existing = streams
        .borrow()
        .readable
        .get(&stream_h)
        .and_then(|rs| rs.byte_controller.as_ref())
        .and_then(|bc| bc.byob_request);
    if existing.is_some() {
        return existing;
    }
    let desc_info = streams
        .borrow()
        .readable
        .get(&stream_h)
        .and_then(|rs| rs.byte_controller.as_ref())
        .and_then(|bc| bc.pending_pull_intos.front())
        .map(|d| {
            (
                d.buffer,
                d.byte_offset + d.bytes_filled,
                d.byte_length - d.bytes_filled,
            )
        });
    let (buf_h, off, len) = desc_info?;
    let view = alloc_view(
        vm,
        PullIntoViewKind::TypedArray(TypedArrayKind::Uint8),
        buf_h,
        off,
        len,
    );
    let br_proto = streams
        .borrow()
        .byob_request_proto
        .unwrap_or(vm.object_proto);
    let br_obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(br_proto)));
    attach_gc_anchor(vm, br_obj, view);
    attach_gc_anchor(vm, br_obj, stream_h);
    let mut reg = streams.borrow_mut();
    reg.byob_requests.insert(
        br_obj,
        ByobRequestState {
            stream: Some(stream_h),
            view: Some(view),
        },
    );
    if let Some(bc) = reg
        .readable
        .get_mut(&stream_h)
        .and_then(|rs| rs.byte_controller.as_mut())
    {
        bc.byob_request = Some(br_obj);
    }
    Some(br_obj)
}

fn byte_get_or_create_byob_view(
    vm: &mut JsVm,
    streams: &SharedStreams,
    stream_h: HeapHandle,
) -> Option<HeapHandle> {
    let br_h = byte_get_or_create_byob_request(vm, streams, stream_h)?;
    streams
        .borrow()
        .byob_requests
        .get(&br_h)
        .and_then(|br| br.view)
}

// ---------------------------------------------------------------------------
// JS Interface Installation (`ReadableByteStreamController`, `BYOBReader`, `BYOBRequest`)
// ---------------------------------------------------------------------------

fn require_byte_controller(
    vm: &mut JsVm,
    streams: &SharedStreams,
    val: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible ReadableByteStreamController receiver"));
    };
    streams
        .borrow()
        .byte_controllers
        .get(&h)
        .copied()
        .ok_or_else(|| vm.type_error("Incompatible ReadableByteStreamController receiver"))
}

fn require_byob_request(
    vm: &mut JsVm,
    streams: &SharedStreams,
    val: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible ReadableStreamBYOBRequest receiver"));
    };
    if streams.borrow().byob_requests.contains_key(&h) {
        Ok(h)
    } else {
        Err(vm.type_error("Incompatible ReadableStreamBYOBRequest receiver"))
    }
}

fn require_byob_reader(
    vm: &mut JsVm,
    streams: &SharedStreams,
    val: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible ReadableStreamBYOBReader receiver"));
    };
    if streams.borrow().byob_readers.contains_key(&h) {
        Ok(h)
    } else {
        Err(vm.type_error("Incompatible ReadableStreamBYOBReader receiver"))
    }
}

pub(crate) fn install_byte_stream_types(vm: &mut JsVm, streams: &SharedStreams) {
    // --- ReadableStreamBYOBRequest ---
    let brp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(brp).unwrap().is_prototype = true;
    set_to_string_tag(vm, brp, "ReadableStreamBYOBRequest");
    streams.borrow_mut().byob_request_proto = Some(brp);

    let ss = Rc::clone(streams);
    define_host_getter(vm, brp, "view", move |vm, this_val, _| {
        let h = require_byob_request(vm, &ss, this_val)?;
        let v = ss.borrow().byob_requests.get(&h).and_then(|br| br.view);
        Ok(v.map_or(JsValue::NULL, JsValue::from_object))
    });
    let ss = Rc::clone(streams);
    define_host_method(vm, brp, "respond", move |vm, this_val, args| {
        let h = require_byob_request(vm, &ss, this_val)?;
        let bytes_written = parse_non_negative_u64_enforce_range(vm, arg(args, 0))?;
        let stream_h = ss
            .borrow()
            .byob_requests
            .get(&h)
            .and_then(|br| br.stream)
            .ok_or_else(|| vm.type_error("ReadableStreamBYOBRequest is no longer active"))?;
        byte_respond_internal(vm, &ss, stream_h, bytes_written)?;
        Ok(JsValue::UNDEFINED)
    });
    let ss = Rc::clone(streams);
    define_host_method(vm, brp, "respondWithNewView", move |vm, this_val, args| {
        let h = require_byob_request(vm, &ss, this_val)?;
        let view_val = arg(args, 0);
        let stream_h = ss
            .borrow()
            .byob_requests
            .get(&h)
            .and_then(|br| br.stream)
            .ok_or_else(|| vm.type_error("ReadableStreamBYOBRequest is no longer active"))?;
        byte_respond_with_new_view(vm, &ss, stream_h, view_val)?;
        Ok(JsValue::UNDEFINED)
    });
    let br_ctor = vm.alloc_host_fn("ReadableStreamBYOBRequest", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: ReadableStreamBYOBRequest"))
    });
    bind_ctor_proto(vm, br_ctor, brp);
    define_global(
        vm,
        "ReadableStreamBYOBRequest",
        JsValue::from_object(br_ctor),
    );

    // --- ReadableByteStreamController ---
    let bcp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(bcp).unwrap().is_prototype = true;
    set_to_string_tag(vm, bcp, "ReadableByteStreamController");
    streams.borrow_mut().byte_controller_proto = Some(bcp);

    let ss = Rc::clone(streams);
    define_host_getter(vm, bcp, "byobRequest", move |vm, this_val, _| {
        let stream_h = require_byte_controller(vm, &ss, this_val)?;
        Ok(byte_get_or_create_byob_request(vm, &ss, stream_h)
            .map_or(JsValue::NULL, JsValue::from_object))
    });
    let ss = Rc::clone(streams);
    define_host_getter(vm, bcp, "desiredSize", move |vm, this_val, _| {
        let stream_h = require_byte_controller(vm, &ss, this_val)?;
        let ds = ss
            .borrow()
            .readable
            .get(&stream_h)
            .and_then(byte_desired_size);
        Ok(ds.map_or(JsValue::NULL, JsValue::from_f64))
    });
    let ss = Rc::clone(streams);
    define_host_method(vm, bcp, "enqueue", move |vm, this_val, args| {
        let stream_h = require_byte_controller(vm, &ss, this_val)?;
        byte_controller_enqueue(vm, &ss, stream_h, arg(args, 0))?;
        Ok(JsValue::UNDEFINED)
    });
    let ss = Rc::clone(streams);
    define_host_method(vm, bcp, "close", move |vm, this_val, _| {
        let stream_h = require_byte_controller(vm, &ss, this_val)?;
        byte_controller_close(vm, &ss, stream_h)?;
        Ok(JsValue::UNDEFINED)
    });
    let ss = Rc::clone(streams);
    define_host_method(vm, bcp, "error", move |vm, this_val, args| {
        let stream_h = require_byte_controller(vm, &ss, this_val)?;
        byte_controller_error(vm, &ss, stream_h, arg(args, 0));
        Ok(JsValue::UNDEFINED)
    });
    let bc_ctor = vm.alloc_host_fn("ReadableByteStreamController", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: ReadableByteStreamController"))
    });
    bind_ctor_proto(vm, bc_ctor, bcp);
    define_global(
        vm,
        "ReadableByteStreamController",
        JsValue::from_object(bc_ctor),
    );

    // --- ReadableStreamBYOBReader ---
    let byob_p = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(byob_p).unwrap().is_prototype = true;
    set_to_string_tag(vm, byob_p, "ReadableStreamBYOBReader");
    streams.borrow_mut().byob_reader_proto = Some(byob_p);

    let ss = Rc::clone(streams);
    define_host_getter(vm, byob_p, "closed", move |vm, this_val, _| {
        let h = match require_byob_reader(vm, &ss, this_val) {
            Ok(h) => h,
            Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
        };
        let cp = ss
            .borrow()
            .byob_readers
            .get(&h)
            .map(|r| r.closed_promise)
            .unwrap_or_else(|| promise_resolve_value(vm, JsValue::UNDEFINED));
        Ok(JsValue::from_object(cp))
    });
    let ss = Rc::clone(streams);
    define_host_method(vm, byob_p, "read", move |vm, this_val, args| {
        let h = match require_byob_reader(vm, &ss, this_val) {
            Ok(h) => h,
            Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
        };
        let view_val = arg(args, 0);
        // Convert options BEFORE lock check per WebIDL? Spec checks view/options then lock;
        // check view & options first so invalid `min: 0` rejects with TypeError cleanly.
        let opts_val = arg(args, 1);
        let min = if opts_val.is_undefined() || opts_val.is_null() {
            1usize
        } else {
            let Some(oh) = opts_val.as_object() else {
                let e = vm.type_error("ReadableStreamBYOBReaderReadOptions must be an object");
                return Ok(JsValue::from_object(promise_reject_value(vm, e.value)));
            };
            let km = vm.heap.intern_key("min");
            let mv = match vm.get_property_by_key(oh, km) {
                Ok(v) => v,
                Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
            };
            if mv.is_undefined() {
                1usize
            } else {
                match parse_positive_u64_enforce_range(vm, mv) {
                    Ok(m) => m,
                    Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
                }
            }
        };
        let stream_opt = ss.borrow().byob_readers.get(&h).and_then(|r| r.stream);
        let Some(stream_h) = stream_opt else {
            let err = vm.type_error("Cannot read from a released ReadableStreamBYOBReader");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        };
        let p = alloc_promise(vm);
        byte_byob_read_with_request(
            vm,
            &ss,
            stream_h,
            view_val,
            min,
            ReadIntoRequest::Promise(p),
        );
        Ok(JsValue::from_object(p))
    });
    let ss = Rc::clone(streams);
    define_host_method(vm, byob_p, "cancel", move |vm, this_val, args| {
        let h = match require_byob_reader(vm, &ss, this_val) {
            Ok(h) => h,
            Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
        };
        let stream_opt = ss.borrow().byob_readers.get(&h).and_then(|r| r.stream);
        let Some(stream_h) = stream_opt else {
            let err = vm.type_error("Cannot cancel a released ReadableStreamBYOBReader");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        };
        Ok(JsValue::from_object(byte_cancel_internal(
            vm,
            &ss,
            stream_h,
            arg(args, 0),
        )))
    });
    let ss = Rc::clone(streams);
    define_host_method(vm, byob_p, "releaseLock", move |vm, this_val, _| {
        let h = require_byob_reader(vm, &ss, this_val)?;
        release_byob_reader(vm, &ss, h);
        Ok(JsValue::UNDEFINED)
    });
    let ss = Rc::clone(streams);
    let byob_ctor = vm.alloc_host_fn("ReadableStreamBYOBReader", move |vm, _, args| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("ReadableStreamBYOBReader constructor requires 'new'"));
        }
        let stream_h = require_readable(vm, &ss, arg(args, 0))?;
        let reader = acquire_byob_reader(vm, &ss, stream_h)?;
        Ok(JsValue::from_object(reader))
    });
    vm.set_fn_length(byob_ctor, 1);
    bind_ctor_proto(vm, byob_ctor, byob_p);
    define_global(
        vm,
        "ReadableStreamBYOBReader",
        JsValue::from_object(byob_ctor),
    );
}

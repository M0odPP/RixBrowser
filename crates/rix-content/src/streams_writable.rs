//! WHATWG Streams Standard §5 (`WritableStream`, `WritableStreamDefaultWriter`,
//! `WritableStreamDefaultController`) with exact 4-state machine (`Writable`,
//! `Closed`, `Erroring`, `Errored`), backpressure `ready` promise tracking,
//! `AbortSignal` controller signaling, and re-entrant queuing strategy support.

use std::collections::VecDeque;
use std::rc::Rc;

use rix_js::promise::{
    alloc_promise, promise_resolve_value, promise_then, reject_promise, resolve_promise,
};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind};

use crate::streams::{require_writable, SharedStreams};
use crate::streams_common::{
    attach_gc_anchor, call_returning_promise, call_size_algorithm, extract_strategy,
    get_method_checked, promise_is_pending, promise_mark_handled, promise_reject_value,
    set_to_string_tag, validate_and_normalize_hwm,
};
use crate::web_apis::{
    arg, bind_ctor_proto, create_internal_abort_controller, define_global, define_host_getter,
    define_host_method, trigger_internal_abort, SharedWebApis,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WritableState {
    Writable,
    Closed,
    Erroring,
    Errored,
}

#[derive(Debug, Clone)]
pub(crate) enum WritableQueueItem {
    Write(JsValue),
    Close,
}

#[derive(Debug, Clone)]
pub(crate) struct WritableQueueEntry {
    pub(crate) item: WritableQueueItem,
    pub(crate) size: f64,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingAbortRequest {
    pub(crate) promise: HeapHandle,
    pub(crate) reason: JsValue,
    pub(crate) was_already_erroring: bool,
}

#[derive(Clone)]
pub(crate) struct WritableStreamState {
    pub(crate) state: WritableState,
    pub(crate) stored_error: JsValue,
    pub(crate) writer: Option<HeapHandle>,
    pub(crate) controller: Option<HeapHandle>,
    pub(crate) abort_controller: Option<HeapHandle>,
    pub(crate) abort_signal: Option<HeapHandle>,
    pub(crate) web_apis: Option<SharedWebApis>,
    pub(crate) queue: VecDeque<WritableQueueEntry>,
    pub(crate) queue_total_size: f64,
    pub(crate) hwm: f64,
    pub(crate) size_fn: Option<HeapHandle>,
    pub(crate) started: bool,
    pub(crate) backpressure: bool,
    pub(crate) in_flight_write_request: Option<HeapHandle>,
    pub(crate) in_flight_close_request: Option<HeapHandle>,
    pub(crate) close_request: Option<HeapHandle>,
    pub(crate) write_requests: VecDeque<HeapHandle>,
    pub(crate) pending_abort_request: Option<PendingAbortRequest>,
    pub(crate) write_fn: Option<HeapHandle>,
    pub(crate) close_fn: Option<HeapHandle>,
    pub(crate) abort_fn: Option<HeapHandle>,
    pub(crate) sink_obj: Option<HeapHandle>,
}

impl WritableStreamState {
    pub(crate) fn new(
        hwm: f64,
        size_fn: Option<HeapHandle>,
        write_fn: Option<HeapHandle>,
        close_fn: Option<HeapHandle>,
        abort_fn: Option<HeapHandle>,
        sink_obj: Option<HeapHandle>,
    ) -> Self {
        Self {
            state: WritableState::Writable,
            stored_error: JsValue::UNDEFINED,
            writer: None,
            controller: None,
            abort_controller: None,
            abort_signal: None,
            web_apis: None,
            queue: VecDeque::new(),
            queue_total_size: 0.0,
            hwm,
            size_fn,
            started: false,
            backpressure: false,
            in_flight_write_request: None,
            in_flight_close_request: None,
            close_request: None,
            write_requests: VecDeque::new(),
            pending_abort_request: None,
            write_fn,
            close_fn,
            abort_fn,
            sink_obj,
        }
    }

    pub(crate) fn collect_roots(&self, out: &mut Vec<JsValue>) {
        out.push(self.stored_error);
        for h in [
            self.writer,
            self.controller,
            self.abort_controller,
            self.abort_signal,
            self.size_fn,
            self.in_flight_write_request,
            self.in_flight_close_request,
            self.close_request,
            self.write_fn,
            self.close_fn,
            self.abort_fn,
            self.sink_obj,
        ]
        .into_iter()
        .flatten()
        {
            out.push(JsValue::from_object(h));
        }
        for e in &self.queue {
            if let WritableQueueItem::Write(v) = e.item {
                out.push(v);
            }
        }
        for &p in &self.write_requests {
            out.push(JsValue::from_object(p));
        }
        if let Some(ref pa) = self.pending_abort_request {
            out.push(JsValue::from_object(pa.promise));
            out.push(pa.reason);
        }
    }

    fn close_queued_or_in_flight(&self) -> bool {
        self.close_request.is_some() || self.in_flight_close_request.is_some()
    }

    fn has_operation_marked_in_flight(&self) -> bool {
        self.in_flight_write_request.is_some() || self.in_flight_close_request.is_some()
    }

    fn desired_size(&self) -> Option<f64> {
        match self.state {
            WritableState::Errored | WritableState::Erroring => None,
            WritableState::Closed => Some(0.0),
            WritableState::Writable => Some(self.hwm - self.queue_total_size),
        }
    }

    fn compute_backpressure(&self) -> bool {
        self.desired_size().is_some_and(|d| d <= 0.0)
    }

    fn enqueue_write(
        &mut self,
        vm: &mut JsVm,
        chunk: JsValue,
        size: f64,
    ) -> Result<(), JsException> {
        if !size.is_finite() || size < 0.0 {
            return Err(vm.range_error("Chunk size must be a finite non-negative number"));
        }
        self.queue.push_back(WritableQueueEntry {
            item: WritableQueueItem::Write(chunk),
            size,
        });
        self.queue_total_size += size;
        Ok(())
    }

    fn enqueue_close(&mut self) {
        self.queue.push_back(WritableQueueEntry {
            item: WritableQueueItem::Close,
            size: 0.0,
        });
    }

    fn dequeue(&mut self) -> Option<WritableQueueItem> {
        let entry = self.queue.pop_front()?;
        self.queue_total_size -= entry.size;
        if self.queue_total_size < 0.0 {
            self.queue_total_size = 0.0;
        }
        Some(entry.item)
    }

    fn reset_queue(&mut self) {
        self.queue.clear();
        self.queue_total_size = 0.0;
    }

    fn clear_algorithms(&mut self) {
        self.write_fn = None;
        self.close_fn = None;
        self.abort_fn = None;
        self.size_fn = None;
    }
}

#[derive(Debug, Clone)]
pub(crate) struct WritableDefaultWriterState {
    pub(crate) stream: Option<HeapHandle>,
    pub(crate) closed_promise: HeapHandle,
    pub(crate) ready_promise: HeapHandle,
}

// ---------------------------------------------------------------------------
// Core WHATWG WritableStream State Transitions (§5.4 / §5.5)
// ---------------------------------------------------------------------------

fn update_backpressure(vm: &mut JsVm, streams: &SharedStreams, ws_h: HeapHandle, new_bp: bool) {
    let writer_ready = {
        let mut st = streams.borrow_mut();
        let Some(ws) = st.writable.get_mut(&ws_h) else {
            return;
        };
        if ws.backpressure == new_bp {
            return;
        }
        ws.backpressure = new_bp;
        ws.writer
            .and_then(|wh| st.writable_writers.get(&wh).map(|w| (wh, w.ready_promise)))
    };
    if let Some((wh, old_rp)) = writer_ready {
        if new_bp {
            let fresh = alloc_promise(vm);
            if let Some(w) = streams.borrow_mut().writable_writers.get_mut(&wh) {
                w.ready_promise = fresh;
            }
        } else {
            resolve_promise(vm, old_rp, JsValue::UNDEFINED);
        }
    }
}

fn ensure_ready_promise_rejected(
    vm: &mut JsVm,
    streams: &SharedStreams,
    writer_h: HeapHandle,
    error: JsValue,
) {
    let rp_opt = streams
        .borrow()
        .writable_writers
        .get(&writer_h)
        .map(|w| w.ready_promise);
    let Some(rp) = rp_opt else {
        return;
    };
    if promise_is_pending(vm, rp) {
        reject_promise(vm, rp, error);
        promise_mark_handled(vm, rp);
    } else {
        let new_rp = promise_reject_value(vm, error);
        promise_mark_handled(vm, new_rp);
        if let Some(w) = streams.borrow_mut().writable_writers.get_mut(&writer_h) {
            w.ready_promise = new_rp;
        }
    }
}

fn reject_close_and_closed_promise_if_needed(
    vm: &mut JsVm,
    streams: &SharedStreams,
    ws_h: HeapHandle,
) {
    let (stored_error, close_req, writer_cp) = {
        let mut st = streams.borrow_mut();
        let Some(ws) = st.writable.get_mut(&ws_h) else {
            return;
        };
        let stored_error = ws.stored_error;
        let cr = ws.close_request.take();
        let writer_h = ws.writer;
        let cp = writer_h
            .and_then(|wh| st.writable_writers.get(&wh))
            .map(|w| w.closed_promise);
        (stored_error, cr, cp)
    };
    if let Some(cr) = close_req {
        reject_promise(vm, cr, stored_error);
    }
    if let Some(cp) = writer_cp {
        reject_promise(vm, cp, stored_error);
        promise_mark_handled(vm, cp);
    }
}

pub(crate) fn writable_start_erroring(
    vm: &mut JsVm,
    streams: &SharedStreams,
    ws_h: HeapHandle,
    reason: JsValue,
) {
    let (writer_opt, finish_now) = {
        let mut st = streams.borrow_mut();
        let Some(ws) = st.writable.get_mut(&ws_h) else {
            return;
        };
        if ws.state != WritableState::Writable {
            return;
        }
        ws.state = WritableState::Erroring;
        ws.stored_error = reason;
        let finish_now = !ws.has_operation_marked_in_flight() && ws.started;
        (ws.writer, finish_now)
    };
    if let Some(wh) = writer_opt {
        ensure_ready_promise_rejected(vm, streams, wh, reason);
    }
    if finish_now {
        writable_finish_erroring(vm, streams, ws_h);
    }
}

fn writable_finish_erroring(vm: &mut JsVm, streams: &SharedStreams, ws_h: HeapHandle) {
    let (stored_error, writes, pending_abort, abort_fn, sink_obj, ac_info) = {
        let mut st = streams.borrow_mut();
        let Some(ws) = st.writable.get_mut(&ws_h) else {
            return;
        };
        if ws.state != WritableState::Erroring {
            return;
        }
        ws.state = WritableState::Errored;
        ws.reset_queue();
        let stored_error = ws.stored_error;
        let writes = std::mem::take(&mut ws.write_requests);
        let pa = ws.pending_abort_request.take();
        let af = ws.abort_fn;
        let sink = ws.sink_obj;
        let ac_info = ws.abort_controller.zip(ws.web_apis.clone());
        ws.clear_algorithms();
        (stored_error, writes, pa, af, sink, ac_info)
    };

    for wp in writes {
        reject_promise(vm, wp, stored_error);
    }

    let Some(pa) = pending_abort else {
        reject_close_and_closed_promise_if_needed(vm, streams, ws_h);
        return;
    };

    if pa.was_already_erroring {
        reject_promise(vm, pa.promise, stored_error);
        reject_close_and_closed_promise_if_needed(vm, streams, ws_h);
        return;
    }

    // Abort signal was already signaled during `writable_abort_internal`, ensure signaled.
    if let Some((ac, wa)) = ac_info {
        trigger_internal_abort(vm, &wa, ac, pa.reason);
    }

    let abort_p = if let Some(af) = abort_fn {
        let this_v = sink_obj.map_or(JsValue::UNDEFINED, JsValue::from_object);
        call_returning_promise(vm, af, this_v, &[pa.reason])
    } else {
        promise_resolve_value(vm, JsValue::UNDEFINED)
    };

    let ss_ok = Rc::clone(streams);
    let pa_p1 = pa.promise;
    let on_ok = vm.alloc_host_fn("ws_abort_ok", move |vm, _, _| {
        resolve_promise(vm, pa_p1, JsValue::UNDEFINED);
        reject_close_and_closed_promise_if_needed(vm, &ss_ok, ws_h);
        Ok(JsValue::UNDEFINED)
    });
    let ss_err = Rc::clone(streams);
    let pa_p2 = pa.promise;
    let on_err = vm.alloc_host_fn("ws_abort_err", move |vm, _, a| {
        reject_promise(vm, pa_p2, arg(a, 0));
        reject_close_and_closed_promise_if_needed(vm, &ss_err, ws_h);
        Ok(JsValue::UNDEFINED)
    });
    let _ = promise_then(vm, abort_p, Some(on_ok), Some(on_err));
}

fn writable_deal_with_rejection(
    vm: &mut JsVm,
    streams: &SharedStreams,
    ws_h: HeapHandle,
    error: JsValue,
) {
    let state = streams.borrow().writable.get(&ws_h).map(|ws| ws.state);
    match state {
        Some(WritableState::Writable) => writable_start_erroring(vm, streams, ws_h, error),
        Some(WritableState::Erroring) => writable_finish_erroring(vm, streams, ws_h),
        _ => {}
    }
}

fn writable_advance_queue_if_needed(vm: &mut JsVm, streams: &SharedStreams, ws_h: HeapHandle) {
    enum NextAction {
        None,
        FinishErroring,
        ProcessClose(Option<HeapHandle>, Option<HeapHandle>),
        ProcessWrite(
            JsValue,
            Option<HeapHandle>,
            Option<HeapHandle>,
            Option<HeapHandle>,
        ),
    }

    let action = {
        let mut st = streams.borrow_mut();
        let Some(ws) = st.writable.get_mut(&ws_h) else {
            return;
        };
        if !ws.started || ws.in_flight_write_request.is_some() {
            return;
        }
        if ws.state == WritableState::Erroring {
            NextAction::FinishErroring
        } else if ws.queue.is_empty() {
            NextAction::None
        } else {
            match ws.queue.front().map(|e| e.item.clone()) {
                Some(WritableQueueItem::Close) => {
                    let _ = ws.dequeue();
                    ws.in_flight_close_request = ws.close_request.take();
                    let cf = ws.close_fn;
                    let sink = ws.sink_obj;
                    ws.clear_algorithms();
                    NextAction::ProcessClose(cf, sink)
                }
                Some(WritableQueueItem::Write(chunk)) => {
                    ws.in_flight_write_request = ws.write_requests.pop_front();
                    NextAction::ProcessWrite(chunk, ws.write_fn, ws.sink_obj, ws.controller)
                }
                None => NextAction::None,
            }
        }
    };

    match action {
        NextAction::None => {}
        NextAction::FinishErroring => writable_finish_erroring(vm, streams, ws_h),
        NextAction::ProcessClose(cf, sink) => {
            let p = if let Some(cf_h) = cf {
                let this_v = sink.map_or(JsValue::UNDEFINED, JsValue::from_object);
                call_returning_promise(vm, cf_h, this_v, &[])
            } else {
                promise_resolve_value(vm, JsValue::UNDEFINED)
            };
            let ss_ok = Rc::clone(streams);
            let on_ok = vm.alloc_host_fn("ws_close_ok", move |vm, _, _| {
                let (if_cr, writer_cp, pending_ab) = {
                    let mut st = ss_ok.borrow_mut();
                    let Some(ws) = st.writable.get_mut(&ws_h) else {
                        return Ok(JsValue::UNDEFINED);
                    };
                    let if_cr = ws.in_flight_close_request.take();
                    let pa = ws.pending_abort_request.take();
                    ws.state = WritableState::Closed;
                    let cp = ws
                        .writer
                        .and_then(|wh| st.writable_writers.get(&wh))
                        .map(|w| w.closed_promise);
                    (if_cr, cp, pa)
                };
                if let Some(cr) = if_cr {
                    resolve_promise(vm, cr, JsValue::UNDEFINED);
                }
                if let Some(pa) = pending_ab {
                    resolve_promise(vm, pa.promise, JsValue::UNDEFINED);
                }
                if let Some(cp) = writer_cp {
                    resolve_promise(vm, cp, JsValue::UNDEFINED);
                }
                Ok(JsValue::UNDEFINED)
            });
            let ss_err = Rc::clone(streams);
            let on_err = vm.alloc_host_fn("ws_close_err", move |vm, _, a| {
                let err = arg(a, 0);
                // WHATWG §5.5.4 WritableStreamFinishInFlightCloseWithError:
                // if an abort request is pending while close() was already in flight,
                // reject the abort promise with the close error and do NOT invoke sink.abort().
                let (if_cr, pending_ab) = {
                    let mut st = ss_err.borrow_mut();
                    match st.writable.get_mut(&ws_h) {
                        Some(ws) => (
                            ws.in_flight_close_request.take(),
                            ws.pending_abort_request.take(),
                        ),
                        None => (None, None),
                    }
                };
                if let Some(cr) = if_cr {
                    reject_promise(vm, cr, err);
                }
                if let Some(pa) = pending_ab {
                    reject_promise(vm, pa.promise, err);
                }
                writable_deal_with_rejection(vm, &ss_err, ws_h, err);
                Ok(JsValue::UNDEFINED)
            });
            let _ = promise_then(vm, p, Some(on_ok), Some(on_err));
        }
        NextAction::ProcessWrite(chunk, wf, sink, ctrl) => {
            let p = if let Some(wf_h) = wf {
                let this_v = sink.map_or(JsValue::UNDEFINED, JsValue::from_object);
                let ctrl_v = ctrl.map_or(JsValue::UNDEFINED, JsValue::from_object);
                // WHATWG Streams §5.5.4: sink.write(chunk, controller) takes 2 arguments!
                call_returning_promise(vm, wf_h, this_v, &[chunk, ctrl_v])
            } else {
                promise_resolve_value(vm, JsValue::UNDEFINED)
            };
            let ss_ok = Rc::clone(streams);
            let on_ok = vm.alloc_host_fn("ws_write_ok", move |vm, _, _| {
                let (if_wr, next_bp_opt) = {
                    let mut st = ss_ok.borrow_mut();
                    let Some(ws) = st.writable.get_mut(&ws_h) else {
                        return Ok(JsValue::UNDEFINED);
                    };
                    let if_wr = ws.in_flight_write_request.take();
                    let _ = ws.dequeue();
                    let next_bp =
                        if !ws.close_queued_or_in_flight() && ws.state == WritableState::Writable {
                            Some(ws.compute_backpressure())
                        } else {
                            None
                        };
                    (if_wr, next_bp)
                };
                if let Some(wr) = if_wr {
                    resolve_promise(vm, wr, JsValue::UNDEFINED);
                }
                if let Some(bp) = next_bp_opt {
                    update_backpressure(vm, &ss_ok, ws_h, bp);
                }
                writable_advance_queue_if_needed(vm, &ss_ok, ws_h);
                Ok(JsValue::UNDEFINED)
            });
            let ss_err = Rc::clone(streams);
            let on_err = vm.alloc_host_fn("ws_write_err", move |vm, _, a| {
                let err = arg(a, 0);
                let if_wr = ss_err
                    .borrow_mut()
                    .writable
                    .get_mut(&ws_h)
                    .and_then(|ws| ws.in_flight_write_request.take());
                if let Some(wr) = if_wr {
                    reject_promise(vm, wr, err);
                }
                writable_deal_with_rejection(vm, &ss_err, ws_h, err);
                Ok(JsValue::UNDEFINED)
            });
            let _ = promise_then(vm, p, Some(on_ok), Some(on_err));
        }
    }
}

pub(crate) fn writable_write_internal(
    vm: &mut JsVm,
    streams: &SharedStreams,
    ws_h: HeapHandle,
    chunk: JsValue,
) -> HeapHandle {
    writable_write_with_writer(vm, streams, ws_h, None, chunk)
}

fn writable_write_with_writer(
    vm: &mut JsVm,
    streams: &SharedStreams,
    ws_h: HeapHandle,
    writer_opt: Option<HeapHandle>,
    chunk: JsValue,
) -> HeapHandle {
    let size_fn = streams
        .borrow()
        .writable
        .get(&ws_h)
        .and_then(|ws| ws.size_fn);
    // WHATWG Streams §5.5.4 WritableStreamDefaultControllerGetChunkSize:
    // if `size()` throws, call `ErrorIfNeeded(e)` and continue with chunkSize = 1.
    let chunk_size = match call_size_algorithm(vm, size_fn, chunk) {
        Ok(sz) => sz,
        Err(e) => {
            writable_start_erroring(vm, streams, ws_h, e.value);
            1.0
        }
    };

    // Re-check writer lock ownership after user `size()` (re-entrancy: size() may call releaseLock()).
    if let Some(wh) = writer_opt {
        let still_owner = streams
            .borrow()
            .writable_writers
            .get(&wh)
            .is_some_and(|w| w.stream == Some(ws_h));
        if !still_owner {
            let err = vm.type_error("Writer lock was released during strategy.size()");
            return promise_reject_value(vm, err.value);
        }
    }

    let (out_p, next_bp_opt, should_advance) = {
        let mut st = streams.borrow_mut();
        let Some(ws) = st.writable.get_mut(&ws_h) else {
            return promise_resolve_value(vm, JsValue::UNDEFINED);
        };
        if ws.state == WritableState::Errored || ws.state == WritableState::Erroring {
            return promise_reject_value(vm, ws.stored_error);
        }
        if ws.state == WritableState::Closed || ws.close_queued_or_in_flight() {
            let err = vm.type_error("Cannot write to a closed or closing WritableStream");
            return promise_reject_value(vm, err.value);
        }
        let p = alloc_promise(vm);
        ws.write_requests.push_back(p);
        if let Err(e) = ws.enqueue_write(vm, chunk, chunk_size) {
            // Rollback write request and error stream.
            ws.write_requests.pop_back();
            drop(st);
            writable_start_erroring(vm, streams, ws_h, e.value);
            return promise_reject_value(vm, e.value);
        }
        let next_bp = if !ws.close_queued_or_in_flight() && ws.state == WritableState::Writable {
            Some(ws.compute_backpressure())
        } else {
            None
        };
        (p, next_bp, true)
    };

    if let Some(bp) = next_bp_opt {
        update_backpressure(vm, streams, ws_h, bp);
    }
    if should_advance {
        writable_advance_queue_if_needed(vm, streams, ws_h);
    }
    out_p
}

pub(crate) fn writable_close_internal(
    vm: &mut JsVm,
    streams: &SharedStreams,
    ws_h: HeapHandle,
) -> HeapHandle {
    let (out_p, writer_ready_to_resolve) = {
        let mut st = streams.borrow_mut();
        let Some(ws) = st.writable.get_mut(&ws_h) else {
            return promise_resolve_value(vm, JsValue::UNDEFINED);
        };
        if ws.state == WritableState::Closed || ws.state == WritableState::Errored {
            let err = vm.type_error("Cannot close a closed or errored WritableStream");
            return promise_reject_value(vm, err.value);
        }
        if ws.close_queued_or_in_flight() {
            let err = vm.type_error("WritableStream is already closing");
            return promise_reject_value(vm, err.value);
        }
        let p = alloc_promise(vm);
        ws.close_request = Some(p);
        let ready_writer = if ws.backpressure && ws.state == WritableState::Writable {
            ws.writer
        } else {
            None
        };
        ws.enqueue_close();
        let ready_to_res = ready_writer
            .and_then(|wh| st.writable_writers.get(&wh))
            .map(|w| w.ready_promise);
        (p, ready_to_res)
    };
    if let Some(rp) = writer_ready_to_resolve {
        resolve_promise(vm, rp, JsValue::UNDEFINED);
    }
    writable_advance_queue_if_needed(vm, streams, ws_h);
    out_p
}

pub(crate) fn writable_abort_internal(
    vm: &mut JsVm,
    streams: &SharedStreams,
    ws_h: HeapHandle,
    reason: JsValue,
) -> HeapHandle {
    let (pre_state, ac_info) = {
        let st = streams.borrow();
        let Some(ws) = st.writable.get(&ws_h) else {
            return promise_resolve_value(vm, JsValue::UNDEFINED);
        };
        (ws.state, ws.abort_controller.zip(ws.web_apis.clone()))
    };
    if pre_state == WritableState::Closed || pre_state == WritableState::Errored {
        return promise_resolve_value(vm, JsValue::UNDEFINED);
    }

    // Signal `controller.signal` abort BEFORE checking pendingAbortRequest (per §5.4.3).
    if let Some((ac, wa)) = ac_info {
        trigger_internal_abort(vm, &wa, ac, reason);
    }

    let (out_p, should_start_erroring) = {
        let mut st = streams.borrow_mut();
        let Some(ws) = st.writable.get_mut(&ws_h) else {
            return promise_resolve_value(vm, JsValue::UNDEFINED);
        };
        if ws.state == WritableState::Closed || ws.state == WritableState::Errored {
            return promise_resolve_value(vm, JsValue::UNDEFINED);
        }
        if let Some(ref existing) = ws.pending_abort_request {
            return existing.promise;
        }
        let was_already_erroring = ws.state == WritableState::Erroring;
        let p = alloc_promise(vm);
        ws.pending_abort_request = Some(PendingAbortRequest {
            promise: p,
            reason: if was_already_erroring {
                JsValue::UNDEFINED
            } else {
                reason
            },
            was_already_erroring,
        });
        (p, !was_already_erroring)
    };
    if should_start_erroring {
        writable_start_erroring(vm, streams, ws_h, reason);
    }
    out_p
}

pub(crate) fn acquire_default_writer(
    vm: &mut JsVm,
    streams: &SharedStreams,
    ws_h: HeapHandle,
) -> Result<HeapHandle, JsException> {
    let writer_proto = streams
        .borrow()
        .writable_writer_proto
        .unwrap_or(vm.object_proto);
    let closed_p = alloc_promise(vm);
    let ready_p = alloc_promise(vm);
    let writer = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(writer_proto),
    ));

    let init = {
        let mut st = streams.borrow_mut();
        let Some(ws) = st.writable.get_mut(&ws_h) else {
            return Err(vm.type_error("Invalid WritableStream"));
        };
        if ws.writer.is_some() {
            return Err(vm.type_error("WritableStream is already locked to a writer"));
        }
        ws.writer = Some(writer);
        let init = (
            ws.state,
            ws.stored_error,
            ws.backpressure,
            ws.close_queued_or_in_flight(),
        );
        st.writable_writers.insert(
            writer,
            WritableDefaultWriterState {
                stream: Some(ws_h),
                closed_promise: closed_p,
                ready_promise: ready_p,
            },
        );
        init
    };
    attach_gc_anchor(vm, writer, ws_h);

    match init.0 {
        WritableState::Writable => {
            if !init.2 || init.3 {
                resolve_promise(vm, ready_p, JsValue::UNDEFINED);
            }
        }
        WritableState::Erroring => {
            reject_promise(vm, ready_p, init.1);
            promise_mark_handled(vm, ready_p);
        }
        WritableState::Closed => {
            resolve_promise(vm, ready_p, JsValue::UNDEFINED);
            resolve_promise(vm, closed_p, JsValue::UNDEFINED);
        }
        WritableState::Errored => {
            reject_promise(vm, ready_p, init.1);
            promise_mark_handled(vm, ready_p);
            reject_promise(vm, closed_p, init.1);
            promise_mark_handled(vm, closed_p);
        }
    }
    Ok(writer)
}

pub(crate) fn release_default_writer(vm: &mut JsVm, streams: &SharedStreams, writer_h: HeapHandle) {
    let info = {
        let mut st = streams.borrow_mut();
        let Some(w) = st.writable_writers.get_mut(&writer_h) else {
            return;
        };
        let Some(ws_h) = w.stream.take() else {
            return;
        };
        let cp = w.closed_promise;
        let rp = w.ready_promise;
        if let Some(ws) = st.writable.get_mut(&ws_h) {
            ws.writer = None;
        }
        (cp, rp)
    };
    let rel_err = vm.type_error("Writer lock was released").value;
    // WHATWG Streams §5.4.4 WritableStreamDefaultWriterRelease:
    // reject `ready` promise BEFORE `closed` promise!
    if promise_is_pending(vm, info.1) {
        reject_promise(vm, info.1, rel_err);
        promise_mark_handled(vm, info.1);
    } else {
        let new_rp = promise_reject_value(vm, rel_err);
        promise_mark_handled(vm, new_rp);
        if let Some(w) = streams.borrow_mut().writable_writers.get_mut(&writer_h) {
            w.ready_promise = new_rp;
        }
    }
    if promise_is_pending(vm, info.0) {
        reject_promise(vm, info.0, rel_err);
        promise_mark_handled(vm, info.0);
    } else {
        let new_cp = promise_reject_value(vm, rel_err);
        promise_mark_handled(vm, new_cp);
        if let Some(w) = streams.borrow_mut().writable_writers.get_mut(&writer_h) {
            w.closed_promise = new_cp;
        }
    }
}

/// Creates a `WritableStream` backed by host callbacks (`write`, `close`, `abort`),
/// used by `dom_sanitizer.rs` streaming HTML parsing and `TextEncoderStream`/`TextDecoderStream`.
pub(crate) fn create_writable_stream_from_callbacks(
    vm: &mut JsVm,
    streams: &SharedStreams,
    write_fn: HeapHandle,
    close_fn: HeapHandle,
    abort_fn: HeapHandle,
) -> HeapHandle {
    create_writable_stream_with_strategy(vm, streams, write_fn, close_fn, abort_fn, 1.0, None)
}

pub(crate) fn create_writable_stream_with_strategy(
    vm: &mut JsVm,
    streams: &SharedStreams,
    write_fn: HeapHandle,
    close_fn: HeapHandle,
    abort_fn: HeapHandle,
    hwm: f64,
    size_fn: Option<HeapHandle>,
) -> HeapHandle {
    let (wsp, wcp) = {
        let s = streams.borrow();
        (
            s.writable_proto.unwrap_or(vm.object_proto),
            s.writable_controller_proto.unwrap_or(vm.object_proto),
        )
    };
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(wsp)));
    let ctrl = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(wcp)));
    let mut ws = WritableStreamState::new(
        hwm,
        size_fn,
        Some(write_fn),
        Some(close_fn),
        Some(abort_fn),
        None,
    );
    ws.started = true;
    ws.backpressure = ws.compute_backpressure();
    ws.controller = Some(ctrl);
    attach_gc_anchor(vm, ctrl, obj);
    let mut reg = streams.borrow_mut();
    reg.writable.insert(obj, ws);
    reg.writable_controllers.insert(ctrl, obj);
    obj
}

fn require_writable_controller(
    vm: &mut JsVm,
    streams: &SharedStreams,
    val: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible WritableStreamDefaultController receiver"));
    };
    streams
        .borrow()
        .writable_controllers
        .get(&h)
        .copied()
        .ok_or_else(|| vm.type_error("Incompatible WritableStreamDefaultController receiver"))
}

fn require_writable_writer(
    vm: &mut JsVm,
    streams: &SharedStreams,
    val: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible WritableStreamDefaultWriter receiver"));
    };
    if streams.borrow().writable_writers.contains_key(&h) {
        Ok(h)
    } else {
        Err(vm.type_error("Incompatible WritableStreamDefaultWriter receiver"))
    }
}

/// Installs `WritableStream`, `WritableStreamDefaultWriter`, and `WritableStreamDefaultController`.
pub(crate) fn install_writable_stream_types(
    vm: &mut JsVm,
    streams: &SharedStreams,
    web_apis: &SharedWebApis,
) -> HeapHandle {
    // --- WritableStreamDefaultController ---
    let wcp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(wcp).unwrap().is_prototype = true;
    set_to_string_tag(vm, wcp, "WritableStreamDefaultController");
    streams.borrow_mut().writable_controller_proto = Some(wcp);

    let ss = Rc::clone(streams);
    define_host_getter(vm, wcp, "signal", move |vm, this_val, _| {
        let ws_h = require_writable_controller(vm, &ss, this_val)?;
        let sig = ss
            .borrow()
            .writable
            .get(&ws_h)
            .and_then(|ws| ws.abort_signal);
        Ok(sig.map_or(JsValue::UNDEFINED, JsValue::from_object))
    });
    let ss = Rc::clone(streams);
    define_host_method(vm, wcp, "error", move |vm, this_val, args| {
        let ws_h = require_writable_controller(vm, &ss, this_val)?;
        let is_writable = ss
            .borrow()
            .writable
            .get(&ws_h)
            .is_some_and(|ws| ws.state == WritableState::Writable);
        if is_writable {
            writable_start_erroring(vm, &ss, ws_h, arg(args, 0));
        }
        Ok(JsValue::UNDEFINED)
    });
    let wc_ctor = vm.alloc_host_fn("WritableStreamDefaultController", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: WritableStreamDefaultController"))
    });
    bind_ctor_proto(vm, wc_ctor, wcp);
    define_global(
        vm,
        "WritableStreamDefaultController",
        JsValue::from_object(wc_ctor),
    );

    // --- WritableStreamDefaultWriter ---
    let wwp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(wwp).unwrap().is_prototype = true;
    set_to_string_tag(vm, wwp, "WritableStreamDefaultWriter");
    streams.borrow_mut().writable_writer_proto = Some(wwp);

    let ss = Rc::clone(streams);
    define_host_getter(vm, wwp, "closed", move |vm, this_val, _| {
        let Some(wh) = this_val.as_object() else {
            let err = vm.type_error("Incompatible WritableStreamDefaultWriter receiver");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        };
        match ss
            .borrow()
            .writable_writers
            .get(&wh)
            .map(|w| w.closed_promise)
        {
            Some(cp) => Ok(JsValue::from_object(cp)),
            None => {
                let err = vm.type_error("Incompatible WritableStreamDefaultWriter receiver");
                Ok(JsValue::from_object(promise_reject_value(vm, err.value)))
            }
        }
    });
    let ss = Rc::clone(streams);
    define_host_getter(vm, wwp, "ready", move |vm, this_val, _| {
        let Some(wh) = this_val.as_object() else {
            let err = vm.type_error("Incompatible WritableStreamDefaultWriter receiver");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        };
        match ss
            .borrow()
            .writable_writers
            .get(&wh)
            .map(|w| w.ready_promise)
        {
            Some(rp) => Ok(JsValue::from_object(rp)),
            None => {
                let err = vm.type_error("Incompatible WritableStreamDefaultWriter receiver");
                Ok(JsValue::from_object(promise_reject_value(vm, err.value)))
            }
        }
    });
    let ss = Rc::clone(streams);
    define_host_getter(vm, wwp, "desiredSize", move |vm, this_val, _| {
        let wh = require_writable_writer(vm, &ss, this_val)?;
        let stream_h = ss
            .borrow()
            .writable_writers
            .get(&wh)
            .and_then(|w| w.stream)
            .ok_or_else(|| vm.type_error("Writer lock has been released"))?;
        let ds = ss
            .borrow()
            .writable
            .get(&stream_h)
            .and_then(WritableStreamState::desired_size);
        Ok(ds.map_or(JsValue::NULL, JsValue::from_f64))
    });
    let ss = Rc::clone(streams);
    define_host_method(vm, wwp, "write", move |vm, this_val, args| {
        let Some(wh) = this_val.as_object() else {
            let err = vm.type_error("Incompatible WritableStreamDefaultWriter receiver");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        };
        let stream_opt = match ss.borrow().writable_writers.get(&wh) {
            Some(w) => w.stream,
            None => {
                let err = vm.type_error("Incompatible WritableStreamDefaultWriter receiver");
                return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
            }
        };
        let Some(ws_h) = stream_opt else {
            let err = vm.type_error("Cannot write to a released WritableStreamDefaultWriter");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        };
        Ok(JsValue::from_object(writable_write_with_writer(
            vm,
            &ss,
            ws_h,
            Some(wh),
            arg(args, 0),
        )))
    });
    let ss = Rc::clone(streams);
    define_host_method(vm, wwp, "close", move |vm, this_val, _| {
        let Some(wh) = this_val.as_object() else {
            let err = vm.type_error("Incompatible WritableStreamDefaultWriter receiver");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        };
        let stream_opt = match ss.borrow().writable_writers.get(&wh) {
            Some(w) => w.stream,
            None => {
                let err = vm.type_error("Incompatible WritableStreamDefaultWriter receiver");
                return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
            }
        };
        let Some(ws_h) = stream_opt else {
            let err = vm.type_error("Cannot close a released WritableStreamDefaultWriter");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        };
        let p = writable_close_internal(vm, &ss, ws_h);
        Ok(JsValue::from_object(p))
    });
    let ss = Rc::clone(streams);
    define_host_method(vm, wwp, "abort", move |vm, this_val, args| {
        let Some(wh) = this_val.as_object() else {
            let err = vm.type_error("Incompatible WritableStreamDefaultWriter receiver");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        };
        let stream_opt = match ss.borrow().writable_writers.get(&wh) {
            Some(w) => w.stream,
            None => {
                let err = vm.type_error("Incompatible WritableStreamDefaultWriter receiver");
                return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
            }
        };
        let Some(ws_h) = stream_opt else {
            let err = vm.type_error("Cannot abort a released WritableStreamDefaultWriter");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        };
        Ok(JsValue::from_object(writable_abort_internal(
            vm,
            &ss,
            ws_h,
            arg(args, 0),
        )))
    });
    let ss = Rc::clone(streams);
    define_host_method(vm, wwp, "releaseLock", move |vm, this_val, _| {
        let wh = require_writable_writer(vm, &ss, this_val)?;
        release_default_writer(vm, &ss, wh);
        Ok(JsValue::UNDEFINED)
    });

    let ss = Rc::clone(streams);
    let ww_ctor = vm.alloc_host_fn("WritableStreamDefaultWriter", move |vm, _, args| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("WritableStreamDefaultWriter requires 'new'"));
        }
        let ws_h = require_writable(vm, &ss, arg(args, 0))?;
        let writer = acquire_default_writer(vm, &ss, ws_h)?;
        Ok(JsValue::from_object(writer))
    });
    vm.set_fn_length(ww_ctor, 1);
    bind_ctor_proto(vm, ww_ctor, wwp);
    define_global(
        vm,
        "WritableStreamDefaultWriter",
        JsValue::from_object(ww_ctor),
    );

    // --- WritableStream ---
    let wsp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(wsp).unwrap().is_prototype = true;
    set_to_string_tag(vm, wsp, "WritableStream");
    streams.borrow_mut().writable_proto = Some(wsp);

    let ss = Rc::clone(streams);
    define_host_getter(vm, wsp, "locked", move |vm, this_val, _| {
        let h = require_writable(vm, &ss, this_val)?;
        let locked = ss
            .borrow()
            .writable
            .get(&h)
            .is_some_and(|ws| ws.writer.is_some());
        Ok(JsValue::from_bool(locked))
    });
    let ss = Rc::clone(streams);
    define_host_method(vm, wsp, "getWriter", move |vm, this_val, _| {
        let h = require_writable(vm, &ss, this_val)?;
        let writer = acquire_default_writer(vm, &ss, h)?;
        Ok(JsValue::from_object(writer))
    });
    let ss = Rc::clone(streams);
    define_host_method(vm, wsp, "abort", move |vm, this_val, args| {
        let h = match require_writable(vm, &ss, this_val) {
            Ok(h) => h,
            Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
        };
        let is_locked = ss
            .borrow()
            .writable
            .get(&h)
            .is_some_and(|ws| ws.writer.is_some());
        if is_locked {
            let err = vm.type_error("Cannot abort a locked WritableStream");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        }
        Ok(JsValue::from_object(writable_abort_internal(
            vm,
            &ss,
            h,
            arg(args, 0),
        )))
    });
    let ss = Rc::clone(streams);
    define_host_method(vm, wsp, "close", move |vm, this_val, _| {
        let h = match require_writable(vm, &ss, this_val) {
            Ok(h) => h,
            Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
        };
        let is_locked = ss
            .borrow()
            .writable
            .get(&h)
            .is_some_and(|ws| ws.writer.is_some());
        if is_locked {
            let err = vm.type_error("Cannot close a locked WritableStream");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        }
        let closing = ss
            .borrow()
            .writable
            .get(&h)
            .is_some_and(WritableStreamState::close_queued_or_in_flight);
        if closing {
            let err = vm.type_error("Cannot close a WritableStream that is already closing");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        }
        let p = writable_close_internal(vm, &ss, h);
        Ok(JsValue::from_object(p))
    });

    let ss = Rc::clone(streams);
    let wa = Rc::clone(web_apis);
    let ws_ctor = vm.alloc_host_fn("WritableStream", move |vm, _, args| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("WritableStream constructor requires 'new'"));
        }
        let sink_val = arg(args, 0);
        if !sink_val.is_undefined() && !sink_val.is_object() {
            return Err(vm.type_error("WritableStream underlyingSink must be an object"));
        }
        let (hwm_val_opt, size_fn) = extract_strategy(vm, arg(args, 1))?;

        let mut abort_fn = None;
        let mut close_fn = None;
        let mut start_fn = None;
        let mut write_fn = None;
        let sink_obj = sink_val.as_object();
        if let Some(sh) = sink_obj {
            // WebIDL dictionary `UnderlyingSink` alphabetical member conversion:
            // `abort`, `close`, `start`, `type`, `write`.
            abort_fn = get_method_checked(vm, sh, "abort")?;
            close_fn = get_method_checked(vm, sh, "close")?;
            start_fn = get_method_checked(vm, sh, "start")?;
            let k_type = vm.heap.intern_key("type");
            let tv = vm.get_property_by_key(sh, k_type)?;
            if !tv.is_undefined() {
                return Err(vm.range_error("Invalid WritableStream type"));
            }
            write_fn = get_method_checked(vm, sh, "write")?;
        }
        let hwm = match hwm_val_opt {
            Some(v) => validate_and_normalize_hwm(vm, v)?,
            None => 1.0,
        };

        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(wsp)));
        let ctrl = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(wcp)));
        let ac_pair = create_internal_abort_controller(vm, &wa);

        let mut ws = WritableStreamState::new(hwm, size_fn, write_fn, close_fn, abort_fn, sink_obj);
        ws.controller = Some(ctrl);
        ws.backpressure = ws.compute_backpressure();
        if let Some((ac_h, sig_h)) = ac_pair {
            ws.abort_controller = Some(ac_h);
            ws.abort_signal = Some(sig_h);
            ws.web_apis = Some(Rc::clone(&wa));
        }
        attach_gc_anchor(vm, ctrl, obj);
        {
            let mut reg = ss.borrow_mut();
            reg.writable.insert(obj, ws);
            reg.writable_controllers.insert(ctrl, obj);
        }

        let start_res = if let Some(sf) = start_fn {
            vm.call_value(
                JsValue::from_object(sf),
                sink_val,
                &[JsValue::from_object(ctrl)],
            )?
        } else {
            JsValue::UNDEFINED
        };
        let start_p = promise_resolve_value(vm, start_res);
        let ss_ok = Rc::clone(&ss);
        let on_ok = vm.alloc_host_fn("ws_start_ok", move |vm, _, _| {
            if let Some(ws) = ss_ok.borrow_mut().writable.get_mut(&obj) {
                ws.started = true;
            }
            writable_advance_queue_if_needed(vm, &ss_ok, obj);
            Ok(JsValue::UNDEFINED)
        });
        let ss_err = Rc::clone(&ss);
        let on_err = vm.alloc_host_fn("ws_start_err", move |vm, _, a| {
            if let Some(ws) = ss_err.borrow_mut().writable.get_mut(&obj) {
                ws.started = true;
            }
            writable_deal_with_rejection(vm, &ss_err, obj, arg(a, 0));
            Ok(JsValue::UNDEFINED)
        });
        let _ = promise_then(vm, start_p, Some(on_ok), Some(on_err));

        Ok(JsValue::from_object(obj))
    });
    bind_ctor_proto(vm, ws_ctor, wsp);
    define_global(vm, "WritableStream", JsValue::from_object(ws_ctor));

    wsp
}

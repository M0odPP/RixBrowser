//! WHATWG Streams Standard §4.8 (`ReadableStreamPipeTo` & `pipeThrough`)
//! supporting `preventClose`, `preventAbort`, `preventCancel`, `AbortSignal` cancellation,
//! backpressure waiting on `writer.ready`, exact shutdown/error/close propagation ordering,
//! and zero-interception internal `ReadRequest` pumping.

use std::cell::RefCell;
use std::rc::Rc;

use rix_js::promise::{
    alloc_promise, promise_resolve_value, promise_then, reject_promise, resolve_promise,
};
use rix_js::{HeapHandle, JsException, JsValue, JsVm};

use crate::streams::{
    acquire_default_reader, readable_cancel_internal, readable_read_with_request,
    release_default_reader, require_readable, require_writable, ReadRequest, ReadableState,
    SharedStreams,
};
use crate::streams_common::{promise_mark_handled, promise_reject_value};
use crate::streams_writable::{
    acquire_default_writer, release_default_writer, writable_abort_internal,
    writable_close_with_error_propagation, writable_write_internal, WritableState,
};
use crate::web_apis::{arg, create_dom_exception, SharedWebApis};

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PipeOptions {
    pub(crate) prevent_close: bool,
    pub(crate) prevent_abort: bool,
    pub(crate) prevent_cancel: bool,
    pub(crate) signal: Option<HeapHandle>,
}

/// Converts WebIDL `StreamPipeOptions` dictionary in exact alphabetical member order:
/// `preventAbort`, `preventCancel`, `preventClose`, `signal`.
pub(crate) fn extract_pipe_options(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    val: JsValue,
) -> Result<PipeOptions, JsException> {
    if val.is_undefined() || val.is_null() {
        return Ok(PipeOptions::default());
    }
    let Some(oh) = val.as_object() else {
        return Err(vm.type_error("StreamPipeOptions must be an object"));
    };
    let k_pa = vm.heap.intern_key("preventAbort");
    let v_pa = vm.get_property_by_key(oh, k_pa)?;
    let prevent_abort = vm.to_boolean(v_pa);

    let k_pc = vm.heap.intern_key("preventCancel");
    let v_pc = vm.get_property_by_key(oh, k_pc)?;
    let prevent_cancel = vm.to_boolean(v_pc);

    let k_pcl = vm.heap.intern_key("preventClose");
    let v_pcl = vm.get_property_by_key(oh, k_pcl)?;
    let prevent_close = vm.to_boolean(v_pcl);

    let k_sig = vm.heap.intern_key("signal");
    let v_sig = vm.get_property_by_key(oh, k_sig)?;
    let signal = if v_sig.is_undefined() {
        None
    } else {
        let Some(sh) = v_sig.as_object() else {
            return Err(vm.type_error("StreamPipeOptions.signal must be an AbortSignal"));
        };
        if !web_apis.borrow().abort_signals.contains_key(&sh) {
            return Err(vm.type_error("StreamPipeOptions.signal must be an AbortSignal"));
        }
        Some(sh)
    };
    Ok(PipeOptions {
        prevent_close,
        prevent_abort,
        prevent_cancel,
        signal,
    })
}

/// Implements `ReadableStream.prototype.pipeThrough(transform, options)` per WHATWG §4.2.4.
pub(crate) fn readable_pipe_through(
    vm: &mut JsVm,
    streams: &SharedStreams,
    web_apis: &SharedWebApis,
    this_val: JsValue,
    transform_val: JsValue,
    options_val: JsValue,
) -> Result<JsValue, JsException> {
    let src_h = require_readable(vm, streams, this_val)?;
    let Some(tr_h) = transform_val.as_object() else {
        return Err(vm.type_error("pipeThrough requires a { readable, writable } object"));
    };
    // WebIDL `ReadableWritablePair` dictionary converts `readable` first, then `writable`.
    let kr = vm.heap.intern_key("readable");
    let readable = vm.get_property_by_key(tr_h, kr)?;
    let _ = require_readable(vm, streams, readable)?;
    let kw = vm.heap.intern_key("writable");
    let writable = vm.get_property_by_key(tr_h, kw)?;
    let dst_h = require_writable(vm, streams, writable)?;
    let opts = extract_pipe_options(vm, web_apis, options_val)?;

    let src_locked = streams
        .borrow()
        .readable
        .get(&src_h)
        .is_some_and(|rs| rs.reader.is_some());
    if src_locked {
        return Err(vm.type_error("Cannot pipeThrough a locked ReadableStream"));
    }
    let dst_locked = streams
        .borrow()
        .writable
        .get(&dst_h)
        .is_some_and(|ws| ws.writer.is_some());
    if dst_locked {
        return Err(vm.type_error("Cannot pipeThrough into a locked WritableStream"));
    }

    let p = readable_stream_pipe_to(vm, streams, web_apis, src_h, dst_h, opts);
    promise_mark_handled(vm, p);
    Ok(readable)
}

/// Implements `ReadableStream.prototype.pipeTo(destination, options)` per WHATWG §4.2.4 / §4.8.
pub(crate) fn readable_pipe_to_method(
    vm: &mut JsVm,
    streams: &SharedStreams,
    web_apis: &SharedWebApis,
    this_val: JsValue,
    dest_val: JsValue,
    options_val: JsValue,
) -> HeapHandle {
    let src_h = match require_readable(vm, streams, this_val) {
        Ok(h) => h,
        Err(e) => return promise_reject_value(vm, e.value),
    };
    let dst_h = match require_writable(vm, streams, dest_val) {
        Ok(h) => h,
        Err(e) => return promise_reject_value(vm, e.value),
    };
    let opts = match extract_pipe_options(vm, web_apis, options_val) {
        Ok(o) => o,
        Err(e) => return promise_reject_value(vm, e.value),
    };
    let src_locked = streams
        .borrow()
        .readable
        .get(&src_h)
        .is_some_and(|rs| rs.reader.is_some());
    if src_locked {
        let err = vm.type_error("Cannot pipeTo a locked ReadableStream");
        return promise_reject_value(vm, err.value);
    }
    let dst_locked = streams
        .borrow()
        .writable
        .get(&dst_h)
        .is_some_and(|ws| ws.writer.is_some());
    if dst_locked {
        let err = vm.type_error("Cannot pipeTo a locked WritableStream");
        return promise_reject_value(vm, err.value);
    }
    readable_stream_pipe_to(vm, streams, web_apis, src_h, dst_h, opts)
}

struct PipeState {
    source: HeapHandle,
    dest: HeapHandle,
    reader: HeapHandle,
    writer: HeapHandle,
    opts: PipeOptions,
    shutting_down: bool,
    promise: HeapHandle,
    current_write: HeapHandle,
    abort_listener: Option<HeapHandle>,
}

fn sync_pipe_gc_roots(streams: &SharedStreams, st: &Rc<RefCell<PipeState>>) {
    let ps = st.borrow();
    let mut handles = vec![ps.source, ps.dest, ps.reader, ps.writer, ps.current_write];
    if let Some(s) = ps.opts.signal {
        handles.push(s);
    }
    if let Some(l) = ps.abort_listener {
        handles.push(l);
    }
    streams
        .borrow_mut()
        .active_pipes
        .insert(ps.promise, handles);
}

fn pipe_finalize(
    vm: &mut JsVm,
    streams: &SharedStreams,
    st: &Rc<RefCell<PipeState>>,
    is_error: bool,
    error: JsValue,
) {
    let (reader, writer, promise, sig_opt, listener_opt) = {
        let mut ps = st.borrow_mut();
        (
            ps.reader,
            ps.writer,
            ps.promise,
            ps.opts.signal,
            ps.abort_listener.take(),
        )
    };
    streams.borrow_mut().active_pipes.remove(&promise);
    release_default_writer(vm, streams, writer);
    release_default_reader(vm, streams, reader);
    if let (Some(sig_h), Some(cb_h)) = (sig_opt, listener_opt) {
        if let Some(rm_fn) = streams.borrow().et_remove_listener_fn {
            let ev_name = JsValue::from_string(vm.heap.alloc_string("abort"));
            let _ = vm.call_value(
                JsValue::from_object(rm_fn),
                JsValue::from_object(sig_h),
                &[ev_name, JsValue::from_object(cb_h)],
            );
        }
    }
    if is_error {
        reject_promise(vm, promise, error);
    } else {
        resolve_promise(vm, promise, JsValue::UNDEFINED);
    }
}

fn wait_for_writes_then(
    vm: &mut JsVm,
    streams: &SharedStreams,
    st: &Rc<RefCell<PipeState>>,
    f: impl FnOnce(&mut JsVm) + 'static,
) {
    let (need_wait, cur_write) = {
        let ps = st.borrow();
        let dest_writable_open = streams.borrow().writable.get(&ps.dest).is_some_and(|ws| {
            ws.state == WritableState::Writable && !ws.close_queued_or_in_flight()
        });
        (dest_writable_open, ps.current_write)
    };
    if need_wait {
        let cell = Rc::new(RefCell::new(Some(f)));
        let cb = vm.alloc_host_fn("pipe_wait_writes", move |vm, _, _| {
            if let Some(func) = cell.borrow_mut().take() {
                func(vm);
            }
            Ok(JsValue::UNDEFINED)
        });
        let _ = promise_then(vm, cur_write, Some(cb), Some(cb));
    } else {
        f(vm);
    }
}

fn pipe_shutdown(
    vm: &mut JsVm,
    streams: &SharedStreams,
    st: &Rc<RefCell<PipeState>>,
    is_error: bool,
    error: JsValue,
) {
    {
        let mut ps = st.borrow_mut();
        if ps.shutting_down {
            return;
        }
        ps.shutting_down = true;
    }
    let ss = Rc::clone(streams);
    let ps_c = Rc::clone(st);
    wait_for_writes_then(vm, streams, st, move |vm| {
        pipe_finalize(vm, &ss, &ps_c, is_error, error);
    });
}

fn pipe_shutdown_with_action(
    vm: &mut JsVm,
    streams: &SharedStreams,
    st: &Rc<RefCell<PipeState>>,
    action: impl FnOnce(&mut JsVm) -> HeapHandle + 'static,
    original_is_error: bool,
    original_error: JsValue,
) {
    {
        let mut ps = st.borrow_mut();
        if ps.shutting_down {
            return;
        }
        ps.shutting_down = true;
    }
    let ss = Rc::clone(streams);
    let ps_c = Rc::clone(st);
    wait_for_writes_then(vm, streams, st, move |vm| {
        let p = action(vm);
        let ss1 = Rc::clone(&ss);
        let ps1 = Rc::clone(&ps_c);
        let on_ok = vm.alloc_host_fn("pipe_shut_ok", move |vm, _, _| {
            pipe_finalize(vm, &ss1, &ps1, original_is_error, original_error);
            Ok(JsValue::UNDEFINED)
        });
        let ss2 = Rc::clone(&ss);
        let ps2 = Rc::clone(&ps_c);
        let on_err = vm.alloc_host_fn("pipe_shut_err", move |vm, _, a| {
            pipe_finalize(vm, &ss2, &ps2, true, arg(a, 0));
            Ok(JsValue::UNDEFINED)
        });
        let _ = promise_then(vm, p, Some(on_ok), Some(on_err));
    });
}

fn pipe_wait_all(vm: &mut JsVm, promises: Vec<HeapHandle>) -> HeapHandle {
    if promises.is_empty() {
        return promise_resolve_value(vm, JsValue::UNDEFINED);
    }
    if promises.len() == 1 {
        return promises[0];
    }
    let out = alloc_promise(vm);
    let remaining = Rc::new(RefCell::new(promises.len()));
    let rejected = Rc::new(RefCell::new(false));
    for p in promises {
        let rem = Rc::clone(&remaining);
        let rej = Rc::clone(&rejected);
        let on_ok = vm.alloc_host_fn_rooted(
            "pipe_all_ok",
            &[JsValue::from_object(out)],
            move |vm, _, _| {
                if *rej.borrow() {
                    return Ok(JsValue::UNDEFINED);
                }
                let mut r = rem.borrow_mut();
                *r = r.saturating_sub(1);
                if *r == 0 {
                    resolve_promise(vm, out, JsValue::UNDEFINED);
                }
                Ok(JsValue::UNDEFINED)
            },
        );
        let rej2 = Rc::clone(&rejected);
        let on_err = vm.alloc_host_fn_rooted(
            "pipe_all_err",
            &[JsValue::from_object(out)],
            move |vm, _, a| {
                let mut r = rej2.borrow_mut();
                if !*r {
                    *r = true;
                    reject_promise(vm, out, arg(a, 0));
                }
                Ok(JsValue::UNDEFINED)
            },
        );
        let _ = promise_then(vm, p, Some(on_ok), Some(on_err));
    }
    out
}

fn pipe_loop_step(vm: &mut JsVm, streams: &SharedStreams, st: &Rc<RefCell<PipeState>>) {
    let (shutting_down, writer_h) = {
        let ps = st.borrow();
        (ps.shutting_down, ps.writer)
    };
    if shutting_down {
        return;
    }
    let ready_p = streams
        .borrow()
        .writable_writers
        .get(&writer_h)
        .map(|w| w.ready_promise)
        .unwrap_or_else(|| promise_resolve_value(vm, JsValue::UNDEFINED));

    let ss_r = Rc::clone(streams);
    let st_r = Rc::clone(st);
    let on_ready = vm.alloc_host_fn("pipe_on_ready", move |vm, _, _| {
        let (shut, src_h, dst_h) = {
            let ps = st_r.borrow();
            (ps.shutting_down, ps.source, ps.dest)
        };
        if shut {
            return Ok(JsValue::UNDEFINED);
        }
        // Check if dest closed/errored while waiting for ready.
        let dest_ok = ss_r.borrow().writable.get(&dst_h).is_some_and(|ws| {
            ws.state == WritableState::Writable && !ws.close_queued_or_in_flight()
        });
        if !dest_ok {
            return Ok(JsValue::UNDEFINED);
        }
        let ss_chunk = Rc::clone(&ss_r);
        let st_chunk = Rc::clone(&st_r);
        let req = ReadRequest::Callback {
            on_chunk: Rc::new(RefCell::new(Some(Box::new(
                move |vm: &mut JsVm, chunk: JsValue| {
                    // WHATWG §4.8: never invoke `sink.write` synchronously inside `controller.enqueue()`;
                    // gate `current_write` immediately and schedule the actual write in a microtask.
                    let write_gate = alloc_promise(vm);
                    st_chunk.borrow_mut().current_write = write_gate;
                    sync_pipe_gc_roots(&ss_chunk, &st_chunk);

                    let ss_w = Rc::clone(&ss_chunk);
                    let st_w = Rc::clone(&st_chunk);
                    let do_write = vm.alloc_host_fn_rooted(
                        "pipe_write_micro",
                        &[chunk, JsValue::from_object(write_gate)],
                        move |vm, _, _| {
                            let dest_writable =
                                ss_w.borrow().writable.get(&dst_h).is_some_and(|ws| {
                                    ws.state == WritableState::Writable
                                        && !ws.close_queued_or_in_flight()
                                });
                            if !dest_writable {
                                resolve_promise(vm, write_gate, JsValue::UNDEFINED);
                                return Ok(JsValue::UNDEFINED);
                            }
                            let wp = writable_write_internal(vm, &ss_w, dst_h, chunk);
                            promise_mark_handled(vm, wp);
                            let settle = vm.alloc_host_fn_rooted(
                                "pipe_write_gate_done",
                                &[JsValue::from_object(write_gate)],
                                move |vm, _, _| {
                                    resolve_promise(vm, write_gate, JsValue::UNDEFINED);
                                    Ok(JsValue::UNDEFINED)
                                },
                            );
                            let _ = promise_then(vm, wp, Some(settle), Some(settle));
                            if !st_w.borrow().shutting_down {
                                pipe_loop_step(vm, &ss_w, &st_w);
                            }
                            Ok(JsValue::UNDEFINED)
                        },
                    );
                    let resolved = promise_resolve_value(vm, JsValue::UNDEFINED);
                    let _ = promise_then(vm, resolved, Some(do_write), None);
                },
            )))),
            on_close: Rc::new(RefCell::new(None)),
            on_error: Rc::new(RefCell::new(None)),
        };
        readable_read_with_request(vm, &ss_r, src_h, req);
        Ok(JsValue::UNDEFINED)
    });
    let on_ready_err = vm.alloc_host_fn("pipe_ready_err", |_, _, _| Ok(JsValue::UNDEFINED));
    let _ = promise_then(vm, ready_p, Some(on_ready), Some(on_ready_err));
}

pub(crate) fn readable_stream_pipe_to(
    vm: &mut JsVm,
    streams: &SharedStreams,
    web_apis: &SharedWebApis,
    src_h: HeapHandle,
    dst_h: HeapHandle,
    opts: PipeOptions,
) -> HeapHandle {
    let reader_h = match acquire_default_reader(vm, streams, src_h) {
        Ok(r) => r,
        Err(e) => return promise_reject_value(vm, e.value),
    };
    let writer_h = match acquire_default_writer(vm, streams, dst_h) {
        Ok(w) => w,
        Err(e) => {
            release_default_reader(vm, streams, reader_h);
            return promise_reject_value(vm, e.value);
        }
    };

    // WHATWG Streams §4.8 ReadableStreamPipeTo step 5: mark source disturbed synchronously!
    if let Some(rs) = streams.borrow_mut().readable.get_mut(&src_h) {
        rs.disturbed = true;
    }

    let promise = alloc_promise(vm);
    let init_write = promise_resolve_value(vm, JsValue::UNDEFINED);
    let st = Rc::new(RefCell::new(PipeState {
        source: src_h,
        dest: dst_h,
        reader: reader_h,
        writer: writer_h,
        opts,
        shutting_down: false,
        promise,
        current_write: init_write,
        abort_listener: None,
    }));
    sync_pipe_gc_roots(streams, &st);

    // 1. Handle `signal` (WHATWG §4.8 step 12).
    if let Some(sig_h) = opts.signal {
        let ss_ab = Rc::clone(streams);
        let wa_ab = Rc::clone(web_apis);
        let st_ab = Rc::clone(&st);
        let run_abort = move |vm: &mut JsVm| {
            let reason = wa_ab
                .borrow()
                .abort_signals
                .get(&sig_h)
                .map_or(JsValue::UNDEFINED, |e| e.reason);
            let error = if reason.is_undefined() {
                create_dom_exception(vm, "AbortError", "The operation was aborted.").value
            } else {
                reason
            };
            let ss_act = Rc::clone(&ss_ab);
            let st_act = Rc::clone(&st_ab);
            pipe_shutdown_with_action(
                vm,
                &ss_ab,
                &st_ab,
                move |vm| {
                    let mut ps = Vec::new();
                    let (src, dst, o) = {
                        let p = st_act.borrow();
                        (p.source, p.dest, p.opts)
                    };
                    if !o.prevent_abort {
                        let dst_writable = ss_act
                            .borrow()
                            .writable
                            .get(&dst)
                            .is_some_and(|ws| ws.state == WritableState::Writable);
                        if dst_writable {
                            ps.push(writable_abort_internal(vm, &ss_act, dst, error));
                        }
                    }
                    if !o.prevent_cancel {
                        let src_readable = ss_act
                            .borrow()
                            .readable
                            .get(&src)
                            .is_some_and(|rs| rs.state == ReadableState::Readable);
                        if src_readable {
                            ps.push(readable_cancel_internal(vm, &ss_act, src, error));
                        }
                    }
                    pipe_wait_all(vm, ps)
                },
                true,
                error,
            );
        };
        let already_aborted = web_apis
            .borrow()
            .abort_signals
            .get(&sig_h)
            .is_some_and(|e| e.aborted);
        if already_aborted {
            run_abort(vm);
            return promise;
        }
        let cb = vm.alloc_host_fn("pipe_abort_listener", move |vm, _, _| {
            run_abort(vm);
            Ok(JsValue::UNDEFINED)
        });
        st.borrow_mut().abort_listener = Some(cb);
        sync_pipe_gc_roots(streams, &st);
        if let Some(add_fn) = streams.borrow().et_add_listener_fn {
            let ev_name = JsValue::from_string(vm.heap.alloc_string("abort"));
            let _ = vm.call_value(
                JsValue::from_object(add_fn),
                JsValue::from_object(sig_h),
                &[ev_name, JsValue::from_object(cb)],
            );
        }
    }

    // Read initial states and reader/writer closed promises.
    let (src_state, src_err, reader_closed_p, dst_state, dst_err, dst_closing, writer_closed_p) = {
        let reg = streams.borrow();
        let (ss_st, ss_err) = reg
            .readable
            .get(&src_h)
            .map_or((ReadableState::Closed, JsValue::UNDEFINED), |r| {
                (r.state, r.stored_error)
            });
        let rcp = reg
            .readable_readers
            .get(&reader_h)
            .map(|r| r.closed_promise)
            .unwrap_or_else(|| promise_resolve_value(vm, JsValue::UNDEFINED));
        let (ds_st, ds_err, ds_cl) = reg
            .writable
            .get(&dst_h)
            .map_or((WritableState::Closed, JsValue::UNDEFINED, false), |w| {
                (w.state, w.stored_error, w.close_queued_or_in_flight())
            });
        let wcp = reg
            .writable_writers
            .get(&writer_h)
            .map(|w| w.closed_promise)
            .unwrap_or_else(|| promise_resolve_value(vm, JsValue::UNDEFINED));
        (ss_st, ss_err, rcp, ds_st, ds_err, ds_cl, wcp)
    };

    // Watcher 1: Errors must be propagated forward (source errors -> abort dest).
    {
        let ss_w1 = Rc::clone(streams);
        let st_w1 = Rc::clone(&st);
        let handle_src_err = move |vm: &mut JsVm, err: JsValue| {
            if !opts.prevent_abort {
                let ss_a = Rc::clone(&ss_w1);
                pipe_shutdown_with_action(
                    vm,
                    &ss_w1,
                    &st_w1,
                    move |vm| writable_abort_internal(vm, &ss_a, dst_h, err),
                    true,
                    err,
                );
            } else {
                pipe_shutdown(vm, &ss_w1, &st_w1, true, err);
            }
        };
        if src_state == ReadableState::Errored {
            handle_src_err(vm, src_err);
        } else {
            let on_err = vm.alloc_host_fn("pipe_src_err", move |vm, _, a| {
                handle_src_err(vm, arg(a, 0));
                Ok(JsValue::UNDEFINED)
            });
            let _ = promise_then(vm, reader_closed_p, None, Some(on_err));
        }
    }

    // Watcher 2: Errors must be propagated backward (dest errors -> cancel source).
    {
        let ss_w2 = Rc::clone(streams);
        let st_w2 = Rc::clone(&st);
        let handle_dst_err = move |vm: &mut JsVm, err: JsValue| {
            if !opts.prevent_cancel {
                let ss_c = Rc::clone(&ss_w2);
                pipe_shutdown_with_action(
                    vm,
                    &ss_w2,
                    &st_w2,
                    move |vm| readable_cancel_internal(vm, &ss_c, src_h, err),
                    true,
                    err,
                );
            } else {
                pipe_shutdown(vm, &ss_w2, &st_w2, true, err);
            }
        };
        if dst_state == WritableState::Errored {
            handle_dst_err(vm, dst_err);
        } else {
            let on_err = vm.alloc_host_fn("pipe_dst_err", move |vm, _, a| {
                handle_dst_err(vm, arg(a, 0));
                Ok(JsValue::UNDEFINED)
            });
            let _ = promise_then(vm, writer_closed_p, None, Some(on_err));
        }
    }

    // Watcher 3: Closing must be propagated forward (source closes -> close dest).
    {
        let ss_w3 = Rc::clone(streams);
        let st_w3 = Rc::clone(&st);
        let handle_src_close = move |vm: &mut JsVm| {
            if !opts.prevent_close {
                let ss_c = Rc::clone(&ss_w3);
                pipe_shutdown_with_action(
                    vm,
                    &ss_w3,
                    &st_w3,
                    move |vm| writable_close_with_error_propagation(vm, &ss_c, dst_h),
                    false,
                    JsValue::UNDEFINED,
                );
            } else {
                pipe_shutdown(vm, &ss_w3, &st_w3, false, JsValue::UNDEFINED);
            }
        };
        if src_state == ReadableState::Closed {
            handle_src_close(vm);
        } else {
            let on_ok = vm.alloc_host_fn("pipe_src_close", move |vm, _, _| {
                handle_src_close(vm);
                Ok(JsValue::UNDEFINED)
            });
            let _ = promise_then(vm, reader_closed_p, Some(on_ok), None);
        }
    }

    // Watcher 4: Closing must be propagated backward (dest already closing/closed -> cancel source).
    if dst_closing || dst_state == WritableState::Closed {
        let dest_closed = vm
            .type_error("Cannot pipe to a closing or closed WritableStream")
            .value;
        if !opts.prevent_cancel {
            let ss_c = Rc::clone(streams);
            pipe_shutdown_with_action(
                vm,
                streams,
                &st,
                move |vm| readable_cancel_internal(vm, &ss_c, src_h, dest_closed),
                true,
                dest_closed,
            );
        } else {
            pipe_shutdown(vm, streams, &st, true, dest_closed);
        }
    }

    // 5. Kick off backpressure-aware pump loop if not already shutting down.
    if !st.borrow().shutting_down {
        pipe_loop_step(vm, streams, &st);
    }
    promise
}

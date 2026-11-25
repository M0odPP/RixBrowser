//! WHATWG/WICG `WebSocketStream` and `WebSocketError` bindings backed by
//! `rix_net::WebSocketConnection` and `ReadableStream`/`WritableStream`.

use std::collections::VecDeque;
use std::rc::Rc;

use rix_js::promise::{alloc_promise, reject_promise, resolve_promise};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};
use rix_net::{WebSocketConnection, WsCommand, WsMessage};
use tokio::sync::mpsc;

use crate::event_loop::{run_microtask_checkpoint, SharedEventLoop};
use crate::events::{add_internal_object_listener, SharedEvents};
use crate::fetch_ws::{
    clamp_u16, is_valid_ws_subprotocol, resolve_ws_url, AsyncNetEvent, SharedNetBridge,
};
use crate::streams::{
    alloc_readable_stream_internal, readable_controller_close, readable_controller_enqueue,
    readable_error_internal, SharedStreams,
};
use crate::streams_common::{promise_mark_handled, promise_reject_value, set_to_string_tag};
use crate::streams_writable::{
    create_writable_stream_with_abort_and_start, writable_start_erroring,
};
use crate::web_apis::{
    abort_info_for_signal, alloc_uint8_array, arg, bind_ctor_proto, create_dom_exception,
    define_global, define_host_getter, define_host_method, extract_bytes_from_js, SharedWebApis,
};

#[derive(Debug, Clone)]
pub(crate) struct WebSocketErrorData {
    pub(crate) close_code: Option<u16>,
    pub(crate) reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WsStreamPhase {
    Connecting,
    Open,
    Closing,
    Closed,
}

pub(crate) struct JsWebSocketStreamEntry {
    pub(crate) js_obj: HeapHandle,
    pub(crate) url: String,
    pub(crate) phase: WsStreamPhase,
    pub(crate) opened_promise: HeapHandle,
    pub(crate) closed_promise: HeapHandle,
    pub(crate) readable: Option<HeapHandle>,
    pub(crate) readable_ctrl: Option<HeapHandle>,
    pub(crate) writable: Option<HeapHandle>,
    pub(crate) pending_writes: VecDeque<HeapHandle>,
    pub(crate) pending_close_promises: Vec<HeapHandle>,
    pub(crate) cmd_tx: mpsc::UnboundedSender<WsCommand>,
    pub(crate) abort_connect: Option<tokio::sync::oneshot::Sender<()>>,
    pub(crate) abort_rejected: bool,
}

impl JsWebSocketStreamEntry {
    pub(crate) fn collect_roots(&self, out: &mut Vec<JsValue>) {
        if self.phase != WsStreamPhase::Closed {
            out.push(JsValue::from_object(self.js_obj));
        }
        out.push(JsValue::from_object(self.opened_promise));
        out.push(JsValue::from_object(self.closed_promise));
        if let Some(h) = self.readable {
            out.push(JsValue::from_object(h));
        }
        if let Some(h) = self.readable_ctrl {
            out.push(JsValue::from_object(h));
        }
        if let Some(h) = self.writable {
            out.push(JsValue::from_object(h));
        }
        for &p in &self.pending_writes {
            out.push(JsValue::from_object(p));
        }
        for &p in &self.pending_close_promises {
            out.push(JsValue::from_object(p));
        }
    }
}

fn validate_ws_close_code_and_reason(
    vm: &mut JsVm,
    code_opt: Option<u16>,
    reason: String,
) -> Result<(Option<u16>, String), JsException> {
    if let Some(c) = code_opt {
        if c != 1000 && !(3000..=4999).contains(&c) {
            return Err(create_dom_exception(
                vm,
                "InvalidAccessError",
                &format!("Invalid WebSocket close code: {c}"),
            ));
        }
    }
    if reason.len() > 123 {
        return Err(create_dom_exception(
            vm,
            "SyntaxError",
            "WebSocket close reason must not be greater than 123 UTF-8 bytes",
        ));
    }
    let final_code = match code_opt {
        Some(c) => Some(c),
        None if !reason.is_empty() => Some(1000),
        None => None,
    };
    Ok((final_code, reason))
}

fn parse_close_info(vm: &mut JsVm, val: JsValue) -> Result<(Option<u16>, String), JsException> {
    if val.is_undefined() || val.is_null() {
        return Ok((None, String::new()));
    }
    let Some(ih) = val.as_object() else {
        return Err(vm.type_error("WebSocketCloseInfo must be an object"));
    };
    let kc = vm.heap.intern_key("closeCode");
    let vc = vm.get_property_by_key(ih, kc)?;
    let code_opt = if vc.is_undefined() {
        None
    } else {
        Some(clamp_u16(vm.to_number(vc)?))
    };
    let kr = vm.heap.intern_key("reason");
    let vr = vm.get_property_by_key(ih, kr)?;
    let reason = if vr.is_undefined() {
        String::new()
    } else {
        vm.to_js_string(vr)?
    };
    validate_ws_close_code_and_reason(vm, code_opt, reason)
}

pub(crate) fn alloc_websocket_error(
    vm: &mut JsVm,
    net: &SharedNetBridge,
    message: &str,
    close_code: Option<u16>,
    reason: String,
) -> HeapHandle {
    let proto = net.borrow().protos.ws_error_proto.unwrap_or(vm.error_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let k_brand = vm.heap.intern_key("__rix_dom_exception__");
    let k_name = vm.heap.intern_key("__rix_de_name__");
    let k_msg = vm.heap.intern_key("__rix_de_message__");
    let k_code = vm.heap.intern_key("__rix_de_code__");
    let n_val = JsValue::from_string(vm.heap.alloc_string("WebSocketError"));
    let m_val = JsValue::from_string(vm.heap.alloc_string(message));
    for (k, v) in [
        (k_brand, JsValue::TRUE),
        (k_name, n_val),
        (k_msg, m_val),
        (k_code, JsValue::from_i32(0)),
    ] {
        vm.heap
            .object_define_data(obj, k, v, PropertyAttrs::empty(), &mut vm.shapes);
    }
    net.borrow_mut()
        .ws_errors
        .insert(obj, WebSocketErrorData { close_code, reason });
    obj
}

fn close_info_from_reason(net: &SharedNetBridge, reason: JsValue) -> (Option<u16>, String) {
    if let Some(h) = reason.as_object() {
        if let Some(data) = net.borrow().ws_errors.get(&h) {
            return (data.close_code, data.reason.clone());
        }
    }
    (None, String::new())
}

fn is_resizable_buffer_source(vm: &JsVm, val: JsValue) -> bool {
    let Some(h) = val.as_object() else {
        return false;
    };
    let buf_h = match vm.heap.get_object(h).map(|o| &o.kind) {
        Some(ObjectKind::ArrayBuffer(_)) => h,
        Some(ObjectKind::TypedArray(ta)) => ta.buffer,
        Some(ObjectKind::DataView(dv)) => dv.buffer,
        _ => return false,
    };
    vm.heap
        .get_object(buf_h)
        .and_then(|o| o.rare.as_ref())
        .is_some_and(|r| r.ab_max_byte_length.is_some() || r.shared_mem.is_some())
}

fn require_ws_stream_id(
    vm: &mut JsVm,
    net: &SharedNetBridge,
    val: JsValue,
) -> Result<u64, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible WebSocketStream receiver"));
    };
    net.borrow()
        .ws_stream_handle_to_id
        .get(&h)
        .copied()
        .ok_or_else(|| vm.type_error("Incompatible WebSocketStream receiver"))
}

fn initiate_ws_stream_close(
    vm: &mut JsVm,
    net: &SharedNetBridge,
    id: u64,
    code: Option<u16>,
    reason: String,
    track_promise: Option<HeapHandle>,
) {
    let (phase, abort_tx, cmd_tx, opened_p, closed_p) = {
        let mut st = net.borrow_mut();
        let Some(entry) = st.ws_streams.get_mut(&id) else {
            if let Some(p) = track_promise {
                resolve_promise(vm, p, JsValue::UNDEFINED);
            }
            return;
        };
        if entry.phase == WsStreamPhase::Closed {
            if let Some(p) = track_promise {
                resolve_promise(vm, p, JsValue::UNDEFINED);
            }
            return;
        }
        if let Some(p) = track_promise {
            entry.pending_close_promises.push(p);
        }
        let prev = entry.phase;
        if prev == WsStreamPhase::Connecting || prev == WsStreamPhase::Open {
            entry.phase = WsStreamPhase::Closing;
        }
        (
            prev,
            entry.abort_connect.take(),
            entry.cmd_tx.clone(),
            entry.opened_promise,
            entry.closed_promise,
        )
    };
    if phase == WsStreamPhase::Connecting {
        if let Some(tx) = abort_tx {
            let _ = tx.send(());
        }
        let err_h = alloc_websocket_error(
            vm,
            net,
            "WebSocket was closed before connection was established",
            Some(1006),
            String::new(),
        );
        let err_v = JsValue::from_object(err_h);
        if let Some(entry) = net.borrow_mut().ws_streams.get_mut(&id) {
            entry.phase = WsStreamPhase::Closed;
            for p in entry.pending_close_promises.drain(..) {
                resolve_promise(vm, p, JsValue::UNDEFINED);
            }
        }
        reject_promise(vm, opened_p, err_v);
        reject_promise(vm, closed_p, err_v);
    } else if phase == WsStreamPhase::Open {
        let _ = cmd_tx.send(WsCommand::Close { code, reason });
    }
}

/// Installs `WebSocketError` and `WebSocketStream` on `vm`.
pub(crate) fn install_websocket_stream(
    vm: &mut JsVm,
    events: &SharedEvents,
    web_apis: &SharedWebApis,
    streams: &SharedStreams,
    net: &SharedNetBridge,
) {
    // 1. `WebSocketError` (subclass of `DOMException`)
    let k_de = vm.heap.intern_key("DOMException");
    let de_ctor = vm
        .heap
        .object_get_own(vm.global, k_de, &vm.shapes)
        .and_then(|p| p.value.as_object());
    let de_proto = de_ctor
        .and_then(|c| vm.heap.object_get_own(c, vm.key_prototype, &vm.shapes))
        .and_then(|p| p.value.as_object())
        .unwrap_or(vm.error_proto);
    let wep = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(de_proto)));
    vm.heap.get_object_mut(wep).unwrap().is_prototype = true;
    set_to_string_tag(vm, wep, "WebSocketError");
    net.borrow_mut().protos.ws_error_proto = Some(wep);

    let nb = Rc::clone(net);
    define_host_getter(vm, wep, "closeCode", move |vm, this_val, _| {
        let h = this_val
            .as_object()
            .ok_or_else(|| vm.type_error("Incompatible WebSocketError receiver"))?;
        let code = nb
            .borrow()
            .ws_errors
            .get(&h)
            .map(|d| d.close_code)
            .ok_or_else(|| vm.type_error("Incompatible WebSocketError receiver"))?;
        Ok(match code {
            Some(c) => JsValue::from_i32(i32::from(c)),
            None => JsValue::NULL,
        })
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, wep, "reason", move |vm, this_val, _| {
        let h = this_val
            .as_object()
            .ok_or_else(|| vm.type_error("Incompatible WebSocketError receiver"))?;
        let reason = nb
            .borrow()
            .ws_errors
            .get(&h)
            .map(|d| d.reason.clone())
            .ok_or_else(|| vm.type_error("Incompatible WebSocketError receiver"))?;
        Ok(JsValue::from_string(vm.heap.alloc_string(&reason)))
    });

    let nb = Rc::clone(net);
    let we_ctor = vm.alloc_host_fn("WebSocketError", move |vm, _, args| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("WebSocketError constructor requires 'new'"));
        }
        let msg_arg = arg(args, 0);
        let msg = if msg_arg.is_undefined() {
            String::new()
        } else {
            vm.to_js_string(msg_arg)?
        };
        let (close_code, reason) = parse_close_info(vm, arg(args, 1))?;
        let obj = alloc_websocket_error(vm, &nb, &msg, close_code, reason);
        Ok(JsValue::from_object(obj))
    });
    if let Some(dc) = de_ctor {
        vm.heap
            .object_set_prototype(we_ctor, Some(dc), &mut vm.shapes);
    }
    bind_ctor_proto(vm, we_ctor, wep);
    define_global(vm, "WebSocketError", JsValue::from_object(we_ctor));

    // 2. `WebSocketStream`
    let wsp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(wsp).unwrap().is_prototype = true;
    set_to_string_tag(vm, wsp, "WebSocketStream");
    net.borrow_mut().protos.ws_stream_proto = Some(wsp);

    let nb = Rc::clone(net);
    define_host_getter(vm, wsp, "url", move |vm, this_val, _| {
        let id = require_ws_stream_id(vm, &nb, this_val)?;
        let u = nb
            .borrow()
            .ws_streams
            .get(&id)
            .map(|s| s.url.clone())
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(&u)))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, wsp, "opened", move |vm, this_val, _| {
        let id = require_ws_stream_id(vm, &nb, this_val)?;
        let p = nb.borrow().ws_streams.get(&id).unwrap().opened_promise;
        Ok(JsValue::from_object(p))
    });
    let nb = Rc::clone(net);
    define_host_getter(vm, wsp, "closed", move |vm, this_val, _| {
        let id = require_ws_stream_id(vm, &nb, this_val)?;
        let p = nb.borrow().ws_streams.get(&id).unwrap().closed_promise;
        Ok(JsValue::from_object(p))
    });
    let nb = Rc::clone(net);
    define_host_method(vm, wsp, "close", move |vm, this_val, args| {
        let id = require_ws_stream_id(vm, &nb, this_val)?;
        let (code, reason) = parse_close_info(vm, arg(args, 0))?;
        initiate_ws_stream_close(vm, &nb, id, code, reason, None);
        Ok(JsValue::UNDEFINED)
    });

    let nb = Rc::clone(net);
    let wa = Rc::clone(web_apis);
    let evs = Rc::clone(events);
    let _ss = Rc::clone(streams);
    let ctor = vm.alloc_host_fn("WebSocketStream", move |vm, _, args| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("WebSocketStream constructor requires 'new'"));
        }
        if args.is_empty() {
            return Err(vm.type_error("WebSocketStream constructor requires at least 1 argument"));
        }
        let raw_url = vm.to_js_string(arg(args, 0))?;
        let (resolved_url, _) = resolve_ws_url(vm, &wa, &raw_url)?;

        let mut protocols: Vec<String> = Vec::new();
        let mut signal_h: Option<HeapHandle> = None;
        let opts_val = arg(args, 1);
        if !opts_val.is_undefined() && !opts_val.is_null() {
            let Some(oh) = opts_val.as_object() else {
                return Err(vm.type_error("WebSocketStreamOptions must be an object"));
            };
            let kp = vm.heap.intern_key("protocols");
            let vp = vm.get_property_by_key(oh, kp)?;
            if !vp.is_undefined() {
                // WebIDL `sequence<USVString>` rejects primitives like strings!
                let Some(ph) = vp.as_object() else {
                    return Err(vm.type_error("WebSocketStreamOptions.protocols must be iterable"));
                };
                let sym = vm.key_symbol_iterator;
                let has_iter = vm
                    .get_property_by_key(ph, sym)
                    .ok()
                    .and_then(JsValue::as_object)
                    .is_some_and(|f| vm.is_callable_handle(f));
                if !has_iter {
                    return Err(vm.type_error("WebSocketStreamOptions.protocols must be iterable"));
                }
                for item in vm.collect_iterable(vp)? {
                    protocols.push(vm.to_js_string(item)?);
                }
            }
            let ks = vm.heap.intern_key("signal");
            let vs = vm.get_property_by_key(oh, ks)?;
            if !vs.is_undefined() {
                let Some(sh) = vs.as_object() else {
                    return Err(
                        vm.type_error("WebSocketStreamOptions.signal must be an AbortSignal")
                    );
                };
                if !wa.borrow().abort_signals.contains_key(&sh) {
                    return Err(
                        vm.type_error("WebSocketStreamOptions.signal must be an AbortSignal")
                    );
                }
                signal_h = Some(sh);
            }
        }
        let mut seen = std::collections::HashSet::with_capacity(protocols.len());
        for p in &protocols {
            if !is_valid_ws_subprotocol(p) {
                return Err(create_dom_exception(
                    vm,
                    "SyntaxError",
                    &format!("Invalid WebSocket subprotocol: '{p}'"),
                ));
            }
            if !seen.insert(p.to_ascii_lowercase()) {
                return Err(create_dom_exception(
                    vm,
                    "SyntaxError",
                    &format!("Duplicate WebSocket subprotocol: '{p}'"),
                ));
            }
        }

        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(wsp)));
        let opened_p = alloc_promise(vm);
        let closed_p = alloc_promise(vm);
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<WsCommand>();
        let (abort_tx, mut abort_rx) = tokio::sync::oneshot::channel::<()>();

        // Pre-aborted signal prevents opening any connection.
        if let Some(sh) = signal_h {
            if let Some((_, true, reason_v)) = abort_info_for_signal(&wa, JsValue::from_object(sh))
            {
                let reason = if reason_v.is_undefined() {
                    create_dom_exception(vm, "AbortError", "The operation was aborted.").value
                } else {
                    reason_v
                };
                reject_promise(vm, opened_p, reason);
                reject_promise(vm, closed_p, reason);
                let id = nb.borrow_mut().alloc_id();
                let mut st = nb.borrow_mut();
                st.ws_streams.insert(
                    id,
                    JsWebSocketStreamEntry {
                        js_obj: obj,
                        url: resolved_url,
                        phase: WsStreamPhase::Closed,
                        opened_promise: opened_p,
                        closed_promise: closed_p,
                        readable: None,
                        readable_ctrl: None,
                        writable: None,
                        pending_writes: VecDeque::new(),
                        pending_close_promises: Vec::new(),
                        cmd_tx,
                        abort_connect: None,
                        abort_rejected: true,
                    },
                );
                st.ws_stream_handle_to_id.insert(obj, id);
                return Ok(JsValue::from_object(obj));
            }
        }

        let origin = wa.borrow().current_origin();
        let mut extra_headers: Vec<(String, String)> = Vec::new();
        if let Ok(parsed_u) = url::Url::parse(&resolved_url) {
            let cookie = wa.borrow_mut().cookie_header_for_url(&parsed_u, false);
            if !cookie.is_empty() {
                extra_headers.push(("Cookie".to_string(), cookie));
            }
        }

        let (id, ev_tx) = {
            let mut st = nb.borrow_mut();
            let id = st.alloc_id();
            st.ws_streams.insert(
                id,
                JsWebSocketStreamEntry {
                    js_obj: obj,
                    url: resolved_url.clone(),
                    phase: WsStreamPhase::Connecting,
                    opened_promise: opened_p,
                    closed_promise: closed_p,
                    readable: None,
                    readable_ctrl: None,
                    writable: None,
                    pending_writes: VecDeque::new(),
                    pending_close_promises: Vec::new(),
                    cmd_tx,
                    abort_connect: Some(abort_tx),
                    abort_rejected: false,
                },
            );
            st.ws_stream_handle_to_id.insert(obj, id);
            (id, st.async_tx.clone())
        };

        // Wire mid-handshake abort listener on `signal`.
        if let Some(sh) = signal_h {
            let nb_ab = Rc::clone(&nb);
            let wa_ab = Rc::clone(&wa);
            let on_abort = vm.alloc_host_fn("ws_stream_abort", move |vm, _, _| {
                let (was_connecting, abort_tx, op, cp) = {
                    let mut st = nb_ab.borrow_mut();
                    let Some(entry) = st.ws_streams.get_mut(&id) else {
                        return Ok(JsValue::UNDEFINED);
                    };
                    if entry.phase != WsStreamPhase::Connecting {
                        return Ok(JsValue::UNDEFINED);
                    }
                    entry.phase = WsStreamPhase::Closed;
                    entry.abort_rejected = true;
                    (
                        true,
                        entry.abort_connect.take(),
                        entry.opened_promise,
                        entry.closed_promise,
                    )
                };
                if was_connecting {
                    if let Some(tx) = abort_tx {
                        let _ = tx.send(());
                    }
                    let reason = abort_info_for_signal(&wa_ab, JsValue::from_object(sh))
                        .map(|(_, _, r)| r)
                        .filter(|r| !r.is_undefined())
                        .unwrap_or_else(|| {
                            create_dom_exception(vm, "AbortError", "The operation was aborted.")
                                .value
                        });
                    reject_promise(vm, op, reason);
                    reject_promise(vm, cp, reason);
                }
                Ok(JsValue::UNDEFINED)
            });
            add_internal_object_listener(&evs, sh, "abort", on_abort, true);
        }

        let Ok(rt_handle) = tokio::runtime::Handle::try_current() else {
            let _ = ev_tx.send(AsyncNetEvent::WsStreamError {
                id,
                message: "Network runtime unavailable".to_string(),
            });
            return Ok(JsValue::from_object(obj));
        };
        rt_handle.spawn(async move {
            let connect_res = tokio::select! {
                biased;
                _ = &mut abort_rx => {
                    return;
                }
                res = WebSocketConnection::connect_with_headers(
                    &resolved_url,
                    Some(&origin),
                    &protocols,
                    &extra_headers,
                ) => res,
            };
            let mut conn = match connect_res {
                Ok(c) => c,
                Err(e) => {
                    let _ = ev_tx.send(AsyncNetEvent::WsStreamError {
                        id,
                        message: e.to_string(),
                    });
                    return;
                }
            };
            let _ = ev_tx.send(AsyncNetEvent::WsStreamOpened {
                id,
                protocol: conn.negotiated_protocol.clone(),
                extensions: conn.negotiated_extensions.clone(),
                set_cookies: std::mem::take(&mut conn.response_set_cookies),
            });

            loop {
                tokio::select! {
                    biased;
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(WsCommand::SendText(t)) => {
                                let _ = conn.send_text(t);
                            }
                            Some(WsCommand::SendBinary(b)) => {
                                let _ = conn.send_binary(b);
                            }
                            Some(WsCommand::Close { code, reason }) => {
                                let _ = conn.close(code, reason);
                            }
                            None => break,
                        }
                    }
                    incoming = conn.recv() => {
                        match incoming {
                            Some(Ok(WsMessage::Text(text))) => {
                                let _ = ev_tx.send(AsyncNetEvent::WsStreamText { id, text });
                            }
                            Some(Ok(WsMessage::Binary(bytes))) => {
                                let _ = ev_tx.send(AsyncNetEvent::WsStreamBinary { id, bytes });
                            }
                            Some(Ok(WsMessage::Sent { .. })) => {
                                let _ = ev_tx.send(AsyncNetEvent::WsStreamSent { id });
                            }
                            Some(Ok(WsMessage::Close { code, reason, was_clean })) => {
                                let _ = ev_tx.send(AsyncNetEvent::WsStreamClosed {
                                    id,
                                    code,
                                    reason,
                                    was_clean,
                                });
                                break;
                            }
                            Some(Err(e)) => {
                                let _ = ev_tx.send(AsyncNetEvent::WsStreamError {
                                    id,
                                    message: e.to_string(),
                                });
                                break;
                            }
                            None => {
                                let _ = ev_tx.send(AsyncNetEvent::WsStreamClosed {
                                    id,
                                    code: 1006,
                                    reason: String::new(),
                                    was_clean: false,
                                });
                                break;
                            }
                        }
                    }
                }
            }
        });

        Ok(JsValue::from_object(obj))
    });
    bind_ctor_proto(vm, ctor, wsp);
    define_global(vm, "WebSocketStream", JsValue::from_object(ctor));
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn deliver_ws_stream_opened(
    vm: &mut JsVm,
    event_loop: &SharedEventLoop,
    web_apis: &SharedWebApis,
    streams: &SharedStreams,
    net: &SharedNetBridge,
    id: u64,
    protocol: String,
    extensions: String,
    set_cookies: Vec<String>,
) {
    let (opened_p, ws_url) = {
        let mut st = net.borrow_mut();
        let Some(entry) = st.ws_streams.get_mut(&id) else {
            return;
        };
        if entry.phase != WsStreamPhase::Connecting {
            return;
        }
        entry.phase = WsStreamPhase::Open;
        entry.abort_connect = None;
        (entry.opened_promise, entry.url.clone())
    };
    if !set_cookies.is_empty() {
        if let Ok(u) = url::Url::parse(&ws_url) {
            let mut wa = web_apis.borrow_mut();
            for sc in &set_cookies {
                wa.set_cookie_from_header(&u, sc, false);
            }
        }
    }

    // Build ReadableStream with cancel -> clean close honoring genuine WebSocketError.
    let nb_cancel = Rc::clone(net);
    let cancel_fn = vm.alloc_host_fn("ws_stream_cancel", move |vm, _, args| {
        let (code, reason) = close_info_from_reason(&nb_cancel, arg(args, 0));
        let p = alloc_promise(vm);
        initiate_ws_stream_close(vm, &nb_cancel, id, code, reason, Some(p));
        Ok(JsValue::from_object(p))
    });
    let (readable_h, readable_ctrl) =
        alloc_readable_stream_internal(vm, streams, 1.0, None, None, Some(cancel_fn), None, true);

    // Build WritableStream with write / close / abort callbacks.
    let nb_w = Rc::clone(net);
    let wa_w = Rc::clone(web_apis);
    let write_fn = vm.alloc_host_fn("ws_stream_write", move |vm, _, args| {
        let chunk = arg(args, 0);
        if is_resizable_buffer_source(vm, chunk) {
            let err =
                vm.type_error("Cannot write resizable or shared ArrayBuffer to WebSocketStream");
            return Ok(JsValue::from_object(promise_reject_value(vm, err.value)));
        }
        let is_binary = chunk.as_object().is_some_and(|h| {
            matches!(
                vm.heap.get_object(h).map(|o| &o.kind),
                Some(
                    ObjectKind::ArrayBuffer(_)
                        | ObjectKind::TypedArray(_)
                        | ObjectKind::DataView(_)
                )
            )
        });
        let cmd = if is_binary {
            match extract_bytes_from_js(vm, &wa_w, chunk) {
                Ok(b) => WsCommand::SendBinary(b),
                Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
            }
        } else {
            match vm.to_js_string(chunk) {
                Ok(s) => WsCommand::SendText(s),
                Err(e) => return Ok(JsValue::from_object(promise_reject_value(vm, e.value))),
            }
        };
        let p = alloc_promise(vm);
        let send_tx = {
            let mut st = nb_w.borrow_mut();
            let Some(entry) = st.ws_streams.get_mut(&id) else {
                let err =
                    create_dom_exception(vm, "InvalidStateError", "WebSocketStream is closed");
                reject_promise(vm, p, err.value);
                return Ok(JsValue::from_object(p));
            };
            if entry.phase != WsStreamPhase::Open {
                let err =
                    create_dom_exception(vm, "InvalidStateError", "WebSocketStream is not open");
                reject_promise(vm, p, err.value);
                return Ok(JsValue::from_object(p));
            }
            entry.pending_writes.push_back(p);
            entry.cmd_tx.clone()
        };
        let _ = send_tx.send(cmd);
        Ok(JsValue::from_object(p))
    });

    let nb_cl = Rc::clone(net);
    let close_fn = vm.alloc_host_fn("ws_stream_wclose", move |vm, _, _| {
        let p = alloc_promise(vm);
        initiate_ws_stream_close(vm, &nb_cl, id, None, String::new(), Some(p));
        Ok(JsValue::from_object(p))
    });

    let nb_ab = Rc::clone(net);
    let abort_fn = vm.alloc_host_fn("ws_stream_wabort", move |vm, _, args| {
        let (code, reason) = close_info_from_reason(&nb_ab, arg(args, 0));
        let p = alloc_promise(vm);
        initiate_ws_stream_close(vm, &nb_ab, id, code, reason, Some(p));
        Ok(JsValue::from_object(p))
    });

    let start_p = alloc_promise(vm);
    resolve_promise(vm, start_p, JsValue::UNDEFINED);
    let writable_h = create_writable_stream_with_abort_and_start(
        vm, streams, web_apis, write_fn, close_fn, abort_fn, 1.0, None, start_p,
    );

    if let Some(entry) = net.borrow_mut().ws_streams.get_mut(&id) {
        entry.readable = Some(readable_h);
        entry.readable_ctrl = Some(readable_ctrl);
        entry.writable = Some(writable_h);
    }

    let info = vm.alloc_ordinary_object();
    let proto_v = JsValue::from_string(vm.heap.alloc_string(&protocol));
    let ext_v = JsValue::from_string(vm.heap.alloc_string(&extensions));
    for (k_name, v) in [
        ("readable", JsValue::from_object(readable_h)),
        ("writable", JsValue::from_object(writable_h)),
        ("protocol", proto_v),
        ("extensions", ext_v),
    ] {
        let k = vm.heap.intern_key(k_name);
        vm.heap
            .object_define_data(info, k, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
    }
    resolve_promise(vm, opened_p, JsValue::from_object(info));
    run_microtask_checkpoint(event_loop, vm);
}

pub(crate) fn deliver_ws_stream_text(
    vm: &mut JsVm,
    event_loop: &SharedEventLoop,
    streams: &SharedStreams,
    net: &SharedNetBridge,
    id: u64,
    text: String,
) {
    let ctrl = {
        let st = net.borrow();
        let Some(entry) = st.ws_streams.get(&id) else {
            return;
        };
        if entry.phase != WsStreamPhase::Open && entry.phase != WsStreamPhase::Closing {
            return;
        }
        entry.readable_ctrl
    };
    if let Some(c) = ctrl {
        let v = JsValue::from_string(vm.heap.alloc_string(&text));
        let _ = readable_controller_enqueue(vm, streams, c, v);
        run_microtask_checkpoint(event_loop, vm);
    }
}

pub(crate) fn deliver_ws_stream_binary(
    vm: &mut JsVm,
    event_loop: &SharedEventLoop,
    streams: &SharedStreams,
    net: &SharedNetBridge,
    id: u64,
    bytes: Vec<u8>,
) {
    let ctrl = {
        let st = net.borrow();
        let Some(entry) = st.ws_streams.get(&id) else {
            return;
        };
        if entry.phase != WsStreamPhase::Open && entry.phase != WsStreamPhase::Closing {
            return;
        }
        entry.readable_ctrl
    };
    if let Some(c) = ctrl {
        let u8a = alloc_uint8_array(vm, bytes);
        let _ = readable_controller_enqueue(vm, streams, c, JsValue::from_object(u8a));
        run_microtask_checkpoint(event_loop, vm);
    }
}

pub(crate) fn deliver_ws_stream_sent(
    vm: &mut JsVm,
    event_loop: &SharedEventLoop,
    net: &SharedNetBridge,
    id: u64,
) {
    let p = net
        .borrow_mut()
        .ws_streams
        .get_mut(&id)
        .and_then(|e| e.pending_writes.pop_front());
    if let Some(write_p) = p {
        resolve_promise(vm, write_p, JsValue::UNDEFINED);
        run_microtask_checkpoint(event_loop, vm);
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn deliver_ws_stream_closed(
    vm: &mut JsVm,
    event_loop: &SharedEventLoop,
    streams: &SharedStreams,
    net: &SharedNetBridge,
    id: u64,
    code: u16,
    reason: String,
    was_clean: bool,
) {
    let (
        prev_phase,
        opened_p,
        closed_p,
        readable_h,
        readable_ctrl,
        writable_h,
        pending_writes,
        close_ps,
    ) = {
        let mut st = net.borrow_mut();
        let Some(entry) = st.ws_streams.get_mut(&id) else {
            return;
        };
        if entry.phase == WsStreamPhase::Closed {
            return;
        }
        let prev = entry.phase;
        entry.phase = WsStreamPhase::Closed;
        entry.abort_connect = None;
        (
            prev,
            entry.opened_promise,
            entry.closed_promise,
            entry.readable,
            entry.readable_ctrl,
            entry.writable,
            std::mem::take(&mut entry.pending_writes),
            std::mem::take(&mut entry.pending_close_promises),
        )
    };

    let had_unwritten = !pending_writes.is_empty();
    let effective_clean = was_clean && !had_unwritten && prev_phase != WsStreamPhase::Connecting;

    if had_unwritten {
        let inv = create_dom_exception(
            vm,
            "InvalidStateError",
            "WebSocketStream closed with unwritten data",
        )
        .value;
        for wp in pending_writes {
            reject_promise(vm, wp, inv);
        }
        if let Some(wh) = writable_h {
            writable_start_erroring(vm, streams, wh, inv);
        }
    }

    if effective_clean {
        if let Some(rc) = readable_ctrl {
            let _ = readable_controller_close(vm, streams, rc);
        }
        if let Some(wh) = writable_h {
            let inv = create_dom_exception(vm, "InvalidStateError", "WebSocketStream closed").value;
            writable_start_erroring(vm, streams, wh, inv);
        }
        for cp in close_ps {
            resolve_promise(vm, cp, JsValue::UNDEFINED);
        }
        let info = vm.alloc_ordinary_object();
        let kc = vm.heap.intern_key("closeCode");
        let kr = vm.heap.intern_key("reason");
        let rv = JsValue::from_string(vm.heap.alloc_string(&reason));
        vm.heap.object_define_data(
            info,
            kc,
            JsValue::from_i32(i32::from(code)),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        vm.heap
            .object_define_data(info, kr, rv, PropertyAttrs::DEFAULT, &mut vm.shapes);
        resolve_promise(vm, closed_p, JsValue::from_object(info));
    } else {
        let err_h = alloc_websocket_error(
            vm,
            net,
            "WebSocket connection closed abnormally",
            Some(code),
            reason,
        );
        let err_v = JsValue::from_object(err_h);
        if prev_phase == WsStreamPhase::Connecting {
            reject_promise(vm, opened_p, err_v);
        }
        if let Some(rh) = readable_h {
            readable_error_internal(vm, streams, rh, err_v);
        }
        if !had_unwritten {
            if let Some(wh) = writable_h {
                writable_start_erroring(vm, streams, wh, err_v);
            }
        }
        for cp in close_ps {
            reject_promise(vm, cp, err_v);
            promise_mark_handled(vm, cp);
        }
        reject_promise(vm, closed_p, err_v);
    }
    run_microtask_checkpoint(event_loop, vm);
}

pub(crate) fn deliver_ws_stream_error(
    vm: &mut JsVm,
    event_loop: &SharedEventLoop,
    streams: &SharedStreams,
    net: &SharedNetBridge,
    id: u64,
    message: String,
) {
    let (prev_phase, opened_p, closed_p, readable_h, writable_h, pending_writes, close_ps) = {
        let mut st = net.borrow_mut();
        let Some(entry) = st.ws_streams.get_mut(&id) else {
            return;
        };
        if entry.phase == WsStreamPhase::Closed {
            return;
        }
        let prev = entry.phase;
        entry.phase = WsStreamPhase::Closed;
        entry.abort_connect = None;
        (
            prev,
            entry.opened_promise,
            entry.closed_promise,
            entry.readable,
            entry.writable,
            std::mem::take(&mut entry.pending_writes),
            std::mem::take(&mut entry.pending_close_promises),
        )
    };
    let err_h = alloc_websocket_error(vm, net, &message, Some(1006), String::new());
    let err_v = JsValue::from_object(err_h);
    if prev_phase == WsStreamPhase::Connecting {
        reject_promise(vm, opened_p, err_v);
    }
    for wp in pending_writes {
        reject_promise(vm, wp, err_v);
    }
    if let Some(rh) = readable_h {
        readable_error_internal(vm, streams, rh, err_v);
    }
    if let Some(wh) = writable_h {
        writable_start_erroring(vm, streams, wh, err_v);
    }
    for cp in close_ps {
        reject_promise(vm, cp, err_v);
        promise_mark_handled(vm, cp);
    }
    reject_promise(vm, closed_p, err_v);
    run_microtask_checkpoint(event_loop, vm);
}

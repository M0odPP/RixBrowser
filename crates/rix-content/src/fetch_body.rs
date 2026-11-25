//! WHATWG Fetch Standard §5.2 (`Body` mixin):
//! `ReadableStream`-backed request/response bodies, `extract_fetch_body`,
//! `clone_fetch_body` (`tee`), zero-interception internal `ReadRequest`
//! consumption (`text`, `json`, `arrayBuffer`, `bytes`, `blob`, `formData`),
//! UTF-8 BOM decoding, and RFC 7578 `multipart/form-data` parsing/serialization.

use std::cell::RefCell;
use std::rc::Rc;

use rix_js::heap::TypedArrayKind;
use rix_js::promise::{alloc_promise, reject_promise, resolve_promise};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind};

use crate::fetch_headers::trim_http_whitespace;
use crate::streams::{
    acquire_default_reader, readable_cancel_internal, readable_read_with_request,
    tee_readable_stream_for_fetch, ReadRequest, SharedStreams,
};
use crate::streams_bytes::create_readable_byte_stream_from_bytes;
use crate::streams_common::promise_reject_value;
use crate::web_apis::{
    alloc_array_buffer, alloc_form_data_from_urlencoded, alloc_uint8_array, create_blob_object,
    BlobEntry, FormDataEntry, FormDataVal, SharedWebApis,
};

#[derive(Debug, Clone)]
pub(crate) struct FetchBody {
    pub(crate) stream: HeapHandle,
    pub(crate) source_bytes: Option<Vec<u8>>,
    pub(crate) content_type: Option<String>,
}

/// Returns `true` when `body` is non-null and its backing `ReadableStream` is disturbed.
pub(crate) fn body_is_used(streams: &SharedStreams, body: Option<&FetchBody>) -> bool {
    body.is_some_and(|b| streams.borrow().is_readable_disturbed(b.stream))
}

/// Returns `true` when `body` is non-null and its backing `ReadableStream` is disturbed or locked.
pub(crate) fn body_is_unusable(streams: &SharedStreams, body: Option<&FetchBody>) -> bool {
    body.is_some_and(|b| {
        let s = streams.borrow();
        s.is_readable_disturbed(b.stream) || s.is_readable_locked(b.stream)
    })
}

/// Decodes UTF-8 bytes per WHATWG Encoding `UTF-8 decode` (strips leading UTF-8 BOM `0xEF 0xBB 0xBF`).
pub(crate) fn utf8_decode_strip_bom(bytes: &[u8]) -> String {
    let stripped = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8_lossy(stripped).into_owned()
}

/// Serializes `URLSearchParams` pairs as `application/x-www-form-urlencoded`.
fn serialize_urlencoded(pairs: &[(String, String)]) -> String {
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        ser.append_pair(k, v);
    }
    ser.finish()
}

/// Serializes a `FormData` object as RFC 7578 `multipart/form-data`.
fn serialize_form_data_multipart(
    web_apis: &SharedWebApis,
    fd: &FormDataEntry,
) -> (Vec<u8>, String) {
    let boundary = "----RixFormBoundary7MA4YWxkTrZu0gW";
    let ct = format!("multipart/form-data; boundary={boundary}");
    if fd.pairs.is_empty() {
        return (Vec::new(), ct);
    }
    let mut out = Vec::new();
    let wa = web_apis.borrow();
    for (name, val) in &fd.pairs {
        out.extend_from_slice(b"--");
        out.extend_from_slice(boundary.as_bytes());
        out.extend_from_slice(b"\r\n");
        let safe_name = name
            .replace('\r', "%0D")
            .replace('\n', "%0A")
            .replace('"', "%22");
        match val {
            FormDataVal::Text(text) => {
                out.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{safe_name}\"\r\n\r\n")
                        .as_bytes(),
                );
                let normalized = normalize_crlf(text);
                out.extend_from_slice(normalized.as_bytes());
                out.extend_from_slice(b"\r\n");
            }
            FormDataVal::File(fh) => {
                let (bytes, ct, filename) = wa
                    .blobs
                    .get(fh)
                    .map(|b| {
                        (
                            b.bytes.as_slice(),
                            if b.content_type.is_empty() {
                                "application/octet-stream"
                            } else {
                                b.content_type.as_str()
                            },
                            b.name.as_deref().unwrap_or("blob"),
                        )
                    })
                    .unwrap_or((b"", "application/octet-stream", "blob"));
                let safe_fn = filename
                    .replace('\r', "%0D")
                    .replace('\n', "%0A")
                    .replace('"', "%22");
                out.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{safe_name}\"; filename=\"{safe_fn}\"\r\nContent-Type: {ct}\r\n\r\n"
                    )
                    .as_bytes(),
                );
                out.extend_from_slice(bytes);
                out.extend_from_slice(b"\r\n");
            }
        }
    }
    out.extend_from_slice(b"--");
    out.extend_from_slice(boundary.as_bytes());
    out.extend_from_slice(b"--\r\n");
    (out, format!("multipart/form-data; boundary={boundary}"))
}

fn normalize_crlf(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push_str("\r\n");
        } else if c == '\n' {
            out.push_str("\r\n");
        } else {
            out.push(c);
        }
    }
    out
}

/// Extracts a WHATWG Fetch §5.2 `Body` from any JS `BodyInit` value.
pub(crate) fn extract_fetch_body(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    streams: &SharedStreams,
    val: JsValue,
) -> Result<Option<FetchBody>, JsException> {
    if val.is_undefined() || val.is_null() {
        return Ok(None);
    }

    if let Some(h) = val.as_object() {
        // 1. ReadableStream — keep the EXACT stream object, reject if disturbed or locked!
        if streams.borrow().readable.contains_key(&h) {
            let unusable = {
                let s = streams.borrow();
                s.is_readable_disturbed(h) || s.is_readable_locked(h)
            };
            if unusable {
                return Err(vm.type_error(
                    "Cannot construct body from a disturbed or locked ReadableStream",
                ));
            }
            return Ok(Some(FetchBody {
                stream: h,
                source_bytes: None,
                content_type: None,
            }));
        }

        // 2. Blob / File
        if let Some(blob) = web_apis.borrow().blobs.get(&h).cloned() {
            let ct = (!blob.content_type.is_empty()).then_some(blob.content_type);
            let stream = create_readable_byte_stream_from_bytes(vm, streams, blob.bytes.clone());
            return Ok(Some(FetchBody {
                stream,
                source_bytes: Some(blob.bytes),
                content_type: ct,
            }));
        }

        // 3. FormData
        if let Some(fd) = web_apis.borrow().form_data.get(&h).cloned() {
            let (bytes, ct) = serialize_form_data_multipart(web_apis, &fd);
            let stream = create_readable_byte_stream_from_bytes(vm, streams, bytes.clone());
            return Ok(Some(FetchBody {
                stream,
                source_bytes: Some(bytes),
                content_type: Some(ct),
            }));
        }

        // 4. URLSearchParams
        if let Some(sp) = web_apis.borrow().search_params.get(&h).cloned() {
            let bytes = serialize_urlencoded(&sp.pairs).into_bytes();
            let stream = create_readable_byte_stream_from_bytes(vm, streams, bytes.clone());
            return Ok(Some(FetchBody {
                stream,
                source_bytes: Some(bytes),
                content_type: Some("application/x-www-form-urlencoded;charset=UTF-8".to_string()),
            }));
        }

        // 5. BufferSource (ArrayBuffer / TypedArray / DataView; reject SharedArrayBuffer!)
        let buf_opt = extract_buffer_source_bytes(vm, h)?;
        if let Some(bytes) = buf_opt {
            let stream = create_readable_byte_stream_from_bytes(vm, streams, bytes.clone());
            return Ok(Some(FetchBody {
                stream,
                source_bytes: Some(bytes),
                content_type: None,
            }));
        }
    }

    // 6. USVString fallback (default Content-Type: text/plain;charset=UTF-8).
    let text = vm.to_js_string(val)?;
    let bytes = text.into_bytes();
    let stream = create_readable_byte_stream_from_bytes(vm, streams, bytes.clone());
    Ok(Some(FetchBody {
        stream,
        source_bytes: Some(bytes),
        content_type: Some("text/plain;charset=UTF-8".to_string()),
    }))
}

fn extract_buffer_source_bytes(
    vm: &mut JsVm,
    h: HeapHandle,
) -> Result<Option<Vec<u8>>, JsException> {
    let kind_info = vm.heap.get_object(h).map(|o| {
        let is_shared = o.rare.as_deref().is_some_and(|r| r.shared_mem.is_some());
        let is_detached = o.rare.as_deref().is_some_and(|r| r.ab_detached);
        match &o.kind {
            ObjectKind::ArrayBuffer(buf) => Some((h, 0usize, buf.len(), is_shared, is_detached)),
            ObjectKind::TypedArray(ta) => Some((
                ta.buffer,
                ta.byte_offset as usize,
                (ta.length as usize) * ta.kind.bytes_per_element(),
                false,
                false,
            )),
            ObjectKind::DataView(dv) => Some((
                dv.buffer,
                dv.byte_offset as usize,
                dv.byte_length as usize,
                false,
                false,
            )),
            _ => None,
        }
    });
    let Some(Some((buf_h, off, len, ab_shared, ab_detached))) = kind_info else {
        return Ok(None);
    };
    let (buf_shared, buf_detached) =
        vm.heap
            .get_object(buf_h)
            .map_or((ab_shared, ab_detached), |o| {
                (
                    o.rare.as_deref().is_some_and(|r| r.shared_mem.is_some()),
                    o.rare.as_deref().is_some_and(|r| r.ab_detached),
                )
            });
    if buf_shared {
        return Err(vm.type_error("SharedArrayBuffer cannot be used as a Fetch body"));
    }
    if buf_detached || len == 0 {
        return Ok(Some(Vec::new()));
    }
    let bytes = vm
        .heap
        .get_object(buf_h)
        .and_then(|o| match &o.kind {
            ObjectKind::ArrayBuffer(buf) => {
                let end = off.saturating_add(len).min(buf.len());
                (off < end).then(|| buf[off..end].to_vec())
            }
            _ => None,
        })
        .unwrap_or_default();
    Ok(Some(bytes))
}

/// Clones a `FetchBody` by teeing its `ReadableStream` per WHATWG Fetch §5.2.
/// Returns `(updated_original_body, cloned_body)`.
pub(crate) fn clone_fetch_body(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    streams: &SharedStreams,
    body: Option<&FetchBody>,
) -> Result<(Option<FetchBody>, Option<FetchBody>), JsException> {
    let Some(b) = body else {
        return Ok((None, None));
    };
    if body_is_unusable(streams, Some(b)) {
        return Err(vm.type_error("Cannot clone a disturbed or locked body"));
    }
    let (b1, b2) = tee_readable_stream_for_fetch(vm, streams, web_apis, b.stream)?;
    Ok((
        Some(FetchBody {
            stream: b1,
            source_bytes: b.source_bytes.clone(),
            content_type: b.content_type.clone(),
        }),
        Some(FetchBody {
            stream: b2,
            source_bytes: b.source_bytes.clone(),
            content_type: b.content_type.clone(),
        }),
    ))
}

/// Transfers ownership of `body` into a new `Request` per WHATWG Fetch §5.4 step 36:
/// keeps the input `Request`'s original `body.stream` identity intact while marking it
/// locked & disturbed, and produces a brand-new `ReadableStream` for the output `Request`.
pub(crate) fn transfer_request_body(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    streams: &SharedStreams,
    body: &FetchBody,
) -> Result<FetchBody, JsException> {
    if body_is_unusable(streams, Some(body)) {
        return Err(vm.type_error("Cannot construct Request from a disturbed or locked Request"));
    }
    if let Some(ref bytes) = body.source_bytes {
        let _ = acquire_default_reader(vm, streams, body.stream)?;
        if let Some(rs) = streams.borrow_mut().readable.get_mut(&body.stream) {
            rs.disturbed = true;
        }
        let new_stream = create_readable_byte_stream_from_bytes(vm, streams, bytes.clone());
        return Ok(FetchBody {
            stream: new_stream,
            source_bytes: Some(bytes.clone()),
            content_type: body.content_type.clone(),
        });
    }
    // User-created ReadableStream: tee acquires a reader on `body.stream` (keeping its
    // identity on the input Request while marking it locked + disturbed!).
    let (_b1, b2) = tee_readable_stream_for_fetch(vm, streams, web_apis, body.stream)?;
    if let Some(rs) = streams.borrow_mut().readable.get_mut(&body.stream) {
        rs.disturbed = true;
    }
    Ok(FetchBody {
        stream: b2,
        source_bytes: None,
        content_type: body.content_type.clone(),
    })
}

/// Consumes all bytes from a `FetchBody` via internal `ReadRequest::Callback`
/// (zero `Object.prototype.then` interception, strict `Uint8Array` chunk check!).
pub(crate) fn consume_fetch_body_promise<F>(
    vm: &mut JsVm,
    streams: &SharedStreams,
    body: Option<&FetchBody>,
    finish: F,
) -> HeapHandle
where
    F: FnOnce(&mut JsVm, Vec<u8>) -> Result<JsValue, JsException> + 'static,
{
    let Some(b) = body else {
        let p = alloc_promise(vm);
        match finish(vm, Vec::new()) {
            Ok(v) => resolve_promise(vm, p, v),
            Err(e) => reject_promise(vm, p, e.value),
        }
        return p;
    };
    if body_is_unusable(streams, Some(b)) {
        let err = vm.type_error("Body is disturbed or locked");
        return promise_reject_value(vm, err.value);
    }
    let stream_h = b.stream;
    let reader_h = match acquire_default_reader(vm, streams, stream_h) {
        Ok(r) => r,
        Err(e) => return promise_reject_value(vm, e.value),
    };
    // WHATWG Fetch §5.2 `consume body`: keep the reader locked on the stream (never release!).
    let out_p = alloc_promise(vm);
    // Keep `out_p` and `reader_h` rooted in `active_pipes` until settled so GC cannot sweep them!
    streams
        .borrow_mut()
        .active_pipes
        .insert(out_p, vec![stream_h, reader_h]);

    let buf = Rc::new(RefCell::new(Vec::<u8>::new()));
    let finish_cell = Rc::new(RefCell::new(Some(finish)));

    fn pump_next<F>(
        vm: &mut JsVm,
        streams: &SharedStreams,
        stream_h: HeapHandle,
        out_p: HeapHandle,
        buf: Rc<RefCell<Vec<u8>>>,
        finish_cell: Rc<RefCell<Option<F>>>,
    ) where
        F: FnOnce(&mut JsVm, Vec<u8>) -> Result<JsValue, JsException> + 'static,
    {
        let ss_c = Rc::clone(streams);
        let buf_c = Rc::clone(&buf);
        let fin_c = Rc::clone(&finish_cell);

        let ss_cl = Rc::clone(streams);
        let buf_cl = Rc::clone(&buf);
        let fin_cl = Rc::clone(&finish_cell);

        let ss_er = Rc::clone(streams);

        let req = ReadRequest::Callback {
            on_chunk: Rc::new(RefCell::new(Some(Box::new(
                move |vm: &mut JsVm, chunk: JsValue| {
                    // WHATWG Streams §4.2.4 "read all bytes": every chunk MUST be a Uint8Array!
                    let chunk_bytes = extract_uint8_array_bytes(vm, chunk);
                    let Some(bytes) = chunk_bytes else {
                        let err = vm
                            .type_error("ReadableStream chunk is not a Uint8Array")
                            .value;
                        let _ = readable_cancel_internal(vm, &ss_c, stream_h, err);
                        ss_c.borrow_mut().active_pipes.remove(&out_p);
                        reject_promise(vm, out_p, err);
                        return;
                    };
                    buf_c.borrow_mut().extend_from_slice(&bytes);
                    pump_next(vm, &ss_c, stream_h, out_p, buf_c, fin_c);
                },
            )))),
            on_close: Rc::new(RefCell::new(Some(Box::new(move |vm: &mut JsVm| {
                ss_cl.borrow_mut().active_pipes.remove(&out_p);
                let bytes = std::mem::take(&mut *buf_cl.borrow_mut());
                if let Some(f) = fin_cl.borrow_mut().take() {
                    match f(vm, bytes) {
                        Ok(v) => resolve_promise(vm, out_p, v),
                        Err(e) => reject_promise(vm, out_p, e.value),
                    }
                }
            })))),
            on_error: Rc::new(RefCell::new(Some(Box::new(
                move |vm: &mut JsVm, err: JsValue| {
                    ss_er.borrow_mut().active_pipes.remove(&out_p);
                    reject_promise(vm, out_p, err);
                },
            )))),
        };
        readable_read_with_request(vm, streams, stream_h, req);
    }

    pump_next(vm, streams, stream_h, out_p, buf, finish_cell);
    out_p
}

fn extract_uint8_array_bytes(vm: &JsVm, val: JsValue) -> Option<Vec<u8>> {
    let h = val.as_object()?;
    let o = vm.heap.get_object(h)?;
    let ObjectKind::TypedArray(ta) = &o.kind else {
        return None;
    };
    if ta.kind != TypedArrayKind::Uint8 {
        return None;
    }
    let off = ta.byte_offset as usize;
    let len = ta.length as usize;
    let buf_obj = vm.heap.get_object(ta.buffer)?;
    if buf_obj.rare.as_deref().is_some_and(|r| r.ab_detached) {
        return Some(Vec::new());
    }
    if let ObjectKind::ArrayBuffer(buf) = &buf_obj.kind {
        let end = off.saturating_add(len).min(buf.len());
        if off <= end {
            return Some(buf[off..end].to_vec());
        }
    }
    Some(Vec::new())
}

// ---------------------------------------------------------------------------
// Body Mixin Converters (text / json / arrayBuffer / bytes / blob / formData)
// ---------------------------------------------------------------------------

pub(crate) fn body_to_text(vm: &mut JsVm, bytes: Vec<u8>) -> Result<JsValue, JsException> {
    let text = utf8_decode_strip_bom(&bytes);
    Ok(JsValue::from_string(vm.heap.alloc_string(&text)))
}

pub(crate) fn body_to_json(vm: &mut JsVm, bytes: Vec<u8>) -> Result<JsValue, JsException> {
    let text = utf8_decode_strip_bom(&bytes);
    let k_json = vm.heap.intern_key("JSON");
    let k_parse = vm.heap.intern_key("parse");
    let json_v = vm.get_property_by_key(vm.global, k_json)?;
    let jh = json_v
        .as_object()
        .ok_or_else(|| vm.type_error("JSON global unavailable"))?;
    let parse_fn = vm.get_property_by_key(jh, k_parse)?;
    let s_val = JsValue::from_string(vm.heap.alloc_string(&text));
    vm.call_value(parse_fn, json_v, &[s_val])
}

pub(crate) fn body_to_array_buffer(vm: &mut JsVm, bytes: Vec<u8>) -> Result<JsValue, JsException> {
    Ok(JsValue::from_object(alloc_array_buffer(vm, bytes)))
}

pub(crate) fn body_to_bytes(vm: &mut JsVm, bytes: Vec<u8>) -> Result<JsValue, JsException> {
    Ok(JsValue::from_object(alloc_uint8_array(vm, bytes)))
}

pub(crate) fn body_to_blob(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    bytes: Vec<u8>,
    content_type: Option<String>,
) -> Result<JsValue, JsException> {
    let mime = content_type
        .as_deref()
        .map(normalize_blob_mime_type)
        .unwrap_or_default();
    let blob = create_blob_object(vm, web_apis, bytes, mime);
    Ok(JsValue::from_object(blob))
}

fn normalize_blob_mime_type(raw: &str) -> String {
    let trimmed = trim_http_whitespace(raw);
    if trimmed.is_empty() {
        return String::new();
    }
    // WHATWG MIME Sniffing §4.4: lowercase type/subtype and parameter names.
    let mut parts = trimmed.split(';');
    let Some(first) = parts.next() else {
        return String::new();
    };
    let mut out = trim_http_whitespace(first).to_ascii_lowercase();
    for p in parts {
        let p = trim_http_whitespace(p);
        if p.is_empty() {
            continue;
        }
        if let Some((k, v)) = p.split_once('=') {
            out.push(';');
            out.push_str(&k.trim().to_ascii_lowercase());
            out.push('=');
            out.push_str(v.trim());
        }
    }
    out
}

/// Parses `multipart/form-data` or `application/x-www-form-urlencoded` per WHATWG Fetch §5.2 `formData()`.
pub(crate) fn body_to_form_data(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    bytes: Vec<u8>,
    content_type: Option<String>,
) -> Result<JsValue, JsException> {
    let Some(ct) = content_type else {
        return Err(vm.type_error("MIME type missing for formData()"));
    };
    let lower_ct = ct.to_ascii_lowercase();
    let essence = lower_ct
        .split(';')
        .next()
        .map(trim_http_whitespace)
        .unwrap_or("");
    if essence == "application/x-www-form-urlencoded" {
        let fd = alloc_form_data_from_urlencoded(vm, web_apis, &bytes);
        return Ok(JsValue::from_object(fd));
    }
    if essence == "multipart/form-data" {
        let boundary = extract_multipart_boundary(&ct)
            .ok_or_else(|| vm.type_error("Missing multipart/form-data boundary"))?;
        let fd = parse_multipart_form_data(vm, web_apis, &bytes, &boundary)?;
        return Ok(JsValue::from_object(fd));
    }
    Err(vm.type_error(format!(
        "Content-Type '{essence}' cannot be parsed as FormData"
    )))
}

fn extract_multipart_boundary(ct: &str) -> Option<String> {
    for param in ct.split(';').skip(1) {
        let param = trim_http_whitespace(param);
        let Some((k, v)) = param.split_once('=') else {
            continue;
        };
        if !k.trim().eq_ignore_ascii_case("boundary") {
            continue;
        }
        let v = v.trim();
        let unquoted = if v.starts_with('"') && v.ends_with('"') && v.len() >= 2 {
            &v[1..v.len() - 1]
        } else {
            v
        };
        if !unquoted.is_empty() {
            return Some(unquoted.to_string());
        }
    }
    None
}

fn parse_multipart_form_data(
    vm: &mut JsVm,
    web_apis: &SharedWebApis,
    bytes: &[u8],
    boundary: &str,
) -> Result<HeapHandle, JsException> {
    let delim = format!("--{boundary}").into_bytes();
    let mut pos = find_subslice(bytes, &delim, 0)
        .ok_or_else(|| vm.type_error("Invalid multipart/form-data body: boundary not found"))?;
    let mut pairs: Vec<(String, FormDataVal)> = Vec::new();

    loop {
        pos += delim.len();
        if pos + 2 <= bytes.len() && &bytes[pos..pos + 2] == b"--" {
            let mut end_pos = pos + 2;
            while end_pos < bytes.len() && matches!(bytes[end_pos], b' ' | b'\t') {
                end_pos += 1;
            }
            if end_pos == bytes.len()
                || (end_pos + 2 <= bytes.len() && &bytes[end_pos..end_pos + 2] == b"\r\n")
                || bytes[end_pos] == b'\n'
            {
                break;
            }
            return Err(vm.type_error("Invalid closing multipart/form-data boundary"));
        }
        // Skip optional trailing horizontal whitespace before CRLF/LF on boundary line.
        while pos < bytes.len() && matches!(bytes[pos], b' ' | b'\t') {
            pos += 1;
        }
        if pos + 2 <= bytes.len() && &bytes[pos..pos + 2] == b"\r\n" {
            pos += 2;
        } else if pos < bytes.len() && bytes[pos] == b'\n' {
            pos += 1;
        } else {
            return Err(vm.type_error("Invalid multipart/form-data boundary line"));
        }

        // Parse part headers until blank line (`\r\n\r\n` or `\n\n`).
        let (header_bytes, body_start) = if let Some(off) = find_subslice(bytes, b"\r\n\r\n", pos) {
            (&bytes[pos..off], off + 4)
        } else if let Some(off) = find_subslice(bytes, b"\n\n", pos) {
            (&bytes[pos..off], off + 2)
        } else {
            return Err(vm.type_error("Invalid multipart/form-data part headers"));
        };

        // Locate next delimiter (`\r\n--boundary` or `\n--boundary`).
        let next_delim = find_next_multipart_delim(bytes, &delim, body_start)
            .ok_or_else(|| vm.type_error("Unterminated multipart/form-data part"))?;
        let part_body = &bytes[body_start..next_delim.body_end];
        pos = next_delim.delim_pos;

        // Inspect Content-Disposition and Content-Type headers.
        let header_str = String::from_utf8_lossy(header_bytes);
        let mut disp_name: Option<String> = None;
        let mut disp_filename: Option<String> = None;
        let mut part_ct = String::from("text/plain");
        for line in header_str.lines() {
            let line = line.trim_end_matches('\r');
            if let Some((hk, hv)) = line.split_once(':') {
                let hk = hk.trim();
                let hv = hv.trim();
                if hk.eq_ignore_ascii_case("content-disposition") {
                    for seg in hv.split(';').skip(1) {
                        let seg = seg.trim();
                        if let Some((pk, pv)) = seg.split_once('=') {
                            let pk = pk.trim();
                            let pv = unquote_disposition_param(pv.trim());
                            if pk.eq_ignore_ascii_case("name") {
                                disp_name = Some(pv);
                            } else if pk.eq_ignore_ascii_case("filename") {
                                disp_filename = Some(pv);
                            }
                        }
                    }
                } else if hk.eq_ignore_ascii_case("content-type") {
                    part_ct = normalize_blob_mime_type(hv);
                }
            }
        }
        let Some(field_name) = disp_name else {
            return Err(vm.type_error("Multipart part missing Content-Disposition name"));
        };
        if let Some(filename) = disp_filename {
            let file_proto = web_apis
                .borrow()
                .protos
                .file_proto
                .unwrap_or(vm.object_proto);
            let file_h = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(file_proto)));
            web_apis.borrow_mut().blobs.insert(
                file_h,
                BlobEntry {
                    bytes: part_body.to_vec(),
                    content_type: part_ct,
                    name: Some(filename),
                    last_modified: Some(0.0),
                },
            );
            pairs.push((field_name, FormDataVal::File(file_h)));
        } else {
            let text = String::from_utf8_lossy(part_body).into_owned();
            pairs.push((field_name, FormDataVal::Text(text)));
        }
    }

    let proto = web_apis
        .borrow()
        .protos
        .form_data_proto
        .unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    web_apis
        .borrow_mut()
        .form_data
        .insert(obj, FormDataEntry { pairs });
    Ok(obj)
}

fn unquote_disposition_param(raw: &str) -> String {
    if raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2 {
        let inner = &raw[1..raw.len() - 1];
        let mut out = String::with_capacity(inner.len());
        let mut chars = inner.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(next) = chars.next() {
                    out.push(next);
                }
            } else {
                out.push(c);
            }
        }
        out
    } else {
        raw.to_string()
    }
}

struct NextDelim {
    body_end: usize,
    delim_pos: usize,
}

fn find_next_multipart_delim(bytes: &[u8], delim: &[u8], start: usize) -> Option<NextDelim> {
    let mut search = start;
    while let Some(pos) = find_subslice(bytes, delim, search) {
        if pos >= start + 2 && &bytes[pos - 2..pos] == b"\r\n" {
            return Some(NextDelim {
                body_end: pos - 2,
                delim_pos: pos,
            });
        }
        if pos > start && bytes[pos - 1] == b'\n' {
            return Some(NextDelim {
                body_end: pos - 1,
                delim_pos: pos,
            });
        }
        if pos == start {
            return Some(NextDelim {
                body_end: start,
                delim_pos: pos,
            });
        }
        search = pos + 1;
    }
    None
}

fn find_subslice(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || from + needle.len() > haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|i| from + i)
}

//! WHATWG Fetch Standard §2.2 & §5.1 (`Headers`, header name/value validation,
//! ByteString conversion, whitespace normalization, forbidden header names,
//! CORS-safelisted request-headers, `Set-Cookie` iteration, and `getSetCookie()`).

use std::collections::HashMap;
use std::rc::Rc;

use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};

use crate::fetch_ws::SharedNetBridge;
use crate::streams_common::set_to_string_tag;
use crate::web_apis::{arg, bind_ctor_proto, define_global, define_host_method};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeaderIterKind {
    Entries,
    Keys,
    Values,
}

#[derive(Debug, Clone)]
pub(crate) struct HeaderIterState {
    pub(crate) target: HeapHandle,
    pub(crate) index: usize,
    pub(crate) kind: HeaderIterKind,
}

// ---------------------------------------------------------------------------
// Header Guard & Validation Rules (WHATWG Fetch §2.2 / §5.1)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum HeadersGuard {
    #[default]
    None,
    Immutable,
    Request,
    RequestNoCors,
    Response,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct HeadersEntry {
    /// Ordered `(lowercase_name, normalized_value)` pairs.
    pub(crate) pairs: Vec<(String, String)>,
    pub(crate) guard: HeadersGuard,
}

/// Converts a JS value to a WebIDL `ByteString` (all code units <= U+00FF).
pub(crate) fn to_byte_string(vm: &mut JsVm, val: JsValue) -> Result<String, JsException> {
    let s = vm.to_js_string(val)?;
    if s.chars().any(|c| (c as u32) > 0xFF) {
        return Err(vm.type_error("String contains non-ISO-8859-1 code point (invalid ByteString)"));
    }
    Ok(s)
}

/// RFC 9110 §5.6.2 HTTP header name (`token` = `1*tchar`).
pub(crate) fn is_valid_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

/// Trims leading and trailing HTTP whitespace (`\t`, `\n`, `\r`, ` `) per WHATWG Fetch §2.2.
pub(crate) fn trim_http_whitespace(s: &str) -> &str {
    s.trim_matches(|c: char| matches!(c, '\t' | '\n' | '\r' | ' '))
}

/// Validates and normalizes an HTTP header value per WHATWG Fetch §2.2:
/// - Must be a valid ByteString (no code points > 0xFF)
/// - Leading and trailing HTTP whitespace (`\t`, `\n`, `\r`, ` `) are stripped
/// - Must NOT contain `\0`, `\r`, or `\n` anywhere inside the trimmed value.
pub(crate) fn normalize_and_validate_header_value(raw: &str) -> Result<String, &'static str> {
    if raw.chars().any(|c| (c as u32) > 0xFF) {
        return Err("Header value contains non-ByteString character");
    }
    let trimmed = trim_http_whitespace(raw);
    if trimmed.chars().any(|c| matches!(c, '\0' | '\r' | '\n')) {
        return Err("Header value contains forbidden NUL/CR/LF character");
    }
    Ok(trimmed.to_string())
}

/// WHATWG Fetch §2.2.1 HTTP method token & forbidden check.
pub(crate) fn is_forbidden_method(method: &str) -> bool {
    method.eq_ignore_ascii_case("CONNECT")
        || method.eq_ignore_ascii_case("TRACE")
        || method.eq_ignore_ascii_case("TRACK")
}

/// WHATWG Fetch §2.2.1 normalize a method (ONLY uppercase the 6 standard methods).
pub(crate) fn normalize_method(method: &str) -> String {
    if method.eq_ignore_ascii_case("DELETE") {
        "DELETE".to_string()
    } else if method.eq_ignore_ascii_case("GET") {
        "GET".to_string()
    } else if method.eq_ignore_ascii_case("HEAD") {
        "HEAD".to_string()
    } else if method.eq_ignore_ascii_case("OPTIONS") {
        "OPTIONS".to_string()
    } else if method.eq_ignore_ascii_case("POST") {
        "POST".to_string()
    } else if method.eq_ignore_ascii_case("PUT") {
        "PUT".to_string()
    } else {
        method.to_string()
    }
}

/// WHATWG Fetch §2.2.2 forbidden request-header check (including `X-HTTP-Method-Override`).
pub(crate) fn is_forbidden_request_header(lower_name: &str, value: &str) -> bool {
    if lower_name.starts_with("proxy-") || lower_name.starts_with("sec-") {
        return true;
    }
    if matches!(
        lower_name,
        "accept-charset"
            | "accept-encoding"
            | "access-control-request-headers"
            | "access-control-request-method"
            | "connection"
            | "content-length"
            | "cookie"
            | "cookie2"
            | "date"
            | "dnt"
            | "expect"
            | "host"
            | "keep-alive"
            | "origin"
            | "referer"
            | "set-cookie"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "via"
    ) {
        return true;
    }
    if matches!(
        lower_name,
        "x-http-method" | "x-http-method-override" | "x-method-override"
    ) {
        // Parse comma-separated HTTP method values; if any is CONNECT/TRACE/TRACK -> forbidden!
        for part in value.split(',') {
            let token = part.trim_matches(|c: char| matches!(c, '\t' | ' '));
            if is_forbidden_method(token) {
                return true;
            }
        }
    }
    false
}

/// WHATWG Fetch §2.2.2 forbidden response-header name.
pub(crate) fn is_forbidden_response_header_name(lower_name: &str) -> bool {
    matches!(lower_name, "set-cookie" | "set-cookie2")
}

/// Checks for CORS-unsafe request-header bytes (WHATWG Fetch §2.2.2).
fn contains_cors_unsafe_request_header_byte(value: &str) -> bool {
    value.chars().any(|c| {
        let code = c as u32;
        (code < 0x20 && code != 0x09)
            || code == 0x7F
            || matches!(
                c,
                '"' | '(' | ')' | ':' | '<' | '>' | '?' | '@' | '[' | '\\' | ']' | '{' | '}'
            )
    })
}

/// WHATWG Fetch §2.2.2 `CORS-safelisted request-header`.
pub(crate) fn is_cors_safelisted_request_header(lower_name: &str, value: &str) -> bool {
    if value.chars().count() > 128 {
        return false;
    }
    match lower_name {
        "accept" => !contains_cors_unsafe_request_header_byte(value),
        "accept-language" | "content-language" => value.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, ' ' | '*' | ',' | '-' | '.' | ';' | '=')
        }),
        "content-type" => {
            if contains_cors_unsafe_request_header_byte(value) {
                return false;
            }
            let essence = parse_mime_essence(value);
            matches!(
                essence.as_deref(),
                Some("application/x-www-form-urlencoded" | "multipart/form-data" | "text/plain")
            )
        }
        "range" => is_simple_range_header_value(value),
        _ => false,
    }
}

/// WHATWG Fetch §2.2.2 `no-CORS-safelisted request-header`.
pub(crate) fn is_no_cors_safelisted_request_header(lower_name: &str, value: &str) -> bool {
    matches!(
        lower_name,
        "accept" | "accept-language" | "content-language" | "content-type"
    ) && is_cors_safelisted_request_header(lower_name, value)
}

pub(crate) fn is_no_cors_safelisted_request_header_name(lower_name: &str) -> bool {
    matches!(
        lower_name,
        "accept" | "accept-language" | "content-language" | "content-type"
    )
}

fn is_simple_range_header_value(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("bytes=") else {
        return false;
    };
    let Some((start, end)) = rest.split_once('-') else {
        return false;
    };
    if start.is_empty() || !start.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if end.is_empty() {
        return true;
    }
    if !end.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    match (start.parse::<u128>(), end.parse::<u128>()) {
        (Ok(s), Ok(e)) => s <= e,
        _ => false,
    }
}

/// Minimal WHATWG MIME Sniffing §4.4 essence extractor (`type/subtype`, lowercased).
pub(crate) fn parse_mime_essence(input: &str) -> Option<String> {
    let trimmed = trim_http_whitespace(input);
    let (main, _params) = trimmed.split_once(';').unwrap_or((trimmed, ""));
    let (ty, sub) = main.split_once('/')?;
    let ty = ty.trim_end_matches(['\t', ' ']);
    let sub = sub.trim_matches(|c: char| matches!(c, '\t' | ' '));
    if !is_valid_header_name(ty) || !is_valid_header_name(sub) {
        return None;
    }
    Some(format!(
        "{}/{}",
        ty.to_ascii_lowercase(),
        sub.to_ascii_lowercase()
    ))
}

/// Validates HTTP `statusText` (`reason-phrase`: `*( HTAB / SP / VCHAR / obs-text )`).
pub(crate) fn is_valid_status_text(s: &str) -> bool {
    s.chars().all(|c| {
        let cp = c as u32;
        cp == 0x09 || (0x20..=0x7E).contains(&cp) || (0x80..=0xFF).contains(&cp)
    })
}

// ---------------------------------------------------------------------------
// HeadersEntry Implementation
// ---------------------------------------------------------------------------

impl HeadersEntry {
    pub(crate) fn with_guard(guard: HeadersGuard) -> Self {
        Self {
            pairs: Vec::new(),
            guard,
        }
    }

    pub(crate) fn from_map(map: &HashMap<String, String>, guard: HeadersGuard) -> Self {
        let mut pairs: Vec<(String, String)> = map
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), trim_http_whitespace(v).to_string()))
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        Self { pairs, guard }
    }

    pub(crate) fn to_map(&self) -> HashMap<String, String> {
        let mut map: HashMap<String, String> = HashMap::new();
        for (k, v) in &self.pairs {
            map.entry(k.clone())
                .and_modify(|e| {
                    e.push_str(", ");
                    e.push_str(v);
                })
                .or_insert_with(|| v.clone());
        }
        map
    }

    pub(crate) fn get_raw(&self, lower_name: &str) -> Option<String> {
        let mut joined = String::new();
        let mut found = false;
        for (k, v) in &self.pairs {
            if k == lower_name {
                if found {
                    joined.push_str(", ");
                }
                joined.push_str(v);
                found = true;
            }
        }
        found.then_some(joined)
    }

    pub(crate) fn get(&self, name: &str) -> Option<String> {
        self.get_raw(&name.to_ascii_lowercase())
    }

    pub(crate) fn get_set_cookie(&self) -> Vec<String> {
        self.pairs
            .iter()
            .filter(|(k, _)| k == "set-cookie")
            .map(|(_, v)| v.clone())
            .collect()
    }

    pub(crate) fn has(&self, name: &str) -> bool {
        let lower = name.to_ascii_lowercase();
        self.pairs.iter().any(|(k, _)| k == &lower)
    }

    /// Raw unchecked append (used for internal system headers like `Response.redirect`).
    pub(crate) fn push_unchecked(&mut self, name: &str, value: String) {
        self.pairs.push((name.to_ascii_lowercase(), value));
    }

    /// Raw unchecked set (used for internal default `Content-Type`).
    pub(crate) fn set_unchecked(&mut self, name: &str, value: String) {
        let lower = name.to_ascii_lowercase();
        if let Some(pos) = self.pairs.iter().position(|(k, _)| k == &lower) {
            self.pairs[pos].1 = value;
            let mut idx = 0usize;
            self.pairs.retain(|(k, _)| {
                let keep = idx == pos || k != &lower;
                idx += 1;
                keep
            });
        } else {
            self.pairs.push((lower, value));
        }
    }

    /// Guard-checked WHATWG Fetch §5.1 `Headers.prototype.append(name, value)`.
    pub(crate) fn append_checked(
        &mut self,
        vm: &mut JsVm,
        name: &str,
        raw_value: &str,
    ) -> Result<(), JsException> {
        let value = normalize_and_validate_header_value(raw_value).map_err(|m| vm.type_error(m))?;
        if !is_valid_header_name(name) {
            return Err(vm.type_error(format!("Invalid header name: '{name}'")));
        }
        let lower = name.to_ascii_lowercase();
        match self.guard {
            HeadersGuard::Immutable => {
                return Err(vm.type_error("Headers are immutable"));
            }
            HeadersGuard::Request => {
                if is_forbidden_request_header(&lower, &value) {
                    return Ok(());
                }
            }
            HeadersGuard::RequestNoCors => {
                let temp = match self.get_raw(&lower) {
                    Some(existing) => format!("{existing}, {value}"),
                    None => value.clone(),
                };
                if !is_no_cors_safelisted_request_header(&lower, &temp) {
                    return Ok(());
                }
            }
            HeadersGuard::Response => {
                if is_forbidden_response_header_name(&lower) {
                    return Ok(());
                }
            }
            HeadersGuard::None => {}
        }
        self.pairs.push((lower, value));
        Ok(())
    }

    /// Guard-checked WHATWG Fetch §5.1 `Headers.prototype.set(name, value)`.
    pub(crate) fn set_checked(
        &mut self,
        vm: &mut JsVm,
        name: &str,
        raw_value: &str,
    ) -> Result<(), JsException> {
        let value = normalize_and_validate_header_value(raw_value).map_err(|m| vm.type_error(m))?;
        if !is_valid_header_name(name) {
            return Err(vm.type_error(format!("Invalid header name: '{name}'")));
        }
        let lower = name.to_ascii_lowercase();
        match self.guard {
            HeadersGuard::Immutable => {
                return Err(vm.type_error("Headers are immutable"));
            }
            HeadersGuard::Request => {
                if is_forbidden_request_header(&lower, &value) {
                    return Ok(());
                }
            }
            HeadersGuard::RequestNoCors => {
                if !is_no_cors_safelisted_request_header(&lower, &value) {
                    return Ok(());
                }
            }
            HeadersGuard::Response => {
                if is_forbidden_response_header_name(&lower) {
                    return Ok(());
                }
            }
            HeadersGuard::None => {}
        }
        self.set_unchecked(&lower, value);
        Ok(())
    }

    /// Guard-checked WHATWG Fetch §5.1 `Headers.prototype.delete(name)`.
    pub(crate) fn delete_checked(&mut self, vm: &mut JsVm, name: &str) -> Result<(), JsException> {
        if !is_valid_header_name(name) {
            return Err(vm.type_error(format!("Invalid header name: '{name}'")));
        }
        let lower = name.to_ascii_lowercase();
        match self.guard {
            HeadersGuard::Immutable => {
                return Err(vm.type_error("Headers are immutable"));
            }
            HeadersGuard::Request => {
                // Check with empty value (method-override headers are only forbidden if value contains CONNECT/TRACE/TRACK).
                if is_forbidden_request_header(&lower, "") {
                    return Ok(());
                }
            }
            HeadersGuard::RequestNoCors => {
                if !is_no_cors_safelisted_request_header_name(&lower) && lower != "range" {
                    return Ok(());
                }
            }
            HeadersGuard::Response => {
                if is_forbidden_response_header_name(&lower) {
                    return Ok(());
                }
            }
            HeadersGuard::None => {}
        }
        self.pairs.retain(|(k, _)| k != &lower);
        Ok(())
    }

    /// WHATWG Fetch §5.1 "sort and combine":
    /// - Sort alphabetically by lowercase header name.
    /// - Combine duplicate non-`set-cookie` headers with `", "`.
    /// - Emit EVERY `set-cookie` header separately in original insertion order!
    pub(crate) fn sort_and_combine(&self) -> Vec<(String, String)> {
        let mut combined: HashMap<String, String> = HashMap::new();
        let mut set_cookies: Vec<String> = Vec::new();
        for (k, v) in &self.pairs {
            if k == "set-cookie" {
                set_cookies.push(v.clone());
            } else {
                combined
                    .entry(k.clone())
                    .and_modify(|e| {
                        e.push_str(", ");
                        e.push_str(v);
                    })
                    .or_insert_with(|| v.clone());
            }
        }
        let mut out: Vec<(String, String)> = combined.into_iter().collect();
        for sc in set_cookies {
            out.push(("set-cookie".to_string(), sc));
        }
        // Stable sort preserves insertion order among multiple `set-cookie` entries!
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

// ---------------------------------------------------------------------------
// HeadersInit Parsing & JS Installation
// ---------------------------------------------------------------------------

pub(crate) fn alloc_headers_from_entry(
    vm: &mut JsVm,
    net: &SharedNetBridge,
    entry: HeadersEntry,
) -> HeapHandle {
    let proto = net.borrow().protos.headers_proto.unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    net.borrow_mut().headers.insert(obj, entry);
    obj
}

/// Fills `entry` from WebIDL `HeadersInit` (`sequence<sequence<ByteString>>` or `record<ByteString, ByteString>`).
pub(crate) fn fill_headers_from_init(
    vm: &mut JsVm,
    _net: &SharedNetBridge,
    entry: &mut HeadersEntry,
    init: JsValue,
) -> Result<(), JsException> {
    if init.is_undefined() {
        return Ok(());
    }
    let Some(h) = init.as_object() else {
        return Err(vm.type_error("HeadersInit must be an object or iterable sequence"));
    };

    // Check `[Symbol.iterator]` FIRST (respects custom `[Symbol.iterator]` on existing Headers too!).
    let iter_method = vm.get_property_by_key(h, vm.key_symbol_iterator)?;
    if !iter_method.is_undefined() && !iter_method.is_null() {
        let items = vm.collect_iterable(init)?;
        for item in items {
            if !item.is_object() {
                return Err(vm.type_error("Headers sequence entry must be an iterable pair"));
            }
            let pair = vm.collect_iterable(item)?;
            if pair.len() != 2 {
                return Err(vm.type_error("Headers sequence entry must have exactly 2 elements"));
            }
            let k = to_byte_string(vm, pair[0])?;
            let v = to_byte_string(vm, pair[1])?;
            entry.append_checked(vm, &k, &v)?;
        }
        return Ok(());
    }

    // WebIDL `record<ByteString, ByteString>`: iterate `[[OwnPropertyKeys]]` (including Symbols!).
    let own_keys = rix_js::proxy::mop_own_property_keys(vm, h)?;
    for (key_val, pk) in own_keys {
        let desc = rix_js::proxy::mop_get_own_property(vm, h, pk)?;
        if !desc.is_some_and(|d| d.enumerable == Some(true)) {
            continue;
        }
        // `to_byte_string` on a Symbol throws TypeError per WebIDL ByteString conversion!
        let k = to_byte_string(vm, key_val)?;
        let v_val = vm.get_property_by_key(h, pk)?;
        let v = to_byte_string(vm, v_val)?;
        entry.append_checked(vm, &k, &v)?;
    }
    Ok(())
}

pub(crate) fn parse_headers_init(
    vm: &mut JsVm,
    net: &SharedNetBridge,
    init: JsValue,
    guard: HeadersGuard,
) -> Result<HeadersEntry, JsException> {
    let mut entry = HeadersEntry::with_guard(guard);
    fill_headers_from_init(vm, net, &mut entry, init)?;
    Ok(entry)
}

pub(crate) fn require_headers(
    vm: &mut JsVm,
    net: &SharedNetBridge,
    val: JsValue,
) -> Result<HeapHandle, JsException> {
    let Some(h) = val.as_object() else {
        return Err(vm.type_error("Incompatible Headers receiver"));
    };
    if net.borrow().headers.contains_key(&h) {
        Ok(h)
    } else {
        Err(vm.type_error("Incompatible Headers receiver"))
    }
}

pub(crate) fn install_headers(vm: &mut JsVm, net: &SharedNetBridge) {
    let hp = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(hp).unwrap().is_prototype = true;
    set_to_string_tag(vm, hp, "Headers");
    net.borrow_mut().protos.headers_proto = Some(hp);

    let nb = Rc::clone(net);
    define_host_method(vm, hp, "append", move |vm, this_val, args| {
        let h = require_headers(vm, &nb, this_val)?;
        if args.len() < 2 {
            return Err(vm.type_error("Headers.append requires 2 arguments"));
        }
        let name = to_byte_string(vm, arg(args, 0))?;
        let value = to_byte_string(vm, arg(args, 1))?;
        let mut entry = nb.borrow().headers.get(&h).cloned().unwrap();
        entry.append_checked(vm, &name, &value)?;
        nb.borrow_mut().headers.insert(h, entry);
        Ok(JsValue::UNDEFINED)
    });

    let nb = Rc::clone(net);
    define_host_method(vm, hp, "set", move |vm, this_val, args| {
        let h = require_headers(vm, &nb, this_val)?;
        if args.len() < 2 {
            return Err(vm.type_error("Headers.set requires 2 arguments"));
        }
        let name = to_byte_string(vm, arg(args, 0))?;
        let value = to_byte_string(vm, arg(args, 1))?;
        let mut entry = nb.borrow().headers.get(&h).cloned().unwrap();
        entry.set_checked(vm, &name, &value)?;
        nb.borrow_mut().headers.insert(h, entry);
        Ok(JsValue::UNDEFINED)
    });

    let nb = Rc::clone(net);
    define_host_method(vm, hp, "delete", move |vm, this_val, args| {
        let h = require_headers(vm, &nb, this_val)?;
        if args.is_empty() {
            return Err(vm.type_error("Headers.delete requires 1 argument"));
        }
        let name = to_byte_string(vm, arg(args, 0))?;
        let mut entry = nb.borrow().headers.get(&h).cloned().unwrap();
        entry.delete_checked(vm, &name)?;
        nb.borrow_mut().headers.insert(h, entry);
        Ok(JsValue::UNDEFINED)
    });

    let nb = Rc::clone(net);
    define_host_method(vm, hp, "get", move |vm, this_val, args| {
        let h = require_headers(vm, &nb, this_val)?;
        if args.is_empty() {
            return Err(vm.type_error("Headers.get requires 1 argument"));
        }
        let name = to_byte_string(vm, arg(args, 0))?;
        if !is_valid_header_name(&name) {
            return Err(vm.type_error(format!("Invalid header name: '{name}'")));
        }
        let val = nb.borrow().headers.get(&h).and_then(|e| e.get(&name));
        Ok(match val {
            Some(s) => JsValue::from_string(vm.heap.alloc_string(&s)),
            None => JsValue::NULL,
        })
    });

    let nb = Rc::clone(net);
    define_host_method(vm, hp, "getSetCookie", move |vm, this_val, _| {
        let h = require_headers(vm, &nb, this_val)?;
        let cookies = nb
            .borrow()
            .headers
            .get(&h)
            .map(HeadersEntry::get_set_cookie)
            .unwrap_or_default();
        let arr = vm.alloc_array();
        for c in cookies {
            let v = JsValue::from_string(vm.heap.alloc_string(&c));
            vm.heap.object_push_element(arr, v);
        }
        Ok(JsValue::from_object(arr))
    });

    let nb = Rc::clone(net);
    define_host_method(vm, hp, "has", move |vm, this_val, args| {
        let h = require_headers(vm, &nb, this_val)?;
        if args.is_empty() {
            return Err(vm.type_error("Headers.has requires 1 argument"));
        }
        let name = to_byte_string(vm, arg(args, 0))?;
        if !is_valid_header_name(&name) {
            return Err(vm.type_error(format!("Invalid header name: '{name}'")));
        }
        let has = nb.borrow().headers.get(&h).is_some_and(|e| e.has(&name));
        Ok(JsValue::from_bool(has))
    });

    // WebIDL §3.7.8.2 live pair iterator prototype inheriting from `%IteratorPrototype%`.
    let hip = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.iterator_proto),
    ));
    vm.heap.get_object_mut(hip).unwrap().is_prototype = true;
    set_to_string_tag(vm, hip, "Headers Iterator");
    net.borrow_mut().protos.headers_iter_proto = Some(hip);
    {
        let nb = Rc::clone(net);
        let next_fn = vm.alloc_host_fn("next", move |vm, this_val, _| {
            let Some(ih) = this_val.as_object() else {
                return Err(vm.type_error("Incompatible Headers Iterator receiver"));
            };
            let Some((target_h, idx, kind)) = nb
                .borrow()
                .header_iterators
                .get(&ih)
                .map(|s| (s.target, s.index, s.kind))
            else {
                return Err(vm.type_error("Incompatible Headers Iterator receiver"));
            };
            let list = nb
                .borrow()
                .headers
                .get(&target_h)
                .map(HeadersEntry::sort_and_combine)
                .unwrap_or_default();
            let res = vm.alloc_ordinary_object();
            let k_val = vm.heap.intern_key("value");
            let k_done = vm.heap.intern_key("done");
            if idx >= list.len() {
                vm.heap.object_define_data(
                    res,
                    k_val,
                    JsValue::UNDEFINED,
                    PropertyAttrs::DEFAULT,
                    &mut vm.shapes,
                );
                vm.heap.object_define_data(
                    res,
                    k_done,
                    JsValue::from_bool(true),
                    PropertyAttrs::DEFAULT,
                    &mut vm.shapes,
                );
                return Ok(JsValue::from_object(res));
            }
            if let Some(st) = nb.borrow_mut().header_iterators.get_mut(&ih) {
                st.index = idx + 1;
            }
            let (k, v) = &list[idx];
            let out_val = match kind {
                HeaderIterKind::Entries => {
                    let pair = vm.alloc_array();
                    let kv = JsValue::from_string(vm.heap.alloc_string(k));
                    let vv = JsValue::from_string(vm.heap.alloc_string(v));
                    vm.heap.object_push_element(pair, kv);
                    vm.heap.object_push_element(pair, vv);
                    JsValue::from_object(pair)
                }
                HeaderIterKind::Keys => JsValue::from_string(vm.heap.alloc_string(k)),
                HeaderIterKind::Values => JsValue::from_string(vm.heap.alloc_string(v)),
            };
            vm.heap
                .object_define_data(res, k_val, out_val, PropertyAttrs::DEFAULT, &mut vm.shapes);
            vm.heap.object_define_data(
                res,
                k_done,
                JsValue::from_bool(false),
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
            Ok(JsValue::from_object(res))
        });
        let k_next = vm.heap.intern_key("next");
        vm.heap.object_define_data(
            hip,
            k_next,
            JsValue::from_object(next_fn),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
    }

    let alloc_hdr_iter = |vm: &mut JsVm,
                          nb: &SharedNetBridge,
                          target: HeapHandle,
                          kind: HeaderIterKind|
     -> HeapHandle {
        let proto = nb
            .borrow()
            .protos
            .headers_iter_proto
            .unwrap_or(vm.iterator_proto);
        let obj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
        nb.borrow_mut().header_iterators.insert(
            obj,
            HeaderIterState {
                target,
                index: 0,
                kind,
            },
        );
        obj
    };

    let nb = Rc::clone(net);
    let entries_fn = vm.alloc_host_fn("entries", move |vm, this_val, _| {
        let h = require_headers(vm, &nb, this_val)?;
        Ok(JsValue::from_object(alloc_hdr_iter(
            vm,
            &nb,
            h,
            HeaderIterKind::Entries,
        )))
    });
    let k_ent = vm.heap.intern_key("entries");
    let sym_it = vm.key_symbol_iterator;
    vm.heap.object_define_data(
        hp,
        k_ent,
        JsValue::from_object(entries_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        hp,
        sym_it,
        JsValue::from_object(entries_fn),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );

    let nb = Rc::clone(net);
    define_host_method(vm, hp, "keys", move |vm, this_val, _| {
        let h = require_headers(vm, &nb, this_val)?;
        Ok(JsValue::from_object(alloc_hdr_iter(
            vm,
            &nb,
            h,
            HeaderIterKind::Keys,
        )))
    });

    let nb = Rc::clone(net);
    define_host_method(vm, hp, "values", move |vm, this_val, _| {
        let h = require_headers(vm, &nb, this_val)?;
        Ok(JsValue::from_object(alloc_hdr_iter(
            vm,
            &nb,
            h,
            HeaderIterKind::Values,
        )))
    });

    let nb = Rc::clone(net);
    define_host_method(vm, hp, "forEach", move |vm, this_val, args| {
        let h = require_headers(vm, &nb, this_val)?;
        if args.is_empty() {
            return Err(vm.type_error("Headers.forEach requires 1 argument"));
        }
        let cb = arg(args, 0);
        if !cb.as_object().is_some_and(|ch| vm.is_callable_handle(ch)) {
            return Err(vm.type_error("Headers.forEach callback must be a function"));
        }
        let this_arg = arg(args, 1);
        let mut idx = 0usize;
        loop {
            let pair = nb
                .borrow()
                .headers
                .get(&h)
                .and_then(|e| e.sort_and_combine().into_iter().nth(idx));
            let Some((k, v)) = pair else {
                break;
            };
            idx += 1;
            let kv = JsValue::from_string(vm.heap.alloc_string(&k));
            let vv = JsValue::from_string(vm.heap.alloc_string(&v));
            vm.call_value(cb, this_arg, &[vv, kv, this_val])?;
        }
        Ok(JsValue::UNDEFINED)
    });

    let nb = Rc::clone(net);
    let ctor = vm.alloc_host_fn("Headers", move |vm, _, args| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("Headers constructor requires 'new'"));
        }
        let entry = parse_headers_init(vm, &nb, arg(args, 0), HeadersGuard::None)?;
        let obj = alloc_headers_from_entry(vm, &nb, entry);
        Ok(JsValue::from_object(obj))
    });
    bind_ctor_proto(vm, ctor, hp);
    define_global(vm, "Headers", JsValue::from_object(ctor));
}

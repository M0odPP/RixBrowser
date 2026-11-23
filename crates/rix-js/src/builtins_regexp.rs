//! ECMAScript 2025 `RegExp`, `%RegExpStringIteratorPrototype%`, `GetSubstitution`,
//! `IsRegExp`, `RegExp.escape`, Annex B `RegExp.prototype.compile`, and global
//! `escape`/`unescape`, backed by the pure-Rust `regress` ECMAScript regex engine.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use crate::heap::{JsObject, ObjectKind, RegExpObject};
use crate::shape::PropertyAttrs;
use crate::value::{HeapHandle, JsValue};
use crate::vm::{JsException, JsVm, NativeFnPtr};
use crate::vm_builtins::{define_global, define_method_len};

const CANONICAL_FLAGS: [char; 8] = ['d', 'g', 'i', 'm', 's', 'u', 'v', 'y'];

/// Legacy `RegExp` static match properties (`input`/`$_`, `lastMatch`/`$&`, `$1`..`$9`, etc.).
#[derive(Debug, Default, Clone)]
pub struct RegExpLegacyStatics {
    pub input: String,
    pub last_match: String,
    pub last_paren: String,
    pub left_context: String,
    pub right_context: String,
    pub parens: [String; 9],
}

#[inline]
fn arg(args: &[JsValue], i: usize) -> JsValue {
    args.get(i).copied().unwrap_or(JsValue::UNDEFINED)
}

/// Validates ECMAScript RegExp flags and returns them in canonical alphabetical order (`dgimsuvy`).
pub fn validate_and_sort_flags(flags: &str) -> Result<String, String> {
    let mut mask = 0u8;
    for ch in flags.chars() {
        let Some(bit_idx) = CANONICAL_FLAGS.iter().position(|&c| c == ch) else {
            return Err(format!("Invalid regular expression flag '{ch}'"));
        };
        let bit = 1u8 << bit_idx;
        if (mask & bit) != 0 {
            return Err(format!("Duplicate regular expression flag '{ch}'"));
        }
        mask |= bit;
    }
    if (mask & (1 << 5)) != 0 && (mask & (1 << 6)) != 0 {
        return Err("RegExp flags 'u' and 'v' cannot be combined".into());
    }
    let mut out = String::with_capacity(8);
    for (i, ch) in CANONICAL_FLAGS.iter().enumerate() {
        if (mask & (1u8 << i)) != 0 {
            out.push(*ch);
        }
    }
    Ok(out)
}

/// Best-effort canonical flag sorting for already-validated literal flags.
pub fn canonicalize_flags(flags: &str) -> String {
    validate_and_sort_flags(flags).unwrap_or_else(|_| flags.to_string())
}

/// Compiles an ECMAScript pattern + canonical flags using `regress`.
pub fn compile_regress_raw(pattern: &str, flags: &str) -> Result<regress::Regex, String> {
    let rf = regress::Flags {
        icase: flags.contains('i'),
        multiline: flags.contains('m'),
        dot_all: flags.contains('s'),
        unicode: flags.contains('u'),
        unicode_sets: flags.contains('v'),
        no_opt: false,
    };
    regress::Regex::with_flags(pattern, rf).map_err(|e| format!("{e}"))
}

fn compile_checked(
    vm: &mut JsVm,
    pattern: &str,
    raw_flags: &str,
) -> Result<(String, Arc<regress::Regex>), JsException> {
    let flags =
        validate_and_sort_flags(raw_flags).map_err(|msg| vm.make_error("SyntaxError", msg))?;
    let compiled = compile_regress_raw(pattern, &flags).map_err(|e| {
        vm.make_error(
            "SyntaxError",
            format!("Invalid regular expression /{pattern}/{flags}: {e}"),
        )
    })?;
    Ok((flags, Arc::new(compiled)))
}

/// ECMA-262 §22.2.3.2.4 `EscapeRegExpPattern(P, F)`.
pub fn escape_regexp_source(pattern: &str) -> String {
    if pattern.is_empty() {
        return "(?:)".to_string();
    }
    let mut out = String::with_capacity(pattern.len() + 4);
    let mut chars = pattern.chars().peekable();
    let mut in_class = false;
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if escaped {
            escaped = false;
            match ch {
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\u{2028}' => out.push_str("\\u2028"),
                '\u{2029}' => out.push_str("\\u2029"),
                _ => {
                    out.push('\\');
                    out.push(ch);
                }
            }
            continue;
        }
        match ch {
            '\\' => {
                if chars.peek().is_none() {
                    out.push('\\');
                } else {
                    escaped = true;
                }
            }
            '[' => {
                in_class = true;
                out.push('[');
            }
            ']' => {
                in_class = false;
                out.push(']');
            }
            '/' if !in_class => out.push_str("\\/"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            _ => out.push(ch),
        }
    }
    out
}

/// ECMA-262 §7.2.8 `IsRegExp(argument)`.
pub fn is_regexp(vm: &mut JsVm, val: JsValue) -> Result<bool, JsException> {
    let Some(h) = val.as_object() else {
        return Ok(false);
    };
    let matcher = vm.get_property_by_key(h, vm.key_symbol_match)?;
    if !matcher.is_undefined() {
        return Ok(vm.to_boolean(matcher));
    }
    Ok(matches!(
        vm.heap.get_object(h).map(|o| &o.kind),
        Some(ObjectKind::RegExp(_))
    ))
}

/// ECMA-262 §22.2.6.2.1 `AdvanceStringIndex(S, index, unicode)` in UTF-16 code units.
pub fn advance_string_index_u16(units: &[u16], index: usize, unicode: bool) -> usize {
    if !unicode || index + 1 >= units.len() {
        return index + 1;
    }
    let first = units[index];
    let second = units[index + 1];
    if (0xD800..=0xDBFF).contains(&first) && (0xDC00..=0xDFFF).contains(&second) {
        index + 2
    } else {
        index + 1
    }
}

/// ECMA-262 §7.3.22 `SpeciesConstructor(O, defaultConstructor)`.
fn species_constructor(
    vm: &mut JsVm,
    obj: HeapHandle,
    default_ctor: HeapHandle,
) -> Result<JsValue, JsException> {
    let c = vm.get_property_by_key(obj, vm.key_constructor)?;
    if c.is_undefined() {
        return Ok(JsValue::from_object(default_ctor));
    }
    let Some(ch) = c.as_object() else {
        return Err(vm.type_error("Species constructor property is not an object"));
    };
    let s = vm.get_property_by_key(ch, vm.key_symbol_species)?;
    if s.is_undefined() || s.is_null() {
        return Ok(JsValue::from_object(default_ctor));
    }
    if s.as_object().is_some_and(|h| vm.is_constructor_handle(h)) {
        return Ok(s);
    }
    Err(vm.type_error("Species is not a constructor"))
}

fn regexp_ctor_handle(vm: &mut JsVm) -> HeapHandle {
    let k = vm.heap.intern_key("RegExp");
    vm.get_property_by_key(vm.global, k)
        .ok()
        .and_then(|v| v.as_object())
        .unwrap_or(vm.regexp_proto)
}

fn set_last_index_strict(vm: &mut JsVm, obj: HeapHandle, val: JsValue) -> Result<(), JsException> {
    let k = vm.heap.intern_key("lastIndex");
    if !vm.set_property_by_key(obj, k, val)? {
        return Err(vm.type_error("Cannot assign to read only property 'lastIndex'"));
    }
    Ok(())
}

fn same_value_num(a: JsValue, b: f64) -> bool {
    match a.as_number() {
        Some(x) if x == 0.0 && b == 0.0 => x.is_sign_positive() == b.is_sign_positive(),
        Some(x) => x == b,
        None => false,
    }
}

fn define_getter_named(vm: &mut JsVm, target: HeapHandle, name: &str, f: NativeFnPtr) {
    let key = vm.heap.intern_key(name);
    let getter = vm.alloc_native_fn(&format!("get {name}"), f);
    vm.set_fn_length(getter, 0);
    vm.heap.object_define_accessor(
        target,
        key,
        Some(getter),
        None,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
}

fn define_sym_method(
    vm: &mut JsVm,
    target: HeapHandle,
    sym_key: crate::PropertyKey,
    label: &str,
    len: u32,
    f: NativeFnPtr,
) {
    let func = vm.alloc_native_fn(label, f);
    vm.set_fn_length(func, len);
    vm.heap.object_define_data(
        target,
        sym_key,
        JsValue::from_object(func),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
}

/// Installs `RegExp`, `RegExp.prototype`, `%RegExpStringIteratorPrototype%`, and
/// Annex B global `escape`/`unescape`.
pub fn install_regexp_and_annexb_globals(vm: &mut JsVm) {
    // %RegExpStringIteratorPrototype%
    let rip = vm.regexp_iter_proto;
    define_method_len(vm, rip, "next", 0, regexp_iter_next);
    let tag_v = JsValue::from_string(vm.heap.alloc_string("RegExp String Iterator"));
    let tag_k = vm.key_symbol_to_string_tag;
    vm.heap.object_define_data(
        rip,
        tag_k,
        tag_v,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );

    let rp = vm.regexp_proto;
    define_method_len(vm, rp, "exec", 1, regexp_exec);
    define_method_len(vm, rp, "test", 1, regexp_test);
    define_method_len(vm, rp, "toString", 0, regexp_to_string);
    define_method_len(vm, rp, "compile", 2, regexp_compile);

    define_getter_named(vm, rp, "source", regexp_get_source);
    define_getter_named(vm, rp, "flags", regexp_get_flags);
    define_getter_named(vm, rp, "hasIndices", |vm, t, _| {
        regexp_flag_getter(vm, t, 'd')
    });
    define_getter_named(vm, rp, "global", |vm, t, _| regexp_flag_getter(vm, t, 'g'));
    define_getter_named(vm, rp, "ignoreCase", |vm, t, _| {
        regexp_flag_getter(vm, t, 'i')
    });
    define_getter_named(vm, rp, "multiline", |vm, t, _| {
        regexp_flag_getter(vm, t, 'm')
    });
    define_getter_named(vm, rp, "dotAll", |vm, t, _| regexp_flag_getter(vm, t, 's'));
    define_getter_named(vm, rp, "unicode", |vm, t, _| regexp_flag_getter(vm, t, 'u'));
    define_getter_named(vm, rp, "unicodeSets", |vm, t, _| {
        regexp_flag_getter(vm, t, 'v')
    });
    define_getter_named(vm, rp, "sticky", |vm, t, _| regexp_flag_getter(vm, t, 'y'));

    let (km, kma, kr, ks, ksp) = (
        vm.key_symbol_match,
        vm.key_symbol_match_all,
        vm.key_symbol_replace,
        vm.key_symbol_search,
        vm.key_symbol_split,
    );
    define_sym_method(vm, rp, km, "[Symbol.match]", 1, regexp_symbol_match);
    define_sym_method(vm, rp, kma, "[Symbol.matchAll]", 1, regexp_symbol_match_all);
    define_sym_method(vm, rp, kr, "[Symbol.replace]", 2, regexp_symbol_replace);
    define_sym_method(vm, rp, ks, "[Symbol.search]", 1, regexp_symbol_search);
    define_sym_method(vm, rp, ksp, "[Symbol.split]", 2, regexp_symbol_split);

    let ctor = vm.alloc_native_fn("RegExp", regexp_ctor);
    vm.mark_constructor(ctor);
    vm.set_fn_length(ctor, 2);
    vm.heap.object_define_data(
        ctor,
        vm.key_prototype,
        JsValue::from_object(rp),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        rp,
        vm.key_constructor,
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    let sp_get = vm.alloc_native_fn("get [Symbol.species]", |_vm, t, _| Ok(t));
    let k_species = vm.key_symbol_species;
    vm.heap.object_define_accessor(
        ctor,
        k_species,
        Some(sp_get),
        None,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    define_method_len(vm, ctor, "escape", 1, regexp_escape);
    install_regexp_legacy_statics(vm, ctor);
    define_global(vm, "RegExp", JsValue::from_object(ctor));

    // Annex B §B.2.1.1 / §B.2.1.2 global `escape` and `unescape`.
    let esc_fn = vm.alloc_native_fn("escape", global_escape);
    vm.set_fn_length(esc_fn, 1);
    define_global(vm, "escape", JsValue::from_object(esc_fn));
    let unesc_fn = vm.alloc_native_fn("unescape", global_unescape);
    vm.set_fn_length(unesc_fn, 1);
    define_global(vm, "unescape", JsValue::from_object(unesc_fn));
}

fn regexp_ctor(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let pattern_arg = arg(args, 0);
    let flags_arg = arg(args, 1);
    let pattern_is_re = is_regexp(vm, pattern_arg)?;
    let is_construct = !vm.native_new_target.is_undefined();

    if !is_construct && pattern_is_re && flags_arg.is_undefined() {
        if let Some(ph) = pattern_arg.as_object() {
            let p_ctor = vm.get_property_by_key(ph, vm.key_constructor)?;
            let re_ctor = regexp_ctor_handle(vm);
            if p_ctor.as_object() == Some(re_ctor) {
                return Ok(pattern_arg);
            }
        }
    }

    let (pat, raw_flags) = if let Some((p, f)) = pattern_arg
        .as_object()
        .and_then(|h| vm.heap.get_object(h))
        .and_then(|o| match &o.kind {
            ObjectKind::RegExp(r) => Some((r.pattern.clone(), r.flags.clone())),
            _ => None,
        }) {
        let f_str = if flags_arg.is_undefined() {
            f
        } else {
            vm.to_js_string(flags_arg)?
        };
        (p, f_str)
    } else if pattern_is_re {
        let ph = pattern_arg.as_object().unwrap();
        let k_src = vm.heap.intern_key("source");
        let k_flg = vm.heap.intern_key("flags");
        let p_val = vm.get_property_by_key(ph, k_src)?;
        let f_val = if flags_arg.is_undefined() {
            vm.get_property_by_key(ph, k_flg)?
        } else {
            flags_arg
        };
        let p = if p_val.is_undefined() {
            String::new()
        } else {
            vm.to_js_string(p_val)?
        };
        let f = if f_val.is_undefined() {
            String::new()
        } else {
            vm.to_js_string(f_val)?
        };
        (p, f)
    } else {
        let p = if pattern_arg.is_undefined() {
            String::new()
        } else {
            vm.to_js_string(pattern_arg)?
        };
        let f = if flags_arg.is_undefined() {
            String::new()
        } else {
            vm.to_js_string(flags_arg)?
        };
        (p, f)
    };

    let (canonical_flags, compiled) = compile_checked(vm, &pat, &raw_flags)?;
    let re = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::RegExp(Box::new(RegExpObject {
            pattern: pat,
            flags: canonical_flags,
            compiled: Some(compiled),
        })),
        Some(vm.regexp_proto),
    ));
    let k_last = vm.heap.intern_key("lastIndex");
    vm.heap.object_define_data(
        re,
        k_last,
        JsValue::from_i32(0),
        PropertyAttrs::WRITABLE,
        &mut vm.shapes,
    );
    Ok(JsValue::from_object(re))
}

fn regexp_get_source(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("RegExp.prototype.source getter called on non-object"));
    };
    if h == vm.regexp_proto {
        return Ok(JsValue::from_string(vm.heap.alloc_string("(?:)")));
    }
    let Some(pat) = vm.heap.get_object(h).and_then(|o| match &o.kind {
        ObjectKind::RegExp(r) => Some(r.pattern.clone()),
        _ => None,
    }) else {
        return Err(vm.type_error("RegExp.prototype.source getter called on non-RegExp"));
    };
    let escaped = escape_regexp_source(&pat);
    Ok(JsValue::from_string(vm.heap.alloc_string(escaped)))
}

fn regexp_flag_getter(
    vm: &mut JsVm,
    this_val: JsValue,
    flag: char,
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("RegExp.prototype flag getter called on non-object"));
    };
    if h == vm.regexp_proto {
        return Ok(JsValue::UNDEFINED);
    }
    let Some(has) = vm.heap.get_object(h).and_then(|o| match &o.kind {
        ObjectKind::RegExp(r) => Some(r.flags.contains(flag)),
        _ => None,
    }) else {
        return Err(vm.type_error("RegExp.prototype flag getter called on non-RegExp"));
    };
    Ok(JsValue::from_bool(has))
}

fn regexp_get_flags(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("RegExp.prototype.flags getter called on non-object"));
    };
    let checks = [
        ("hasIndices", 'd'),
        ("global", 'g'),
        ("ignoreCase", 'i'),
        ("multiline", 'm'),
        ("dotAll", 's'),
        ("unicode", 'u'),
        ("unicodeSets", 'v'),
        ("sticky", 'y'),
    ];
    let mut out = String::with_capacity(8);
    for (prop, ch) in checks {
        let k = vm.heap.intern_key(prop);
        let v = vm.get_property_by_key(h, k)?;
        if vm.to_boolean(v) {
            out.push(ch);
        }
    }
    Ok(JsValue::from_string(vm.heap.alloc_string(out)))
}

fn regexp_to_string(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("RegExp.prototype.toString requires an object receiver"));
    };
    let k_src = vm.heap.intern_key("source");
    let k_flg = vm.heap.intern_key("flags");
    let src_v = vm.get_property_by_key(h, k_src)?;
    let pattern = vm.to_js_string(src_v)?;
    let flg_v = vm.get_property_by_key(h, k_flg)?;
    let flags = vm.to_js_string(flg_v)?;
    Ok(JsValue::from_string(
        vm.heap.alloc_string(format!("/{pattern}/{flags}")),
    ))
}

/// ECMA-262 §22.2.6.2.1 `RegExpExec(R, S)` (respects custom `.exec` method).
pub fn regexp_exec_abstract(
    vm: &mut JsVm,
    rx: HeapHandle,
    input: JsValue,
) -> Result<JsValue, JsException> {
    let k_exec = vm.heap.intern_key("exec");
    let exec_fn = vm.get_property_by_key(rx, k_exec)?;
    if exec_fn
        .as_object()
        .is_some_and(|h| vm.is_callable_handle(h))
    {
        let res = vm.call_value(exec_fn, JsValue::from_object(rx), &[input])?;
        if !res.is_null() && !res.is_object() {
            return Err(
                vm.type_error("RegExp exec method returned something other than an Object or null")
            );
        }
        return Ok(res);
    }
    regexp_builtin_exec(vm, rx, input)
}

fn regexp_test(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(rx) = this_val.as_object() else {
        return Err(vm.type_error("RegExp.prototype.test requires an object receiver"));
    };
    let s = vm.to_js_string(arg(args, 0))?;
    let sv = JsValue::from_string(vm.heap.alloc_string(s));
    let m = regexp_exec_abstract(vm, rx, sv)?;
    Ok(JsValue::from_bool(!m.is_null()))
}

fn regexp_exec(vm: &mut JsVm, this_val: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(rx) = this_val.as_object() else {
        return Err(vm.type_error("RegExp.prototype.exec called on non-RegExp"));
    };
    let s = vm.to_js_string(arg(args, 0))?;
    let sv = JsValue::from_string(vm.heap.alloc_string(s));
    regexp_builtin_exec(vm, rx, sv)
}

/// ECMA-262 §22.2.6.2 `RegExpBuiltinExec(R, S)`.
fn regexp_builtin_exec(
    vm: &mut JsVm,
    rx: HeapHandle,
    input_val: JsValue,
) -> Result<JsValue, JsException> {
    let (pat, flags, compiled_opt) = match vm.heap.get_object(rx).map(|o| &o.kind) {
        Some(ObjectKind::RegExp(r)) => (r.pattern.clone(), r.flags.clone(), r.compiled.clone()),
        _ => return Err(vm.type_error("RegExp.prototype.exec called on non-RegExp")),
    };
    let compiled = match compiled_opt {
        Some(c) => c,
        None => {
            let (_, c) = compile_checked(vm, &pat, &flags)?;
            if let Some(ObjectKind::RegExp(r)) = vm.heap.get_object_mut(rx).map(|o| &mut o.kind) {
                r.compiled = Some(Arc::clone(&c));
            }
            c
        }
    };

    let input = vm.to_js_string(input_val)?;
    let units: Vec<u16> = input.encode_utf16().collect();
    let len = units.len();

    let k_last = vm.heap.intern_key("lastIndex");
    let li_val = vm.get_property_by_key(rx, k_last)?;
    let mut last_index = vm.to_length(li_val)?;

    let is_global = flags.contains('g');
    let is_sticky = flags.contains('y');
    let full_unicode = flags.contains('u') || flags.contains('v');
    let has_indices = flags.contains('d');

    if !is_global && !is_sticky {
        last_index = 0;
    }
    if last_index > len as u64 {
        if is_global || is_sticky {
            set_last_index_strict(vm, rx, JsValue::from_i32(0))?;
        }
        return Ok(JsValue::NULL);
    }
    let start = last_index as usize;
    let matched = if full_unicode {
        compiled.find_from_utf16(&units, start).next()
    } else {
        compiled.find_from_ucs2(&units, start).next()
    };
    let Some(m) = matched.filter(|m| !is_sticky || m.start() == start) else {
        if is_global || is_sticky {
            set_last_index_strict(vm, rx, JsValue::from_i32(0))?;
        }
        return Ok(JsValue::NULL);
    };

    if is_global || is_sticky {
        set_last_index_strict(vm, rx, JsValue::from_f64(m.end() as f64))?;
    }

    // Update legacy RegExp static properties for direct `%RegExp.prototype%` instances.
    if vm.heap.get_object(rx).and_then(|o| o.prototype) == Some(vm.regexp_proto) {
        let mut statics = RegExpLegacyStatics {
            input: input.clone(),
            last_match: String::from_utf16_lossy(&units[m.start()..m.end()]),
            last_paren: String::new(),
            left_context: String::from_utf16_lossy(&units[..m.start()]),
            right_context: String::from_utf16_lossy(&units[m.end()..]),
            parens: Default::default(),
        };
        for (i, cap) in m.captures.iter().enumerate() {
            if let Some(r) = cap {
                let s = String::from_utf16_lossy(&units[r.start..r.end]);
                if i < 9 {
                    statics.parens[i] = s.clone();
                }
                statics.last_paren = s;
            }
        }
        vm.regexp_statics = statics;
    }

    let arr = vm.alloc_array();
    for g in m.groups() {
        let v = match g {
            Some(r) => JsValue::from_string(
                vm.heap
                    .alloc_string(String::from_utf16_lossy(&units[r.start..r.end])),
            ),
            None => JsValue::UNDEFINED,
        };
        vm.heap.object_push_element(arr, v);
    }

    let k_index = vm.heap.intern_key("index");
    let k_input = vm.heap.intern_key("input");
    let k_groups = vm.heap.intern_key("groups");
    vm.heap.object_define_data(
        arr,
        k_index,
        JsValue::from_f64(m.start() as f64),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
    let inp_s = JsValue::from_string(vm.heap.alloc_string(input));
    vm.heap
        .object_define_data(arr, k_input, inp_s, PropertyAttrs::DEFAULT, &mut vm.shapes);

    let named: Vec<(&str, Option<regress::Range>)> = m.named_groups().collect();
    let groups_val = if named.is_empty() {
        JsValue::UNDEFINED
    } else {
        let gobj = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, None));
        for &(name, ref r) in &named {
            let v = match r {
                Some(rng) => JsValue::from_string(
                    vm.heap
                        .alloc_string(String::from_utf16_lossy(&units[rng.start..rng.end])),
                ),
                None => JsValue::UNDEFINED,
            };
            let k = vm.heap.intern_key(name);
            vm.heap
                .object_define_data(gobj, k, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
        }
        JsValue::from_object(gobj)
    };
    vm.heap.object_define_data(
        arr,
        k_groups,
        groups_val,
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );

    if has_indices {
        let idx_arr = vm.alloc_array();
        for g in m.groups() {
            let v = match g {
                Some(r) => {
                    let pair = vm.alloc_array();
                    vm.heap
                        .object_push_element(pair, JsValue::from_f64(r.start as f64));
                    vm.heap
                        .object_push_element(pair, JsValue::from_f64(r.end as f64));
                    JsValue::from_object(pair)
                }
                None => JsValue::UNDEFINED,
            };
            vm.heap.object_push_element(idx_arr, v);
        }
        let idx_groups = if named.is_empty() {
            JsValue::UNDEFINED
        } else {
            let gobj = vm
                .heap
                .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, None));
            for &(name, ref r) in &named {
                let v = match r {
                    Some(rng) => {
                        let pair = vm.alloc_array();
                        vm.heap
                            .object_push_element(pair, JsValue::from_f64(rng.start as f64));
                        vm.heap
                            .object_push_element(pair, JsValue::from_f64(rng.end as f64));
                        JsValue::from_object(pair)
                    }
                    None => JsValue::UNDEFINED,
                };
                let k = vm.heap.intern_key(name);
                vm.heap
                    .object_define_data(gobj, k, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
            }
            JsValue::from_object(gobj)
        };
        vm.heap.object_define_data(
            idx_arr,
            k_groups,
            idx_groups,
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        let k_indices = vm.heap.intern_key("indices");
        vm.heap.object_define_data(
            arr,
            k_indices,
            JsValue::from_object(idx_arr),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
    }

    Ok(JsValue::from_object(arr))
}

fn regexp_symbol_match(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(rx) = this_val.as_object() else {
        return Err(vm.type_error("RegExp.prototype[Symbol.match] requires an object receiver"));
    };
    let s = vm.to_js_string(arg(args, 0))?;
    let sv = JsValue::from_string(vm.heap.alloc_string(s.clone()));
    let k_flags = vm.heap.intern_key("flags");
    let flags_v = vm.get_property_by_key(rx, k_flags)?;
    let flags = vm.to_js_string(flags_v)?;
    if !flags.contains('g') {
        return regexp_exec_abstract(vm, rx, sv);
    }
    let full_unicode = flags.contains('u') || flags.contains('v');
    set_last_index_strict(vm, rx, JsValue::from_i32(0))?;
    let units: Vec<u16> = s.encode_utf16().collect();
    let arr = vm.alloc_array();
    let mut n = 0u64;
    let k_last = vm.heap.intern_key("lastIndex");
    loop {
        let result = regexp_exec_abstract(vm, rx, sv)?;
        if result.is_null() {
            return Ok(if n == 0 {
                JsValue::NULL
            } else {
                JsValue::from_object(arr)
            });
        }
        let m0 = vm.get_elem(result, JsValue::from_i32(0))?;
        let match_str = vm.to_js_string(m0)?;
        let mv = JsValue::from_string(vm.heap.alloc_string(match_str.clone()));
        vm.array_create_data_prop(arr, n, mv)?;
        if match_str.is_empty() {
            let li = vm.get_property_by_key(rx, k_last)?;
            let idx = vm.to_length(li)? as usize;
            let next = advance_string_index_u16(&units, idx, full_unicode);
            set_last_index_strict(vm, rx, JsValue::from_f64(next as f64))?;
        }
        n += 1;
        if n > 1_000_000 {
            return Err(vm.range_error("Script execution iteration limit exceeded"));
        }
    }
}

fn regexp_symbol_match_all(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(r) = this_val.as_object() else {
        return Err(vm.type_error("RegExp.prototype[Symbol.matchAll] requires an object receiver"));
    };
    let s = vm.to_js_string(arg(args, 0))?;
    let default_ctor = regexp_ctor_handle(vm);
    let c = species_constructor(vm, r, default_ctor)?;
    let k_flags = vm.heap.intern_key("flags");
    let flags_v = vm.get_property_by_key(r, k_flags)?;
    let flags = vm.to_js_string(flags_v)?;
    let fv = JsValue::from_string(vm.heap.alloc_string(flags.clone()));
    let matcher_val = vm.construct_value(c, &[this_val, fv], c)?;
    let Some(matcher) = matcher_val.as_object() else {
        return Err(vm.type_error("Species constructor returned non-object"));
    };
    let k_last = vm.heap.intern_key("lastIndex");
    let li = vm.get_property_by_key(r, k_last)?;
    let last_idx = vm.to_length(li)?;
    set_last_index_strict(vm, matcher, JsValue::from_f64(last_idx as f64))?;
    let global = flags.contains('g');
    let full_unicode = flags.contains('u') || flags.contains('v');

    let iter = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Ordinary,
        Some(vm.regexp_iter_proto),
    ));
    let done_flag = Rc::new(Cell::new(false));
    let k_step = vm.heap.intern_key("__re_iter_step");
    let units = Rc::new(s.encode_utf16().collect::<Vec<u16>>());
    let s_rc = Rc::new(s);
    let step_fn = vm.alloc_host_fn("", move |vm, _t, _a| {
        if done_flag.get() {
            return Ok(crate::promise::make_iter_result(
                vm,
                JsValue::UNDEFINED,
                true,
            ));
        }
        let sv = JsValue::from_string(vm.heap.alloc_string(s_rc.as_str()));
        let m = regexp_exec_abstract(vm, matcher, sv)?;
        if m.is_null() {
            done_flag.set(true);
            return Ok(crate::promise::make_iter_result(
                vm,
                JsValue::UNDEFINED,
                true,
            ));
        }
        if !global {
            done_flag.set(true);
            return Ok(crate::promise::make_iter_result(vm, m, false));
        }
        let m0 = vm.get_elem(m, JsValue::from_i32(0))?;
        let m_str = vm.to_js_string(m0)?;
        if m_str.is_empty() {
            let k_li = vm.heap.intern_key("lastIndex");
            let li = vm.get_property_by_key(matcher, k_li)?;
            let idx = vm.to_length(li)? as usize;
            let next = advance_string_index_u16(&units, idx, full_unicode);
            set_last_index_strict(vm, matcher, JsValue::from_f64(next as f64))?;
        }
        Ok(crate::promise::make_iter_result(vm, m, false))
    });
    vm.heap.object_define_data(
        iter,
        k_step,
        JsValue::from_object(step_fn),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    if let Some(o) = vm.heap.get_object_mut(iter) {
        o.extra_refs.push(matcher);
    }
    Ok(JsValue::from_object(iter))
}

fn regexp_iter_next(
    vm: &mut JsVm,
    this_val: JsValue,
    _args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("RegExp String Iterator.next called on incompatible receiver"));
    };
    let k_step = vm.heap.intern_key("__re_iter_step");
    let Some(step) = vm
        .heap
        .object_get_own(h, k_step, &vm.shapes)
        .map(|p| p.value)
    else {
        return Err(vm.type_error("RegExp String Iterator.next called on incompatible receiver"));
    };
    vm.call_value(step, JsValue::UNDEFINED, &[])
}

/// ECMA-262 §22.1.3.19.1 `GetSubstitution(matched, str, position, captures, namedCaptures, replacement)`.
pub fn get_substitution(
    vm: &mut JsVm,
    matched: &str,
    str_u16: &[u16],
    pos_u16: usize,
    captures: &[Option<String>],
    named_captures: Option<HeapHandle>,
    replacement: &str,
) -> Result<String, JsException> {
    let match_len_u16 = matched.encode_utf16().count();
    let tail_pos = (pos_u16 + match_len_u16).min(str_u16.len());
    let bytes = replacement.as_bytes();
    let mut out = String::with_capacity(replacement.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'$' || i + 1 >= bytes.len() {
            let ch = replacement[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        match bytes[i + 1] {
            b'$' => {
                out.push('$');
                i += 2;
            }
            b'&' => {
                out.push_str(matched);
                i += 2;
            }
            b'`' => {
                out.push_str(&String::from_utf16_lossy(
                    &str_u16[..pos_u16.min(str_u16.len())],
                ));
                i += 2;
            }
            b'\'' => {
                out.push_str(&String::from_utf16_lossy(&str_u16[tail_pos..]));
                i += 2;
            }
            b'<' if named_captures.is_some() => {
                if let Some(rel_end) = replacement[i + 2..].find('>') {
                    let name = &replacement[i + 2..i + 2 + rel_end];
                    let k = vm.heap.intern_key(name);
                    let cap_v = vm.get_property_by_key(named_captures.unwrap(), k)?;
                    if !cap_v.is_undefined() {
                        out.push_str(&vm.to_js_string(cap_v)?);
                    }
                    i += 3 + rel_end;
                } else {
                    out.push_str("$<");
                    i += 2;
                }
            }
            d @ b'0'..=b'9' => {
                let n1 = (d - b'0') as usize;
                let n2 = bytes
                    .get(i + 2)
                    .filter(|c| c.is_ascii_digit())
                    .map(|c| n1 * 10 + (c - b'0') as usize);
                if let Some(nn) = n2.filter(|&nn| nn >= 1 && nn <= captures.len()) {
                    if let Some(ref s) = captures[nn - 1] {
                        out.push_str(s);
                    }
                    i += 3;
                } else if n1 >= 1 && n1 <= captures.len() {
                    if let Some(ref s) = captures[n1 - 1] {
                        out.push_str(s);
                    }
                    i += 2;
                } else {
                    out.push('$');
                    i += 1;
                }
            }
            _ => {
                out.push('$');
                i += 1;
            }
        }
    }
    Ok(out)
}

fn regexp_symbol_replace(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(rx) = this_val.as_object() else {
        return Err(vm.type_error("RegExp.prototype[Symbol.replace] requires an object receiver"));
    };
    let s = vm.to_js_string(arg(args, 0))?;
    let units: Vec<u16> = s.encode_utf16().collect();
    let repl_val = arg(args, 1);
    let functional = repl_val
        .as_object()
        .is_some_and(|h| vm.is_callable_handle(h));
    let repl_str = if functional {
        String::new()
    } else {
        vm.to_js_string(repl_val)?
    };
    let k_flags = vm.heap.intern_key("flags");
    let flags_v = vm.get_property_by_key(rx, k_flags)?;
    let flags = vm.to_js_string(flags_v)?;
    let global = flags.contains('g');
    let full_unicode = flags.contains('u') || flags.contains('v');
    if global {
        set_last_index_strict(vm, rx, JsValue::from_i32(0))?;
    }

    let sv = JsValue::from_string(vm.heap.alloc_string(s.clone()));
    let k_last = vm.heap.intern_key("lastIndex");
    let mut results = Vec::new();
    loop {
        let res = regexp_exec_abstract(vm, rx, sv)?;
        if res.is_null() {
            break;
        }
        results.push(res);
        if !global {
            break;
        }
        let m0 = vm.get_elem(res, JsValue::from_i32(0))?;
        if vm.to_js_string(m0)?.is_empty() {
            let li = vm.get_property_by_key(rx, k_last)?;
            let idx = vm.to_length(li)? as usize;
            let next = advance_string_index_u16(&units, idx, full_unicode);
            set_last_index_strict(vm, rx, JsValue::from_f64(next as f64))?;
        }
        if results.len() > 1_000_000 {
            return Err(vm.range_error("Script execution iteration limit exceeded"));
        }
    }

    let mut accumulated = String::new();
    let mut next_source_pos = 0usize;
    let k_index = vm.heap.intern_key("index");
    let k_groups = vm.heap.intern_key("groups");
    for res in results {
        let Some(res_h) = res.as_object() else {
            continue;
        };
        let n_caps = vm.array_get_len(res_h)?.saturating_sub(1).min(100_000) as usize;
        let m0 = vm.get_elem(res, JsValue::from_i32(0))?;
        let matched = vm.to_js_string(m0)?;
        let idx_v = vm.get_property_by_key(res_h, k_index)?;
        let pos = (vm.to_integer_or_infinity(idx_v)?.max(0.0) as usize).min(units.len());

        let mut captures = Vec::with_capacity(n_caps);
        for n in 1..=n_caps {
            let cv = vm.get_elem(res, JsValue::from_f64(n as f64))?;
            if cv.is_undefined() {
                captures.push(None);
            } else {
                captures.push(Some(vm.to_js_string(cv)?));
            }
        }
        let named = vm.get_property_by_key(res_h, k_groups)?;
        let replacement = if functional {
            let mut cb_args = Vec::with_capacity(captures.len() + 4);
            cb_args.push(JsValue::from_string(vm.heap.alloc_string(matched.as_str())));
            for c in &captures {
                cb_args.push(match c {
                    Some(cs) => JsValue::from_string(vm.heap.alloc_string(cs.as_str())),
                    None => JsValue::UNDEFINED,
                });
            }
            cb_args.push(JsValue::from_f64(pos as f64));
            cb_args.push(sv);
            if !named.is_undefined() {
                cb_args.push(named);
            }
            let r = vm.call_value(repl_val, JsValue::UNDEFINED, &cb_args)?;
            vm.to_js_string(r)?
        } else {
            let named_h = if named.is_undefined() {
                None
            } else {
                Some(vm.to_object(named)?)
            };
            get_substitution(vm, &matched, &units, pos, &captures, named_h, &repl_str)?
        };
        if pos >= next_source_pos {
            accumulated.push_str(&String::from_utf16_lossy(&units[next_source_pos..pos]));
            accumulated.push_str(&replacement);
            next_source_pos = (pos + matched.encode_utf16().count()).min(units.len());
        }
    }
    if next_source_pos < units.len() {
        accumulated.push_str(&String::from_utf16_lossy(&units[next_source_pos..]));
    }
    Ok(JsValue::from_string(vm.heap.alloc_string(accumulated)))
}

fn regexp_symbol_search(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(rx) = this_val.as_object() else {
        return Err(vm.type_error("RegExp.prototype[Symbol.search] requires an object receiver"));
    };
    let s = vm.to_js_string(arg(args, 0))?;
    let sv = JsValue::from_string(vm.heap.alloc_string(s));
    let k_last = vm.heap.intern_key("lastIndex");
    let prev_li = vm.get_property_by_key(rx, k_last)?;
    if !same_value_num(prev_li, 0.0) {
        set_last_index_strict(vm, rx, JsValue::from_i32(0))?;
    }
    let result = regexp_exec_abstract(vm, rx, sv)?;
    let cur_li = vm.get_property_by_key(rx, k_last)?;
    if !vm.strict_eq(cur_li, prev_li)
        && !(cur_li.as_number().is_some_and(f64::is_nan)
            && prev_li.as_number().is_some_and(f64::is_nan))
    {
        set_last_index_strict(vm, rx, prev_li)?;
    }
    if result.is_null() {
        return Ok(JsValue::from_i32(-1));
    }
    let k_index = vm.heap.intern_key("index");
    vm.get_property_by_key(result.as_object().unwrap(), k_index)
}

fn regexp_symbol_split(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(rx) = this_val.as_object() else {
        return Err(vm.type_error("RegExp.prototype[Symbol.split] requires an object receiver"));
    };
    let s = vm.to_js_string(arg(args, 0))?;
    let units: Vec<u16> = s.encode_utf16().collect();
    let default_ctor = regexp_ctor_handle(vm);
    let c = species_constructor(vm, rx, default_ctor)?;
    let k_flags = vm.heap.intern_key("flags");
    let flags_v = vm.get_property_by_key(rx, k_flags)?;
    let flags = vm.to_js_string(flags_v)?;
    let unicode_matching = flags.contains('u') || flags.contains('v');
    let new_flags = if flags.contains('y') {
        flags
    } else {
        format!("{flags}y")
    };
    let nfv = JsValue::from_string(vm.heap.alloc_string(new_flags));
    let splitter_val = vm.construct_value(c, &[this_val, nfv], c)?;
    let Some(splitter) = splitter_val.as_object() else {
        return Err(vm.type_error("Species constructor returned non-object"));
    };
    let out = vm.alloc_array();
    let lim = if arg(args, 1).is_undefined() {
        u32::MAX
    } else {
        vm.to_u32(arg(args, 1))?
    };
    if lim == 0 {
        return Ok(JsValue::from_object(out));
    }
    let sv = JsValue::from_string(vm.heap.alloc_string(s));
    let size = units.len();
    if size == 0 {
        let z = regexp_exec_abstract(vm, splitter, sv)?;
        if z.is_null() {
            vm.array_create_data_prop(out, 0, sv)?;
        }
        return Ok(JsValue::from_object(out));
    }
    let k_last = vm.heap.intern_key("lastIndex");
    let (mut p, mut q, mut len_a) = (0usize, 0usize, 0u32);
    while q < size {
        set_last_index_strict(vm, splitter, JsValue::from_f64(q as f64))?;
        let z = regexp_exec_abstract(vm, splitter, sv)?;
        if z.is_null() {
            q = advance_string_index_u16(&units, q, unicode_matching);
            continue;
        }
        let li = vm.get_property_by_key(splitter, k_last)?;
        let e = (vm.to_length(li)? as usize).min(size);
        if e == p {
            q = advance_string_index_u16(&units, q, unicode_matching);
            continue;
        }
        let piece = String::from_utf16_lossy(&units[p..q]);
        let pv = JsValue::from_string(vm.heap.alloc_string(piece));
        vm.array_create_data_prop(out, u64::from(len_a), pv)?;
        len_a += 1;
        if len_a == lim {
            return Ok(JsValue::from_object(out));
        }
        p = e;
        let zh = z.as_object().unwrap();
        let num_caps = vm.array_get_len(zh)?.saturating_sub(1).min(100_000);
        for i in 1..=num_caps {
            let cap = vm.get_elem(z, JsValue::from_f64(i as f64))?;
            vm.array_create_data_prop(out, u64::from(len_a), cap)?;
            len_a += 1;
            if len_a == lim {
                return Ok(JsValue::from_object(out));
            }
        }
        q = p;
    }
    let tail = String::from_utf16_lossy(&units[p..size]);
    let tv = JsValue::from_string(vm.heap.alloc_string(tail));
    vm.array_create_data_prop(out, u64::from(len_a), tv)?;
    Ok(JsValue::from_object(out))
}

/// Annex B §B.2.4.1 `RegExp.prototype.compile(pattern, flags)`.
fn regexp_compile(
    vm: &mut JsVm,
    this_val: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("RegExp.prototype.compile called on non-RegExp"));
    };
    if !matches!(
        vm.heap.get_object(h).map(|o| &o.kind),
        Some(ObjectKind::RegExp(_))
    ) {
        return Err(vm.type_error("RegExp.prototype.compile called on non-RegExp"));
    }
    let pat_arg = arg(args, 0);
    let flg_arg = arg(args, 1);
    let (pat, raw_flags) = if let Some((p, f)) = pat_arg
        .as_object()
        .and_then(|ph| vm.heap.get_object(ph))
        .and_then(|o| match &o.kind {
            ObjectKind::RegExp(r) => Some((r.pattern.clone(), r.flags.clone())),
            _ => None,
        }) {
        if !flg_arg.is_undefined() {
            return Err(
                vm.type_error("Cannot supply flags when constructing one RegExp from another")
            );
        }
        (p, f)
    } else {
        let p = if pat_arg.is_undefined() {
            String::new()
        } else {
            vm.to_js_string(pat_arg)?
        };
        let f = if flg_arg.is_undefined() {
            String::new()
        } else {
            vm.to_js_string(flg_arg)?
        };
        (p, f)
    };
    let (flags, compiled) = compile_checked(vm, &pat, &raw_flags)?;
    if let Some(ObjectKind::RegExp(r)) = vm.heap.get_object_mut(h).map(|o| &mut o.kind) {
        r.pattern = pat;
        r.flags = flags;
        r.compiled = Some(compiled);
    }
    set_last_index_strict(vm, h, JsValue::from_i32(0))?;
    Ok(this_val)
}

/// ES2025 `RegExp.escape(S)`.
fn regexp_escape(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let Some(sh) = arg(args, 0).as_string() else {
        return Err(vm.type_error("RegExp.escape requires a string argument"));
    };
    let s = vm.heap.materialize_string(sh).unwrap_or_default();
    let mut out = String::with_capacity(s.len() * 2);
    for (idx, ch) in s.chars().enumerate() {
        let cp = ch as u32;
        if idx == 0 && ch.is_ascii_alphanumeric() {
            out.push_str(&format!("\\x{cp:02x}"));
        } else if matches!(
            ch,
            '^' | '$'
                | '\\'
                | '.'
                | '*'
                | '+'
                | '?'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '|'
                | '/'
        ) {
            out.push('\\');
            out.push(ch);
        } else {
            match ch {
                '\t' => out.push_str("\\t"),
                '\n' => out.push_str("\\n"),
                '\u{000B}' => out.push_str("\\v"),
                '\u{000C}' => out.push_str("\\f"),
                '\r' => out.push_str("\\r"),
                ',' | '-' | '=' | '<' | '>' | '#' | '&' | '!' | '%' | ':' | ';' | '@' | '~'
                | '\'' | '`' | '"' => out.push_str(&format!("\\x{cp:02x}")),
                c if is_es_whitespace_or_lt(c) => {
                    if cp <= 0xFF {
                        out.push_str(&format!("\\x{cp:02x}"));
                    } else {
                        out.push_str(&format!("\\u{cp:04x}"));
                    }
                }
                _ => out.push(ch),
            }
        }
    }
    Ok(JsValue::from_string(vm.heap.alloc_string(out)))
}

/// Returns `true` if `c` is an ECMAScript `WhiteSpace` or `LineTerminator` code point.
pub fn is_es_whitespace_or_lt(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// Annex B §B.2.1.1 `escape(string)`.
fn global_escape(vm: &mut JsVm, _this: JsValue, args: &[JsValue]) -> Result<JsValue, JsException> {
    let s = vm.to_js_string(arg(args, 0))?;
    let mut out = String::with_capacity(s.len());
    for u in s.encode_utf16() {
        if u < 128 {
            let b = u as u8;
            if b.is_ascii_alphanumeric()
                || matches!(b, b'@' | b'*' | b'_' | b'+' | b'-' | b'.' | b'/')
            {
                out.push(b as char);
                continue;
            }
        }
        if u < 256 {
            out.push_str(&format!("%{u:02X}"));
        } else {
            out.push_str(&format!("%u{u:04X}"));
        }
    }
    Ok(JsValue::from_string(vm.heap.alloc_string(out)))
}

/// Annex B §B.2.1.2 `unescape(string)`.
fn global_unescape(
    vm: &mut JsVm,
    _this: JsValue,
    args: &[JsValue],
) -> Result<JsValue, JsException> {
    let s = vm.to_js_string(arg(args, 0))?;
    let units: Vec<u16> = s.encode_utf16().collect();
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let mut k = 0usize;
    let hex_val = |u: u16| -> Option<u16> {
        match u {
            0x30..=0x39 => Some(u - 0x30),
            0x41..=0x46 => Some(u - 0x41 + 10),
            0x61..=0x66 => Some(u - 0x61 + 10),
            _ => None,
        }
    };
    while k < units.len() {
        if units[k] == b'%' as u16 {
            if k + 5 < units.len() && units[k + 1] == b'u' as u16 {
                if let (Some(a), Some(b), Some(c), Some(d)) = (
                    hex_val(units[k + 2]),
                    hex_val(units[k + 3]),
                    hex_val(units[k + 4]),
                    hex_val(units[k + 5]),
                ) {
                    out.push((a << 12) | (b << 8) | (c << 4) | d);
                    k += 6;
                    continue;
                }
            } else if k + 2 < units.len() {
                if let (Some(a), Some(b)) = (hex_val(units[k + 1]), hex_val(units[k + 2])) {
                    out.push((a << 4) | b);
                    k += 3;
                    continue;
                }
            }
        }
        out.push(units[k]);
        k += 1;
    }
    Ok(JsValue::from_string(
        vm.heap.alloc_string(String::from_utf16_lossy(&out)),
    ))
}

fn install_regexp_legacy_statics(vm: &mut JsVm, ctor: HeapHandle) {
    let require_re_ctor = move |vm: &mut JsVm, this_val: JsValue| -> Result<(), JsException> {
        if this_val.as_object() == Some(ctor) {
            Ok(())
        } else {
            Err(vm.type_error("Legacy RegExp static accessor requires RegExp constructor receiver"))
        }
    };
    let set_input = vm.alloc_host_fn("set input", move |vm, this_val, args| {
        require_re_ctor(vm, this_val)?;
        vm.regexp_statics.input = vm.to_js_string(arg(args, 0))?;
        Ok(JsValue::UNDEFINED)
    });
    vm.set_fn_length(set_input, 1);
    let get_input = vm.alloc_host_fn("get input", move |vm, this_val, _| {
        require_re_ctor(vm, this_val)?;
        let s = vm.regexp_statics.input.clone();
        Ok(JsValue::from_string(vm.heap.alloc_string(s)))
    });
    for name in ["input", "$_"] {
        let k = vm.heap.intern_key(name);
        vm.heap.object_define_accessor(
            ctor,
            k,
            Some(get_input),
            Some(set_input),
            PropertyAttrs::CONFIGURABLE,
            &mut vm.shapes,
        );
    }
    let fields: &[(&[&str], u8)] = &[
        (&["lastMatch", "$&"], 0),
        (&["lastParen", "$+"], 1),
        (&["leftContext", "$`"], 2),
        (&["rightContext", "$'"], 3),
    ];
    for &(names, tag) in fields {
        let g = vm.alloc_host_fn("", move |vm, this_val, _| {
            require_re_ctor(vm, this_val)?;
            let s = match tag {
                0 => vm.regexp_statics.last_match.clone(),
                1 => vm.regexp_statics.last_paren.clone(),
                2 => vm.regexp_statics.left_context.clone(),
                _ => vm.regexp_statics.right_context.clone(),
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(s)))
        });
        for &n in names {
            let k = vm.heap.intern_key(n);
            vm.heap.object_define_accessor(
                ctor,
                k,
                Some(g),
                None,
                PropertyAttrs::CONFIGURABLE,
                &mut vm.shapes,
            );
        }
    }
    for idx in 0..9usize {
        let g = vm.alloc_host_fn("", move |vm, this_val, _| {
            require_re_ctor(vm, this_val)?;
            let s = vm.regexp_statics.parens[idx].clone();
            Ok(JsValue::from_string(vm.heap.alloc_string(s)))
        });
        let k = vm.heap.intern_key(&format!("${}", idx + 1));
        vm.heap.object_define_accessor(
            ctor,
            k,
            Some(g),
            None,
            PropertyAttrs::CONFIGURABLE,
            &mut vm.shapes,
        );
    }
}

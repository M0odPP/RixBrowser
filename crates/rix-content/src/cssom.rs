//! W3C CSSOM (`CSS`, `CSSStyleSheet`, `CSSRuleList`, `CSSStyleRule`,
//! `CSSStyleDeclaration`, `document.styleSheets`, `document.adoptedStyleSheets`,
//! `<style>.sheet`, `getComputedStyle(el, pseudo)`, and validated inline `el.style`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use rix_css::{
    evaluate_supports_condition_str, is_logical_property, parse_and_expand_declaration,
    parse_and_serialize_declaration, parse_and_serialize_keyframe_selector,
    parse_and_serialize_page_selector, parse_and_serialize_selector_for_context, parse_cssom_rules,
    parse_single_cssom_rule_with_context, parse_single_keyframe_rule, resolve_logical_to_physical,
    serialize_css_tokens, strip_outer_css_comments, validate_and_serialize_page_descriptor,
    AlignValue, BorderCollapseMode, BoxSizingMode, CalcExpr, ClearMode, ColorRgba, ComputedStyle,
    CssAtom, CssColor, CssomNestingContext, CssomParseError, CssomRuleKind, CursorKind,
    DirectionMode, DisplayMode, EmptyCellsMode, EvalContext, FilterOp, FlexDirectionMode,
    FlexWrapMode, FloatMode, FontStyleMode, GridAutoFlowMode, LengthPercentage,
    LengthPercentageAuto, LineHeightValue, LineStyle, MixBlendMode, OverflowMode,
    PointerEventsMode, PositionMode, PropertyIdKind, RixPseudoElement, SelectorNamespaceMap,
    SizeValue, TableLayoutMode, TextAlignMode, TextOverflowMode, UserSelectMode, VerticalAlignMode,
    VisibilityMode, WordBreakMode, ZIndexValue, COMPUTED_STYLE_LONGHANDS,
};
use rix_dom::{NodeData, NodeId};
use rix_js::promise::{alloc_promise, reject_promise, resolve_promise};
use rix_js::{HeapHandle, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};

use crate::dom_bridge::{
    arg, define_global, define_host_accessor, define_host_method, get_attr_string, require_node,
    set_element_attr, wrap_node, ResolvedLayoutBox, SharedHost,
};
use crate::web_apis::create_dom_exception;

/// Mutable backing state for a live CSSOM `CSSStyleSheet` (either `<style>.sheet`, `<link>.sheet`, `@import`, or constructed).
#[derive(Debug, Clone)]
pub struct CssStyleSheetState {
    pub owner_node: Option<NodeId>,
    pub rules: Vec<CssomRuleKind>,
    pub disabled: bool,
    pub is_constructed: bool,
    pub source_hash: u64,
    pub title: Option<String>,
    pub href: Option<String>,
    pub media: Rc<RefCell<Vec<String>>>,
    pub parent_sheet: Option<HeapHandle>,
    pub owner_rule: Option<HeapHandle>,
    pub js_handle: Option<HeapHandle>,
    pub media_proxy: Option<JsValue>,
}

/// Shared handle to a live [`CssStyleSheetState`].
pub type SharedStyleSheet = Rc<RefCell<CssStyleSheetState>>;

impl CssStyleSheetState {
    #[must_use]
    pub fn from_css(css: &str, owner_node: Option<NodeId>, is_constructed: bool) -> Self {
        Self {
            owner_node,
            rules: parse_cssom_rules(css),
            disabled: false,
            is_constructed,
            source_hash: simple_hash(css.as_bytes()),
            title: None,
            href: None,
            media: Rc::new(RefCell::new(Vec::new())),
            parent_sheet: None,
            owner_rule: None,
            js_handle: None,
            media_proxy: None,
        }
    }

    pub fn sync_from_text_if_changed(&mut self, css: &str) {
        let h = simple_hash(css.as_bytes());
        if h != self.source_hash {
            self.rules = parse_cssom_rules(css);
            self.source_hash = h;
        }
    }

    #[must_use]
    pub fn to_css_text(&self) -> String {
        let mut out = String::new();
        for (i, r) in self.rules.iter().enumerate() {
            if i > 0 {
                out.push('\n');
            }
            out.push_str(&r.to_css_text());
        }
        out
    }

    #[must_use]
    pub fn rule_list(&self, path: &[usize]) -> Option<&[CssomRuleKind]> {
        let mut cur: &[CssomRuleKind] = &self.rules;
        for &idx in path {
            cur = cur.get(idx)?.child_rules()?;
        }
        Some(cur)
    }

    pub fn rule_list_mut(&mut self, path: &[usize]) -> Option<&mut Vec<CssomRuleKind>> {
        let mut cur: &mut Vec<CssomRuleKind> = &mut self.rules;
        for &idx in path {
            cur = cur.get_mut(idx)?.child_rules_mut()?;
        }
        Some(cur)
    }

    #[must_use]
    pub fn rule_at(&self, path: &[usize]) -> Option<&CssomRuleKind> {
        let (&last, prefix) = path.split_last()?;
        self.rule_list(prefix)?.get(last)
    }

    pub fn rule_at_mut(&mut self, path: &[usize]) -> Option<&mut CssomRuleKind> {
        let (&last, prefix) = path.split_last()?;
        self.rule_list_mut(prefix)?.get_mut(last)
    }

    #[must_use]
    pub fn nesting_context_for_list(&self, path: &[usize]) -> CssomNestingContext {
        let mut ctx = CssomNestingContext::TopLevel;
        let mut cur: &[CssomRuleKind] = &self.rules;
        for &idx in path {
            let Some(rule) = cur.get(idx) else {
                break;
            };
            match rule {
                CssomRuleKind::Style { rules, .. } => {
                    ctx = CssomNestingContext::InsideStyleRule;
                    cur = rules;
                }
                CssomRuleKind::Scope { rules, .. } => {
                    ctx = CssomNestingContext::InsideScopeRule;
                    cur = rules;
                }
                CssomRuleKind::Media { rules, .. }
                | CssomRuleKind::Supports { rules, .. }
                | CssomRuleKind::Container { rules, .. }
                | CssomRuleKind::LayerBlock { rules, .. } => {
                    cur = rules;
                }
                _ => break,
            }
        }
        ctx
    }

    #[must_use]
    pub fn active_namespaces(&self) -> SelectorNamespaceMap {
        let mut ns = SelectorNamespaceMap::default();
        for r in &self.rules {
            if let CssomRuleKind::Namespace { prefix, url } = r {
                let atom = CssAtom::new(url);
                if let Some(p) = prefix {
                    ns.prefixes.insert(p.clone(), atom);
                } else {
                    ns.default_namespace = Some(atom);
                }
            }
        }
        ns
    }
}

fn simple_hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// Installs global `CSS`, `CSSStyleSheet`, `CSSRule*`, `<style>.sheet`,
/// `document.styleSheets`, `document.adoptedStyleSheets`, and `getComputedStyle(el, pseudo)`.
pub fn install_cssom(
    vm: &mut JsVm,
    host: &SharedHost,
    wa: &crate::web_animations::SharedWebAnimations,
) {
    install_css_namespace(vm, host);
    install_css_rule_ctors(vm, host);
    install_css_stylesheet_ctor(vm, host);
    install_style_element_sheet_accessor(vm, host);
    install_document_stylesheets(vm, host);
    install_get_computed_style(vm, host, wa);
    install_web_animations_and_typed_om(vm, host);
    install_document_fonts(vm, host);
}

fn install_document_fonts(vm: &mut JsVm, host: &SharedHost) {
    let dp = host.borrow().prototypes.document_proto;

    // Global `FontFace` constructor (CSS Font Loading L3 §2).
    let ff_proto = vm.alloc_ordinary_object();
    if let Some(o) = vm.heap.get_object_mut(ff_proto) {
        o.is_prototype = true;
    }
    define_host_method(vm, ff_proto, "load", move |vm, this_val, _| {
        if let Some(h) = this_val.as_object() {
            let k_status = vm.heap.intern_key("status");
            let loaded_s = JsValue::from_string(vm.heap.alloc_string("loaded"));
            let _ = vm.set_property_by_key(h, k_status, loaded_s);
        }
        let p = alloc_promise(vm);
        resolve_promise(vm, p, this_val);
        Ok(JsValue::from_object(p))
    });
    let ff_ctor = vm.alloc_host_fn("FontFace", move |vm, _, args| {
        let family = vm.value_to_rust_string(arg(args, 0));
        let face = vm
            .heap
            .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(ff_proto)));
        let loaded_p = alloc_promise(vm);
        resolve_promise(vm, loaded_p, JsValue::from_object(face));
        for (k, v) in [
            (
                "family",
                JsValue::from_string(vm.heap.alloc_string(&family)),
            ),
            (
                "style",
                JsValue::from_string(vm.heap.alloc_string("normal")),
            ),
            (
                "weight",
                JsValue::from_string(vm.heap.alloc_string("normal")),
            ),
            (
                "stretch",
                JsValue::from_string(vm.heap.alloc_string("normal")),
            ),
            (
                "unicodeRange",
                JsValue::from_string(vm.heap.alloc_string("U+0-10FFFF")),
            ),
            (
                "variant",
                JsValue::from_string(vm.heap.alloc_string("normal")),
            ),
            (
                "display",
                JsValue::from_string(vm.heap.alloc_string("auto")),
            ),
            (
                "status",
                JsValue::from_string(vm.heap.alloc_string("loaded")),
            ),
            ("loaded", JsValue::from_object(loaded_p)),
        ] {
            let key = vm.heap.intern_key(k);
            vm.heap
                .object_define_data(face, key, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
        }
        Ok(JsValue::from_object(face))
    });
    vm.mark_constructor(ff_ctor);
    let k_proto = vm.heap.intern_key("prototype");
    let k_ctor = vm.heap.intern_key("constructor");
    vm.heap.object_define_data(
        ff_ctor,
        k_proto,
        JsValue::from_object(ff_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        ff_proto,
        k_ctor,
        JsValue::from_object(ff_ctor),
        PropertyAttrs::CONFIGURABLE | PropertyAttrs::WRITABLE,
        &mut vm.shapes,
    );
    define_global(vm, "FontFace", JsValue::from_object(ff_ctor));

    // Per-document `FontFaceSet` (`document.fonts`, CSS Font Loading L3 §3).
    let ffs_proto = vm.alloc_ordinary_object();
    if let Some(o) = vm.heap.get_object_mut(ffs_proto) {
        o.is_prototype = true;
    }
    let fonts_list: Rc<RefCell<Vec<JsValue>>> = Rc::new(RefCell::new(Vec::new()));
    let fonts_obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(ffs_proto)));
    let ready_p = alloc_promise(vm);
    resolve_promise(vm, ready_p, JsValue::from_object(fonts_obj));
    let k_ready = vm.heap.intern_key("ready");
    vm.heap.object_define_data(
        fonts_obj,
        k_ready,
        JsValue::from_object(ready_p),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
    let k_status = vm.heap.intern_key("status");
    let loaded_s = vm.heap.alloc_string("loaded".to_string());
    vm.heap.object_define_data(
        fonts_obj,
        k_status,
        JsValue::from_string(loaded_s),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
    {
        let fl = Rc::clone(&fonts_list);
        let g_size = vm.alloc_host_fn("get size", move |_, _, _| {
            Ok(JsValue::from_i32(fl.borrow().len() as i32))
        });
        define_host_accessor(vm, ffs_proto, "size", Some(g_size), None);
    }
    {
        let fl = Rc::clone(&fonts_list);
        define_host_method(vm, ffs_proto, "load", move |vm, _, _| {
            let arr = vm.alloc_array();
            for &face in fl.borrow().iter() {
                vm.heap.object_push_element(arr, face);
            }
            let p = alloc_promise(vm);
            resolve_promise(vm, p, JsValue::from_object(arr));
            Ok(JsValue::from_object(p))
        });
    }
    define_host_method(vm, ffs_proto, "check", move |_, _, _| Ok(JsValue::TRUE));
    {
        let fl = Rc::clone(&fonts_list);
        define_host_method(vm, ffs_proto, "add", move |_, this_val, args| {
            let face = arg(args, 0);
            let mut list = fl.borrow_mut();
            if !list.contains(&face) {
                list.push(face);
            }
            Ok(this_val)
        });
    }
    {
        let fl = Rc::clone(&fonts_list);
        define_host_method(vm, ffs_proto, "delete", move |_, _, args| {
            let face = arg(args, 0);
            let mut list = fl.borrow_mut();
            if let Some(pos) = list.iter().position(|v| *v == face) {
                list.remove(pos);
                Ok(JsValue::TRUE)
            } else {
                Ok(JsValue::FALSE)
            }
        });
    }
    {
        let fl = Rc::clone(&fonts_list);
        define_host_method(vm, ffs_proto, "clear", move |_, _, _| {
            fl.borrow_mut().clear();
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let fl = Rc::clone(&fonts_list);
        define_host_method(vm, ffs_proto, "forEach", move |vm, this_val, args| {
            let cb = arg(args, 0);
            let this_arg = arg(args, 1);
            let items: Vec<JsValue> = fl.borrow().clone();
            for item in items {
                vm.call_value(cb, this_arg, &[item, item, this_val])?;
            }
            Ok(JsValue::UNDEFINED)
        });
    }
    {
        let fl = Rc::clone(&fonts_list);
        let values_fn = vm.alloc_host_fn("values", move |vm, _, _| {
            let arr = vm.alloc_array();
            for &face in fl.borrow().iter() {
                vm.heap.object_push_element(arr, face);
            }
            let iter_fn = vm.get_property_by_key(arr, vm.key_symbol_iterator)?;
            vm.call_value(iter_fn, JsValue::from_object(arr), &[])
        });
        for name in ["values", "keys", "entries"] {
            let k = vm.heap.intern_key(name);
            vm.heap.object_define_data(
                ffs_proto,
                k,
                JsValue::from_object(values_fn),
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
        }
        let k_iter = vm.key_symbol_iterator;
        vm.heap.object_define_data(
            ffs_proto,
            k_iter,
            JsValue::from_object(values_fn),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
    }
    define_host_method(vm, ffs_proto, "addEventListener", |_, _, _| {
        Ok(JsValue::UNDEFINED)
    });
    define_host_method(vm, ffs_proto, "removeEventListener", |_, _, _| {
        Ok(JsValue::UNDEFINED)
    });
    define_host_method(vm, ffs_proto, "dispatchEvent", |_, _, _| Ok(JsValue::TRUE));

    let ffs_ctor = vm.alloc_host_fn("FontFaceSet", |vm, _, _| {
        Err(vm.type_error("Illegal constructor: FontFaceSet"))
    });
    vm.mark_constructor(ffs_ctor);
    vm.heap.object_define_data(
        ffs_ctor,
        k_proto,
        JsValue::from_object(ffs_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        ffs_proto,
        k_ctor,
        JsValue::from_object(ffs_ctor),
        PropertyAttrs::CONFIGURABLE | PropertyAttrs::WRITABLE,
        &mut vm.shapes,
    );
    define_global(vm, "FontFaceSet", JsValue::from_object(ffs_ctor));

    let fl_root = Rc::clone(&fonts_list);
    vm.add_host_root_collector(move |roots| {
        roots.push(JsValue::from_object(fonts_obj));
        roots.push(JsValue::from_object(ffs_proto));
        roots.push(JsValue::from_object(ff_proto));
        for &v in fl_root.borrow().iter() {
            roots.push(v);
        }
    });
    let fonts_val = JsValue::from_object(fonts_obj);
    let g_fonts = vm.alloc_host_fn("get fonts", move |_, _, _| Ok(fonts_val));
    define_host_accessor(vm, dp, "fonts", Some(g_fonts), None);
}

// ---------------------------------------------------------------------------
// Global `CSS` namespace (`CSS.supports`, `CSS.escape`, `CSS.registerProperty`)
// ---------------------------------------------------------------------------

fn install_css_namespace(vm: &mut JsVm, host: &SharedHost) {
    let css_obj = vm.alloc_ordinary_object();
    let k_tag = vm.key_symbol_to_string_tag;
    let v_tag = JsValue::from_string(vm.heap.alloc_string("CSS"));
    vm.heap.object_define_data(
        css_obj,
        k_tag,
        v_tag,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );

    define_host_method(vm, css_obj, "supports", move |vm, _, args| {
        if args.is_empty() {
            return Err(vm.type_error("CSS.supports requires at least 1 argument"));
        }
        let supported = if args.len() >= 2 && !arg(args, 1).is_undefined() {
            let prop = vm.value_to_rust_string(arg(args, 0));
            let val = vm.value_to_rust_string(arg(args, 1));
            parse_and_serialize_declaration(&prop, &val).is_some()
        } else {
            let cond = vm.value_to_rust_string(arg(args, 0));
            evaluate_supports_condition_str(&cond)
        };
        Ok(JsValue::from_bool(supported))
    });

    define_host_method(vm, css_obj, "escape", move |vm, _, args| {
        if args.is_empty() {
            return Err(vm.type_error("CSS.escape requires 1 argument"));
        }
        let raw = vm.value_to_rust_string(arg(args, 0));
        let escaped = css_escape(&raw);
        Ok(JsValue::from_string(vm.heap.alloc_string(escaped)))
    });

    let h = Rc::clone(host);
    define_host_method(vm, css_obj, "registerProperty", move |vm, _, args| {
        let Some(obj) = arg(args, 0).as_object() else {
            return Err(vm.type_error("CSS.registerProperty requires an object argument"));
        };
        let k_name = vm.heap.intern_key("name");
        let name = vm
            .get_property_by_key(obj, k_name)
            .map(|v| vm.value_to_rust_string(v))
            .unwrap_or_default();
        if !name.starts_with("--") || name.len() <= 2 {
            return Err(create_dom_exception(
                vm,
                "SyntaxError",
                "Custom property name must start with '--'",
            ));
        }
        let k_syntax = vm.heap.intern_key("syntax");
        let syntax = vm
            .get_property_by_key(obj, k_syntax)
            .ok()
            .filter(|v| !v.is_undefined())
            .map(|v| vm.value_to_rust_string(v))
            .unwrap_or_else(|| "*".to_string());
        let k_inherits = vm.heap.intern_key("inherits");
        let inherits = vm
            .get_property_by_key(obj, k_inherits)
            .ok()
            .filter(|v| !v.is_undefined())
            .map(|v| vm.to_boolean(v))
            .ok_or_else(|| vm.type_error("CSS.registerProperty requires 'inherits'"))?;
        let k_init = vm.heap.intern_key("initialValue");
        let initial_value = vm
            .get_property_by_key(obj, k_init)
            .ok()
            .filter(|v| !v.is_undefined())
            .map(|v| vm.value_to_rust_string(v));

        let mut st = h.borrow_mut();
        if st.registered_properties.contains_key(&name) {
            return Err(create_dom_exception(
                vm,
                "InvalidModificationError",
                "Custom property already registered",
            ));
        }
        st.registered_properties
            .insert(name, (syntax, inherits, initial_value));
        st.mark_full_sheets_rebuild();
        Ok(JsValue::UNDEFINED)
    });

    define_global(vm, "CSS", JsValue::from_object(css_obj));
}

/// Exact CSSOM §2.1 `serialize an identifier` (`CSS.escape`).
#[must_use]
pub fn css_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    let chars: Vec<char> = input.chars().collect();
    for (i, &ch) in chars.iter().enumerate() {
        let cp = ch as u32;
        if cp == 0x0000 {
            out.push('\u{FFFD}');
        } else if (0x0001..=0x001F).contains(&cp)
            || cp == 0x007F
            || (i == 0 && ch.is_ascii_digit())
            || (i == 1 && ch.is_ascii_digit() && chars[0] == '-')
        {
            use std::fmt::Write as _;
            let _ = write!(out, "\\{cp:x} ");
        } else if i == 0 && chars.len() == 1 && ch == '-' {
            out.push('\\');
            out.push(ch);
        } else if cp >= 0x0080 || ch == '-' || ch == '_' || ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else {
            out.push('\\');
            out.push(ch);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// CSSRule / CSSStyleSheet constructors & live wrappers
// ---------------------------------------------------------------------------

fn install_css_rule_ctors(vm: &mut JsVm, host: &SharedHost) {
    let kp = vm.heap.intern_key("prototype");
    let kc = vm.heap.intern_key("constructor");

    let define_iface = |vm: &mut JsVm,
                        host: &SharedHost,
                        name: &'static str,
                        parent_proto: Option<HeapHandle>,
                        parent_ctor: Option<HeapHandle>|
     -> (HeapHandle, HeapHandle) {
        let p = vm.alloc_ordinary_object();
        if let Some(pp) = parent_proto {
            vm.heap.object_set_prototype(p, Some(pp), &mut vm.shapes);
        }
        vm.heap.get_object_mut(p).unwrap().is_prototype = true;
        let c = vm.alloc_host_fn(name, |vm, _, _| Err(vm.type_error("Illegal constructor")));
        if let Some(pc) = parent_ctor {
            vm.heap.object_set_prototype(c, Some(pc), &mut vm.shapes);
        }
        vm.heap.object_define_data(
            c,
            kp,
            JsValue::from_object(p),
            PropertyAttrs::empty(),
            &mut vm.shapes,
        );
        vm.heap.object_define_data(
            p,
            kc,
            JsValue::from_object(c),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        define_global(vm, name, JsValue::from_object(c));
        host.borrow_mut().css_rule_protos.insert(name, p);
        (c, p)
    };

    let (rule_ctor, rule_proto) = define_iface(vm, host, "CSSRule", None, None);
    for (name, code) in [
        ("STYLE_RULE", 1i32),
        ("IMPORT_RULE", 3),
        ("MEDIA_RULE", 4),
        ("FONT_FACE_RULE", 5),
        ("PAGE_RULE", 6),
        ("KEYFRAMES_RULE", 7),
        ("KEYFRAME_RULE", 8),
        ("NAMESPACE_RULE", 10),
        ("SUPPORTS_RULE", 12),
    ] {
        let k = vm.heap.intern_key(name);
        let v = JsValue::from_i32(code);
        vm.heap
            .object_define_data(rule_ctor, k, v, PropertyAttrs::empty(), &mut vm.shapes);
        vm.heap
            .object_define_data(rule_proto, k, v, PropertyAttrs::empty(), &mut vm.shapes);
    }

    // W3C CSSOM / CSS Nesting L1 / Cascade L5-6 WebIDL inheritance chain:
    // CSSRule <- CSSGroupingRule <- (CSSStyleRule, CSSConditionRule, CSSLayerBlockRule, CSSScopeRule)
    // CSSConditionRule <- (CSSMediaRule, CSSSupportsRule, CSSContainerRule)
    // CSSRule <- (CSSNestedDeclarations, CSSImportRule, CSSLayerStatementRule, CSSNamespaceRule, ...)
    let (grouping_ctor, grouping_proto) = define_iface(
        vm,
        host,
        "CSSGroupingRule",
        Some(rule_proto),
        Some(rule_ctor),
    );
    let _ = define_iface(
        vm,
        host,
        "CSSStyleRule",
        Some(grouping_proto),
        Some(grouping_ctor),
    );
    let (cond_ctor, cond_proto) = define_iface(
        vm,
        host,
        "CSSConditionRule",
        Some(grouping_proto),
        Some(grouping_ctor),
    );
    for cond_sub in ["CSSMediaRule", "CSSSupportsRule", "CSSContainerRule"] {
        let _ = define_iface(vm, host, cond_sub, Some(cond_proto), Some(cond_ctor));
    }
    for group_sub in ["CSSLayerBlockRule", "CSSScopeRule", "CSSPageRule"] {
        let _ = define_iface(
            vm,
            host,
            group_sub,
            Some(grouping_proto),
            Some(grouping_ctor),
        );
    }
    for rule_sub in [
        "CSSNestedDeclarations",
        "CSSImportRule",
        "CSSLayerStatementRule",
        "CSSNamespaceRule",
        "CSSKeyframesRule",
        "CSSKeyframeRule",
        "CSSPropertyRule",
        "CSSFontFaceRule",
        "CSSCounterStyleRule",
    ] {
        let _ = define_iface(vm, host, rule_sub, Some(rule_proto), Some(rule_ctor));
    }
    // CSSStyleDeclaration, CSSPageDescriptors, CSSRuleList, MediaList, StyleSheet, StyleSheetList.
    let (decl_ctor, decl_proto) = define_iface(vm, host, "CSSStyleDeclaration", None, None);
    attach_array_like_iterator(vm, decl_proto);
    let (_, page_desc_proto) = define_iface(
        vm,
        host,
        "CSSPageDescriptors",
        Some(decl_proto),
        Some(decl_ctor),
    );
    for (js_prop, css_prop) in [
        ("margin", "margin"),
        ("margin-top", "margin-top"),
        ("marginTop", "margin-top"),
        ("margin-right", "margin-right"),
        ("marginRight", "margin-right"),
        ("margin-bottom", "margin-bottom"),
        ("marginBottom", "margin-bottom"),
        ("margin-left", "margin-left"),
        ("marginLeft", "margin-left"),
        ("page-orientation", "page-orientation"),
        ("pageOrientation", "page-orientation"),
        ("size", "size"),
    ] {
        let g = vm.alloc_host_fn("get pageDescriptor", move |vm, this_val, _| {
            let Some(th) = this_val.as_object() else {
                return Ok(JsValue::from_string(vm.heap.alloc_string("")));
            };
            let k_gpv = vm.heap.intern_key("getPropertyValue");
            let func = vm.get_property_by_key(th, k_gpv)?;
            let arg_s = JsValue::from_string(vm.heap.alloc_string(css_prop));
            vm.call_value(func, this_val, &[arg_s])
        });
        let s = vm.alloc_host_fn("set pageDescriptor", move |vm, this_val, args| {
            let Some(th) = this_val.as_object() else {
                return Ok(JsValue::UNDEFINED);
            };
            let k_sp = vm.heap.intern_key("setProperty");
            let func = vm.get_property_by_key(th, k_sp)?;
            let arg_p = JsValue::from_string(vm.heap.alloc_string(css_prop));
            let _ = vm.call_value(func, this_val, &[arg_p, arg(args, 0)])?;
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, page_desc_proto, js_prop, Some(g), Some(s));
    }
    let _ = define_iface(vm, host, "CSSRuleList", None, None);
    let _ = define_iface(vm, host, "MediaList", None, None);
    let _ = define_iface(vm, host, "StyleSheet", None, None);
    let _ = define_iface(vm, host, "StyleSheetList", None, None);
}

/// Normalizes a single CSS media query per CSSOM §6.6 (`MediaList`).
/// Invalid queries normalize to `"not all"`.
fn normalize_single_media_query(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return "not all".to_string();
    }
    // Validate balanced parens and reject disallowed punctuation (`$`, `;`, `@`, `{`, `}`, `!`, etc.).
    let mut depth = 0i32;
    let mut prev_space = false;
    let mut out = String::with_capacity(trimmed.len());
    for ch in trimmed.chars() {
        if ch.is_ascii_whitespace() {
            if !prev_space && !out.is_empty() {
                out.push(' ');
                prev_space = true;
            }
            continue;
        }
        prev_space = false;
        match ch {
            '(' => {
                depth += 1;
                out.push('(');
            }
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return "not all".to_string();
                }
                out.push(')');
            }
            c if c.is_ascii_alphanumeric()
                || matches!(c, '-' | '_' | ':' | '.' | '/' | '<' | '>' | '=' | '+') =>
            {
                out.push(c.to_ascii_lowercase());
            }
            _ => return "not all".to_string(),
        }
    }
    if depth != 0 {
        return "not all".to_string();
    }
    let clean = out.trim().to_string();
    if clean.is_empty() {
        return "not all".to_string();
    }
    // Single-token check: if it is a modifier keyword alone (`not`, `only`, `and`, `or`), invalid.
    if matches!(clean.as_str(), "not" | "only" | "and" | "or") {
        return "not all".to_string();
    }
    clean
}

pub(crate) fn parse_media_query_list(raw: &str) -> Vec<String> {
    if raw.trim().is_empty() {
        return Vec::new();
    }
    raw.split(',').map(normalize_single_media_query).collect()
}

fn create_media_list_proxy(
    vm: &mut JsVm,
    host: &SharedHost,
    media: Rc<RefCell<Vec<String>>>,
    on_change: Option<Rc<dyn Fn()>>,
) -> JsValue {
    let proto = host
        .borrow()
        .css_rule_protos
        .get("MediaList")
        .copied()
        .unwrap_or(vm.object_proto);
    let target = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    attach_array_like_iterator(vm, target);

    let m_mt1 = Rc::clone(&media);
    let g_mt = vm.alloc_host_fn("get mediaText", move |vm, _, _| {
        let txt = m_mt1.borrow().join(", ");
        Ok(JsValue::from_string(vm.heap.alloc_string(txt)))
    });
    let m_mt2 = Rc::clone(&media);
    let cb_mt = on_change.clone();
    let s_mt = vm.alloc_host_fn("set mediaText", move |vm, _, args| {
        let v = arg(args, 0);
        let raw = if v.is_null() {
            String::new()
        } else {
            vm.value_to_rust_string(v)
        };
        *m_mt2.borrow_mut() = parse_media_query_list(&raw);
        if let Some(ref cb) = cb_mt {
            cb();
        }
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, target, "mediaText", Some(g_mt), Some(s_mt));

    let m_item = Rc::clone(&media);
    define_host_method(vm, target, "item", move |vm, _, args| {
        let idx = vm.to_number(arg(args, 0)).unwrap_or(-1.0);
        if idx < 0.0 || !idx.is_finite() {
            return Ok(JsValue::NULL);
        }
        Ok(m_item
            .borrow()
            .get(idx as usize)
            .map_or(JsValue::NULL, |s| {
                JsValue::from_string(vm.heap.alloc_string(s))
            }))
    });

    let m_app = Rc::clone(&media);
    let cb_app = on_change.clone();
    define_host_method(vm, target, "appendMedium", move |vm, _, args| {
        let raw = vm.value_to_rust_string(arg(args, 0));
        let norm = normalize_single_media_query(&raw);
        let mut list = m_app.borrow_mut();
        if !list.iter().any(|x| x == &norm) {
            list.push(norm);
        }
        drop(list);
        if let Some(ref cb) = cb_app {
            cb();
        }
        Ok(JsValue::UNDEFINED)
    });

    let m_del = Rc::clone(&media);
    let cb_del = on_change;
    define_host_method(vm, target, "deleteMedium", move |vm, _, args| {
        let raw = vm.value_to_rust_string(arg(args, 0));
        let norm = normalize_single_media_query(&raw);
        let mut list = m_del.borrow_mut();
        let before = list.len();
        list.retain(|x| x != &norm);
        let removed = list.len() != before;
        drop(list);
        if !removed {
            return Err(create_dom_exception(
                vm,
                "NotFoundError",
                "Media query not found in MediaList",
            ));
        }
        if let Some(ref cb) = cb_del {
            cb();
        }
        Ok(JsValue::UNDEFINED)
    });

    let m_to_str = Rc::clone(&media);
    define_host_method(vm, target, "toString", move |vm, _, _| {
        let txt = m_to_str.borrow().join(", ");
        Ok(JsValue::from_string(vm.heap.alloc_string(txt)))
    });

    let handler = vm.alloc_ordinary_object();
    let m_get = Rc::clone(&media);
    define_host_method(vm, handler, "get", move |vm, _, args| {
        let key_val = arg(args, 1);
        if key_val.is_symbol() {
            return forward_symbol_get(vm, arg(args, 0), key_val);
        }
        let prop = vm.value_to_rust_string(key_val);
        if prop == "length" {
            return Ok(JsValue::from_i32(m_get.borrow().len() as i32));
        }
        if let Ok(idx) = prop.parse::<usize>() {
            return Ok(m_get.borrow().get(idx).map_or(JsValue::UNDEFINED, |s| {
                JsValue::from_string(vm.heap.alloc_string(s))
            }));
        }
        let k = vm.heap.intern_key(&prop);
        if let Some(t_h) = arg(args, 0).as_object() {
            return vm.get_property_by_key(t_h, k);
        }
        Ok(JsValue::UNDEFINED)
    });
    define_host_method(vm, handler, "set", move |vm, _, args| {
        let prop = vm.value_to_rust_string(arg(args, 1));
        if prop == "mediaText" {
            if let Some(t_h) = arg(args, 0).as_object() {
                let k = vm.heap.intern_key("mediaText");
                vm.set_property_by_key(t_h, k, arg(args, 2))?;
            }
            return Ok(JsValue::TRUE);
        }
        Ok(JsValue::TRUE)
    });

    let proxy = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Proxy(rix_js::ProxyObject {
            target,
            handler,
            revoked: false,
        }),
        Some(proto),
    ));
    JsValue::from_object(proxy)
}

fn install_css_stylesheet_ctor(vm: &mut JsVm, host: &SharedHost) {
    let ss_proto = host
        .borrow()
        .css_rule_protos
        .get("StyleSheet")
        .copied()
        .unwrap_or(vm.object_proto);
    let proto = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(ss_proto)));
    vm.heap.get_object_mut(proto).unwrap().is_prototype = true;
    let h = Rc::clone(host);
    let ctor = vm.alloc_host_fn("CSSStyleSheet", move |vm, _, args| {
        let mut state = CssStyleSheetState::from_css("", None, true);
        if let Some(opts_h) = arg(args, 0).as_object() {
            let k_dis = vm.heap.intern_key("disabled");
            if let Ok(v) = vm.get_property_by_key(opts_h, k_dis) {
                if !v.is_undefined() {
                    state.disabled = vm.to_boolean(v);
                }
            }
            let k_base = vm.heap.intern_key("baseURL");
            if let Ok(v) = vm.get_property_by_key(opts_h, k_base) {
                if !v.is_undefined() {
                    let raw = vm.value_to_rust_string(v);
                    let doc_url = h.borrow().main_document_url.clone();
                    let parsed = url::Url::parse(&doc_url)
                        .ok()
                        .and_then(|b| b.join(&raw).ok())
                        .or_else(|| url::Url::parse(&raw).ok());
                    if parsed.is_none() {
                        return Err(create_dom_exception(
                            vm,
                            "NotAllowedError",
                            "Failed to construct 'CSSStyleSheet': invalid baseURL",
                        ));
                    }
                }
            }
            let k_med = vm.heap.intern_key("media");
            if let Ok(v) = vm.get_property_by_key(opts_h, k_med) {
                if !v.is_undefined() && !v.is_null() {
                    let media_str = if let Some(mh) = v.as_object() {
                        let k_mt = vm.heap.intern_key("mediaText");
                        match vm.get_property_by_key(mh, k_mt) {
                            Ok(mt) if !mt.is_undefined() => vm.value_to_rust_string(mt),
                            _ => vm.value_to_rust_string(v),
                        }
                    } else {
                        vm.value_to_rust_string(v)
                    };
                    *state.media.borrow_mut() = parse_media_query_list(&media_str);
                }
            }
        }
        let sheet_state = Rc::new(RefCell::new(state));
        let obj = wrap_stylesheet(vm, &h, sheet_state, Some(proto));
        Ok(JsValue::from_object(obj))
    });
    let kp = vm.heap.intern_key("prototype");
    let kc = vm.heap.intern_key("constructor");
    vm.heap.object_define_data(
        ctor,
        kp,
        JsValue::from_object(proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        proto,
        kc,
        JsValue::from_object(ctor),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
    vm.mark_constructor(ctor);
    define_global(vm, "CSSStyleSheet", JsValue::from_object(ctor));
    host.borrow_mut().css_stylesheet_proto = Some(proto);
}

fn sync_sheet_to_dom(host: &SharedHost, state: &SharedStyleSheet) {
    let (owner_opt, css_text) = {
        let mut st = state.borrow_mut();
        let css = st.to_css_text();
        st.source_hash = simple_hash(css.as_bytes());
        (st.owner_node, css)
    };
    if let Some(owner) = owner_opt {
        let doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = doc_rc.borrow_mut();
        if doc.get(owner).is_some() {
            let children: Vec<NodeId> = doc.children(owner).collect();
            for c in children {
                doc.remove_child(c);
            }
            if !css_text.is_empty() {
                let txt = doc.create_text(&css_text);
                doc.append_child(owner, txt);
            }
            doc.bump_style_revision_for(owner);
        }
    }
    host.borrow_mut().mark_sheets_dirty();
}

pub(crate) fn wrap_stylesheet(
    vm: &mut JsVm,
    host: &SharedHost,
    state: SharedStyleSheet,
    proto_override: Option<HeapHandle>,
) -> HeapHandle {
    if let Some(existing) = state.borrow().js_handle {
        if vm.heap.get_object(existing).is_some() {
            return existing;
        }
    }
    let proto = proto_override
        .or_else(|| host.borrow().css_stylesheet_proto)
        .unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    state.borrow_mut().js_handle = Some(obj);
    host.borrow_mut()
        .stylesheet_handle_map
        .insert(obj, Rc::clone(&state));

    // Live `cssRules` / `rules` Proxy so `const {cssRules} = sheet; sheet.insertRule(...)` stays live!
    let live_rules =
        create_live_css_rule_list(vm, host, Rc::clone(&state), obj, obj, None, Vec::new());
    let k_rules = vm.heap.intern_key("cssRules");
    let k_rules_alias = vm.heap.intern_key("rules");
    vm.heap.object_define_data(
        obj,
        k_rules,
        live_rules,
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        obj,
        k_rules_alias,
        live_rules,
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );

    // `ownerNode`, `href`, `title`, `type`, `parentStyleSheet`, `ownerRule`, `media`
    let st_owner = Rc::clone(&state);
    let h_owner = Rc::clone(host);
    let g_owner = vm.alloc_host_fn("get ownerNode", move |vm, _, _| {
        let owner = st_owner.borrow().owner_node;
        Ok(owner.map_or(JsValue::NULL, |nid| wrap_node(vm, &h_owner, nid)))
    });
    define_host_accessor(vm, obj, "ownerNode", Some(g_owner), None);

    let st_href = Rc::clone(&state);
    let g_href = vm.alloc_host_fn("get href", move |vm, _, _| {
        let href = st_href.borrow().href.clone();
        Ok(href.map_or(JsValue::NULL, |s| {
            JsValue::from_string(vm.heap.alloc_string(s))
        }))
    });
    define_host_accessor(vm, obj, "href", Some(g_href), None);

    let st_title = Rc::clone(&state);
    let g_title = vm.alloc_host_fn("get title", move |vm, _, _| {
        let title = st_title.borrow().title.clone();
        Ok(title.filter(|s| !s.is_empty()).map_or(JsValue::NULL, |s| {
            JsValue::from_string(vm.heap.alloc_string(s))
        }))
    });
    define_host_accessor(vm, obj, "title", Some(g_title), None);

    let st_ps = Rc::clone(&state);
    let g_ps = vm.alloc_host_fn("get parentStyleSheet", move |_, _, _| {
        Ok(st_ps
            .borrow()
            .parent_sheet
            .map_or(JsValue::NULL, JsValue::from_object))
    });
    define_host_accessor(vm, obj, "parentStyleSheet", Some(g_ps), None);

    let st_or = Rc::clone(&state);
    let g_or = vm.alloc_host_fn("get ownerRule", move |_, _, _| {
        Ok(st_or
            .borrow()
            .owner_rule
            .map_or(JsValue::NULL, JsValue::from_object))
    });
    define_host_accessor(vm, obj, "ownerRule", Some(g_or), None);

    let media_rc = Rc::clone(&state.borrow().media);
    let existing_mp = state.borrow().media_proxy;
    let media_proxy = if let Some(mp) = existing_mp {
        mp
    } else {
        let h_med = Rc::clone(host);
        let mp = create_media_list_proxy(
            vm,
            host,
            Rc::clone(&media_rc),
            Some(Rc::new(move || {
                h_med.borrow_mut().mark_sheets_dirty();
            })),
        );
        state.borrow_mut().media_proxy = Some(mp);
        mp
    };
    let g_med = vm.alloc_host_fn("get media", move |_, _, _| Ok(media_proxy));
    let h_med_s = Rc::clone(host);
    let s_med = vm.alloc_host_fn("set media", move |vm, _, args| {
        let v = arg(args, 0);
        let raw = if v.is_null() {
            String::new()
        } else {
            vm.value_to_rust_string(v)
        };
        *media_rc.borrow_mut() = parse_media_query_list(&raw);
        h_med_s.borrow_mut().mark_sheets_dirty();
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, obj, "media", Some(g_med), Some(s_med));

    let k_type = vm.heap.intern_key("type");
    let v_type = JsValue::from_string(vm.heap.alloc_string("text/css"));
    vm.heap
        .object_define_data(obj, k_type, v_type, PropertyAttrs::empty(), &mut vm.shapes);

    // `disabled` getter/setter
    let st_d1 = Rc::clone(&state);
    let g_dis = vm.alloc_host_fn("get disabled", move |_, _, _| {
        Ok(JsValue::from_bool(st_d1.borrow().disabled))
    });
    let st_d2 = Rc::clone(&state);
    let h_d2 = Rc::clone(host);
    let s_dis = vm.alloc_host_fn("set disabled", move |vm, _, args| {
        st_d2.borrow_mut().disabled = vm.to_boolean(arg(args, 0));
        h_d2.borrow_mut().mark_sheets_dirty();
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, obj, "disabled", Some(g_dis), Some(s_dis));

    // `insertRule(rule, index = 0)`, `deleteRule(index)`, and legacy `removeRule(index)` / `addRule`
    attach_rule_list_mutators(vm, obj, host, Rc::clone(&state), Vec::new());

    let st_add = Rc::clone(&state);
    define_host_method(vm, obj, "addRule", move |vm, this_val, args| {
        let sel = if args.is_empty() || arg(args, 0).is_undefined() {
            "undefined".to_string()
        } else {
            vm.value_to_rust_string(arg(args, 0))
        };
        let style_str = if args.len() < 2 || arg(args, 1).is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(arg(args, 1))
        };
        let idx_val = if args.len() >= 3 && !arg(args, 2).is_undefined() {
            arg(args, 2)
        } else {
            JsValue::from_i32(st_add.borrow().rules.len() as i32)
        };
        let combined = format!("{sel} {{ {style_str} }}");
        let combined_js = JsValue::from_string(vm.heap.alloc_string(combined));
        let k_ins = vm.heap.intern_key("insertRule");
        let ins_fn = vm.get_property_by_key(obj, k_ins)?;
        let _ = vm.call_value(ins_fn, this_val, &[combined_js, idx_val])?;
        Ok(JsValue::from_i32(-1))
    });

    // `replaceSync(text)` & `replace(text)`
    let st_rs = Rc::clone(&state);
    let h_rs = Rc::clone(host);
    define_host_method(vm, obj, "replaceSync", move |vm, _, args| {
        if !st_rs.borrow().is_constructed {
            return Err(create_dom_exception(
                vm,
                "NotAllowedError",
                "replaceSync() can only be called on constructed CSSStyleSheets",
            ));
        }
        let text = vm.value_to_rust_string(arg(args, 0));
        let mut parsed = parse_cssom_rules(&text);
        parsed.retain(|r| !matches!(r, CssomRuleKind::Import { .. }));
        detach_and_clear_child_rule_cache(vm, obj);
        {
            let mut st = st_rs.borrow_mut();
            st.rules = parsed;
            st.source_hash = simple_hash(text.as_bytes());
        }
        sync_sheet_to_dom(&h_rs, &st_rs);
        Ok(JsValue::UNDEFINED)
    });

    let st_rep = Rc::clone(&state);
    let h_rep = Rc::clone(host);
    define_host_method(vm, obj, "replace", move |vm, this_val, args| {
        let p = alloc_promise(vm);
        if !st_rep.borrow().is_constructed {
            let err = create_dom_exception(
                vm,
                "NotAllowedError",
                "replace() can only be called on constructed CSSStyleSheets",
            );
            reject_promise(vm, p, err.value);
            return Ok(JsValue::from_object(p));
        }
        let text = vm.value_to_rust_string(arg(args, 0));
        let mut parsed = parse_cssom_rules(&text);
        parsed.retain(|r| !matches!(r, CssomRuleKind::Import { .. }));
        detach_and_clear_child_rule_cache(vm, obj);
        {
            let mut st = st_rep.borrow_mut();
            st.rules = parsed;
            st.source_hash = simple_hash(text.as_bytes());
        }
        sync_sheet_to_dom(&h_rep, &st_rep);
        resolve_promise(vm, p, this_val);
        Ok(JsValue::from_object(p))
    });

    obj
}

fn forward_symbol_get(
    vm: &mut JsVm,
    target_val: JsValue,
    key_val: JsValue,
) -> Result<JsValue, rix_js::JsException> {
    if key_val.as_symbol() == Some(vm.sym_iterator) {
        if let Some(t_h) = target_val.as_object() {
            let k = vm.key_symbol_iterator;
            return vm.get_property_by_key(t_h, k);
        }
    }
    Ok(JsValue::UNDEFINED)
}

fn get_or_init_child_cache(vm: &mut JsVm, owner_obj: HeapHandle) -> HeapHandle {
    let k = vm.heap.intern_key("__rix_rule_cache__");
    if let Some(prop) = vm.heap.object_get_own(owner_obj, k, &vm.shapes) {
        if let Some(h) = prop.value.as_object() {
            return h;
        }
    }
    let cache = vm.alloc_ordinary_object();
    vm.heap.object_define_data(
        owner_obj,
        k,
        JsValue::from_object(cache),
        PropertyAttrs::WRITABLE | PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    cache
}

fn detach_rule_wrapper_tree(vm: &mut JsVm, rule_h: HeapHandle) {
    let k_det = vm.heap.intern_key("__rix_detach__");
    if let Some(prop) = vm.heap.object_get_own(rule_h, k_det, &vm.shapes) {
        let f = prop.value;
        let _ = vm.call_value(f, JsValue::from_object(rule_h), &[]);
    }
    detach_and_clear_child_rule_cache(vm, rule_h);
}

fn detach_and_clear_child_rule_cache(vm: &mut JsVm, owner_obj: HeapHandle) {
    let k = vm.heap.intern_key("__rix_rule_cache__");
    if let Some(prop) = vm.heap.object_get_own(owner_obj, k, &vm.shapes) {
        if let Some(cache_h) = prop.value.as_object() {
            let children: Vec<HeapHandle> = {
                let mut out = Vec::new();
                for i in 0..512 {
                    let ki = vm.heap.intern_key(&i.to_string());
                    if let Some(cp) = vm.heap.object_get_own(cache_h, ki, &vm.shapes) {
                        if let Some(ch) = cp.value.as_object() {
                            out.push(ch);
                        }
                    }
                }
                out
            };
            for ch in children {
                detach_rule_wrapper_tree(vm, ch);
            }
        }
    }
    let fresh = vm.alloc_ordinary_object();
    vm.heap.object_define_data(
        owner_obj,
        k,
        JsValue::from_object(fresh),
        PropertyAttrs::WRITABLE | PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
}

fn update_rule_wrapper_index(vm: &mut JsVm, rule_h: HeapHandle, new_idx: usize) {
    let k_upd = vm.heap.intern_key("__rix_set_idx__");
    if let Some(prop) = vm.heap.object_get_own(rule_h, k_upd, &vm.shapes) {
        let f = prop.value;
        let _ = vm.call_value(
            f,
            JsValue::from_object(rule_h),
            &[JsValue::from_i32(new_idx as i32)],
        );
    }
}

fn shift_child_rule_cache_on_insert(
    vm: &mut JsVm,
    owner_obj: HeapHandle,
    insert_idx: usize,
    old_len: usize,
) {
    let cache = get_or_init_child_cache(vm, owner_obj);
    for i in (insert_idx..old_len).rev() {
        let k_from = vm.heap.intern_key(&i.to_string());
        let k_to = vm.heap.intern_key(&(i + 1).to_string());
        if let Some(prop) = vm.heap.object_get_own(cache, k_from, &vm.shapes) {
            let val = prop.value;
            let _ = vm.delete_property_by_key(cache, k_from);
            vm.heap
                .object_define_data(cache, k_to, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
            if let Some(ch) = val.as_object() {
                update_rule_wrapper_index(vm, ch, i + 1);
            }
        }
    }
}

fn shift_child_rule_cache_on_delete(
    vm: &mut JsVm,
    owner_obj: HeapHandle,
    del_idx: usize,
    old_len: usize,
) {
    let cache = get_or_init_child_cache(vm, owner_obj);
    let k_del = vm.heap.intern_key(&del_idx.to_string());
    if let Some(prop) = vm.heap.object_get_own(cache, k_del, &vm.shapes) {
        let val = prop.value;
        let _ = vm.delete_property_by_key(cache, k_del);
        if let Some(ch) = val.as_object() {
            detach_rule_wrapper_tree(vm, ch);
        }
    }
    for i in (del_idx + 1)..old_len {
        let k_from = vm.heap.intern_key(&i.to_string());
        let k_to = vm.heap.intern_key(&(i - 1).to_string());
        if let Some(prop) = vm.heap.object_get_own(cache, k_from, &vm.shapes) {
            let val = prop.value;
            let _ = vm.delete_property_by_key(cache, k_from);
            vm.heap
                .object_define_data(cache, k_to, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
            if let Some(ch) = val.as_object() {
                update_rule_wrapper_index(vm, ch, i - 1);
            }
        }
    }
}

fn attach_array_like_iterator(vm: &mut JsVm, target: HeapHandle) {
    let sym_fn = vm.alloc_host_fn("[Symbol.iterator]", move |vm, this_val, _| {
        let iter_obj = vm.alloc_ordinary_object();
        let cursor = Rc::new(RefCell::new(0usize));
        let target_val = this_val;
        define_host_method(vm, iter_obj, "next", move |vm, _, _| {
            let idx = *cursor.borrow();
            let len = target_val
                .as_object()
                .and_then(|o| {
                    let kl = vm.heap.intern_key("length");
                    vm.get_property_by_key(o, kl).ok()
                })
                .and_then(|v| vm.to_number(v).ok())
                .unwrap_or(0.0) as usize;
            let res = vm.alloc_ordinary_object();
            let kv = vm.heap.intern_key("value");
            let kd = vm.heap.intern_key("done");
            if idx >= len {
                vm.heap.object_define_data(
                    res,
                    kv,
                    JsValue::UNDEFINED,
                    PropertyAttrs::DEFAULT,
                    &mut vm.shapes,
                );
                vm.heap.object_define_data(
                    res,
                    kd,
                    JsValue::TRUE,
                    PropertyAttrs::DEFAULT,
                    &mut vm.shapes,
                );
            } else {
                *cursor.borrow_mut() = idx + 1;
                let item = target_val
                    .as_object()
                    .and_then(|o| {
                        let ki = vm.heap.intern_key(&idx.to_string());
                        vm.get_property_by_key(o, ki).ok()
                    })
                    .unwrap_or(JsValue::UNDEFINED);
                vm.heap
                    .object_define_data(res, kv, item, PropertyAttrs::DEFAULT, &mut vm.shapes);
                vm.heap.object_define_data(
                    res,
                    kd,
                    JsValue::FALSE,
                    PropertyAttrs::DEFAULT,
                    &mut vm.shapes,
                );
            }
            Ok(JsValue::from_object(res))
        });
        let self_iter = vm.alloc_host_fn("[Symbol.iterator]", |_, this_v, _| Ok(this_v));
        let sk = vm.key_symbol_iterator;
        vm.heap.object_define_data(
            iter_obj,
            sk,
            JsValue::from_object(self_iter),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        Ok(JsValue::from_object(iter_obj))
    });
    let sk = vm.key_symbol_iterator;
    vm.heap.object_define_data(
        target,
        sk,
        JsValue::from_object(sym_fn),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
}

fn attach_rule_list_mutators(
    vm: &mut JsVm,
    target_obj: HeapHandle,
    host: &SharedHost,
    state: SharedStyleSheet,
    list_path: Vec<usize>,
) {
    let st_ins = Rc::clone(&state);
    let h_ins = Rc::clone(host);
    let p_ins = list_path.clone();
    define_host_method(vm, target_obj, "insertRule", move |vm, _, args| {
        if args.is_empty() {
            return Err(vm.type_error("insertRule requires at least 1 argument"));
        }
        let rule_str = vm.value_to_rust_string(arg(args, 0));
        let index = if args.len() >= 2 && !arg(args, 1).is_undefined() {
            let n = vm.to_number(arg(args, 1))?;
            if !n.is_finite() || n < 0.0 {
                return Err(create_dom_exception(
                    vm,
                    "IndexSizeError",
                    "Invalid rule index",
                ));
            }
            n as usize
        } else {
            0usize
        };
        let (ns, nesting, list_len, is_constructed) = {
            let st = st_ins.borrow();
            let Some(list) = st.rule_list(&p_ins) else {
                return Err(create_dom_exception(
                    vm,
                    "IndexSizeError",
                    "Parent grouping rule no longer exists",
                ));
            };
            (
                st.active_namespaces(),
                st.nesting_context_for_list(&p_ins),
                list.len(),
                st.is_constructed,
            )
        };
        if index > list_len {
            return Err(create_dom_exception(
                vm,
                "IndexSizeError",
                "Rule index exceeds cssRules.length",
            ));
        }
        let ns_opt = (!ns.is_empty()).then_some(&ns);
        let parsed = parse_single_cssom_rule_with_context(&rule_str, ns_opt, nesting).map_err(
            |e| match e {
                CssomParseError::SyntaxError(msg) => create_dom_exception(vm, "SyntaxError", &msg),
                CssomParseError::HierarchyRequestError(msg) => {
                    create_dom_exception(vm, "HierarchyRequestError", &msg)
                }
            },
        )?;
        if is_constructed && p_ins.is_empty() && matches!(parsed, CssomRuleKind::Import { .. }) {
            return Err(create_dom_exception(
                vm,
                "SyntaxError",
                "@import rules are disallowed in constructed CSSStyleSheets",
            ));
        }
        if !p_ins.is_empty()
            && matches!(
                parsed,
                CssomRuleKind::Import { .. } | CssomRuleKind::Namespace { .. }
            )
        {
            return Err(create_dom_exception(
                vm,
                "HierarchyRequestError",
                "Rule cannot be inserted inside a CSSGroupingRule",
            ));
        }
        {
            let mut st = st_ins.borrow_mut();
            let Some(list) = st.rule_list_mut(&p_ins) else {
                return Err(create_dom_exception(
                    vm,
                    "IndexSizeError",
                    "Parent grouping rule no longer exists",
                ));
            };
            list.insert(index, parsed);
        }
        shift_child_rule_cache_on_insert(vm, target_obj, index, list_len);
        sync_sheet_to_dom(&h_ins, &st_ins);
        Ok(JsValue::from_i32(index as i32))
    });

    let is_top = list_path.is_empty();
    let st_del = Rc::clone(&state);
    let h_del = Rc::clone(host);
    let p_del = list_path.clone();
    let del_fn = vm.alloc_host_fn("deleteRule", move |vm, _, args| {
        if args.is_empty() {
            return Err(vm.type_error("deleteRule requires 1 argument"));
        }
        let n = vm.to_number(arg(args, 0))?;
        let index = if n.is_finite() && n >= 0.0 {
            n as usize
        } else {
            return Err(create_dom_exception(
                vm,
                "IndexSizeError",
                "Invalid rule index",
            ));
        };
        let old_len = {
            let mut st = st_del.borrow_mut();
            let Some(list) = st.rule_list_mut(&p_del) else {
                return Err(create_dom_exception(
                    vm,
                    "IndexSizeError",
                    "Parent grouping rule no longer exists",
                ));
            };
            let len = list.len();
            if index >= len {
                return Err(create_dom_exception(
                    vm,
                    "IndexSizeError",
                    "Rule index out of bounds",
                ));
            }
            list.remove(index);
            len
        };
        shift_child_rule_cache_on_delete(vm, target_obj, index, old_len);
        sync_sheet_to_dom(&h_del, &st_del);
        Ok(JsValue::UNDEFINED)
    });
    let k_del = vm.heap.intern_key("deleteRule");
    vm.heap.object_define_data(
        target_obj,
        k_del,
        JsValue::from_object(del_fn),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
    if is_top {
        let st_rem = Rc::clone(&state);
        let h_rem = Rc::clone(host);
        let p_rem = list_path;
        let rem_fn = vm.alloc_host_fn("removeRule", move |vm, _, args| {
            let index = if args.is_empty() || arg(args, 0).is_undefined() {
                0usize
            } else {
                let n = vm.to_number(arg(args, 0))?;
                if n.is_finite() && n >= 0.0 {
                    n as usize
                } else {
                    return Err(create_dom_exception(
                        vm,
                        "IndexSizeError",
                        "Invalid rule index",
                    ));
                }
            };
            let old_len = {
                let mut st = st_rem.borrow_mut();
                let Some(list) = st.rule_list_mut(&p_rem) else {
                    return Err(create_dom_exception(
                        vm,
                        "IndexSizeError",
                        "Parent grouping rule no longer exists",
                    ));
                };
                let len = list.len();
                if index >= len {
                    return Err(create_dom_exception(
                        vm,
                        "IndexSizeError",
                        "Rule index out of bounds",
                    ));
                }
                list.remove(index);
                len
            };
            shift_child_rule_cache_on_delete(vm, target_obj, index, old_len);
            sync_sheet_to_dom(&h_rem, &st_rem);
            Ok(JsValue::UNDEFINED)
        });
        let k_rem = vm.heap.intern_key("removeRule");
        vm.heap.object_define_data(
            target_obj,
            k_rem,
            JsValue::from_object(rem_fn),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
    }
}

fn create_live_css_rule_list(
    vm: &mut JsVm,
    host: &SharedHost,
    state: SharedStyleSheet,
    parent_sheet: HeapHandle,
    cache_owner: HeapHandle,
    parent_rule: Option<HeapHandle>,
    list_path: Vec<usize>,
) -> JsValue {
    let rl_proto = host
        .borrow()
        .css_rule_protos
        .get("CSSRuleList")
        .copied()
        .unwrap_or(vm.object_proto);
    let target = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(rl_proto)));
    attach_array_like_iterator(vm, target);
    let st_item = Rc::clone(&state);
    let h_item = Rc::clone(host);
    let p_item = list_path.clone();
    define_host_method(vm, target, "item", move |vm, _, args| {
        let idx = vm.to_number(arg(args, 0)).unwrap_or(-1.0);
        if idx < 0.0 || !idx.is_finite() {
            return Ok(JsValue::NULL);
        }
        let i = idx as usize;
        let len = st_item
            .borrow()
            .rule_list(&p_item)
            .map_or(0, <[CssomRuleKind]>::len);
        if i >= len {
            return Ok(JsValue::NULL);
        }
        let mut child_path = p_item.clone();
        child_path.push(i);
        Ok(JsValue::from_object(wrap_css_rule(
            vm,
            &h_item,
            Rc::clone(&st_item),
            child_path,
            parent_sheet,
            cache_owner,
            parent_rule,
        )))
    });

    let handler = vm.alloc_ordinary_object();
    let st_get = Rc::clone(&state);
    let h_get = Rc::clone(host);
    let p_get = list_path;
    define_host_method(vm, handler, "get", move |vm, _, args| {
        let key_val = arg(args, 1);
        if key_val.is_symbol() {
            return forward_symbol_get(vm, arg(args, 0), key_val);
        }
        let prop = vm.value_to_rust_string(key_val);
        if prop == "length" {
            let len = st_get
                .borrow()
                .rule_list(&p_get)
                .map_or(0, <[CssomRuleKind]>::len);
            return Ok(JsValue::from_i32(len as i32));
        }
        if let Ok(idx) = prop.parse::<usize>() {
            let len = st_get
                .borrow()
                .rule_list(&p_get)
                .map_or(0, <[CssomRuleKind]>::len);
            if idx < len {
                let mut child_path = p_get.clone();
                child_path.push(idx);
                return Ok(JsValue::from_object(wrap_css_rule(
                    vm,
                    &h_get,
                    Rc::clone(&st_get),
                    child_path,
                    parent_sheet,
                    cache_owner,
                    parent_rule,
                )));
            }
            return Ok(JsValue::UNDEFINED);
        }
        let k = vm.heap.intern_key(&prop);
        if let Some(t_h) = arg(args, 0).as_object() {
            if vm.heap.object_get_own(t_h, k, &vm.shapes).is_some() {
                return vm.get_property_by_key(t_h, k);
            }
        }
        Ok(JsValue::UNDEFINED)
    });

    let proxy = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Proxy(rix_js::ProxyObject {
            target,
            handler,
            revoked: false,
        }),
        Some(vm.object_proto),
    ));
    JsValue::from_object(proxy)
}

fn wrap_css_rule(
    vm: &mut JsVm,
    host: &SharedHost,
    state: SharedStyleSheet,
    rule_path: Vec<usize>,
    parent_sheet: HeapHandle,
    cache_owner: HeapHandle,
    parent_rule: Option<HeapHandle>,
) -> HeapHandle {
    let cache = get_or_init_child_cache(vm, cache_owner);
    let idx_str = rule_path.last().copied().unwrap_or(0).to_string();
    let cache_key = vm.heap.intern_key(&idx_str);
    if let Some(prop) = vm.heap.object_get_own(cache, cache_key, &vm.shapes) {
        if let Some(h) = prop.value.as_object() {
            return h;
        }
    }

    let (rule_type, proto_name, has_style_decl, is_grouping) = {
        let st = state.borrow();
        match st.rule_at(&rule_path) {
            Some(CssomRuleKind::Style { .. }) => (1i32, "CSSStyleRule", true, true),
            Some(CssomRuleKind::NestedDeclarations { .. }) => {
                (0, "CSSNestedDeclarations", true, false)
            }
            Some(CssomRuleKind::Import { .. }) => (3, "CSSImportRule", false, false),
            Some(CssomRuleKind::Media { .. }) => (4, "CSSMediaRule", false, true),
            Some(CssomRuleKind::FontFace { .. }) => (5, "CSSFontFaceRule", true, false),
            Some(CssomRuleKind::Page { .. }) => (6, "CSSPageRule", false, true),
            Some(CssomRuleKind::Keyframes { .. }) => (7, "CSSKeyframesRule", false, false),
            Some(CssomRuleKind::Keyframe { .. }) => (8, "CSSKeyframeRule", true, false),
            Some(CssomRuleKind::Namespace { .. }) => (10, "CSSNamespaceRule", false, false),
            Some(CssomRuleKind::Supports { .. }) => (12, "CSSSupportsRule", false, true),
            Some(CssomRuleKind::Container { .. }) => (0, "CSSContainerRule", false, true),
            Some(CssomRuleKind::LayerBlock { .. }) => (0, "CSSLayerBlockRule", false, true),
            Some(CssomRuleKind::LayerStatement { .. }) => {
                (0, "CSSLayerStatementRule", false, false)
            }
            Some(CssomRuleKind::Scope { .. }) => (0, "CSSScopeRule", false, true),
            Some(CssomRuleKind::Property { .. }) => (0, "CSSPropertyRule", false, false),
            Some(CssomRuleKind::Other { css_text })
                if css_text
                    .trim_start()
                    .to_ascii_lowercase()
                    .starts_with("@counter-style") =>
            {
                (0, "CSSCounterStyleRule", false, false)
            }
            _ => (0, "CSSRule", false, false),
        }
    };
    let proto = host
        .borrow()
        .css_rule_protos
        .get(proto_name)
        .copied()
        .or_else(|| host.borrow().css_rule_protos.get("CSSRule").copied())
        .unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));

    let path_rc = Rc::new(RefCell::new(rule_path));
    let ps_slot = Rc::new(Cell::new(Some(parent_sheet)));
    let pr_slot = Rc::new(Cell::new(parent_rule));
    let detached_flag = Rc::new(Cell::new(false));
    let ps_det = Rc::clone(&ps_slot);
    let pr_det = Rc::clone(&pr_slot);
    let det_f = Rc::clone(&detached_flag);
    let detach_fn = vm.alloc_host_fn("__rix_detach__", move |_, _, _| {
        ps_det.set(None);
        pr_det.set(None);
        det_f.set(true);
        Ok(JsValue::UNDEFINED)
    });
    let k_det = vm.heap.intern_key("__rix_detach__");
    vm.heap.object_define_data(
        obj,
        k_det,
        JsValue::from_object(detach_fn),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    let path_upd = Rc::clone(&path_rc);
    let upd_fn = vm.alloc_host_fn("__rix_set_idx__", move |vm, _, args| {
        let idx = vm.to_number(arg(args, 0)).unwrap_or(0.0) as usize;
        let mut p = path_upd.borrow_mut();
        if let Some(last) = p.last_mut() {
            *last = idx;
        }
        Ok(JsValue::UNDEFINED)
    });
    let k_upd = vm.heap.intern_key("__rix_set_idx__");
    vm.heap.object_define_data(
        obj,
        k_upd,
        JsValue::from_object(upd_fn),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );

    let k_type = vm.heap.intern_key("type");
    vm.heap.object_define_data(
        obj,
        k_type,
        JsValue::from_i32(rule_type),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    let ps_get = Rc::clone(&ps_slot);
    let g_ps = vm.alloc_host_fn("get parentStyleSheet", move |_, _, _| {
        Ok(ps_get.get().map_or(JsValue::NULL, JsValue::from_object))
    });
    define_host_accessor(vm, obj, "parentStyleSheet", Some(g_ps), None);

    // `parentRule` (null for top-level rules or detached rules, exact cached parent wrapper for nested rules)
    let pr_get = Rc::clone(&pr_slot);
    let g_pr = vm.alloc_host_fn("get parentRule", move |_, _, _| {
        Ok(pr_get.get().map_or(JsValue::NULL, JsValue::from_object))
    });
    define_host_accessor(vm, obj, "parentRule", Some(g_pr), None);

    // `cssText`
    let st_ct = Rc::clone(&state);
    let p_ct = Rc::clone(&path_rc);
    let det_ct = Rc::clone(&detached_flag);
    let g_ct = vm.alloc_host_fn("get cssText", move |vm, _, _| {
        let text = if det_ct.get() {
            String::new()
        } else {
            let p = p_ct.borrow();
            st_ct
                .borrow()
                .rule_at(&p)
                .map(CssomRuleKind::to_css_text)
                .unwrap_or_default()
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(text)))
    });
    define_host_accessor(vm, obj, "cssText", Some(g_ct), None);

    // `selectorText` (for CSSStyleRule & CSSPageRule)
    if proto_name == "CSSStyleRule" {
        let st_s1 = Rc::clone(&state);
        let p_s1 = Rc::clone(&path_rc);
        let g_sel = vm.alloc_host_fn("get selectorText", move |vm, _, _| {
            let p = p_s1.borrow();
            let sel = match st_s1.borrow().rule_at(&p) {
                Some(CssomRuleKind::Style { selector_text, .. }) => selector_text.clone(),
                _ => String::new(),
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(sel)))
        });
        let st_s2 = Rc::clone(&state);
        let h_s2 = Rc::clone(host);
        let p_s2 = Rc::clone(&path_rc);
        let det_s2 = Rc::clone(&detached_flag);
        let s_sel = vm.alloc_host_fn("set selectorText", move |vm, _, args| {
            if det_s2.get() {
                return Ok(JsValue::UNDEFINED);
            }
            let raw = vm.value_to_rust_string(arg(args, 0));
            let p = p_s2.borrow();
            let (ns, nesting) = {
                let st = st_s2.borrow();
                let parent_list = &p[..p.len().saturating_sub(1)];
                (
                    st.active_namespaces(),
                    st.nesting_context_for_list(parent_list),
                )
            };
            let ns_opt = (!ns.is_empty()).then_some(&ns);
            if let Ok(canonical) = parse_and_serialize_selector_for_context(&raw, ns_opt, nesting) {
                if let Some(CssomRuleKind::Style { selector_text, .. }) =
                    st_s2.borrow_mut().rule_at_mut(&p)
                {
                    *selector_text = canonical;
                }
                drop(p);
                sync_sheet_to_dom(&h_s2, &st_s2);
            }
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, obj, "selectorText", Some(g_sel), Some(s_sel));
    } else if proto_name == "CSSPageRule" {
        let st_ps1 = Rc::clone(&state);
        let p_ps1 = Rc::clone(&path_rc);
        let det_ps1 = Rc::clone(&detached_flag);
        let detached_page_sel = Rc::new(RefCell::new(String::new()));
        let dps1 = Rc::clone(&detached_page_sel);
        let g_sel = vm.alloc_host_fn("get selectorText", move |vm, _, _| {
            let sel = if det_ps1.get() {
                dps1.borrow().clone()
            } else {
                let p = p_ps1.borrow();
                match st_ps1.borrow().rule_at(&p) {
                    Some(CssomRuleKind::Page { selector_text, .. }) => selector_text.clone(),
                    _ => String::new(),
                }
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(sel)))
        });
        let st_ps2 = Rc::clone(&state);
        let h_ps2 = Rc::clone(host);
        let p_ps2 = Rc::clone(&path_rc);
        let det_ps2 = Rc::clone(&detached_flag);
        let dps2 = Rc::clone(&detached_page_sel);
        let s_sel = vm.alloc_host_fn("set selectorText", move |vm, _, args| {
            let raw = vm.value_to_rust_string(arg(args, 0));
            if let Some(canonical) = parse_and_serialize_page_selector(&raw) {
                if det_ps2.get() {
                    *dps2.borrow_mut() = canonical;
                } else {
                    let p = p_ps2.borrow();
                    if let Some(CssomRuleKind::Page { selector_text, .. }) =
                        st_ps2.borrow_mut().rule_at_mut(&p)
                    {
                        *selector_text = canonical;
                    }
                    drop(p);
                    sync_sheet_to_dom(&h_ps2, &st_ps2);
                }
            }
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, obj, "selectorText", Some(g_sel), Some(s_sel));

        let page_style =
            create_page_descriptors_obj(vm, host, Rc::clone(&state), Rc::clone(&path_rc));
        let g_style = vm.alloc_host_fn("get style", move |_, _, _| Ok(page_style));
        define_host_accessor(vm, obj, "style", Some(g_style), None);
    } else if proto_name == "CSSKeyframeRule" {
        let st_k1 = Rc::clone(&state);
        let p_k1 = Rc::clone(&path_rc);
        let g_kt = vm.alloc_host_fn("get keyText", move |vm, _, _| {
            let p = p_k1.borrow();
            let kt = match st_k1.borrow().rule_at(&p) {
                Some(CssomRuleKind::Keyframe { key_text, .. }) => key_text.clone(),
                _ => String::new(),
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(kt)))
        });
        let st_k2 = Rc::clone(&state);
        let h_k2 = Rc::clone(host);
        let p_k2 = Rc::clone(&path_rc);
        let s_kt = vm.alloc_host_fn("set keyText", move |vm, _, args| {
            let raw = vm.value_to_rust_string(arg(args, 0));
            let Some(canonical) = parse_and_serialize_keyframe_selector(&raw) else {
                return Err(create_dom_exception(
                    vm,
                    "SyntaxError",
                    "Invalid CSSKeyframeRule keyText",
                ));
            };
            {
                let p = p_k2.borrow();
                if let Some(CssomRuleKind::Keyframe { key_text, .. }) =
                    st_k2.borrow_mut().rule_at_mut(&p)
                {
                    *key_text = canonical;
                }
            }
            sync_sheet_to_dom(&h_k2, &st_k2);
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, obj, "keyText", Some(g_kt), Some(s_kt));
    }

    // `style` (`CSSStyleDeclaration` with `[PutForwards=cssText]`)
    if has_style_decl {
        let style_decl = create_rule_style_proxy(vm, host, Rc::clone(&state), Rc::clone(&path_rc));
        let g_style = vm.alloc_host_fn("get style", move |_, _, _| Ok(style_decl));
        let st_rs = Rc::clone(&state);
        let h_rs = Rc::clone(host);
        let p_rs = Rc::clone(&path_rc);
        let s_style = vm.alloc_host_fn("set style", move |vm, _, args| {
            let val = vm.value_to_rust_string(arg(args, 0));
            let validated = canonicalize_css_decls(&val);
            {
                let p = p_rs.borrow();
                if let Some(decls_text) = st_rs
                    .borrow_mut()
                    .rule_at_mut(&p)
                    .and_then(CssomRuleKind::declarations_text_mut)
                {
                    *decls_text = serialize_style_decls_with_priority(&validated);
                }
            }
            sync_sheet_to_dom(&h_rs, &st_rs);
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, obj, "style", Some(g_style), Some(s_style));
    }

    // `CSSGroupingRule` (`CSSStyleRule`, `@media`, `@supports`, `@container`, `@layer`, `@scope`, `@page`)
    if is_grouping {
        let init_path = path_rc.borrow().clone();
        let live_child_rules = create_live_css_rule_list(
            vm,
            host,
            Rc::clone(&state),
            parent_sheet,
            obj,
            Some(obj),
            init_path.clone(),
        );
        let k_cr = vm.heap.intern_key("cssRules");
        vm.heap.object_define_data(
            obj,
            k_cr,
            live_child_rules,
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        attach_rule_list_mutators(vm, obj, host, Rc::clone(&state), init_path);
    }

    // `CSSContainerRule.containerName` / `CSSContainerRule.containerQuery`
    if proto_name == "CSSContainerRule" {
        let st_cn = Rc::clone(&state);
        let p_cn = Rc::clone(&path_rc);
        let g_cn = vm.alloc_host_fn("get containerName", move |vm, _, _| {
            let p = p_cn.borrow();
            let txt = match st_cn.borrow().rule_at(&p) {
                Some(CssomRuleKind::Container {
                    container_name: Some(n),
                    ..
                }) => n.clone(),
                _ => String::new(),
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(txt)))
        });
        define_host_accessor(vm, obj, "containerName", Some(g_cn), None);

        let st_cq = Rc::clone(&state);
        let p_cq = Rc::clone(&path_rc);
        let g_cq = vm.alloc_host_fn("get containerQuery", move |vm, _, _| {
            let p = p_cq.borrow();
            let txt = match st_cq.borrow().rule_at(&p) {
                Some(CssomRuleKind::Container {
                    container_query, ..
                }) => container_query.clone(),
                _ => String::new(),
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(txt)))
        });
        define_host_accessor(vm, obj, "containerQuery", Some(g_cq), None);
    }

    // `CSSScopeRule.start` / `CSSScopeRule.end`
    if proto_name == "CSSScopeRule" {
        let st_sc1 = Rc::clone(&state);
        let p_sc1 = Rc::clone(&path_rc);
        let g_start = vm.alloc_host_fn("get start", move |vm, _, _| {
            let p = p_sc1.borrow();
            let val = match st_sc1.borrow().rule_at(&p) {
                Some(CssomRuleKind::Scope { start: Some(s), .. }) => {
                    JsValue::from_string(vm.heap.alloc_string(s.as_str()))
                }
                _ => JsValue::NULL,
            };
            Ok(val)
        });
        define_host_accessor(vm, obj, "start", Some(g_start), None);

        let st_sc2 = Rc::clone(&state);
        let p_sc2 = Rc::clone(&path_rc);
        let g_end = vm.alloc_host_fn("get end", move |vm, _, _| {
            let p = p_sc2.borrow();
            let val = match st_sc2.borrow().rule_at(&p) {
                Some(CssomRuleKind::Scope { end: Some(e), .. }) => {
                    JsValue::from_string(vm.heap.alloc_string(e.as_str()))
                }
                _ => JsValue::NULL,
            };
            Ok(val)
        });
        define_host_accessor(vm, obj, "end", Some(g_end), None);
    }

    // `CSSLayerStatementRule.nameList`
    if proto_name == "CSSLayerStatementRule" {
        let st_nl = Rc::clone(&state);
        let p_nl = Rc::clone(&path_rc);
        let g_nl = vm.alloc_host_fn("get nameList", move |vm, _, _| {
            let arr = vm.alloc_array();
            let p = p_nl.borrow();
            if let Some(CssomRuleKind::LayerStatement { names }) = st_nl.borrow().rule_at(&p) {
                for n in names {
                    let s = vm.heap.alloc_string(n.as_str());
                    vm.heap.object_push_element(arr, JsValue::from_string(s));
                }
            }
            Ok(JsValue::from_object(arr))
        });
        define_host_accessor(vm, obj, "nameList", Some(g_nl), None);
    }

    // `CSSImportRule` (`href`, `layerName`, `supportsText`, `media`, `styleSheet`)
    if proto_name == "CSSImportRule" {
        let st_i1 = Rc::clone(&state);
        let p_i1 = Rc::clone(&path_rc);
        let g_href = vm.alloc_host_fn("get href", move |vm, _, _| {
            let p = p_i1.borrow();
            let s = match st_i1.borrow().rule_at(&p) {
                Some(CssomRuleKind::Import { url, .. }) => url.clone(),
                _ => String::new(),
            };
            Ok(JsValue::from_string(vm.heap.alloc_string(s)))
        });
        define_host_accessor(vm, obj, "href", Some(g_href), None);

        let st_i2 = Rc::clone(&state);
        let p_i2 = Rc::clone(&path_rc);
        let g_ln = vm.alloc_host_fn("get layerName", move |vm, _, _| {
            let p = p_i2.borrow();
            let v = match st_i2.borrow().rule_at(&p) {
                Some(CssomRuleKind::Import {
                    layer: Some(Some(n)),
                    ..
                }) => JsValue::from_string(vm.heap.alloc_string(n.as_str())),
                Some(CssomRuleKind::Import {
                    layer: Some(None), ..
                }) => JsValue::from_string(vm.heap.alloc_string("")),
                _ => JsValue::NULL,
            };
            Ok(v)
        });
        define_host_accessor(vm, obj, "layerName", Some(g_ln), None);

        let st_i3 = Rc::clone(&state);
        let p_i3 = Rc::clone(&path_rc);
        let g_sup = vm.alloc_host_fn("get supportsText", move |vm, _, _| {
            let p = p_i3.borrow();
            let v = match st_i3.borrow().rule_at(&p) {
                Some(CssomRuleKind::Import {
                    supports: Some(s), ..
                }) => JsValue::from_string(vm.heap.alloc_string(s.as_str())),
                _ => JsValue::NULL,
            };
            Ok(v)
        });
        define_host_accessor(vm, obj, "supportsText", Some(g_sup), None);

        let init_media = {
            let p = path_rc.borrow();
            match state.borrow().rule_at(&p) {
                Some(CssomRuleKind::Import { media, .. }) => parse_media_query_list(media),
                _ => Vec::new(),
            }
        };
        let imp_media_rc = Rc::new(RefCell::new(init_media));
        let imp_media_sync = Rc::clone(&imp_media_rc);
        let st_im = Rc::clone(&state);
        let p_im = Rc::clone(&path_rc);
        let h_im = Rc::clone(host);
        let imp_media_proxy = create_media_list_proxy(
            vm,
            host,
            Rc::clone(&imp_media_rc),
            Some(Rc::new(move || {
                let txt = imp_media_sync.borrow().join(", ");
                {
                    let p = p_im.borrow();
                    if let Some(CssomRuleKind::Import { media, .. }) =
                        st_im.borrow_mut().rule_at_mut(&p)
                    {
                        *media = txt;
                    }
                }
                sync_sheet_to_dom(&h_im, &st_im);
            })),
        );
        let g_med = vm.alloc_host_fn("get media", move |_, _, _| Ok(imp_media_proxy));
        define_host_accessor(vm, obj, "media", Some(g_med), None);

        // `CSSImportRule.styleSheet` (shares the exact same `MediaList` object!)
        let imp_href_raw = {
            let p = path_rc.borrow();
            match state.borrow().rule_at(&p) {
                Some(CssomRuleKind::Import { url, .. }) => url.clone(),
                _ => String::new(),
            }
        };
        let resolved_href = url::Url::parse(&host.borrow().main_document_url)
            .ok()
            .and_then(|b| b.join(&imp_href_raw).ok())
            .map(|u| u.to_string())
            .unwrap_or_else(|| imp_href_raw.clone());
        let mut child_sheet = CssStyleSheetState::from_css("", None, false);
        child_sheet.href = Some(resolved_href);
        child_sheet.parent_sheet = Some(parent_sheet);
        child_sheet.owner_rule = Some(obj);
        child_sheet.media = Rc::clone(&imp_media_rc);
        child_sheet.media_proxy = Some(imp_media_proxy);
        let child_sheet_obj = wrap_stylesheet(vm, host, Rc::new(RefCell::new(child_sheet)), None);
        let g_ss = vm.alloc_host_fn("get styleSheet", move |_, _, _| {
            Ok(JsValue::from_object(child_sheet_obj))
        });
        define_host_accessor(vm, obj, "styleSheet", Some(g_ss), None);
    }

    // `CSSMediaRule.media` (live `MediaList` synced to rule's `condition_text`)
    if proto_name == "CSSMediaRule" {
        let init_media = {
            let p = path_rc.borrow();
            match state.borrow().rule_at(&p) {
                Some(CssomRuleKind::Media { condition_text, .. }) => {
                    parse_media_query_list(condition_text)
                }
                _ => Vec::new(),
            }
        };
        let mr_media_rc = Rc::new(RefCell::new(init_media));
        let mr_media_sync = Rc::clone(&mr_media_rc);
        let st_mr = Rc::clone(&state);
        let p_mr = Rc::clone(&path_rc);
        let h_mr = Rc::clone(host);
        let mr_media_proxy = create_media_list_proxy(
            vm,
            host,
            Rc::clone(&mr_media_rc),
            Some(Rc::new(move || {
                let txt = mr_media_sync.borrow().join(", ");
                {
                    let p = p_mr.borrow();
                    if let Some(CssomRuleKind::Media { condition_text, .. }) =
                        st_mr.borrow_mut().rule_at_mut(&p)
                    {
                        *condition_text = txt;
                    }
                }
                sync_sheet_to_dom(&h_mr, &st_mr);
            })),
        );
        let g_med = vm.alloc_host_fn("get media", move |_, _, _| Ok(mr_media_proxy));
        let st_mr2 = Rc::clone(&state);
        let p_mr2 = Rc::clone(&path_rc);
        let h_mr2 = Rc::clone(host);
        let s_med = vm.alloc_host_fn("set media", move |vm, _, args| {
            let v = arg(args, 0);
            let raw = if v.is_null() {
                String::new()
            } else {
                vm.value_to_rust_string(v)
            };
            let list = parse_media_query_list(&raw);
            let txt = list.join(", ");
            *mr_media_rc.borrow_mut() = list;
            {
                let p = p_mr2.borrow();
                if let Some(CssomRuleKind::Media { condition_text, .. }) =
                    st_mr2.borrow_mut().rule_at_mut(&p)
                {
                    *condition_text = txt;
                }
            }
            sync_sheet_to_dom(&h_mr2, &st_mr2);
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, obj, "media", Some(g_med), Some(s_med));
    }

    // `conditionText` / `name` / `namespaceURI` / `prefix`
    let st_cond = Rc::clone(&state);
    let p_cond = Rc::clone(&path_rc);
    let g_cond = vm.alloc_host_fn("get conditionText", move |vm, _, _| {
        let p = p_cond.borrow();
        let txt = match st_cond.borrow().rule_at(&p) {
            Some(
                CssomRuleKind::Media { condition_text, .. }
                | CssomRuleKind::Supports { condition_text, .. }
                | CssomRuleKind::Container { condition_text, .. },
            ) => condition_text.clone(),
            _ => String::new(),
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(txt)))
    });
    define_host_accessor(vm, obj, "conditionText", Some(g_cond), None);

    let st_name = Rc::clone(&state);
    let p_name = Rc::clone(&path_rc);
    let g_name = vm.alloc_host_fn("get name", move |vm, _, _| {
        let p = p_name.borrow();
        let txt = match st_name.borrow().rule_at(&p) {
            Some(
                CssomRuleKind::LayerBlock { name, .. }
                | CssomRuleKind::Keyframes { name, .. }
                | CssomRuleKind::Property { name, .. },
            ) => name.clone(),
            _ => String::new(),
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(txt)))
    });
    let s_name_opt = if proto_name == "CSSKeyframesRule" {
        let st_ns2 = Rc::clone(&state);
        let h_ns2 = Rc::clone(host);
        let p_ns2 = Rc::clone(&path_rc);
        Some(vm.alloc_host_fn("set name", move |vm, _, args| {
            let new_name = vm.value_to_rust_string(arg(args, 0));
            {
                let p = p_ns2.borrow();
                if let Some(CssomRuleKind::Keyframes { name, .. }) =
                    st_ns2.borrow_mut().rule_at_mut(&p)
                {
                    *name = new_name;
                }
            }
            sync_sheet_to_dom(&h_ns2, &st_ns2);
            Ok(JsValue::UNDEFINED)
        }))
    } else {
        None
    };
    define_host_accessor(vm, obj, "name", Some(g_name), s_name_opt);

    let st_ns = Rc::clone(&state);
    let p_ns = Rc::clone(&path_rc);
    let g_ns = vm.alloc_host_fn("get namespaceURI", move |vm, _, _| {
        let p = p_ns.borrow();
        let txt = match st_ns.borrow().rule_at(&p) {
            Some(CssomRuleKind::Namespace { url, .. }) => url.clone(),
            _ => String::new(),
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(txt)))
    });
    define_host_accessor(vm, obj, "namespaceURI", Some(g_ns), None);

    let st_pref = Rc::clone(&state);
    let p_pref = Rc::clone(&path_rc);
    let g_pref = vm.alloc_host_fn("get prefix", move |vm, _, _| {
        let p = p_pref.borrow();
        let txt = match st_pref.borrow().rule_at(&p) {
            Some(CssomRuleKind::Namespace { prefix, .. }) => prefix.clone().unwrap_or_default(),
            _ => String::new(),
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(txt)))
    });
    define_host_accessor(vm, obj, "prefix", Some(g_pref), None);

    // `CSSKeyframesRule`: live `cssRules`, `length`, `appendRule`, `deleteRule`, `findRule`, and indexed Proxy!
    let final_handle = if proto_name == "CSSKeyframesRule" {
        let init_path = path_rc.borrow().clone();
        let live_kf_rules = create_live_css_rule_list(
            vm,
            host,
            Rc::clone(&state),
            parent_sheet,
            obj,
            Some(obj),
            init_path,
        );
        let k_cr = vm.heap.intern_key("cssRules");
        vm.heap.object_define_data(
            obj,
            k_cr,
            live_kf_rules,
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );

        let st_len = Rc::clone(&state);
        let p_len = Rc::clone(&path_rc);
        let g_len = vm.alloc_host_fn("get length", move |_, _, _| {
            let p = p_len.borrow();
            let len = st_len
                .borrow()
                .rule_list(&p)
                .map_or(0, <[CssomRuleKind]>::len);
            Ok(JsValue::from_i32(len as i32))
        });
        define_host_accessor(vm, obj, "length", Some(g_len), None);

        let st_app = Rc::clone(&state);
        let h_app = Rc::clone(host);
        let p_app = Rc::clone(&path_rc);
        define_host_method(vm, obj, "appendRule", move |vm, _, args| {
            let raw = vm.value_to_rust_string(arg(args, 0));
            if let Some(kf_rule) = parse_single_keyframe_rule(&raw) {
                {
                    let p = p_app.borrow();
                    if let Some(list) = st_app.borrow_mut().rule_list_mut(&p) {
                        list.push(kf_rule);
                    }
                }
                sync_sheet_to_dom(&h_app, &st_app);
            }
            Ok(JsValue::UNDEFINED)
        });

        let st_find = Rc::clone(&state);
        let h_find = Rc::clone(host);
        let p_find = Rc::clone(&path_rc);
        define_host_method(vm, obj, "findRule", move |vm, _, args| {
            let raw = vm.value_to_rust_string(arg(args, 0));
            let Some(target_key) = parse_and_serialize_keyframe_selector(&raw) else {
                return Ok(JsValue::NULL);
            };
            let found_idx = {
                let p = p_find.borrow();
                st_find.borrow().rule_list(&p).and_then(|list| {
                    list.iter().rposition(|r| {
                        matches!(r, CssomRuleKind::Keyframe { key_text, .. } if key_text == &target_key)
                    })
                })
            };
            let Some(idx) = found_idx else {
                return Ok(JsValue::NULL);
            };
            let mut child_path = p_find.borrow().clone();
            child_path.push(idx);
            Ok(JsValue::from_object(wrap_css_rule(
                vm,
                &h_find,
                Rc::clone(&st_find),
                child_path,
                parent_sheet,
                obj,
                Some(obj),
            )))
        });

        let st_del = Rc::clone(&state);
        let h_del = Rc::clone(host);
        let p_del = Rc::clone(&path_rc);
        define_host_method(vm, obj, "deleteRule", move |vm, _, args| {
            let raw = vm.value_to_rust_string(arg(args, 0));
            let Some(target_key) = parse_and_serialize_keyframe_selector(&raw) else {
                return Ok(JsValue::UNDEFINED);
            };
            let removed = {
                let p = p_del.borrow();
                let mut st = st_del.borrow_mut();
                if let Some(list) = st.rule_list_mut(&p) {
                    let old_len = list.len();
                    if let Some(idx) = list.iter().rposition(|r| {
                        matches!(r, CssomRuleKind::Keyframe { key_text, .. } if key_text == &target_key)
                    }) {
                        list.remove(idx);
                        Some((idx, old_len))
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
            if let Some((idx, old_len)) = removed {
                shift_child_rule_cache_on_delete(vm, obj, idx, old_len);
                sync_sheet_to_dom(&h_del, &st_del);
            }
            Ok(JsValue::UNDEFINED)
        });

        // Wrap in an indexed Proxy so `keyframesRule[0]`, `keyframesRule[1]` work per CSS Animations L1!
        let handler = vm.alloc_ordinary_object();
        let st_kg = Rc::clone(&state);
        let h_kg = Rc::clone(host);
        let p_kg = Rc::clone(&path_rc);
        define_host_method(vm, handler, "get", move |vm, _, args| {
            let key_val = arg(args, 1);
            if key_val.is_symbol() {
                return forward_symbol_get(vm, arg(args, 0), key_val);
            }
            let prop = vm.value_to_rust_string(key_val);
            if let Ok(idx) = prop.parse::<usize>() {
                let len = {
                    let p = p_kg.borrow();
                    st_kg
                        .borrow()
                        .rule_list(&p)
                        .map_or(0, <[CssomRuleKind]>::len)
                };
                if idx < len {
                    let mut child_path = p_kg.borrow().clone();
                    child_path.push(idx);
                    return Ok(JsValue::from_object(wrap_css_rule(
                        vm,
                        &h_kg,
                        Rc::clone(&st_kg),
                        child_path,
                        parent_sheet,
                        obj,
                        Some(obj),
                    )));
                }
                return Ok(JsValue::UNDEFINED);
            }
            let k = vm.heap.intern_key(&prop);
            vm.get_property_by_key(obj, k)
        });
        define_host_method(vm, handler, "set", move |vm, _, args| {
            let prop = vm.value_to_rust_string(arg(args, 1));
            let k = vm.heap.intern_key(&prop);
            vm.set_property_by_key(obj, k, arg(args, 2))?;
            Ok(JsValue::TRUE)
        });
        vm.heap.alloc_object(JsObject::with_kind(
            ObjectKind::Proxy(rix_js::ProxyObject {
                target: obj,
                handler,
                revoked: false,
            }),
            Some(proto),
        ))
    } else {
        obj
    };

    vm.heap.object_define_data(
        cache,
        cache_key,
        JsValue::from_object(final_handle),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
    final_handle
}

fn create_page_descriptors_obj(
    vm: &mut JsVm,
    host: &SharedHost,
    state: SharedStyleSheet,
    rule_path: Rc<RefCell<Vec<usize>>>,
) -> JsValue {
    let proto = host
        .borrow()
        .css_rule_protos
        .get("CSSPageDescriptors")
        .copied()
        .unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));

    let st1 = Rc::clone(&state);
    let p1 = Rc::clone(&rule_path);
    define_host_method(vm, obj, "getPropertyValue", move |vm, _, args| {
        let prop = normalize_css_prop_name(&vm.value_to_rust_string(arg(args, 0)));
        let p = p1.borrow();
        let raw = st1
            .borrow()
            .rule_at(&p)
            .and_then(CssomRuleKind::declarations_text)
            .unwrap_or("")
            .to_string();
        let decls = parse_style_decls_with_priority(&raw);
        let val = lookup_decl_or_shorthand(&decls, &prop);
        Ok(JsValue::from_string(vm.heap.alloc_string(val)))
    });

    let st2 = Rc::clone(&state);
    let h2 = Rc::clone(host);
    let p2 = Rc::clone(&rule_path);
    define_host_method(vm, obj, "setProperty", move |vm, _, args| {
        let prop = normalize_css_prop_name(&vm.value_to_rust_string(arg(args, 0)));
        let raw_val = if args.len() < 2 || arg(args, 1).is_null() {
            String::new()
        } else {
            vm.value_to_rust_string(arg(args, 1))
        };
        if raw_val.trim().is_empty() {
            let p = p2.borrow();
            let cur = st2
                .borrow()
                .rule_at(&p)
                .and_then(CssomRuleKind::declarations_text)
                .unwrap_or("")
                .to_string();
            let mut decls = parse_style_decls_with_priority(&cur);
            let _ = remove_decl_or_shorthand(&mut decls, &prop);
            if let Some(dt) = st2
                .borrow_mut()
                .rule_at_mut(&p)
                .and_then(CssomRuleKind::declarations_text_mut)
            {
                *dt = serialize_style_decls_with_priority(&decls);
            }
            drop(p);
            sync_sheet_to_dom(&h2, &st2);
            return Ok(JsValue::UNDEFINED);
        }
        let Some(canonical) = validate_and_serialize_page_descriptor(&prop, &raw_val) else {
            return Ok(JsValue::UNDEFINED);
        };
        {
            let p = p2.borrow();
            let cur = st2
                .borrow()
                .rule_at(&p)
                .and_then(CssomRuleKind::declarations_text)
                .unwrap_or("")
                .to_string();
            let mut decls = parse_style_decls_with_priority(&cur);
            if prop == "margin" {
                if let Some(exp) = parse_and_expand_declaration("margin", &raw_val) {
                    for (lk, lv) in exp {
                        upsert_decl(&mut decls, lk, lv, false, false);
                    }
                }
            } else {
                upsert_decl(&mut decls, prop, canonical, false, false);
            }
            if let Some(dt) = st2
                .borrow_mut()
                .rule_at_mut(&p)
                .and_then(CssomRuleKind::declarations_text_mut)
            {
                *dt = serialize_style_decls_with_priority(&decls);
            }
        }
        sync_sheet_to_dom(&h2, &st2);
        Ok(JsValue::UNDEFINED)
    });

    let st3 = Rc::clone(&state);
    let h3 = Rc::clone(host);
    let p3 = Rc::clone(&rule_path);
    define_host_method(vm, obj, "removeProperty", move |vm, _, args| {
        let prop = normalize_css_prop_name(&vm.value_to_rust_string(arg(args, 0)));
        let old = {
            let p = p3.borrow();
            let cur = st3
                .borrow()
                .rule_at(&p)
                .and_then(CssomRuleKind::declarations_text)
                .unwrap_or("")
                .to_string();
            let mut decls = parse_style_decls_with_priority(&cur);
            let old = remove_decl_or_shorthand(&mut decls, &prop);
            if let Some(dt) = st3
                .borrow_mut()
                .rule_at_mut(&p)
                .and_then(CssomRuleKind::declarations_text_mut)
            {
                *dt = serialize_style_decls_with_priority(&decls);
            }
            old
        };
        sync_sheet_to_dom(&h3, &st3);
        Ok(JsValue::from_string(vm.heap.alloc_string(old)))
    });

    JsValue::from_object(obj)
}

fn alloc_style_decl_target(vm: &mut JsVm, host: &SharedHost) -> HeapHandle {
    let proto = host
        .borrow()
        .css_rule_protos
        .get("CSSStyleDeclaration")
        .copied()
        .unwrap_or(vm.object_proto);
    let target = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    attach_array_like_iterator(vm, target);
    target
}

fn create_rule_style_proxy(
    vm: &mut JsVm,
    host: &SharedHost,
    state: SharedStyleSheet,
    rule_path: Rc<RefCell<Vec<usize>>>,
) -> JsValue {
    let target = alloc_style_decl_target(vm, host);
    let st1 = Rc::clone(&state);
    let p1 = Rc::clone(&rule_path);
    define_host_method(vm, target, "getPropertyValue", move |vm, _, args| {
        let prop = normalize_css_prop_name(&vm.value_to_rust_string(arg(args, 0)));
        let p = p1.borrow();
        let raw = st1
            .borrow()
            .rule_at(&p)
            .and_then(CssomRuleKind::declarations_text)
            .unwrap_or("")
            .to_string();
        let decls = parse_style_decls_with_priority(&raw);
        let val = lookup_decl_or_shorthand(&decls, &prop);
        Ok(JsValue::from_string(vm.heap.alloc_string(val)))
    });
    let st_prio = Rc::clone(&state);
    let p_prio = Rc::clone(&rule_path);
    define_host_method(vm, target, "getPropertyPriority", move |vm, _, args| {
        let prop = normalize_css_prop_name(&vm.value_to_rust_string(arg(args, 0)));
        let p = p_prio.borrow();
        let raw = st_prio
            .borrow()
            .rule_at(&p)
            .and_then(CssomRuleKind::declarations_text)
            .unwrap_or("")
            .to_string();
        let decls = parse_style_decls_with_priority(&raw);
        let imp = decls
            .iter()
            .find(|(k, _, _)| k == &prop)
            .is_some_and(|(_, _, i)| *i);
        let s = if imp { "important" } else { "" };
        Ok(JsValue::from_string(vm.heap.alloc_string(s)))
    });
    let st_item = Rc::clone(&state);
    let p_item = Rc::clone(&rule_path);
    define_host_method(vm, target, "item", move |vm, _, args| {
        let idx = vm.to_number(arg(args, 0)).unwrap_or(-1.0);
        if idx < 0.0 || !idx.is_finite() {
            return Ok(JsValue::from_string(vm.heap.alloc_string("")));
        }
        let p = p_item.borrow();
        let raw = st_item
            .borrow()
            .rule_at(&p)
            .and_then(CssomRuleKind::declarations_text)
            .unwrap_or("")
            .to_string();
        let decls = parse_style_decls_with_priority(&raw);
        let name = decls
            .get(idx as usize)
            .map(|(k, _, _)| k.as_str())
            .unwrap_or("");
        Ok(JsValue::from_string(vm.heap.alloc_string(name)))
    });
    let st2 = Rc::clone(&state);
    let h2 = Rc::clone(host);
    let p2 = Rc::clone(&rule_path);
    define_host_method(vm, target, "setProperty", move |vm, _, args| {
        let prop = normalize_css_prop_name(&vm.value_to_rust_string(arg(args, 0)));
        let raw_val = if args.len() < 2 || arg(args, 1).is_null() {
            String::new()
        } else {
            vm.value_to_rust_string(arg(args, 1))
        };
        let priority = if args.len() >= 3 && !arg(args, 2).is_undefined() && !arg(args, 2).is_null()
        {
            vm.value_to_rust_string(arg(args, 2))
        } else {
            String::new()
        };
        let important = if priority.is_empty() {
            false
        } else if priority.eq_ignore_ascii_case("important") {
            true
        } else {
            return Ok(JsValue::UNDEFINED);
        };
        let p = p2.borrow();
        let Some(cur) = st2
            .borrow()
            .rule_at(&p)
            .and_then(CssomRuleKind::declarations_text)
            .map(str::to_string)
        else {
            return Ok(JsValue::UNDEFINED);
        };
        let mut decls = parse_style_decls_with_priority(&cur);
        if !set_decl_expanded(&mut decls, &prop, &raw_val, important, false) {
            return Ok(JsValue::UNDEFINED);
        }
        if let Some(declarations_text) = st2
            .borrow_mut()
            .rule_at_mut(&p)
            .and_then(CssomRuleKind::declarations_text_mut)
        {
            *declarations_text = serialize_style_decls_with_priority(&decls);
        }
        drop(p);
        sync_sheet_to_dom(&h2, &st2);
        Ok(JsValue::UNDEFINED)
    });
    let st3 = Rc::clone(&state);
    let h3 = Rc::clone(host);
    let p3 = Rc::clone(&rule_path);
    define_host_method(vm, target, "removeProperty", move |vm, _, args| {
        let prop = normalize_css_prop_name(&vm.value_to_rust_string(arg(args, 0)));
        let p = p3.borrow();
        let cur = st3
            .borrow()
            .rule_at(&p)
            .and_then(CssomRuleKind::declarations_text)
            .unwrap_or("")
            .to_string();
        let mut decls = parse_style_decls_with_priority(&cur);
        let old = remove_decl_or_shorthand(&mut decls, &prop);
        if let Some(declarations_text) = st3
            .borrow_mut()
            .rule_at_mut(&p)
            .and_then(CssomRuleKind::declarations_text_mut)
        {
            *declarations_text = serialize_style_decls_with_priority(&decls);
        }
        drop(p);
        sync_sheet_to_dom(&h3, &st3);
        Ok(JsValue::from_string(vm.heap.alloc_string(old)))
    });

    let handler = vm.alloc_ordinary_object();
    let st_get = Rc::clone(&state);
    let p_get = Rc::clone(&rule_path);
    define_host_method(vm, handler, "get", move |vm, _, args| {
        let key_val = arg(args, 1);
        if key_val.is_symbol() {
            return forward_symbol_get(vm, arg(args, 0), key_val);
        }
        let prop = vm.value_to_rust_string(key_val);
        let k = vm.heap.intern_key(&prop);
        if let Some(t_h) = arg(args, 0).as_object() {
            if vm.heap.object_get_own(t_h, k, &vm.shapes).is_some() {
                return vm.get_property_by_key(t_h, k);
            }
        }
        let p = p_get.borrow();
        let raw = st_get
            .borrow()
            .rule_at(&p)
            .and_then(CssomRuleKind::declarations_text)
            .unwrap_or("")
            .to_string();
        let decls = parse_style_decls_with_priority(&raw);
        if prop == "cssText" {
            let txt = serialize_style_decls_with_priority(&decls);
            return Ok(JsValue::from_string(vm.heap.alloc_string(txt)));
        }
        if prop == "length" {
            return Ok(JsValue::from_i32(decls.len() as i32));
        }
        if let Ok(idx) = prop.parse::<usize>() {
            return Ok(decls.get(idx).map_or(JsValue::UNDEFINED, |(name, _, _)| {
                JsValue::from_string(vm.heap.alloc_string(name))
            }));
        }
        let css_key = camel_to_kebab(&prop);
        let val = lookup_decl_or_shorthand(&decls, &css_key);
        Ok(JsValue::from_string(vm.heap.alloc_string(val)))
    });
    let st_set = Rc::clone(&state);
    let h_set = Rc::clone(host);
    let p_set = rule_path;
    define_host_method(vm, handler, "set", move |vm, _, args| {
        let prop = vm.value_to_rust_string(arg(args, 1));
        let raw_val = vm.value_to_rust_string(arg(args, 2));
        if prop == "cssText" {
            let validated = canonicalize_css_decls(&raw_val);
            {
                let p = p_set.borrow();
                if let Some(declarations_text) = st_set
                    .borrow_mut()
                    .rule_at_mut(&p)
                    .and_then(CssomRuleKind::declarations_text_mut)
                {
                    *declarations_text = serialize_style_decls_with_priority(&validated);
                }
            }
            sync_sheet_to_dom(&h_set, &st_set);
            return Ok(JsValue::TRUE);
        }
        let css_key = camel_to_kebab(&prop);
        let p = p_set.borrow();
        let Some(cur) = st_set
            .borrow()
            .rule_at(&p)
            .and_then(CssomRuleKind::declarations_text)
            .map(str::to_string)
        else {
            return Ok(JsValue::TRUE);
        };
        let mut decls = parse_style_decls_with_priority(&cur);
        if !set_decl_expanded(&mut decls, &css_key, &raw_val, false, false) {
            return Ok(JsValue::TRUE);
        }
        if let Some(declarations_text) = st_set
            .borrow_mut()
            .rule_at_mut(&p)
            .and_then(CssomRuleKind::declarations_text_mut)
        {
            *declarations_text = serialize_style_decls_with_priority(&decls);
        }
        drop(p);
        sync_sheet_to_dom(&h_set, &st_set);
        Ok(JsValue::TRUE)
    });

    let proxy = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Proxy(rix_js::ProxyObject {
            target,
            handler,
            revoked: false,
        }),
        Some(vm.object_proto),
    ));
    JsValue::from_object(proxy)
}

// ---------------------------------------------------------------------------
// `<style>.sheet`, `document.styleSheets`, `document.adoptedStyleSheets`
// ---------------------------------------------------------------------------

fn get_or_create_style_element_sheet(host: &SharedHost, nid: NodeId) -> Option<SharedStyleSheet> {
    let (is_style, is_link_sheet, is_connected, css_text, title_attr, media_attr, href_attr) = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let doc = doc_rc.borrow();
        let Some(NodeData::Element(el)) = doc.get(nid) else {
            return None;
        };
        let is_style = el.name.local.as_str().eq_ignore_ascii_case("style");
        let is_link = el.name.local.as_str().eq_ignore_ascii_case("link")
            && el.get_attribute("rel").is_some_and(|r| {
                r.split_ascii_whitespace()
                    .any(|tok| tok.eq_ignore_ascii_case("stylesheet"))
            })
            && el.get_attribute("href").is_some_and(|h| !h.is_empty());
        if !is_style && !is_link {
            return None;
        }
        let connected = doc.is_connected(nid);
        let text = if is_style {
            doc.inner_text(nid)
        } else {
            String::new()
        };
        let title_attr = el
            .get_attribute("title")
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let media_attr = el.get_attribute("media").map(str::to_string);
        let href_attr = el.get_attribute("href").map(str::to_string);
        (
            is_style, is_link, connected, text, title_attr, media_attr, href_attr,
        )
    };
    if !is_connected {
        // Per HTML spec, disconnected `<style>` / `<link>` elements have `sheet === null`.
        return None;
    }
    let resolved_href = if is_link_sheet {
        href_attr.as_deref().map(|raw| {
            url::Url::parse(&host.borrow().main_document_url)
                .ok()
                .and_then(|b| b.join(raw).ok())
                .map(|u| u.to_string())
                .unwrap_or_else(|| raw.to_string())
        })
    } else {
        None
    };
    let effective_css = if is_style {
        css_text
    } else {
        host.borrow()
            .document
            .borrow()
            .all_dynamic_stylesheets()
            .get(&nid)
            .cloned()
            .unwrap_or_default()
    };
    let mut st = host.borrow_mut();
    let sheet = st
        .style_element_sheets
        .entry(nid)
        .or_insert_with(|| {
            let mut s = CssStyleSheetState::from_css(&effective_css, Some(nid), false);
            s.title = title_attr.clone();
            s.href = resolved_href.clone();
            if let Some(ref m) = media_attr {
                *s.media.borrow_mut() = parse_media_query_list(m);
            }
            Rc::new(RefCell::new(s))
        })
        .clone();
    drop(st);
    {
        let mut s = sheet.borrow_mut();
        if is_style {
            s.sync_from_text_if_changed(&effective_css);
        }
        s.title = title_attr;
        if is_link_sheet {
            s.href = resolved_href;
        }
    }
    Some(sheet)
}

fn install_style_element_sheet_accessor(vm: &mut JsVm, host: &SharedHost) {
    let ep = host.borrow().prototypes.element_proto;
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get sheet", move |vm, this_val, _| {
        let nid = require_node(vm, &h, this_val)?;
        let Some(sheet_state) = get_or_create_style_element_sheet(&h, nid) else {
            return Ok(JsValue::NULL);
        };
        Ok(JsValue::from_object(wrap_stylesheet(
            vm,
            &h,
            sheet_state,
            None,
        )))
    });
    define_host_accessor(vm, ep, "sheet", Some(g), None);
}

/// Syncs a JS array of `CSSStyleSheet` wrappers into either `Document.adoptedStyleSheets`
/// (`shadow_root == None`) or `ShadowRoot.adoptedStyleSheets` (`Some(sr_id)`).
fn sync_adopted_array_to_host(
    vm: &mut JsVm,
    host: &SharedHost,
    arr_h: HeapHandle,
    shadow_root: Option<NodeId>,
) -> Result<(), rix_js::JsException> {
    let k_len = vm.heap.intern_key("length");
    let len = vm
        .get_property_by_key(arr_h, k_len)
        .ok()
        .and_then(|v| vm.to_number(v).ok())
        .unwrap_or(0.0)
        .max(0.0) as usize;
    let mut new_sheets = Vec::with_capacity(len);
    for i in 0..len {
        let ki = vm.heap.intern_key(&i.to_string());
        let v = vm.get_property_by_key(arr_h, ki)?;
        let Some(sh_h) = v.as_object() else {
            return Err(vm.type_error("adoptedStyleSheets must contain CSSStyleSheet objects"));
        };
        let Some(st) = host.borrow().stylesheet_handle_map.get(&sh_h).cloned() else {
            return Err(vm.type_error("adoptedStyleSheets must contain CSSStyleSheet objects"));
        };
        if !st.borrow().is_constructed {
            return Err(create_dom_exception(
                vm,
                "NotAllowedError",
                "Cannot adopt a non-constructed CSSStyleSheet",
            ));
        }
        new_sheets.push(st);
    }
    if let Some(sr_id) = shadow_root {
        host.borrow()
            .shadow_dom
            .borrow_mut()
            .adopted_stylesheets
            .insert(sr_id, new_sheets);
        host.borrow_mut().mark_sheets_dirty();
    } else {
        let mut hst = host.borrow_mut();
        hst.adopted_stylesheets = new_sheets;
        hst.mark_sheets_dirty();
    }
    Ok(())
}

/// Creates a live WebIDL `ObservableArray<CSSStyleSheet>` Proxy for `Document` or `ShadowRoot`
/// so `.length = 0`, `.pop()`, `.push()`, `[i] = sheet`, `.splice()`, etc. immediately sync
/// to the CSS cascade.
pub(crate) fn create_adopted_stylesheets_proxy(
    vm: &mut JsVm,
    host: &SharedHost,
    shadow_root: Option<NodeId>,
) -> JsValue {
    let sheets: Vec<SharedStyleSheet> = if let Some(sr_id) = shadow_root {
        host.borrow()
            .shadow_dom
            .borrow()
            .adopted_stylesheets
            .get(&sr_id)
            .cloned()
            .unwrap_or_default()
    } else {
        host.borrow().adopted_stylesheets.clone()
    };
    let target = vm.alloc_array();
    for sh in sheets {
        let obj = wrap_stylesheet(vm, host, sh, None);
        vm.heap
            .object_push_element(target, JsValue::from_object(obj));
    }

    let handler = vm.alloc_ordinary_object();
    let h_set = Rc::clone(host);
    define_host_method(vm, handler, "set", move |vm, _, args| {
        let Some(t_h) = arg(args, 0).as_object() else {
            return Ok(JsValue::FALSE);
        };
        let prop_val = arg(args, 1);
        let new_val = arg(args, 2);
        let prop_str = vm.value_to_rust_string(prop_val);
        if prop_str.parse::<usize>().is_ok() {
            // Validate element is a constructed CSSStyleSheet before mutating backing array!
            let Some(sh_h) = new_val.as_object() else {
                return Err(vm.type_error("adoptedStyleSheets must contain CSSStyleSheet objects"));
            };
            let st_opt = h_set.borrow().stylesheet_handle_map.get(&sh_h).cloned();
            let Some(st) = st_opt else {
                return Err(vm.type_error("adoptedStyleSheets must contain CSSStyleSheet objects"));
            };
            if !st.borrow().is_constructed {
                return Err(create_dom_exception(
                    vm,
                    "NotAllowedError",
                    "Cannot adopt a non-constructed CSSStyleSheet",
                ));
            }
        }
        let k = vm.heap.intern_key(&prop_str);
        vm.set_property_by_key(t_h, k, new_val)?;
        if prop_str == "length" || prop_str.parse::<usize>().is_ok() {
            sync_adopted_array_to_host(vm, &h_set, t_h, shadow_root)?;
        }
        Ok(JsValue::TRUE)
    });

    let h_del = Rc::clone(host);
    define_host_method(vm, handler, "deleteProperty", move |vm, _, args| {
        let Some(t_h) = arg(args, 0).as_object() else {
            return Ok(JsValue::FALSE);
        };
        let prop_str = vm.value_to_rust_string(arg(args, 1));
        let k = vm.heap.intern_key(&prop_str);
        let _ = vm.delete_property_by_key(t_h, k);
        if prop_str.parse::<usize>().is_ok() {
            let _ = sync_adopted_array_to_host(vm, &h_del, t_h, shadow_root);
        }
        Ok(JsValue::TRUE)
    });

    let proxy = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Proxy(rix_js::ProxyObject {
            target,
            handler,
            revoked: false,
        }),
        Some(vm.array_proto),
    ));
    JsValue::from_object(proxy)
}

fn install_document_stylesheets(vm: &mut JsVm, host: &SharedHost) {
    let dp = host.borrow().prototypes.document_proto;

    // `document.styleSheets` & `shadowRoot.styleSheets` (does NOT include `adoptedStyleSheets`)
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get styleSheets", move |vm, this_val, _| {
        let doc_root = crate::dom_bridge::extract_node(vm, &h, this_val)
            .unwrap_or_else(|| h.borrow().document.borrow().root());
        let style_nodes: Vec<NodeId> = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            doc.descendants(doc_root)
                .filter(|&n| {
                    matches!(
                        doc.get(n),
                        Some(NodeData::Element(el))
                            if el.name.local.as_str().eq_ignore_ascii_case("style")
                                || (el.name.local.as_str().eq_ignore_ascii_case("link")
                                    && el.get_attribute("disabled").is_none()
                                    && el.get_attribute("rel").is_some_and(|r| r
                                        .split_ascii_whitespace()
                                        .any(|t| t.eq_ignore_ascii_case("stylesheet")))
                                    && el.get_attribute("href").is_some_and(|hv| !hv.is_empty()))
                    )
                })
                .collect()
        };
        let ssl_proto = h
            .borrow()
            .css_rule_protos
            .get("StyleSheetList")
            .copied()
            .unwrap_or(vm.array_proto);
        let arr = vm.alloc_array();
        vm.heap
            .object_set_prototype(arr, Some(ssl_proto), &mut vm.shapes);
        for nid in style_nodes {
            if let Some(sheet_state) = get_or_create_style_element_sheet(&h, nid) {
                let obj = wrap_stylesheet(vm, &h, sheet_state, None);
                vm.heap.object_push_element(arr, JsValue::from_object(obj));
            }
        }
        define_host_method(vm, arr, "item", |vm, t, args| {
            let idx = vm.to_number(arg(args, 0)).unwrap_or(-1.0);
            if idx < 0.0 || !idx.is_finite() {
                return Ok(JsValue::NULL);
            }
            let Some(th) = t.as_object() else {
                return Ok(JsValue::NULL);
            };
            let k = vm.heap.intern_key(&(idx as usize).to_string());
            match vm.get_property_by_key(th, k) {
                Ok(v) if !v.is_undefined() => Ok(v),
                _ => Ok(JsValue::NULL),
            }
        });
        attach_array_like_iterator(vm, arr);
        Ok(JsValue::from_object(arr))
    });
    define_host_accessor(vm, dp, "styleSheets", Some(g), None);
    let srp = host.borrow().prototypes.shadow_root_proto;
    define_host_accessor(vm, srp, "styleSheets", Some(g), None);

    // `document.adoptedStyleSheets` (live ObservableArray Proxy)
    let h1 = Rc::clone(host);
    let g_ad = vm.alloc_host_fn("get adoptedStyleSheets", move |vm, _, _| {
        Ok(create_adopted_stylesheets_proxy(vm, &h1, None))
    });
    let h2 = Rc::clone(host);
    let s_ad = vm.alloc_host_fn("set adoptedStyleSheets", move |vm, _, args| {
        let items = vm.collect_iterable(arg(args, 0))?;
        let mut new_sheets = Vec::with_capacity(items.len());
        {
            let hb = h2.borrow();
            for item in items {
                let Some(sh_h) = item.as_object() else {
                    return Err(
                        vm.type_error("adoptedStyleSheets must contain CSSStyleSheet objects")
                    );
                };
                let Some(st) = hb.stylesheet_handle_map.get(&sh_h) else {
                    return Err(
                        vm.type_error("adoptedStyleSheets must contain CSSStyleSheet objects")
                    );
                };
                if !st.borrow().is_constructed {
                    return Err(create_dom_exception(
                        vm,
                        "NotAllowedError",
                        "Cannot adopt a non-constructed CSSStyleSheet",
                    ));
                }
                new_sheets.push(Rc::clone(st));
            }
        }
        {
            let mut hst = h2.borrow_mut();
            hst.adopted_stylesheets = new_sheets;
            hst.mark_sheets_dirty();
        }
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, dp, "adoptedStyleSheets", Some(g_ad), Some(s_ad));
}

// ---------------------------------------------------------------------------
// `window.getComputedStyle(element, pseudoElt?)` & full computed serialization
// ---------------------------------------------------------------------------

enum ComputedPseudoTarget {
    Element,
    Pseudo(RixPseudoElement),
    Highlight(String),
    Invalid,
}

fn decode_css_ident_strict(raw: &str) -> Option<String> {
    let s = raw.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0C'));
    if s.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            let mut hex = String::new();
            while hex.len() < 6 && chars.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                hex.push(chars.next().unwrap());
            }
            if !hex.is_empty() {
                if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                    if let Some(decoded) = char::from_u32(cp) {
                        out.push(decoded);
                    }
                }
                if chars.peek().is_some_and(|c| c.is_ascii_whitespace()) {
                    chars.next();
                }
            } else {
                let esc = chars.next()?;
                out.push(esc);
            }
        } else if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || !ch.is_ascii() {
            out.push(ch);
        } else {
            return None;
        }
    }
    if out.is_empty() {
        return None;
    }
    // CSS `<ident>` cannot start with a digit or `--` followed by nothing / `-<digit>`.
    let first = out.chars().next()?;
    if first.is_ascii_digit() {
        return None;
    }
    if first == '-' {
        if let Some(second) = out.chars().nth(1) {
            if second.is_ascii_digit() {
                return None;
            }
        }
    }
    Some(out)
}

fn parse_get_computed_style_pseudo(raw: &str) -> ComputedPseudoTarget {
    if raw.is_empty() || !raw.starts_with(':') {
        // Per CSSOM §4.4.1, arguments not starting with `:` are ignored (treated as element itself).
        return ComputedPseudoTarget::Element;
    }
    if let Some(after_two) = raw.strip_prefix("::") {
        // Functional `::highlight(<ident>)` (EOF may implicitly close `)` per CSS Syntax!).
        if after_two.len() >= 10 && after_two[..10].eq_ignore_ascii_case("highlight(") {
            let rest = &after_two[10..];
            let inner = if let Some(pos) = rest.find(')') {
                // Reject any trailing tokens after `)`.
                if pos + 1 != rest.len() {
                    return ComputedPseudoTarget::Invalid;
                }
                &rest[..pos]
            } else {
                rest
            };
            return match decode_css_ident_strict(inner) {
                Some(ident) => ComputedPseudoTarget::Highlight(ident),
                None => ComputedPseudoTarget::Invalid,
            };
        }
        // Standard non-functional double-colon pseudo-elements (exact match, NO trailing whitespace/tokens!).
        match after_two.to_ascii_lowercase().as_str() {
            "before" => ComputedPseudoTarget::Pseudo(RixPseudoElement::Before),
            "after" => ComputedPseudoTarget::Pseudo(RixPseudoElement::After),
            "marker" => ComputedPseudoTarget::Pseudo(RixPseudoElement::Marker),
            "placeholder" => ComputedPseudoTarget::Pseudo(RixPseudoElement::Placeholder),
            "selection" => ComputedPseudoTarget::Pseudo(RixPseudoElement::Selection),
            "first-line" => ComputedPseudoTarget::Pseudo(RixPseudoElement::FirstLine),
            "first-letter" => ComputedPseudoTarget::Pseudo(RixPseudoElement::FirstLetter),
            "backdrop" => ComputedPseudoTarget::Pseudo(RixPseudoElement::Backdrop),
            "file-selector-button" => {
                ComputedPseudoTarget::Pseudo(RixPseudoElement::FileSelectorButton)
            }
            "details-content" => ComputedPseudoTarget::Pseudo(RixPseudoElement::DetailsContent),
            "cue" => ComputedPseudoTarget::Pseudo(RixPseudoElement::Cue),
            "target-text" => ComputedPseudoTarget::Pseudo(RixPseudoElement::TargetText),
            _ => ComputedPseudoTarget::Invalid,
        }
    } else if let Some(after_one) = raw.strip_prefix(':') {
        // Legacy single-colon CSS2 pseudo-elements ONLY (exact match, NO trailing whitespace/tokens!).
        match after_one.to_ascii_lowercase().as_str() {
            "before" => ComputedPseudoTarget::Pseudo(RixPseudoElement::Before),
            "after" => ComputedPseudoTarget::Pseudo(RixPseudoElement::After),
            "first-line" => ComputedPseudoTarget::Pseudo(RixPseudoElement::FirstLine),
            "first-letter" => ComputedPseudoTarget::Pseudo(RixPseudoElement::FirstLetter),
            _ => ComputedPseudoTarget::Invalid,
        }
    } else {
        ComputedPseudoTarget::Invalid
    }
}

fn create_empty_computed_style_proxy(vm: &mut JsVm, host: &SharedHost) -> JsValue {
    let target = alloc_style_decl_target(vm, host);
    define_host_method(vm, target, "getPropertyValue", move |vm, _, _| {
        Ok(JsValue::from_string(vm.heap.alloc_string("")))
    });
    define_host_method(vm, target, "getPropertyPriority", move |vm, _, _| {
        Ok(JsValue::from_string(vm.heap.alloc_string("")))
    });
    define_host_method(vm, target, "item", move |vm, _, _| {
        Ok(JsValue::from_string(vm.heap.alloc_string("")))
    });
    define_host_method(vm, target, "setProperty", move |vm, _, _| {
        Err(create_dom_exception(
            vm,
            "NoModificationAllowedError",
            "Computed style declaration is read-only",
        ))
    });
    define_host_method(vm, target, "removeProperty", move |vm, _, _| {
        Err(create_dom_exception(
            vm,
            "NoModificationAllowedError",
            "Computed style declaration is read-only",
        ))
    });
    let handler = vm.alloc_ordinary_object();
    define_host_method(vm, handler, "ownKeys", move |vm, _, _| {
        Ok(JsValue::from_object(vm.alloc_array()))
    });
    define_host_method(vm, handler, "get", move |vm, _, args| {
        let key_val = arg(args, 1);
        if key_val.is_symbol() {
            return forward_symbol_get(vm, arg(args, 0), key_val);
        }
        let prop = vm.value_to_rust_string(key_val);
        if prop == "length" {
            return Ok(JsValue::from_i32(0));
        }
        let k = vm.heap.intern_key(&prop);
        if let Some(t_h) = arg(args, 0).as_object() {
            if vm.heap.object_get_own(t_h, k, &vm.shapes).is_some() {
                return vm.get_property_by_key(t_h, k);
            }
        }
        if prop.parse::<usize>().is_ok() {
            return Ok(JsValue::UNDEFINED);
        }
        Ok(JsValue::from_string(vm.heap.alloc_string("")))
    });
    let proxy = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Proxy(rix_js::ProxyObject {
            target,
            handler,
            revoked: false,
        }),
        Some(vm.object_proto),
    ));
    JsValue::from_object(proxy)
}

fn resolve_highlight_property(
    host: &SharedHost,
    nid: NodeId,
    highlight_name: &str,
    css_prop: &str,
) -> Option<String> {
    let (elem_id, elem_tag) = match host.borrow().document.borrow().get(nid) {
        Some(NodeData::Element(e)) => (
            e.get_attribute("id").map(str::to_string),
            Some(e.name.local.as_str().to_ascii_lowercase()),
        ),
        _ => (None, None),
    };
    let needle = format!("::highlight({highlight_name})");
    let css_blocks: Vec<String> = {
        let hb = host.borrow();
        let doc = hb.document.borrow();
        let mut out = Vec::new();
        for n in doc.descendants_with_shadow_roots(doc.root()) {
            if let Some(NodeData::Element(e)) = doc.get(n) {
                if e.name.local.as_str().eq_ignore_ascii_case("style") {
                    out.push(doc.inner_text(n));
                }
            }
        }
        out
    };
    let mut matched_val = None;
    for css in css_blocks {
        // Scan top-level `<selector> { <decls> }` blocks directly so `::highlight(<name>)`
        // rules dropped by `lightningcss` are still resolved per CSS Highlight API L1.
        let mut rest = css.as_str();
        while let Some(open) = rest.find('{') {
            let sel_raw = rest[..open].trim();
            let after_open = &rest[open + 1..];
            let Some(close) = after_open.find('}') else {
                break;
            };
            let body = &after_open[..close];
            rest = &after_open[close + 1..];
            if !sel_raw.contains(&needle) {
                continue;
            }
            for part in sel_raw.split(',') {
                let p = part.trim();
                let Some((prefix_raw, _)) = p.split_once(&needle) else {
                    continue;
                };
                let prefix = prefix_raw.trim();
                let matches = prefix.is_empty()
                    || prefix == "*"
                    || elem_id
                        .as_deref()
                        .is_some_and(|id| prefix == format!("#{id}"))
                    || elem_tag
                        .as_deref()
                        .is_some_and(|t| prefix.eq_ignore_ascii_case(t));
                if matches {
                    let decls = parse_style_decls_with_priority(body);
                    let v = lookup_decl_or_shorthand(&decls, css_prop);
                    if !v.is_empty() {
                        matched_val = Some(v);
                    }
                }
            }
        }
    }
    matched_val
}

/// Returns `true` iff `nid` is connected to a rendered document's flat tree per CSSOM §4.4.1:
/// - Detached elements and shadow trees attached to detached hosts return `false`.
/// - Light-DOM descendants of a shadow host that are not assigned to any `<slot>` return `false`.
/// - Elements inside a sub-document whose owning `<iframe>` has `display: none` or is detached return `false`.
fn is_in_flat_rendered_tree(host: &SharedHost, nid: NodeId) -> bool {
    let main_root = host.borrow().document.borrow().root();
    let mut cur = nid;
    for _ in 0..512 {
        let step = {
            let hb = host.borrow();
            let doc = hb.document.borrow();
            match doc.parent(cur) {
                Some(p) => {
                    // If parent is a shadow host, `cur` must be slotted to participate in the flat tree.
                    if let Some(NodeData::Element(pe)) = doc.get(p) {
                        if let Some(sr_id) = pe.shadow_root {
                            let sdom = hb.shadow_dom.borrow();
                            let in_manual =
                                sdom.manual_assignments.values().any(|v| v.contains(&cur));
                            let slot_name = match doc.get(cur) {
                                Some(NodeData::Element(ce)) => {
                                    ce.get_attribute("slot").unwrap_or("").to_string()
                                }
                                _ => String::new(),
                            };
                            let has_matching_slot = in_manual
                                || doc.descendants(sr_id).any(|dn| {
                                    matches!(
                                        doc.get(dn),
                                        Some(NodeData::Element(se))
                                            if se.name.local.as_str().eq_ignore_ascii_case("slot")
                                                && se.get_attribute("name").unwrap_or("")
                                                    == slot_name
                                    )
                                });
                            if !has_matching_slot {
                                return false;
                            }
                        }
                    }
                    Some(p)
                }
                None => {
                    if cur == main_root {
                        return true;
                    }
                    match doc.get(cur) {
                        Some(NodeData::ShadowRoot { host: sh_host, .. }) => Some(*sh_host),
                        Some(NodeData::Document) => {
                            // Sub-document (e.g. iframe): find owning iframe element.
                            let iframe_opt = doc
                                .descendants_with_shadow_roots(main_root)
                                .into_iter()
                                .find(|&n| {
                                    matches!(
                                        doc.get(n),
                                        Some(NodeData::Element(el)) if el.content_document == Some(cur)
                                    )
                                });
                            let Some(iframe_nid) = iframe_opt else {
                                return false;
                            };
                            drop(doc);
                            drop(hb);
                            if !is_in_flat_rendered_tree(host, iframe_nid) {
                                return false;
                            }
                            let is_none = {
                                let mut hm = host.borrow_mut();
                                let ctx = hm.ensure_styles();
                                ctx.style_for(iframe_nid)
                                    .is_some_and(|s| s.display == DisplayMode::None)
                            };
                            return !is_none;
                        }
                        _ => return false,
                    }
                }
            }
        };
        let Some(next) = step else {
            return false;
        };
        cur = next;
    }
    false
}

fn create_highlight_computed_style_proxy(
    vm: &mut JsVm,
    host: SharedHost,
    nid: NodeId,
    highlight_name: String,
) -> JsValue {
    let base = create_computed_style_proxy(vm, Rc::clone(&host), None, nid, None);
    let Some(base_h) = base.as_object() else {
        return base;
    };
    let handler = vm.alloc_ordinary_object();
    let h_get = Rc::clone(&host);
    let hn_get = highlight_name;
    define_host_method(vm, handler, "get", move |vm, _, args| {
        let key_val = arg(args, 1);
        if key_val.is_symbol() {
            return forward_symbol_get(vm, arg(args, 0), key_val);
        }
        let prop = vm.value_to_rust_string(key_val);
        if !matches!(
            prop.as_str(),
            "length"
                | "item"
                | "getPropertyValue"
                | "getPropertyPriority"
                | "setProperty"
                | "removeProperty"
                | "cssText"
        ) && prop.parse::<usize>().is_err()
        {
            let css_key = camel_to_kebab(&prop);
            if let Some(val) = resolve_highlight_property(&h_get, nid, &hn_get, &css_key) {
                return Ok(JsValue::from_string(vm.heap.alloc_string(val)));
            }
        }
        let k = vm.heap.intern_key(&prop);
        vm.get_property_by_key(base_h, k)
    });
    let proxy = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Proxy(rix_js::ProxyObject {
            target: base_h,
            handler,
            revoked: false,
        }),
        Some(vm.object_proto),
    ));
    JsValue::from_object(proxy)
}

fn install_get_computed_style(
    vm: &mut JsVm,
    host: &SharedHost,
    wa: &crate::web_animations::SharedWebAnimations,
) {
    let h = Rc::clone(host);
    let wa_r = Rc::clone(wa);
    let gcs = vm.alloc_host_fn("getComputedStyle", move |vm, _, args| {
        let nid = require_node(vm, &h, arg(args, 0))?;
        let target = if args.len() >= 2 && !arg(args, 1).is_undefined() && !arg(args, 1).is_null() {
            let raw = vm.value_to_rust_string(arg(args, 1));
            parse_get_computed_style_pseudo(&raw)
        } else {
            ComputedPseudoTarget::Element
        };
        if matches!(target, ComputedPseudoTarget::Invalid) || !is_in_flat_rendered_tree(&h, nid) {
            return Ok(create_empty_computed_style_proxy(vm, &h));
        }
        match target {
            ComputedPseudoTarget::Element => Ok(create_computed_style_proxy(
                vm,
                Rc::clone(&h),
                Some(Rc::clone(&wa_r)),
                nid,
                None,
            )),
            ComputedPseudoTarget::Pseudo(pe) => Ok(create_computed_style_proxy(
                vm,
                Rc::clone(&h),
                Some(Rc::clone(&wa_r)),
                nid,
                Some(pe),
            )),
            ComputedPseudoTarget::Highlight(name) => Ok(create_highlight_computed_style_proxy(
                vm,
                Rc::clone(&h),
                nid,
                name,
            )),
            ComputedPseudoTarget::Invalid => Ok(create_empty_computed_style_proxy(vm, &h)),
        }
    });
    define_global(vm, "getComputedStyle", JsValue::from_object(gcs));
}

fn camel_to_kebab(input: &str) -> String {
    if input.starts_with("--") {
        return input.to_string();
    }
    if input == "cssFloat" {
        return "float".to_string();
    }
    let mut out = String::with_capacity(input.len() + 4);
    for ch in input.chars() {
        if ch.is_ascii_uppercase() {
            out.push('-');
            out.push(ch.to_ascii_lowercase());
        } else {
            out.push(ch);
        }
    }
    out
}

fn canonicalize_legacy_css_prop_alias(lower: &str) -> &str {
    match lower {
        "-webkit-align-content" => "align-content",
        "-webkit-align-items" => "align-items",
        "-webkit-align-self" => "align-self",
        "-webkit-flex" => "flex",
        "-webkit-flex-basis" => "flex-basis",
        "-webkit-flex-direction" => "flex-direction",
        "-webkit-flex-flow" => "flex-flow",
        "-webkit-flex-grow" => "flex-grow",
        "-webkit-flex-shrink" => "flex-shrink",
        "-webkit-flex-wrap" => "flex-wrap",
        "-webkit-justify-content" => "justify-content",
        "-webkit-order" => "order",
        "grid-gap" => "gap",
        "grid-row-gap" => "row-gap",
        "grid-column-gap" => "column-gap",
        other => other,
    }
}

fn normalize_css_prop_name(input: &str) -> String {
    if input.starts_with("--") {
        // CSSOM §6.7.2: custom property names are case-sensitive and must NOT be trimmed
        // (so `setProperty("--foo ", "bar")` is rejected as invalid).
        input.to_string()
    } else {
        let lower = input.trim().to_ascii_lowercase();
        canonicalize_legacy_css_prop_alias(&lower).to_string()
    }
}

fn parse_raw_style_decls(raw: &str) -> Vec<(String, String, bool)> {
    let mut out: Vec<(String, String, bool)> = Vec::new();
    for part in split_css_declarations(raw) {
        if let Some((k, v)) = part.split_once(':') {
            let k_clean = strip_outer_css_comments(k).trim();
            let key = if k_clean.starts_with("--") {
                k_clean.to_string()
            } else {
                k_clean.to_ascii_lowercase()
            };
            if key.is_empty() {
                continue;
            }
            let is_custom = key.starts_with("--");
            let v_clean = strip_outer_css_comments(v);
            let mut val = if is_custom {
                if v_clean.is_empty() || v_clean.bytes().all(|b| b.is_ascii_whitespace()) {
                    " ".to_string()
                } else {
                    v_clean
                        .trim_matches(|c: char| c.is_ascii_whitespace())
                        .to_string()
                }
            } else {
                v_clean.trim().to_string()
            };
            let mut important = false;
            if let Some(stripped) = strip_important(&val) {
                val = if is_custom
                    && (stripped.is_empty() || stripped.bytes().all(|b| b.is_ascii_whitespace()))
                {
                    " ".to_string()
                } else {
                    stripped.trim().to_string()
                };
                important = true;
            }
            if !val.is_empty() || is_custom {
                out.push((key, val, important));
            }
        }
    }
    out
}

fn canonicalize_css_decls(raw: &str) -> Vec<(String, String, bool)> {
    let mut validated: Vec<(String, String, bool)> = Vec::new();
    for (k, v, imp) in parse_raw_style_decls(raw) {
        let _ = set_decl_expanded(&mut validated, &k, &v, imp, true);
    }
    validated
}

fn parse_style_decls_with_priority(raw: &str) -> Vec<(String, String, bool)> {
    // Fast path for already-serialized inline style strings containing pending empty longhands (`margin-right: ;`),
    // while still validating raw HTML `style="..."` attributes cleanly.
    let mut out: Vec<(String, String, bool)> = Vec::new();
    for part in split_css_declarations(raw) {
        if let Some((k, v)) = part.split_once(':') {
            let k_clean = strip_outer_css_comments(k).trim();
            let key = if k_clean.starts_with("--") {
                k_clean.to_string()
            } else {
                k_clean.to_ascii_lowercase()
            };
            if key.is_empty() {
                continue;
            }
            let is_custom = key.starts_with("--");
            let v_clean = strip_outer_css_comments(v);
            let mut important = false;
            let mut val = if is_custom {
                if v_clean.is_empty() || v_clean.bytes().all(|b| b.is_ascii_whitespace()) {
                    " ".to_string()
                } else {
                    v_clean
                        .trim_matches(|c: char| c.is_ascii_whitespace())
                        .to_string()
                }
            } else {
                v_clean.trim().to_string()
            };
            if let Some(stripped) = strip_important(&val) {
                val = if is_custom
                    && (stripped.is_empty() || stripped.bytes().all(|b| b.is_ascii_whitespace()))
                {
                    " ".to_string()
                } else {
                    stripped.trim().to_string()
                };
                important = true;
            }
            if !is_custom && val.is_empty() {
                // Pending-substitution placeholder longhand (`margin-right: ;`)
                upsert_decl(&mut out, key, String::new(), important, true);
                continue;
            }
            let _ = set_decl_expanded(&mut out, &key, &val, important, true);
        }
    }
    out
}

fn strip_important(val: &str) -> Option<String> {
    let trimmed = val.trim_end();
    if trimmed.len() >= 9 {
        let lower = trimmed.to_ascii_lowercase();
        if let Some(idx) = lower.rfind('!') {
            if lower[idx + 1..].trim() == "important" {
                return Some(trimmed[..idx].trim_end().to_string());
            }
        }
    }
    None
}

fn split_css_declarations(raw: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = raw.as_bytes();
    let mut start = 0usize;
    let mut depth = 0usize;
    let mut in_str: Option<u8> = None;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = in_str {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        match b {
            b'\\' => {
                i += 2;
                continue;
            }
            b'"' | b'\'' => in_str = Some(b),
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth = depth.saturating_sub(1),
            b';' if depth == 0 => {
                out.push(&raw[start..i]);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < raw.len() {
        out.push(&raw[start..]);
    }
    out
}

fn is_affected_by_all_shorthand(prop: &str) -> bool {
    !prop.starts_with("--") && prop != "direction" && prop != "unicode-bidi" && prop != "all"
}

fn serialize_style_decls_with_priority(decls: &[(String, String, bool)]) -> String {
    const GRID_ALL: [&str; 6] = [
        "grid-template-rows",
        "grid-template-columns",
        "grid-template-areas",
        "grid-auto-rows",
        "grid-auto-columns",
        "grid-auto-flow",
    ];
    const GRID_TMPL: [&str; 3] = [
        "grid-template-rows",
        "grid-template-columns",
        "grid-template-areas",
    ];
    const BORDER_ALL_13: [&str; 13] = [
        "border-top-width",
        "border-right-width",
        "border-bottom-width",
        "border-left-width",
        "border-top-style",
        "border-right-style",
        "border-bottom-style",
        "border-left-style",
        "border-top-color",
        "border-right-color",
        "border-bottom-color",
        "border-left-color",
        "border-image",
    ];
    const BORDER_WIDTH_ALL: [&str; 4] = [
        "border-top-width",
        "border-right-width",
        "border-bottom-width",
        "border-left-width",
    ];
    const BORDER_STYLE_ALL: [&str; 4] = [
        "border-top-style",
        "border-right-style",
        "border-bottom-style",
        "border-left-style",
    ];
    const BORDER_COLOR_ALL: [&str; 4] = [
        "border-top-color",
        "border-right-color",
        "border-bottom-color",
        "border-left-color",
    ];
    const BORDER_SIDES: [(&str, [&str; 3]); 4] = [
        (
            "border-top",
            ["border-top-width", "border-top-style", "border-top-color"],
        ),
        (
            "border-right",
            [
                "border-right-width",
                "border-right-style",
                "border-right-color",
            ],
        ),
        (
            "border-bottom",
            [
                "border-bottom-width",
                "border-bottom-style",
                "border-bottom-color",
            ],
        ),
        (
            "border-left",
            [
                "border-left-width",
                "border-left-style",
                "border-left-color",
            ],
        ),
    ];
    const MARGIN_ALL: [&str; 4] = ["margin-top", "margin-right", "margin-bottom", "margin-left"];
    const PADDING_ALL: [&str; 4] = [
        "padding-top",
        "padding-right",
        "padding-bottom",
        "padding-left",
    ];
    const OVERFLOW_ALL: [&str; 2] = ["overflow-x", "overflow-y"];
    const OUTLINE_ALL: [&str; 3] = ["outline-width", "outline-style", "outline-color"];
    const LIST_STYLE_ALL: [&str; 3] =
        ["list-style-position", "list-style-image", "list-style-type"];
    const FLEX_ALL: [&str; 3] = ["flex-grow", "flex-shrink", "flex-basis"];

    let get_prio = |name: &str| -> Option<bool> {
        decls.iter().find(|(k, _, _)| k == name).map(|(_, _, i)| *i)
    };
    let quad_sh = |names: [&str; 4], sh_name: &str| -> Option<(String, bool)> {
        let v = lookup_decl_or_shorthand(decls, sh_name);
        if v.is_empty() {
            return None;
        }
        let p = get_prio(names[0])?;
        if names.iter().all(|n| get_prio(n) == Some(p)) {
            Some((v, p))
        } else {
            None
        }
    };

    let grid_sh = lookup_decl_or_shorthand(decls, "grid");
    let grid_tmpl_sh = if grid_sh.is_empty() {
        lookup_decl_or_shorthand(decls, "grid-template")
    } else {
        String::new()
    };
    let border_sh: Option<(String, bool)> = {
        let v = lookup_decl_or_shorthand(decls, "border");
        if !v.is_empty() {
            get_prio("border-top-width").and_then(|p| {
                BORDER_ALL_13
                    .iter()
                    .all(|n| get_prio(n) == Some(p))
                    .then_some((v, p))
            })
        } else {
            None
        }
    };
    let bw_sh = if border_sh.is_none() {
        quad_sh(BORDER_WIDTH_ALL, "border-width")
    } else {
        None
    };
    let bs_sh = if border_sh.is_none() {
        quad_sh(BORDER_STYLE_ALL, "border-style")
    } else {
        None
    };
    let bc_sh = if border_sh.is_none() {
        quad_sh(BORDER_COLOR_ALL, "border-color")
    } else {
        None
    };
    let use_per_side_border =
        border_sh.is_none() && bw_sh.is_none() && bs_sh.is_none() && bc_sh.is_none();
    let side_sh: [Option<(String, bool)>; 4] = if use_per_side_border {
        BORDER_SIDES.map(|(sh, lhs)| {
            let v = lookup_decl_or_shorthand(decls, sh);
            if v.is_empty() {
                None
            } else {
                get_prio(lhs[0])
                    .and_then(|p| lhs.iter().all(|n| get_prio(n) == Some(p)).then_some((v, p)))
            }
        })
    } else {
        [None, None, None, None]
    };
    let margin_sh = quad_sh(MARGIN_ALL, "margin");
    let padding_sh = quad_sh(PADDING_ALL, "padding");
    let overflow_sh: Option<(String, bool)> = {
        let v = lookup_decl_or_shorthand(decls, "overflow");
        if !v.is_empty() {
            get_prio("overflow-x")
                .filter(|&p| get_prio("overflow-y") == Some(p))
                .map(|p| (v, p))
        } else {
            None
        }
    };
    let outline_sh: Option<(String, bool)> = {
        let v = lookup_decl_or_shorthand(decls, "outline");
        if !v.is_empty() {
            get_prio("outline-width")
                .filter(|&p| OUTLINE_ALL.iter().all(|n| get_prio(n) == Some(p)))
                .map(|p| (v, p))
        } else {
            None
        }
    };
    let list_style_sh: Option<(String, bool)> = {
        let v = lookup_decl_or_shorthand(decls, "list-style");
        if !v.is_empty() {
            get_prio("list-style-type")
                .filter(|&p| LIST_STYLE_ALL.iter().all(|n| get_prio(n) == Some(p)))
                .map(|p| (v, p))
        } else {
            None
        }
    };
    let flex_sh: Option<(String, bool)> = {
        let v = lookup_decl_or_shorthand(decls, "flex");
        if !v.is_empty() {
            get_prio("flex-grow")
                .filter(|&p| FLEX_ALL.iter().all(|n| get_prio(n) == Some(p)))
                .map(|p| (v, p))
        } else {
            None
        }
    };

    let mut emitted_grid = false;
    let mut emitted_tmpl = false;
    let mut emitted_border = false;
    let mut emitted_bw = false;
    let mut emitted_bs = false;
    let mut emitted_bc = false;
    let mut emitted_sides = [false; 4];
    let mut emitted_margin = false;
    let mut emitted_padding = false;
    let mut emitted_overflow = false;
    let mut emitted_outline = false;
    let mut emitted_list_style = false;
    let mut emitted_flex = false;
    let mut out = Vec::with_capacity(decls.len());
    let fmt_one = |k: &str, v: &str, imp: bool| -> String {
        let sep = if k.starts_with("--") && v.starts_with(' ') {
            ":"
        } else {
            ": "
        };
        if imp {
            if v.is_empty() {
                format!("{k}{sep}!important;")
            } else {
                format!("{k}{sep}{v} !important;")
            }
        } else {
            format!("{k}{sep}{v};")
        }
    };
    for (k, v, imp) in decls {
        let ks = k.as_str();
        if !grid_sh.is_empty() && GRID_ALL.contains(&ks) {
            if !emitted_grid {
                emitted_grid = true;
                out.push(fmt_one("grid", &grid_sh, *imp));
            }
            continue;
        }
        if !grid_tmpl_sh.is_empty() && GRID_TMPL.contains(&ks) {
            if !emitted_tmpl {
                emitted_tmpl = true;
                out.push(fmt_one("grid-template", &grid_tmpl_sh, *imp));
            }
            continue;
        }
        if let Some((ref sh_v, sh_imp)) = border_sh {
            if BORDER_ALL_13.contains(&ks) {
                if !emitted_border {
                    emitted_border = true;
                    out.push(fmt_one("border", sh_v, sh_imp));
                }
                continue;
            }
        }
        if let Some((ref sh_v, sh_imp)) = bw_sh {
            if BORDER_WIDTH_ALL.contains(&ks) {
                if !emitted_bw {
                    emitted_bw = true;
                    out.push(fmt_one("border-width", sh_v, sh_imp));
                }
                continue;
            }
        }
        if let Some((ref sh_v, sh_imp)) = bs_sh {
            if BORDER_STYLE_ALL.contains(&ks) {
                if !emitted_bs {
                    emitted_bs = true;
                    out.push(fmt_one("border-style", sh_v, sh_imp));
                }
                continue;
            }
        }
        if let Some((ref sh_v, sh_imp)) = bc_sh {
            if BORDER_COLOR_ALL.contains(&ks) {
                if !emitted_bc {
                    emitted_bc = true;
                    out.push(fmt_one("border-color", sh_v, sh_imp));
                }
                continue;
            }
        }
        if use_per_side_border {
            let mut handled_side = false;
            for (idx, (sh_name, lhs)) in BORDER_SIDES.iter().enumerate() {
                if lhs.contains(&ks) {
                    if let Some((ref sh_v, sh_imp)) = side_sh[idx] {
                        if !emitted_sides[idx] {
                            emitted_sides[idx] = true;
                            out.push(fmt_one(sh_name, sh_v, sh_imp));
                        }
                        handled_side = true;
                    }
                    break;
                }
            }
            if handled_side {
                continue;
            }
        }
        if let Some((ref sh_v, sh_imp)) = margin_sh {
            if MARGIN_ALL.contains(&ks) {
                if !emitted_margin {
                    emitted_margin = true;
                    out.push(fmt_one("margin", sh_v, sh_imp));
                }
                continue;
            }
        }
        if let Some((ref sh_v, sh_imp)) = padding_sh {
            if PADDING_ALL.contains(&ks) {
                if !emitted_padding {
                    emitted_padding = true;
                    out.push(fmt_one("padding", sh_v, sh_imp));
                }
                continue;
            }
        }
        if let Some((ref sh_v, sh_imp)) = overflow_sh {
            if OVERFLOW_ALL.contains(&ks) {
                if !emitted_overflow {
                    emitted_overflow = true;
                    out.push(fmt_one("overflow", sh_v, sh_imp));
                }
                continue;
            }
        }
        if let Some((ref sh_v, sh_imp)) = outline_sh {
            if OUTLINE_ALL.contains(&ks) {
                if !emitted_outline {
                    emitted_outline = true;
                    out.push(fmt_one("outline", sh_v, sh_imp));
                }
                continue;
            }
        }
        if let Some((ref sh_v, sh_imp)) = list_style_sh {
            if LIST_STYLE_ALL.contains(&ks) {
                if !emitted_list_style {
                    emitted_list_style = true;
                    out.push(fmt_one("list-style", sh_v, sh_imp));
                }
                continue;
            }
        }
        if let Some((ref sh_v, sh_imp)) = flex_sh {
            if FLEX_ALL.contains(&ks) {
                if !emitted_flex {
                    emitted_flex = true;
                    out.push(fmt_one("flex", sh_v, sh_imp));
                }
                continue;
            }
        }
        out.push(fmt_one(k, v, *imp));
    }
    out.join(" ")
}

fn upsert_decl(
    decls: &mut Vec<(String, String, bool)>,
    k: String,
    v: String,
    important: bool,
    respect_important_cascade: bool,
) {
    if let Some(existing) = decls.iter_mut().find(|(ek, _, _)| ek == &k) {
        if respect_important_cascade && existing.2 && !important {
            return;
        }
        existing.1 = v;
        existing.2 = important;
    } else {
        decls.push((k, v, important));
    }
}

fn collapse_2_sides_str(a: &str, b: &str) -> String {
    if a == b {
        a.to_string()
    } else {
        format!("{a} {b}")
    }
}

fn collapse_4_sides_str(t: &str, r: &str, b: &str, l: &str) -> String {
    if t == r && r == b && b == l {
        t.to_string()
    } else if t == b && r == l {
        format!("{t} {r}")
    } else if r == l {
        format!("{t} {r} {b}")
    } else {
        format!("{t} {r} {b} {l}")
    }
}

fn format_border_triple(w: &str, s: &str, c: &str) -> String {
    if w == s && s == c && rix_css::GlobalKeyword::from_ident(w).is_some() {
        return w.to_string();
    }
    if rix_css::GlobalKeyword::from_ident(w).is_some()
        || rix_css::GlobalKeyword::from_ident(s).is_some()
        || rix_css::GlobalKeyword::from_ident(c).is_some()
    {
        return String::new();
    }
    let mut parts = Vec::with_capacity(3);
    if !w.is_empty() && w != "medium" {
        parts.push(w);
    }
    if !s.is_empty() && s != "none" {
        parts.push(s);
    }
    if !c.is_empty() && c != "currentcolor" {
        parts.push(c);
    }
    if parts.is_empty() {
        parts.push("none");
    }
    parts.join(" ")
}

fn collapse_css_wide_pair(a: &str, b: &str) -> Option<String> {
    let ka = rix_css::GlobalKeyword::from_ident(a).is_some();
    let kb = rix_css::GlobalKeyword::from_ident(b).is_some();
    if ka && kb {
        (a == b).then(|| a.to_string())
    } else if ka || kb {
        Some(String::new())
    } else {
        None
    }
}

fn has_var_ref_ci(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.contains("var(") || lower.contains("env(")
}

fn lookup_decl_or_shorthand(decls: &[(String, String, bool)], prop: &str) -> String {
    let prop = if prop == "word-wrap" {
        "overflow-wrap"
    } else {
        prop
    };
    // CSS Cascade L5 `all` shorthand semantics:
    // `all` returns its CSS-wide keyword ONLY if no affected property overrides it with a
    // different value or priority after `all`.
    if prop == "all" {
        if let Some((all_idx, (_, all_val, all_imp))) =
            decls.iter().enumerate().find(|(_, (k, _, _))| k == "all")
        {
            let overridden = decls.iter().enumerate().any(|(idx, (k, v, imp))| {
                idx > all_idx && is_affected_by_all_shorthand(k) && (v != all_val || imp != all_imp)
            });
            return if overridden {
                String::new()
            } else {
                all_val.clone()
            };
        }
        return String::new();
    }
    if let Some((_, v, _)) = decls.iter().find(|(k, _, _)| k == prop) {
        return v.clone();
    }
    let get = |name: &str| -> Option<(&str, bool)> {
        if let Some((_, v, imp)) = decls.iter().find(|(k, _, _)| k == name) {
            return (!v.is_empty() && !has_var_ref_ci(v)).then_some((v.as_str(), *imp));
        }
        if is_affected_by_all_shorthand(name) {
            if let Some((_, av, aimp)) = decls.iter().find(|(k, _, _)| k == "all") {
                return (!av.is_empty() && !has_var_ref_ci(av)).then_some((av.as_str(), *aimp));
            }
        }
        None
    };
    let get_pair = |a: &str, b: &str| -> Option<String> {
        let (va, ia) = get(a)?;
        let (vb, ib) = get(b)?;
        if ia != ib {
            return None;
        }
        if let Some(kw) = collapse_css_wide_pair(va, vb) {
            return (!kw.is_empty()).then_some(kw);
        }
        Some(collapse_2_sides_str(va, vb))
    };
    let get_quad = |t: &str, r: &str, b: &str, l: &str| -> Option<String> {
        let (vt, it) = get(t)?;
        let (vr, ir) = get(r)?;
        let (vb, ib) = get(b)?;
        let (vl, il) = get(l)?;
        if it != ir || ir != ib || ib != il {
            return None;
        }
        let any_kw = [vt, vr, vb, vl]
            .iter()
            .any(|v| rix_css::GlobalKeyword::from_ident(v).is_some());
        if any_kw {
            return (vt == vr && vr == vb && vb == vl).then(|| vt.to_string());
        }
        Some(collapse_4_sides_str(vt, vr, vb, vl))
    };
    let get_border_side = |prefix: &str| -> Option<String> {
        let (w, iw) = get(&format!("{prefix}-width"))?;
        let (s, is_) = get(&format!("{prefix}-style"))?;
        let (c, ic) = get(&format!("{prefix}-color"))?;
        if iw != is_ || is_ != ic {
            return None;
        }
        let formatted = format_border_triple(w, s, c);
        (!formatted.is_empty()).then_some(formatted)
    };
    let res = match prop {
        "margin" => get_quad("margin-top", "margin-right", "margin-bottom", "margin-left"),
        "margin-block" => get_pair("margin-block-start", "margin-block-end"),
        "margin-inline" => get_pair("margin-inline-start", "margin-inline-end"),
        "padding" => get_quad(
            "padding-top",
            "padding-right",
            "padding-bottom",
            "padding-left",
        ),
        "padding-block" => get_pair("padding-block-start", "padding-block-end"),
        "padding-inline" => get_pair("padding-inline-start", "padding-inline-end"),
        "inset" => get_quad("top", "right", "bottom", "left"),
        "inset-block" => get_pair("inset-block-start", "inset-block-end"),
        "inset-inline" => get_pair("inset-inline-start", "inset-inline-end"),
        "border-width" => get_quad(
            "border-top-width",
            "border-right-width",
            "border-bottom-width",
            "border-left-width",
        ),
        "border-block-width" => get_pair("border-block-start-width", "border-block-end-width"),
        "border-inline-width" => get_pair("border-inline-start-width", "border-inline-end-width"),
        "border-style" => get_quad(
            "border-top-style",
            "border-right-style",
            "border-bottom-style",
            "border-left-style",
        ),
        "border-block-style" => get_pair("border-block-start-style", "border-block-end-style"),
        "border-inline-style" => get_pair("border-inline-start-style", "border-inline-end-style"),
        "border-color" => get_quad(
            "border-top-color",
            "border-right-color",
            "border-bottom-color",
            "border-left-color",
        ),
        "border-block-color" => get_pair("border-block-start-color", "border-block-end-color"),
        "border-inline-color" => get_pair("border-inline-start-color", "border-inline-end-color"),
        "border-top" => get_border_side("border-top"),
        "border-right" => get_border_side("border-right"),
        "border-bottom" => get_border_side("border-bottom"),
        "border-left" => get_border_side("border-left"),
        "border-block-start" => get_border_side("border-block-start"),
        "border-block-end" => get_border_side("border-block-end"),
        "border-inline-start" => get_border_side("border-inline-start"),
        "border-inline-end" => get_border_side("border-inline-end"),
        "border-block" => get_border_side("border-block-start").and_then(|s| {
            let e = get_border_side("border-block-end")?;
            (s == e).then_some(s)
        }),
        "border-inline" => get_border_side("border-inline-start").and_then(|s| {
            let e = get_border_side("border-inline-end")?;
            (s == e).then_some(s)
        }),
        "border" => (|| {
            let t = get_border_side("border-top")?;
            let r = get_border_side("border-right")?;
            let b = get_border_side("border-bottom")?;
            let l = get_border_side("border-left")?;
            let (bi, ibi) = get("border-image")?;
            let (_, it) = get("border-top-width")?;
            let (_, ir) = get("border-right-width")?;
            let (_, ib) = get("border-bottom-width")?;
            let (_, il) = get("border-left-width")?;
            if it != ir || ir != ib || ib != il || il != ibi {
                return None;
            }
            if t == r && r == b && b == l {
                if rix_css::GlobalKeyword::from_ident(&t).is_some() {
                    return (bi == t).then_some(t);
                }
                if bi == "none" {
                    return Some(t);
                }
            }
            None
        })(),
        "overflow" => get_pair("overflow-x", "overflow-y"),
        "outline" => (|| {
            let (w, iw) = get("outline-width")?;
            let (s, is_) = get("outline-style")?;
            let (c, ic) = get("outline-color")?;
            if iw != is_ || is_ != ic {
                return None;
            }
            let f = format_border_triple(w, s, c);
            (!f.is_empty()).then_some(f)
        })(),
        "list-style" => (|| {
            let (pos, ip) = get("list-style-position")?;
            let (img, ii) = get("list-style-image")?;
            let (ty, it) = get("list-style-type")?;
            if ip != ii || ii != it {
                return None;
            }
            if pos == img && img == ty && rix_css::GlobalKeyword::from_ident(pos).is_some() {
                return Some(pos.to_string());
            }
            if rix_css::GlobalKeyword::from_ident(pos).is_some()
                || rix_css::GlobalKeyword::from_ident(img).is_some()
                || rix_css::GlobalKeyword::from_ident(ty).is_some()
            {
                return None;
            }
            let mut parts = Vec::with_capacity(3);
            if pos != "outside" {
                parts.push(pos);
            }
            if img != "none" {
                parts.push(img);
            }
            if ty != "disc" || parts.is_empty() {
                parts.push(ty);
            }
            Some(parts.join(" "))
        })(),
        "border-radius" => get_quad(
            "border-top-left-radius",
            "border-top-right-radius",
            "border-bottom-right-radius",
            "border-bottom-left-radius",
        ),
        "flex" => (|| {
            let (g, ig) = get("flex-grow")?;
            let (s, is_) = get("flex-shrink")?;
            let (b, ib) = get("flex-basis")?;
            if ig != is_ || is_ != ib {
                return None;
            }
            if g == s && s == b && rix_css::GlobalKeyword::from_ident(g).is_some() {
                return Some(g.to_string());
            }
            if rix_css::GlobalKeyword::from_ident(g).is_some()
                || rix_css::GlobalKeyword::from_ident(s).is_some()
                || rix_css::GlobalKeyword::from_ident(b).is_some()
            {
                return None;
            }
            Some(format!("{g} {s} {b}"))
        })(),
        "flex-flow" => (|| {
            let (d, id) = get("flex-direction")?;
            let (w, iw) = get("flex-wrap")?;
            if id != iw {
                return None;
            }
            if d == "row" && w == "nowrap" {
                Some("row".to_string())
            } else if d == "row" {
                Some(w.to_string())
            } else if w == "nowrap" {
                Some(d.to_string())
            } else {
                Some(format!("{d} {w}"))
            }
        })(),
        "gap" => get_pair("row-gap", "column-gap"),
        "grid-row" => (|| {
            let (s, is_) = get("grid-row-start")?;
            let (e, ie) = get("grid-row-end")?;
            if is_ != ie {
                return None;
            }
            let ps = rix_css::grid_css::parse_grid_line(s)?;
            let pe = rix_css::grid_css::parse_grid_line(e)?;
            Some(rix_css::grid_css::serialize_grid_row_or_col_shortest(
                &ps, &pe, None,
            ))
        })(),
        "grid-column" => (|| {
            let (s, is_) = get("grid-column-start")?;
            let (e, ie) = get("grid-column-end")?;
            if is_ != ie {
                return None;
            }
            let ps = rix_css::grid_css::parse_grid_line(s)?;
            let pe = rix_css::grid_css::parse_grid_line(e)?;
            Some(rix_css::grid_css::serialize_grid_row_or_col_shortest(
                &ps, &pe, None,
            ))
        })(),
        "grid-area" => (|| {
            let (rs, irs) = get("grid-row-start")?;
            let (cs, ics) = get("grid-column-start")?;
            let (re, ire) = get("grid-row-end")?;
            let (ce, ice) = get("grid-column-end")?;
            if irs != ics || ics != ire || ire != ice {
                return None;
            }
            let prs = rix_css::grid_css::parse_grid_line(rs)?;
            let pcs = rix_css::grid_css::parse_grid_line(cs)?;
            let pre = rix_css::grid_css::parse_grid_line(re)?;
            let pce = rix_css::grid_css::parse_grid_line(ce)?;
            Some(rix_css::grid_css::serialize_grid_area_shortest(
                &prs, &pcs, &pre, &pce, None,
            ))
        })(),
        "grid-template" => (|| {
            let (r, ir) = get("grid-template-rows")?;
            let (c, ic) = get("grid-template-columns")?;
            let (a, ia) = get("grid-template-areas")?;
            if ir != ic || ic != ia {
                return None;
            }
            let pr = rix_css::grid_css::parse_grid_template_track_list(r)?;
            let pc = rix_css::grid_css::parse_grid_template_track_list(c)?;
            let pa = rix_css::grid_css::parse_grid_template_areas(a)?;
            rix_css::grid_css::serialize_grid_template_from_values(&pr, &pc, &pa)
        })(),
        "grid" => (|| {
            let (r, ir) = get("grid-template-rows")?;
            let (c, ic) = get("grid-template-columns")?;
            let (a, ia) = get("grid-template-areas")?;
            let (ar, iar) = get("grid-auto-rows")?;
            let (ac, iac) = get("grid-auto-columns")?;
            let (af, iaf) = get("grid-auto-flow")?;
            if ir != ic || ic != ia || ia != iar || iar != iac || iac != iaf {
                return None;
            }
            let pr = rix_css::grid_css::parse_grid_template_track_list(r)?;
            let pc = rix_css::grid_css::parse_grid_template_track_list(c)?;
            let pa = rix_css::grid_css::parse_grid_template_areas(a)?;
            let par = rix_css::grid_css::parse_grid_auto_tracks(ar)?;
            let pac = rix_css::grid_css::parse_grid_auto_tracks(ac)?;
            let paf = rix_css::grid_css::parse_grid_auto_flow(af)?;
            rix_css::grid_css::serialize_grid_from_values(&pr, &pc, &pa, &par, &pac, paf)
        })(),
        "grid-lanes" => (|| {
            let (c, ic) = get("grid-template-columns")?;
            let (r, ir) = get("grid-template-rows")?;
            let (a, ia) = get("grid-template-areas")?;
            let (d, id) = get("grid-lanes-direction")?;
            if ic != ir || ir != ia || ia != id {
                return None;
            }
            let pc = rix_css::grid_css::parse_grid_template_track_list(c)?;
            let pr = rix_css::grid_css::parse_grid_template_track_list(r)?;
            let pa = rix_css::grid_css::parse_grid_template_areas(a)?;
            let pd = rix_css::grid_css::parse_grid_lanes_direction(d)?;
            rix_css::grid_css::serialize_grid_lanes_from_values(&pc, &pr, &pa, pd, None)
        })(),
        "white-space" => (|| {
            let (c, ic) = get("white-space-collapse")?;
            let (w, iw) = get("text-wrap-mode")?;
            let (t, it) = get("white-space-trim")?;
            if ic != iw || iw != it {
                return None;
            }
            if c == w && w == t && rix_css::GlobalKeyword::from_ident(c).is_some() {
                return Some(c.to_string());
            }
            let pc = rix_css::WhiteSpaceCollapseMode::parse(c)?;
            let pw = rix_css::TextWrapMode::parse(w)?;
            let pt = rix_css::WhiteSpaceTrim::parse(t)?;
            Some(rix_css::text_css::serialize_white_space(pc, pw, pt))
        })(),
        "text-wrap" => (|| {
            let (m, im) = get("text-wrap-mode")?;
            let (s, is_) = get("text-wrap-style")?;
            if im != is_ {
                return None;
            }
            if m == s && rix_css::GlobalKeyword::from_ident(m).is_some() {
                return Some(m.to_string());
            }
            let pm = rix_css::TextWrapMode::parse(m)?;
            let ps = rix_css::TextWrapStyle::parse(s)?;
            Some(rix_css::text_css::serialize_text_wrap(pm, ps))
        })(),
        "text-spacing" => (|| {
            let (t, it) = get("text-spacing-trim")?;
            let (a, ia) = get("text-autospace")?;
            if it != ia {
                return None;
            }
            if t == a && rix_css::GlobalKeyword::from_ident(t).is_some() {
                return Some(t.to_string());
            }
            let pt = rix_css::TextSpacingTrimMode::parse(t)?;
            let pa = rix_css::TextAutospaceValue::parse(a)?;
            Some(rix_css::text_css::serialize_text_spacing(pt, pa))
        })(),
        "transition" => (|| {
            let (p, ip) = get("transition-property")?;
            let (du, idu) = get("transition-duration")?;
            let (tf, itf) = get("transition-timing-function")?;
            let (de, ide) = get("transition-delay")?;
            let (be, ibe) = get("transition-behavior")?;
            if ip != idu || idu != itf || itf != ide || ide != ibe {
                return None;
            }
            if p == du
                && du == tf
                && tf == de
                && de == be
                && rix_css::GlobalKeyword::from_ident(p).is_some()
            {
                return Some(p.to_string());
            }
            rix_css::anim_css::serialize_transition_shorthand_from_longhands(p, du, tf, de, be)
        })(),
        "animation" => (|| {
            let (du, idu) = get("animation-duration")?;
            let (tf, itf) = get("animation-timing-function")?;
            let (de, ide) = get("animation-delay")?;
            let (it, iit) = get("animation-iteration-count")?;
            let (di, idi) = get("animation-direction")?;
            let (fi, ifi) = get("animation-fill-mode")?;
            let (st, ist) = get("animation-play-state")?;
            let (nm, inm) = get("animation-name")?;
            let (tl, itl) = get("animation-timeline")?;
            let (rs, irs) = get("animation-range-start")?;
            let (re, ire) = get("animation-range-end")?;
            if idu != itf
                || itf != ide
                || ide != iit
                || iit != idi
                || idi != ifi
                || ifi != ist
                || ist != inm
                || inm != itl
                || itl != irs
                || irs != ire
            {
                return None;
            }
            if du == tf
                && tf == de
                && de == it
                && it == di
                && di == fi
                && fi == st
                && st == nm
                && nm == tl
                && tl == rs
                && rs == re
                && rix_css::GlobalKeyword::from_ident(du).is_some()
            {
                return Some(du.to_string());
            }
            rix_css::anim_css::serialize_animation_shorthand_from_longhands(
                du, tf, de, it, di, fi, st, nm, tl, rs, re,
            )
        })(),
        "animation-range" => (|| {
            let (rs, irs) = get("animation-range-start")?;
            let (re, ire) = get("animation-range-end")?;
            if irs != ire {
                return None;
            }
            if rs == re && rix_css::GlobalKeyword::from_ident(rs).is_some() {
                return Some(rs.to_string());
            }
            let (starts, _) = rix_css::anim_css::parse_animation_range_point_list(rs, false)?;
            let (ends, _) = rix_css::anim_css::parse_animation_range_point_list(re, true)?;
            if starts.len() != ends.len() {
                return None;
            }
            Some(rix_css::anim_css::serialize_animation_range_shorthand(
                &starts, &ends, None,
            ))
        })(),
        _ => {
            // If `all: <css-wide-keyword>` was declared and `prop` is a recognized standard
            // property affected by `all`, return that CSS-wide keyword!
            if is_affected_by_all_shorthand(prop) && PropertyIdKind::from_name(prop).is_some() {
                decls
                    .iter()
                    .find(|(k, _, _)| k == "all")
                    .map(|(_, v, _)| v.clone())
            } else {
                None
            }
        }
    };
    res.unwrap_or_default()
}

fn remove_decl_or_shorthand(decls: &mut Vec<(String, String, bool)>, prop: &str) -> String {
    let prop = if prop == "word-wrap" {
        "overflow-wrap"
    } else {
        prop
    };
    let old = lookup_decl_or_shorthand(decls, prop);
    if prop == "all" {
        decls.retain(|(k, _, _)| !is_affected_by_all_shorthand(k) && k != "all");
        return old;
    }
    if let Some(kind) = PropertyIdKind::from_name(prop) {
        let longhands = kind.shorthand_longhands();
        if longhands.len() > 1 {
            let names: Vec<&str> = longhands.iter().map(|k| k.css_name()).collect();
            decls.retain(|(k, _, _)| k != prop && !names.contains(&k.as_str()));
            return old;
        }
    }
    decls.retain(|(k, _, _)| k != prop);
    old
}

fn set_decl_expanded(
    decls: &mut Vec<(String, String, bool)>,
    prop: &str,
    raw_val: &str,
    important: bool,
    respect_important_cascade: bool,
) -> bool {
    if raw_val.is_empty() {
        remove_decl_or_shorthand(decls, prop);
        return true;
    }
    let Some(pairs) = parse_and_expand_declaration(prop, raw_val) else {
        return false;
    };
    if prop == "all" {
        // Setting `all` overrides all prior affected declarations (keeping `direction`,
        // `unicode-bidi`, and custom properties `--*`), unless protected by `!important`.
        let blocked = respect_important_cascade
            && !important
            && decls.iter().any(|(k, _, i)| k == "all" && *i);
        if blocked {
            return true;
        }
        decls.retain(|(k, _, i)| {
            !is_affected_by_all_shorthand(k) && k != "all"
                || (respect_important_cascade && !important && *i && k != "all")
        });
        for (k, v) in pairs {
            upsert_decl(decls, k, v, important, respect_important_cascade);
        }
        return true;
    }
    if let Some(kind) = PropertyIdKind::from_name(prop) {
        let longhands = kind.shorthand_longhands();
        if longhands.len() > 1 {
            if pairs.len() > 1 {
                // Shorthand expanding to concrete longhands: clear any prior shorthand entry
                // unless it is `!important` and we are cascading a non-important declaration.
                let blocked = respect_important_cascade
                    && !important
                    && decls.iter().any(|(k, _, i)| k == prop && *i);
                if blocked {
                    return true;
                }
                decls.retain(|(k, _, _)| k != prop);
            } else {
                // Shorthand kept unexpanded (`var(...)` or `background: green`): remove constituent
                // longhands unless protected by `!important` during parsing, and when mutated via
                // CSSOM (`!respect_important_cascade`) move the shorthand to the end so it overrides
                // overlapping shorthands like `border-width: var(--width)`.
                let names: Vec<&str> = longhands.iter().map(|k| k.css_name()).collect();
                decls.retain(|(k, _, i)| {
                    !names.contains(&k.as_str()) || (respect_important_cascade && !important && *i)
                });
                if !respect_important_cascade {
                    decls.retain(|(k, _, _)| k != prop);
                }
            }
        } else if !prop.starts_with("--") {
            // Setting a longhand when a parent shorthand currently holds a pending `var()` value
            // (CSS Variables L1 §3): either blocked by `!important`, or splits the shorthand into
            // empty pending-substitution entries for its other longhands.
            let mut split_shorthand: Option<(usize, Vec<String>, bool)> = None;
            for (idx, (dk, dv, dimp)) in decls.iter().enumerate() {
                if has_var_ref_ci(dv) {
                    if let Some(sh_kind) = PropertyIdKind::from_name(dk) {
                        let sh_lhs = sh_kind.shorthand_longhands();
                        if sh_lhs.len() > 1 && sh_lhs.contains(&kind) {
                            if respect_important_cascade && *dimp && !important {
                                return true;
                            }
                            let others = sh_lhs
                                .into_iter()
                                .filter(|&lh| lh != kind)
                                .map(|lh| lh.css_name().to_string())
                                .collect();
                            split_shorthand = Some((idx, others, *dimp));
                            break;
                        }
                    }
                }
            }
            if let Some((idx, others, sh_imp)) = split_shorthand {
                decls.remove(idx);
                for other in others {
                    upsert_decl(decls, other, String::new(), sh_imp, false);
                }
            }
        }
    }
    for (k, v) in pairs {
        upsert_decl(decls, k, v, important, respect_important_cascade);
    }
    true
}

/// Parses and validates inline `cssText`, updating the element's `style` attribute.
/// Used by both `el.style.cssText = ...` and `el.style = ...` (`[PutForwards=cssText]`).
pub(crate) fn set_inline_style_css_text(vm: &mut JsVm, host: &SharedHost, nid: NodeId, val: &str) {
    host.borrow_mut().clear_inline_color_exprs(nid);
    let validated = canonicalize_css_decls(val);
    set_element_attr(
        vm,
        host,
        nid,
        "style",
        &serialize_style_decls_with_priority(&validated),
    );
}

fn sync_inline_color_expr(host: &SharedHost, nid: NodeId, prop: &str, raw_val: &str) {
    if let Some(kind) = PropertyIdKind::from_name(prop) {
        if rix_css::cssom_decl::is_color_longhand(kind) {
            let expr = if raw_val.trim().is_empty() {
                None
            } else {
                rix_css::parse_css_color(raw_val.trim()).map(|mut e| {
                    e.bind_random_property(kind.css_name());
                    e
                })
            };
            host.borrow_mut().set_inline_color_expr(nid, kind, expr);
        } else {
            // Setting/removing a shorthand (`border`, `border-color`, `border-block-color`,
            // `border-inline-color`, `outline`, `text-decoration`, `all`, etc.) must clear
            // any previously cached longhand color expressions on its constituent longhands.
            let longhands = kind.shorthand_longhands();
            if longhands.len() > 1 || kind == PropertyIdKind::All {
                let mut hb = host.borrow_mut();
                if kind == PropertyIdKind::All {
                    hb.clear_inline_color_exprs(nid);
                } else {
                    for lh in longhands {
                        if rix_css::cssom_decl::is_color_longhand(lh) {
                            hb.set_inline_color_expr(nid, lh, None);
                        }
                    }
                }
            }
        }
    }
}

/// Creates a CSSOM-validated inline `CSSStyleDeclaration` Proxy for `element.style`.
pub(crate) fn create_inline_style_proxy(vm: &mut JsVm, host: &SharedHost, nid: NodeId) -> JsValue {
    let target = alloc_style_decl_target(vm, host);
    let h = Rc::clone(host);
    define_host_method(vm, target, "getPropertyValue", move |vm, _, args| {
        let prop = normalize_css_prop_name(&vm.value_to_rust_string(arg(args, 0)));
        let raw = get_attr_string(&h, nid, "style").unwrap_or_default();
        let decls = parse_style_decls_with_priority(&raw);
        let val = lookup_decl_or_shorthand(&decls, &prop);
        Ok(JsValue::from_string(vm.heap.alloc_string(val)))
    });
    let h = Rc::clone(host);
    define_host_method(vm, target, "getPropertyPriority", move |vm, _, args| {
        let prop = normalize_css_prop_name(&vm.value_to_rust_string(arg(args, 0)));
        let raw = get_attr_string(&h, nid, "style").unwrap_or_default();
        let decls = parse_style_decls_with_priority(&raw);
        let imp = if let Some((_, _, i)) = decls.iter().find(|(k, _, _)| k == &prop) {
            *i
        } else if let Some(kind) = PropertyIdKind::from_name(&prop) {
            let lh = kind.shorthand_longhands();
            lh.len() > 1
                && lh.iter().all(|l| {
                    decls
                        .iter()
                        .find(|(k, _, _)| k == l.css_name())
                        .is_some_and(|(_, _, i)| *i)
                })
        } else {
            false
        };
        let s = if imp { "important" } else { "" };
        Ok(JsValue::from_string(vm.heap.alloc_string(s)))
    });
    let h = Rc::clone(host);
    define_host_method(vm, target, "setProperty", move |vm, _, args| {
        let prop = normalize_css_prop_name(&vm.value_to_rust_string(arg(args, 0)));
        let raw_val = if args.len() < 2 || arg(args, 1).is_null() {
            String::new()
        } else {
            vm.value_to_rust_string(arg(args, 1))
        };
        let priority = if args.len() >= 3 && !arg(args, 2).is_undefined() && !arg(args, 2).is_null()
        {
            vm.value_to_rust_string(arg(args, 2))
        } else {
            String::new()
        };
        let important = if priority.is_empty() {
            false
        } else if priority.eq_ignore_ascii_case("important") {
            true
        } else {
            return Ok(JsValue::UNDEFINED);
        };
        let raw = get_attr_string(&h, nid, "style").unwrap_or_default();
        let mut decls = parse_style_decls_with_priority(&raw);
        if !set_decl_expanded(&mut decls, &prop, &raw_val, important, false) {
            // Invalid CSS value: ignore per CSSOM §6.7.2!
            return Ok(JsValue::UNDEFINED);
        }
        sync_inline_color_expr(&h, nid, &prop, &raw_val);
        set_element_attr(
            vm,
            &h,
            nid,
            "style",
            &serialize_style_decls_with_priority(&decls),
        );
        Ok(JsValue::UNDEFINED)
    });
    let h = Rc::clone(host);
    define_host_method(vm, target, "removeProperty", move |vm, _, args| {
        let prop = normalize_css_prop_name(&vm.value_to_rust_string(arg(args, 0)));
        let raw = get_attr_string(&h, nid, "style").unwrap_or_default();
        let mut decls = parse_style_decls_with_priority(&raw);
        let before_len = decls.len();
        let old = remove_decl_or_shorthand(&mut decls, &prop);
        if decls.len() != before_len {
            sync_inline_color_expr(&h, nid, &prop, "");
            set_element_attr(
                vm,
                &h,
                nid,
                "style",
                &serialize_style_decls_with_priority(&decls),
            );
        }
        Ok(JsValue::from_string(vm.heap.alloc_string(old)))
    });
    let h = Rc::clone(host);
    define_host_method(vm, target, "item", move |vm, _, args| {
        let idx = vm.to_number(arg(args, 0)).unwrap_or(-1.0);
        if idx < 0.0 || !idx.is_finite() {
            return Ok(JsValue::from_string(vm.heap.alloc_string("")));
        }
        let raw = get_attr_string(&h, nid, "style").unwrap_or_default();
        let decls = parse_style_decls_with_priority(&raw);
        let name = decls
            .get(idx as usize)
            .map(|(k, _, _)| k.as_str())
            .unwrap_or("");
        Ok(JsValue::from_string(vm.heap.alloc_string(name)))
    });

    let handler = vm.alloc_ordinary_object();
    let h_has = Rc::clone(host);
    define_host_method(vm, handler, "has", move |vm, _, args| {
        let prop_name = vm.value_to_rust_string(arg(args, 1));
        if matches!(
            prop_name.as_str(),
            "cssText"
                | "length"
                | "item"
                | "getPropertyValue"
                | "getPropertyPriority"
                | "setProperty"
                | "removeProperty"
                | "cssFloat"
        ) {
            return Ok(JsValue::TRUE);
        }
        if let Ok(idx) = prop_name.parse::<usize>() {
            let raw = get_attr_string(&h_has, nid, "style").unwrap_or_default();
            let decls = parse_style_decls_with_priority(&raw);
            return Ok(JsValue::from_bool(idx < decls.len()));
        }
        let css_key = camel_to_kebab(&prop_name);
        if PropertyIdKind::from_name(&css_key).is_some() {
            return Ok(JsValue::TRUE);
        }
        Ok(JsValue::FALSE)
    });
    let h_keys = Rc::clone(host);
    define_host_method(vm, handler, "ownKeys", move |vm, _, _| {
        let arr = vm.alloc_array();
        let raw = get_attr_string(&h_keys, nid, "style").unwrap_or_default();
        let decls = parse_style_decls_with_priority(&raw);
        for i in 0..decls.len() {
            let s = vm.heap.alloc_string(i.to_string());
            vm.heap.object_push_element(arr, JsValue::from_string(s));
        }
        Ok(JsValue::from_object(arr))
    });
    let h_desc = Rc::clone(host);
    define_host_method(
        vm,
        handler,
        "getOwnPropertyDescriptor",
        move |vm, _, args| {
            let prop_name = vm.value_to_rust_string(arg(args, 1));
            if let Ok(idx) = prop_name.parse::<usize>() {
                let raw = get_attr_string(&h_desc, nid, "style").unwrap_or_default();
                let decls = parse_style_decls_with_priority(&raw);
                let Some((name, _, _)) = decls.get(idx) else {
                    return Ok(JsValue::UNDEFINED);
                };
                let desc = vm.alloc_ordinary_object();
                let kv = vm.heap.intern_key("value");
                let kw = vm.heap.intern_key("writable");
                let ke = vm.heap.intern_key("enumerable");
                let kc = vm.heap.intern_key("configurable");
                let val = JsValue::from_string(vm.heap.alloc_string(name.as_str()));
                vm.heap
                    .object_define_data(desc, kv, val, PropertyAttrs::DEFAULT, &mut vm.shapes);
                vm.heap.object_define_data(
                    desc,
                    kw,
                    JsValue::FALSE,
                    PropertyAttrs::DEFAULT,
                    &mut vm.shapes,
                );
                vm.heap.object_define_data(
                    desc,
                    ke,
                    JsValue::TRUE,
                    PropertyAttrs::DEFAULT,
                    &mut vm.shapes,
                );
                vm.heap.object_define_data(
                    desc,
                    kc,
                    JsValue::TRUE,
                    PropertyAttrs::DEFAULT,
                    &mut vm.shapes,
                );
                return Ok(JsValue::from_object(desc));
            }
            Ok(JsValue::UNDEFINED)
        },
    );
    let h_get = Rc::clone(host);
    define_host_method(vm, handler, "get", move |vm, _, args| {
        let key_val = arg(args, 1);
        if key_val.is_symbol() {
            return forward_symbol_get(vm, arg(args, 0), key_val);
        }
        let prop_name = vm.value_to_rust_string(key_val);
        if prop_name == "cssText" {
            let raw = get_attr_string(&h_get, nid, "style").unwrap_or_default();
            let decls = parse_style_decls_with_priority(&raw);
            let canon = serialize_style_decls_with_priority(&decls);
            return Ok(JsValue::from_string(vm.heap.alloc_string(canon)));
        }
        let k = vm.heap.intern_key(&prop_name);
        if let Some(t_h) = arg(args, 0).as_object() {
            if vm.heap.object_get_own(t_h, k, &vm.shapes).is_some() {
                return vm.get_property_by_key(t_h, k);
            }
        }
        let raw = get_attr_string(&h_get, nid, "style").unwrap_or_default();
        let decls = parse_style_decls_with_priority(&raw);
        if prop_name == "length" {
            return Ok(JsValue::from_i32(decls.len() as i32));
        }
        if let Ok(idx) = prop_name.parse::<usize>() {
            return Ok(decls.get(idx).map_or(JsValue::UNDEFINED, |(name, _, _)| {
                JsValue::from_string(vm.heap.alloc_string(name))
            }));
        }
        let css_key = inline_prop_to_css_name(&prop_name);
        let val = lookup_decl_or_shorthand(&decls, &css_key);
        Ok(JsValue::from_string(vm.heap.alloc_string(val)))
    });
    let h_set = Rc::clone(host);
    define_host_method(vm, handler, "set", move |vm, _, args| {
        let prop_name = vm.value_to_rust_string(arg(args, 1));
        let val_arg = arg(args, 2);
        let val = if val_arg.is_null() {
            String::new()
        } else {
            vm.value_to_rust_string(val_arg)
        };
        if prop_name == "cssText" {
            set_inline_style_css_text(vm, &h_set, nid, &val);
            return Ok(JsValue::TRUE);
        }
        let css_key = inline_prop_to_css_name(&prop_name);
        let raw = get_attr_string(&h_set, nid, "style").unwrap_or_default();
        let mut decls = parse_style_decls_with_priority(&raw);
        if !set_decl_expanded(&mut decls, &css_key, &val, false, false) {
            return Ok(JsValue::TRUE);
        }
        sync_inline_color_expr(&h_set, nid, &css_key, &val);
        set_element_attr(
            vm,
            &h_set,
            nid,
            "style",
            &serialize_style_decls_with_priority(&decls),
        );
        Ok(JsValue::TRUE)
    });

    let proxy = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Proxy(rix_js::ProxyObject {
            target,
            handler,
            revoked: false,
        }),
        Some(vm.object_proto),
    ));
    JsValue::from_object(proxy)
}

fn inline_prop_to_css_name(prop_name: &str) -> String {
    if prop_name == "cssFloat" {
        return "float".to_string();
    }
    let kebab = camel_to_kebab(prop_name);
    match kebab.as_str() {
        "webkit-filter" | "-webkit-filter" => "filter".to_string(),
        "webkit-backdrop-filter" | "-webkit-backdrop-filter" => "backdrop-filter".to_string(),
        "webkit-transform" | "-webkit-transform" => "transform".to_string(),
        "webkit-transition" | "-webkit-transition" => "transition".to_string(),
        "webkit-animation" | "-webkit-animation" => "animation".to_string(),
        "webkit-box-sizing" | "-webkit-box-sizing" => "box-sizing".to_string(),
        other => {
            if let Some(rest) = other.strip_prefix("webkit-") {
                format!("-webkit-{rest}")
            } else {
                other.to_string()
            }
        }
    }
}

fn kebab_to_camel(kebab: &str) -> String {
    let mut out = String::with_capacity(kebab.len());
    let mut upper_next = false;
    for ch in kebab.chars() {
        if ch == '-' {
            upper_next = true;
        } else if upper_next {
            out.push(ch.to_ascii_uppercase());
            upper_next = false;
        } else {
            out.push(ch);
        }
    }
    out
}

fn computed_custom_keys(style: &ComputedStyle) -> Vec<String> {
    let mut keys: Vec<String> = style
        .custom_properties
        .visible_keys()
        .into_iter()
        .map(|k| k.as_str().to_string())
        .collect();
    keys.sort();
    keys
}

fn live_style_for(
    host: &SharedHost,
    nid: NodeId,
    pseudo: Option<RixPseudoElement>,
) -> Arc<ComputedStyle> {
    host.borrow_mut()
        .computed_style_for_node_pseudo(nid, pseudo)
        .unwrap_or_else(|| Arc::new(ComputedStyle::default()))
}

fn prop_needs_layout_resolution(prop: &str) -> bool {
    let lower = prop.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "width"
            | "height"
            | "inline-size"
            | "block-size"
            | "perspective-origin"
            | "margin"
            | "margin-top"
            | "margin-right"
            | "margin-bottom"
            | "margin-left"
            | "margin-block"
            | "margin-block-start"
            | "margin-block-end"
            | "margin-inline"
            | "margin-inline-start"
            | "margin-inline-end"
            | "padding"
            | "padding-top"
            | "padding-right"
            | "padding-bottom"
            | "padding-left"
            | "padding-block"
            | "padding-block-start"
            | "padding-block-end"
            | "padding-inline"
            | "padding-inline-start"
            | "padding-inline-end"
            | "top"
            | "right"
            | "bottom"
            | "left"
            | "inset"
            | "inset-block"
            | "inset-block-start"
            | "inset-block-end"
            | "inset-inline"
            | "inset-inline-start"
            | "inset-inline-end"
    )
}

fn prop_needs_parent_flex_check(prop: &str) -> bool {
    let lower = prop.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "min-width" | "min-height" | "min-inline-size" | "min-block-size"
    )
}

fn live_computed_property(
    host: &SharedHost,
    nid: NodeId,
    pseudo: Option<RixPseudoElement>,
    prop: &str,
) -> String {
    if !host.borrow().document.borrow().is_connected(nid) {
        return String::new();
    }
    let style = live_style_for(host, nid, pseudo);
    let needs_grid_layout = matches!(
        style.display,
        DisplayMode::Grid
            | DisplayMode::InlineGrid
            | DisplayMode::GridLanes
            | DisplayMode::InlineGridLanes
            | DisplayMode::InlineGridLanesMulti
    ) && matches!(
        prop.to_ascii_lowercase().as_str(),
        "grid-template-columns" | "grid-template-rows"
    );
    let layout_box =
        if pseudo.is_none() && (prop_needs_layout_resolution(prop) || needs_grid_layout) {
            host.borrow_mut().resolved_layout_box_for_node(nid)
        } else {
            None
        };
    let parent_flex_grid = if pseudo.is_none() && prop_needs_parent_flex_check(prop) {
        host.borrow_mut().parent_is_flex_or_grid(nid)
    } else {
        false
    };
    let ch_px = Some(host.borrow_mut().ch_advance_px(&style));
    format_computed_property_resolved(&style, prop, layout_box.as_ref(), parent_flex_grid, ch_px)
}

pub(crate) fn create_computed_style_proxy(
    vm: &mut JsVm,
    host: SharedHost,
    wa: Option<crate::web_animations::SharedWebAnimations>,
    nid: NodeId,
    pseudo: Option<RixPseudoElement>,
) -> JsValue {
    let target = alloc_style_decl_target(vm, &host);
    let h1 = Rc::clone(&host);
    let wa1 = wa.clone();
    define_host_method(vm, target, "getPropertyValue", move |vm, _, args| {
        if let Some(ref wa_ref) = wa1 {
            crate::css_animations_api::flush_if_styles_dirty(vm, &h1, wa_ref);
        }
        let prop = vm.value_to_rust_string(arg(args, 0));
        let val = live_computed_property(&h1, nid, pseudo, &prop);
        Ok(JsValue::from_string(vm.heap.alloc_string(val)))
    });
    define_host_method(vm, target, "getPropertyPriority", move |vm, _, _| {
        Ok(JsValue::from_string(vm.heap.alloc_string("")))
    });
    let h_item = Rc::clone(&host);
    define_host_method(vm, target, "item", move |vm, _, args| {
        let idx = vm.to_number(arg(args, 0)).unwrap_or(-1.0);
        if idx < 0.0 || !idx.is_finite() {
            return Ok(JsValue::from_string(vm.heap.alloc_string("")));
        }
        let i = idx as usize;
        if let Some(&name) = COMPUTED_STYLE_LONGHANDS.get(i) {
            return Ok(JsValue::from_string(vm.heap.alloc_string(name)));
        }
        let style = live_style_for(&h_item, nid, pseudo);
        let custom = computed_custom_keys(&style);
        let name = custom
            .get(i - COMPUTED_STYLE_LONGHANDS.len())
            .map(String::as_str)
            .unwrap_or("");
        Ok(JsValue::from_string(vm.heap.alloc_string(name)))
    });
    define_host_method(vm, target, "setProperty", move |vm, _, _| {
        Err(create_dom_exception(
            vm,
            "NoModificationAllowedError",
            "Computed style declaration is read-only",
        ))
    });
    define_host_method(vm, target, "removeProperty", move |vm, _, _| {
        Err(create_dom_exception(
            vm,
            "NoModificationAllowedError",
            "Computed style declaration is read-only",
        ))
    });

    let handler = vm.alloc_ordinary_object();
    let h_has = Rc::clone(&host);
    define_host_method(vm, handler, "has", move |vm, _, args| {
        let prop = vm.value_to_rust_string(arg(args, 1));
        if matches!(
            prop.as_str(),
            "getPropertyValue"
                | "getPropertyPriority"
                | "setProperty"
                | "removeProperty"
                | "item"
                | "length"
                | "cssText"
                | "cssFloat"
        ) {
            return Ok(JsValue::TRUE);
        }
        if let Ok(idx) = prop.parse::<usize>() {
            let style = live_style_for(&h_has, nid, pseudo);
            let total = COMPUTED_STYLE_LONGHANDS.len() + style.custom_properties.len();
            return Ok(JsValue::from_bool(idx < total));
        }
        let css_key = camel_to_kebab(&prop);
        Ok(JsValue::from_bool(
            PropertyIdKind::from_name(&css_key).is_some(),
        ))
    });

    let h_desc = Rc::clone(&host);
    define_host_method(
        vm,
        handler,
        "getOwnPropertyDescriptor",
        move |vm, _, args| {
            let prop = vm.value_to_rust_string(arg(args, 1));
            let val = if prop == "length" {
                let style = live_style_for(&h_desc, nid, pseudo);
                let total = COMPUTED_STYLE_LONGHANDS.len() + style.custom_properties.len();
                Some(JsValue::from_i32(total as i32))
            } else if let Ok(idx) = prop.parse::<usize>() {
                if let Some(&name) = COMPUTED_STYLE_LONGHANDS.get(idx) {
                    Some(JsValue::from_string(vm.heap.alloc_string(name)))
                } else {
                    let style = live_style_for(&h_desc, nid, pseudo);
                    let custom = computed_custom_keys(&style);
                    custom
                        .get(idx - COMPUTED_STYLE_LONGHANDS.len())
                        .map(|n| JsValue::from_string(vm.heap.alloc_string(n.as_str())))
                }
            } else {
                let css_key = camel_to_kebab(&prop);
                if PropertyIdKind::from_name(&css_key).is_some() {
                    let v = live_computed_property(&h_desc, nid, pseudo, &css_key);
                    Some(JsValue::from_string(vm.heap.alloc_string(v)))
                } else {
                    let k = vm.heap.intern_key(&prop);
                    arg(args, 0).as_object().and_then(|t_h| {
                        vm.heap
                            .object_get_own(t_h, k, &vm.shapes)
                            .and_then(|_| vm.get_property_by_key(t_h, k).ok())
                    })
                }
            };
            let Some(v) = val else {
                return Ok(JsValue::UNDEFINED);
            };
            let desc = vm.alloc_ordinary_object();
            let kv = vm.heap.intern_key("value");
            let kw = vm.heap.intern_key("writable");
            let ke = vm.heap.intern_key("enumerable");
            let kc = vm.heap.intern_key("configurable");
            vm.heap
                .object_define_data(desc, kv, v, PropertyAttrs::DEFAULT, &mut vm.shapes);
            vm.heap.object_define_data(
                desc,
                kw,
                JsValue::TRUE,
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
            vm.heap.object_define_data(
                desc,
                ke,
                JsValue::TRUE,
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
            vm.heap.object_define_data(
                desc,
                kc,
                JsValue::TRUE,
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
            Ok(JsValue::from_object(desc))
        },
    );

    let h_keys = Rc::clone(&host);
    define_host_method(vm, handler, "ownKeys", move |vm, _, _| {
        let arr = vm.alloc_array();
        let style = live_style_for(&h_keys, nid, pseudo);
        let custom = computed_custom_keys(&style);
        let total = COMPUTED_STYLE_LONGHANDS.len() + custom.len();
        for i in 0..total {
            let s = vm.heap.alloc_string(i.to_string());
            vm.heap.object_push_element(arr, JsValue::from_string(s));
        }
        for &name in COMPUTED_STYLE_LONGHANDS {
            let k1 = vm.heap.alloc_string(name);
            vm.heap.object_push_element(arr, JsValue::from_string(k1));
            if name.contains('-') {
                let camel = kebab_to_camel(name);
                let k2 = vm.heap.alloc_string(camel);
                vm.heap.object_push_element(arr, JsValue::from_string(k2));
            }
        }
        for name in custom {
            let s = vm.heap.alloc_string(name);
            vm.heap.object_push_element(arr, JsValue::from_string(s));
        }
        Ok(JsValue::from_object(arr))
    });

    let h_get = host;
    let wa_get = wa;
    define_host_method(vm, handler, "get", move |vm, _, args| {
        let key_val = arg(args, 1);
        if key_val.is_symbol() {
            return forward_symbol_get(vm, arg(args, 0), key_val);
        }
        let prop = vm.value_to_rust_string(key_val);
        if prop == "length" {
            if let Some(ref wa_ref) = wa_get {
                crate::css_animations_api::flush_if_styles_dirty(vm, &h_get, wa_ref);
            }
            let style = live_style_for(&h_get, nid, pseudo);
            let total = COMPUTED_STYLE_LONGHANDS.len() + style.custom_properties.len();
            return Ok(JsValue::from_i32(total as i32));
        }
        if let Ok(idx) = prop.parse::<usize>() {
            if let Some(&name) = COMPUTED_STYLE_LONGHANDS.get(idx) {
                return Ok(JsValue::from_string(vm.heap.alloc_string(name)));
            }
            let style = live_style_for(&h_get, nid, pseudo);
            let custom = computed_custom_keys(&style);
            if let Some(name) = custom.get(idx - COMPUTED_STYLE_LONGHANDS.len()) {
                return Ok(JsValue::from_string(vm.heap.alloc_string(name.as_str())));
            }
            return Ok(JsValue::UNDEFINED);
        }
        let k = vm.heap.intern_key(&prop);
        if let Some(t_h) = arg(args, 0).as_object() {
            if vm.heap.object_get_own(t_h, k, &vm.shapes).is_some() {
                return vm.get_property_by_key(t_h, k);
            }
        }
        if let Some(ref wa_ref) = wa_get {
            crate::css_animations_api::flush_if_styles_dirty(vm, &h_get, wa_ref);
        }
        let css_key = camel_to_kebab(&prop);
        let val = live_computed_property(&h_get, nid, pseudo, &css_key);
        Ok(JsValue::from_string(vm.heap.alloc_string(val)))
    });
    let proxy = vm.heap.alloc_object(JsObject::with_kind(
        ObjectKind::Proxy(rix_js::ProxyObject {
            target,
            handler,
            revoked: false,
        }),
        Some(vm.object_proto),
    ));
    JsValue::from_object(proxy)
}

fn format_rgba(c: ColorRgba) -> String {
    rix_css::format_legacy_rgba(c)
}

fn format_css_color(c: CssColor, current_color: ColorRgba, dark: bool) -> String {
    format_rgba(c.resolve(current_color, dark))
}

fn format_css_color_resolved(
    c: CssColor,
    cc_resolved: rix_css::ResolvedColor,
    dark: bool,
) -> String {
    match c {
        CssColor::CurrentColor => cc_resolved.to_computed_css_string(),
        other => format_rgba(other.resolve(cc_resolved.to_srgb_u8(), dark)),
    }
}

fn format_computed_color4(
    expr_opt: Option<&rix_css::CssColorExpr>,
    fallback: ColorRgba,
    current_resolved: rix_css::ResolvedColor,
    dark: bool,
    eval: &EvalContext,
) -> String {
    if let Some(expr) = expr_opt {
        expr.resolve(current_resolved, dark, eval)
            .to_computed_css_string()
    } else {
        format_rgba(fallback)
    }
}

fn format_num(v: f32) -> String {
    rix_css::values_math::format_math_f64(f64::from(v))
}

fn format_px(v: f32) -> String {
    let lim = rix_css::values_math::MAX_CSS_PX as f32;
    let c = if v.is_nan() { 0.0 } else { v.clamp(-lim, lim) };
    format!("{}px", format_num(c))
}

fn eval_ctx_for_style(style: &ComputedStyle) -> EvalContext {
    let mut ctx = style.text.eval_ctx;
    ctx.font_size_px = style.text.font_size_px;
    ctx
}

fn decompose_linear_calc(expr: &CalcExpr, eval: &EvalContext) -> Option<(f32, f32)> {
    match expr {
        CalcExpr::Length(v, unit) => Some((unit.resolve_px(*v, eval), 0.0)),
        CalcExpr::Percent(p) => Some((0.0, *p)),
        CalcExpr::Number(n) => Some((*n, 0.0)),
        CalcExpr::Sum(a, b) => {
            let (ax, ap) = decompose_linear_calc(a, eval)?;
            let (bx, bp) = decompose_linear_calc(b, eval)?;
            Some((ax + bx, ap + bp))
        }
        CalcExpr::Product(k, a) => {
            let (ax, ap) = decompose_linear_calc(a, eval)?;
            Some((*k * ax, *k * ap))
        }
        _ => None,
    }
}

fn format_lp_opts(lp: &LengthPercentage, eval: &EvalContext, nonneg: bool) -> String {
    match lp {
        LengthPercentage::Zero => "0px".into(),
        LengthPercentage::Length(_, _) => {
            let px = lp.resolve_px(0.0, eval);
            format_px(if nonneg { px.max(0.0) } else { px })
        }
        LengthPercentage::Percent(p) => format!("{}%", format_num(*p * 100.0)),
        LengthPercentage::Calc(expr) => {
            if let CalcExpr::Math(node) = expr.as_ref() {
                return node.serialize_computed(
                    eval,
                    None,
                    rix_css::values_math::ValueKind::LengthPercentage,
                    nonneg.then_some(0.0),
                    None,
                );
            }
            if let Some((px, pct)) = decompose_linear_calc(expr, eval) {
                if pct.abs() < 1e-6 {
                    return format_px(if nonneg { px.max(0.0) } else { px });
                }
                if px.abs() < 1e-4 {
                    return format!("{}%", format_num(pct * 100.0));
                }
                let pct_str = format!("{}%", format_num(pct * 100.0));
                return if px < 0.0 {
                    format!("calc({pct_str} - {})", format_px(-px))
                } else {
                    format!("calc({pct_str} + {})", format_px(px))
                };
            }
            let px = lp.resolve_px(0.0, eval);
            format_px(if nonneg { px.max(0.0) } else { px })
        }
    }
}

fn format_lp_used(
    lp: &LengthPercentage,
    eval: &EvalContext,
    nonneg: bool,
    basis_px: f32,
) -> Option<String> {
    if let LengthPercentage::Calc(expr) = lp {
        if let CalcExpr::Math(node) = expr.as_ref() {
            return Some(node.serialize_computed(
                eval,
                Some((f64::from(basis_px), false)),
                rix_css::values_math::ValueKind::LengthPercentage,
                nonneg.then_some(0.0),
                None,
            ));
        }
    }
    None
}

fn format_size_opts(s: &SizeValue, eval: &EvalContext, nonneg: bool) -> String {
    match s {
        SizeValue::Auto => "auto".into(),
        SizeValue::None => "none".into(),
        SizeValue::MinContent => "min-content".into(),
        SizeValue::MaxContent => "max-content".into(),
        SizeValue::FitContent(_) => "fit-content".into(),
        SizeValue::Stretch => "stretch".into(),
        SizeValue::Content => "content".into(),
        SizeValue::CalcSize(cs) => cs.serialize_computed(eval),
        SizeValue::Length(lp) => format_lp_opts(lp, eval, nonneg),
    }
}

fn format_lp(lp: &LengthPercentage, eval: &EvalContext) -> String {
    format_lp_opts(lp, eval, false)
}

fn format_lpa(v: &LengthPercentageAuto, eval: &EvalContext) -> String {
    match v {
        LengthPercentageAuto::Auto => "auto".into(),
        LengthPercentageAuto::Value(lp) => format_lp_opts(lp, eval, false),
    }
}

fn format_corner_radius(cr: &rix_css::CornerRadius, eval: &EvalContext) -> String {
    let x = format_lp_opts(&cr.x, eval, true);
    let y = format_lp_opts(&cr.y, eval, true);
    if x == y {
        x
    } else {
        format!("{x} {y}")
    }
}

fn format_line_style(ls: LineStyle) -> &'static str {
    match ls {
        LineStyle::None => "none",
        LineStyle::Hidden => "hidden",
        LineStyle::Dotted => "dotted",
        LineStyle::Dashed => "dashed",
        LineStyle::Solid => "solid",
        LineStyle::Double => "double",
        LineStyle::Groove => "groove",
        LineStyle::Ridge => "ridge",
        LineStyle::Inset => "inset",
        LineStyle::Outset => "outset",
        LineStyle::Auto => "auto",
    }
}

fn format_overflow(m: OverflowMode) -> &'static str {
    match m {
        OverflowMode::Visible => "visible",
        OverflowMode::Hidden => "hidden",
        OverflowMode::Clip => "clip",
        OverflowMode::Scroll => "scroll",
        OverflowMode::Auto => "auto",
    }
}

fn format_align(a: AlignValue) -> &'static str {
    a.as_css_str()
}

/// Serializes any computed CSS property from [`ComputedStyle`] according to CSSOM.
#[must_use]
pub fn format_computed_property(style: &ComputedStyle, prop: &str) -> String {
    format_computed_property_resolved(style, prop, None, false, None)
}

fn resolve_inset_computed(
    side: &LengthPercentageAuto,
    opposite: &LengthPercentageAuto,
    position: PositionMode,
    eval: &EvalContext,
    layout_box: Option<&ResolvedLayoutBox>,
    is_vertical: bool,
    used_side: impl FnOnce(&ResolvedLayoutBox) -> f32,
) -> String {
    let Some(lb) = layout_box else {
        return format_lpa(side, eval);
    };
    match position {
        PositionMode::Static => format_lpa(side, eval),
        PositionMode::Sticky => match side {
            LengthPercentageAuto::Auto => "auto".to_string(),
            LengthPercentageAuto::Value(lp) => {
                let basis = if is_vertical {
                    lb.scroll_container_content_size.height
                } else {
                    lb.scroll_container_content_size.width
                };
                format_lp_used(lp, eval, false, basis)
                    .unwrap_or_else(|| format_px(lp.resolve_px(basis, eval)))
            }
        },
        PositionMode::Relative => {
            let basis = if is_vertical {
                lb.parent_content_size.height
            } else {
                lb.parent_content_size.width
            };
            match (side, opposite) {
                (LengthPercentageAuto::Value(lp), _) => format_lp_used(lp, eval, false, basis)
                    .unwrap_or_else(|| format_px(lp.resolve_px(basis, eval))),
                (LengthPercentageAuto::Auto, LengthPercentageAuto::Value(opp)) => {
                    let opp_px = opp.resolve_px(basis, eval);
                    format_px(-opp_px)
                }
                (LengthPercentageAuto::Auto, LengthPercentageAuto::Auto) => "0px".to_string(),
            }
        }
        PositionMode::Absolute | PositionMode::Fixed => {
            let basis = if is_vertical {
                lb.cb_padding_size.height
            } else {
                lb.cb_padding_size.width
            };
            match side {
                LengthPercentageAuto::Value(lp) => format_lp_used(lp, eval, false, basis)
                    .unwrap_or_else(|| format_px(lp.resolve_px(basis, eval))),
                LengthPercentageAuto::Auto => format_px(used_side(lb)),
            }
        }
    }
}

pub fn format_computed_property_resolved(
    style: &ComputedStyle,
    prop: &str,
    layout_box: Option<&ResolvedLayoutBox>,
    parent_flex_grid: bool,
    ch_px: Option<f32>,
) -> String {
    if prop.starts_with("--") {
        let key = rix_core::intern_str(prop);
        if let Some(tokens) = style.custom_properties.get(&key) {
            return serialize_css_tokens(tokens);
        }
        return String::new();
    }
    let lower = prop.to_ascii_lowercase();
    let effective: &str = match PropertyIdKind::from_name(&lower) {
        Some(kind) if is_logical_property(kind) => {
            resolve_logical_to_physical(kind, style.text.writing_mode, style.text.direction)
                .css_name()
        }
        _ => lower.as_str(),
    };
    let cc = style.text.color;
    let dark = match style.text.color_scheme_mode {
        1 => false,
        2 => true,
        _ => style.color_scheme_dark,
    };
    let mut eval = eval_ctx_for_style(style);
    if ch_px.is_some() {
        eval.ch_px = ch_px;
    }
    let cc_resolved = style.text.color_computed.as_deref().map_or_else(
        || rix_css::ResolvedColor::from_rgba(cc),
        |e| e.resolve(rix_css::ResolvedColor::from_rgba(cc), dark, &eval),
    );

    // Computed shorthand serialization (both logical and physical)
    let fmt_pair = |a: &str, b: &str| -> String {
        let va = format_computed_property_resolved(style, a, layout_box, parent_flex_grid, ch_px);
        let vb = format_computed_property_resolved(style, b, layout_box, parent_flex_grid, ch_px);
        collapse_2_sides_str(&va, &vb)
    };
    let fmt_quad = |t: &str, r: &str, b: &str, l: &str| -> String {
        let vt = format_computed_property_resolved(style, t, layout_box, parent_flex_grid, ch_px);
        let vr = format_computed_property_resolved(style, r, layout_box, parent_flex_grid, ch_px);
        let vb = format_computed_property_resolved(style, b, layout_box, parent_flex_grid, ch_px);
        let vl = format_computed_property_resolved(style, l, layout_box, parent_flex_grid, ch_px);
        collapse_4_sides_str(&vt, &vr, &vb, &vl)
    };
    let fmt_border_side = |prefix: &str| -> String {
        let w = format_computed_property_resolved(
            style,
            &format!("{prefix}-width"),
            layout_box,
            parent_flex_grid,
            ch_px,
        );
        let s = format_computed_property_resolved(
            style,
            &format!("{prefix}-style"),
            layout_box,
            parent_flex_grid,
            ch_px,
        );
        let c = format_computed_property_resolved(
            style,
            &format!("{prefix}-color"),
            layout_box,
            parent_flex_grid,
            ch_px,
        );
        format!("{w} {s} {c}")
    };

    match effective {
        "margin" => fmt_quad("margin-top", "margin-right", "margin-bottom", "margin-left"),
        "margin-block" => fmt_pair("margin-block-start", "margin-block-end"),
        "margin-inline" => fmt_pair("margin-inline-start", "margin-inline-end"),
        "padding" => fmt_quad(
            "padding-top",
            "padding-right",
            "padding-bottom",
            "padding-left",
        ),
        "padding-block" => fmt_pair("padding-block-start", "padding-block-end"),
        "padding-inline" => fmt_pair("padding-inline-start", "padding-inline-end"),
        "inset" => fmt_quad("top", "right", "bottom", "left"),
        "inset-block" => fmt_pair("inset-block-start", "inset-block-end"),
        "inset-inline" => fmt_pair("inset-inline-start", "inset-inline-end"),
        "border-width" => fmt_quad(
            "border-top-width",
            "border-right-width",
            "border-bottom-width",
            "border-left-width",
        ),
        "border-block-width" => fmt_pair("border-block-start-width", "border-block-end-width"),
        "border-inline-width" => fmt_pair("border-inline-start-width", "border-inline-end-width"),
        "border-style" => fmt_quad(
            "border-top-style",
            "border-right-style",
            "border-bottom-style",
            "border-left-style",
        ),
        "border-block-style" => fmt_pair("border-block-start-style", "border-block-end-style"),
        "border-inline-style" => fmt_pair("border-inline-start-style", "border-inline-end-style"),
        "border-color" => fmt_quad(
            "border-top-color",
            "border-right-color",
            "border-bottom-color",
            "border-left-color",
        ),
        "border-block-color" => fmt_pair("border-block-start-color", "border-block-end-color"),
        "border-inline-color" => fmt_pair("border-inline-start-color", "border-inline-end-color"),
        "border-top" => fmt_border_side("border-top"),
        "border-right" => fmt_border_side("border-right"),
        "border-bottom" => fmt_border_side("border-bottom"),
        "border-left" => fmt_border_side("border-left"),
        "border-block-start" => fmt_border_side("border-block-start"),
        "border-block-end" => fmt_border_side("border-block-end"),
        "border-inline-start" => fmt_border_side("border-inline-start"),
        "border-inline-end" => fmt_border_side("border-inline-end"),
        "border-block" => {
            let s = fmt_border_side("border-block-start");
            let e = fmt_border_side("border-block-end");
            if s == e {
                s
            } else {
                String::new()
            }
        }
        "border-inline" => {
            let s = fmt_border_side("border-inline-start");
            let e = fmt_border_side("border-inline-end");
            if s == e {
                s
            } else {
                String::new()
            }
        }
        "border" => {
            let t = fmt_border_side("border-top");
            let r = fmt_border_side("border-right");
            let b = fmt_border_side("border-bottom");
            let l = fmt_border_side("border-left");
            if t == r && r == b && b == l {
                t
            } else {
                String::new()
            }
        }
        "border-radius" => fmt_quad(
            "border-top-left-radius",
            "border-top-right-radius",
            "border-bottom-right-radius",
            "border-bottom-left-radius",
        ),
        "overscroll-behavior" => fmt_pair("overscroll-behavior-x", "overscroll-behavior-y"),
        "writing-mode" => style.text.writing_mode.as_css_str().into(),
        "caption-side" => style.text.caption_side.as_css_str().into(),
        "resize" => style.visual.resize.as_css_str().into(),
        "overscroll-behavior-x" => style.visual.overscroll_behavior_x.as_css_str().into(),
        "overscroll-behavior-y" => style.visual.overscroll_behavior_y.as_css_str().into(),
        "scroll-behavior" => style.visual.scroll_behavior.as_css_str().into(),
        "content-visibility" => style.visual.content_visibility.as_css_str().into(),
        "zoom" => format!("{}", style.visual.zoom),
        "display" => style.display.to_css_string().into(),
        "overlay" => match style.overlay {
            rix_css::OverlayMode::None => "none",
            rix_css::OverlayMode::Auto => "auto",
        }
        .into(),
        "reading-flow" => match style
            .effects
            .as_ref()
            .map_or(rix_css::ReadingFlowMode::Normal, |e| e.reading_flow)
        {
            rix_css::ReadingFlowMode::Normal => "normal",
            rix_css::ReadingFlowMode::FlexVisual => "flex-visual",
            rix_css::ReadingFlowMode::FlexFlow => "flex-flow",
            rix_css::ReadingFlowMode::GridRows => "grid-rows",
            rix_css::ReadingFlowMode::GridColumns => "grid-columns",
            rix_css::ReadingFlowMode::GridOrder => "grid-order",
            rix_css::ReadingFlowMode::GridLanesFlow => "grid-lanes-flow",
            rix_css::ReadingFlowMode::GridLanesTrack => "grid-lanes-track",
            rix_css::ReadingFlowMode::GridLanesOrder => "grid-lanes-order",
            rix_css::ReadingFlowMode::SourceOrder => "source-order",
        }
        .into(),
        "reading-order" => style
            .effects
            .as_ref()
            .map_or(0, |e| e.reading_order)
            .to_string(),
        "text-justify" => match style.text.text_justify {
            rix_css::TextJustifyMode::Auto => "auto",
            rix_css::TextJustifyMode::None => "none",
            rix_css::TextJustifyMode::InterWord => "inter-word",
            rix_css::TextJustifyMode::InterCharacter => "inter-character",
        }
        .into(),
        "text-align-last" => match style.text.text_align_last {
            rix_css::TextAlignLastMode::Auto => "auto",
            rix_css::TextAlignLastMode::Start => "start",
            rix_css::TextAlignLastMode::End => "end",
            rix_css::TextAlignLastMode::Left => "left",
            rix_css::TextAlignLastMode::Right => "right",
            rix_css::TextAlignLastMode::Center => "center",
            rix_css::TextAlignLastMode::Justify => "justify",
            rix_css::TextAlignLastMode::MatchParent => "match-parent",
        }
        .into(),
        "hyphenate-character" => match &style.text.hyphenate_character {
            Some(s) => rix_css::substitution::quote_css_string(s.as_str()),
            None => "auto".into(),
        },
        "hyphenate-limit-lines" => match style.text.hyphenate_limit_lines {
            Some(n) => n.to_string(),
            None => "no-limit".into(),
        },
        "hyphenate-limit-chars" => style.text.hyphenate_limit_chars.serialize(),
        "hyphens" => style.text.hyphens.as_str().into(),
        "line-break" => style.text.line_break.as_str().into(),
        "white-space-collapse" => style.text.white_space_collapse.as_str().into(),
        "white-space-trim" => style.white_space_trim.serialize(),
        "text-wrap" => rix_css::text_css::serialize_text_wrap(
            style.text.text_wrap_mode,
            style.text.text_wrap_style,
        ),
        "text-wrap-mode" => style.text.text_wrap_mode.as_str().into(),
        "text-wrap-style" => style.text.text_wrap_style.as_str().into(),
        "text-spacing" => rix_css::text_css::serialize_text_spacing(
            style.text.text_spacing_trim,
            style.text.text_autospace,
        ),
        "text-spacing-trim" => style.text.text_spacing_trim.as_str().into(),
        "text-autospace" => style.text.text_autospace.serialize(),
        "text-autospace-size" => {
            rix_css::values_math::format_math_f64(f64::from(style.text.text_autospace_size))
        }
        "hanging-punctuation" => style.text.hanging_punctuation.serialize_canonical(),
        "text-fit" => style.text.text_fit.serialize_computed(),
        "text-group-align" => style.text_group_align.as_str().into(),
        "word-space-transform" => style.text.word_space_transform.as_str().into(),
        "wrap-inside" => style.wrap_inside.as_str().into(),
        "contain" => {
            let bits = style.effects.as_ref().map_or(0, |e| e.contain);
            if bits == 0 {
                "none".into()
            } else if bits == 0x1D {
                "strict".into()
            } else if bits == 0x1C {
                "content".into()
            } else {
                let mut parts = Vec::new();
                if bits & 0x01 != 0 {
                    parts.push("size");
                }
                if bits & 0x02 != 0 {
                    parts.push("inline-size");
                }
                if bits & 0x04 != 0 {
                    parts.push("layout");
                }
                if bits & 0x08 != 0 {
                    parts.push("style");
                }
                if bits & 0x10 != 0 {
                    parts.push("paint");
                }
                parts.join(" ")
            }
        }
        "container-type" => {
            let bits = style.effects.as_ref().map_or(0, |e| e.container_type);
            if bits == 0 {
                "normal".into()
            } else {
                let mut parts = Vec::new();
                if bits & 0x01 != 0 {
                    parts.push("size");
                }
                if bits & 0x02 != 0 {
                    parts.push("inline-size");
                }
                if bits & 0x04 != 0 {
                    parts.push("scroll-state");
                }
                parts.join(" ")
            }
        }
        "container-name" => {
            let names = style
                .effects
                .as_ref()
                .map(|e| e.container_name.as_slice())
                .unwrap_or(&[]);
            if names.is_empty() {
                "none".into()
            } else {
                names
                    .iter()
                    .map(|n| n.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        }
        "container" => {
            let name = format_computed_property(style, "container-name");
            let ty = format_computed_property(style, "container-type");
            if ty == "normal" {
                name
            } else {
                format!("{name} / {ty}")
            }
        }
        "position" => match style.position {
            PositionMode::Static => "static",
            PositionMode::Relative => "relative",
            PositionMode::Absolute => "absolute",
            PositionMode::Fixed => "fixed",
            PositionMode::Sticky => "sticky",
        }
        .into(),
        "float" => match style.float_mode {
            FloatMode::None => "none",
            FloatMode::Left => "left",
            FloatMode::Right => "right",
            FloatMode::InlineStart => "inline-start",
            FloatMode::InlineEnd => "inline-end",
        }
        .into(),
        "clear" => match style.clear_mode {
            ClearMode::None => "none",
            ClearMode::Left => "left",
            ClearMode::Right => "right",
            ClearMode::Both => "both",
            ClearMode::InlineStart => "inline-start",
            ClearMode::InlineEnd => "inline-end",
        }
        .into(),
        "box-sizing" => match style.box_sizing {
            BoxSizingMode::ContentBox => "content-box",
            BoxSizingMode::BorderBox => "border-box",
        }
        .into(),
        "overflow-x" => format_overflow(style.overflow.x).into(),
        "overflow-y" => format_overflow(style.overflow.y).into(),
        "overflow" => {
            if style.overflow.x == style.overflow.y {
                format_overflow(style.overflow.x).into()
            } else {
                format!(
                    "{} {}",
                    format_overflow(style.overflow.x),
                    format_overflow(style.overflow.y)
                )
            }
        }
        "visibility" => match style.visibility {
            VisibilityMode::Visible => "visible",
            VisibilityMode::Hidden => "hidden",
            VisibilityMode::Collapse => "collapse",
        }
        .into(),
        "pointer-events" => match style.pointer_events {
            PointerEventsMode::Auto => "auto",
            PointerEventsMode::None => "none",
        }
        .into(),
        "cursor" => match style.cursor {
            CursorKind::Auto => "auto",
            CursorKind::Default => "default",
            CursorKind::Pointer => "pointer",
            CursorKind::Text => "text",
            CursorKind::VerticalText => "vertical-text",
            CursorKind::Move => "move",
            CursorKind::AllScroll => "all-scroll",
            CursorKind::NotAllowed => "not-allowed",
            CursorKind::NoDrop => "no-drop",
            CursorKind::Grab => "grab",
            CursorKind::Grabbing => "grabbing",
            CursorKind::EResize => "e-resize",
            CursorKind::NeResize => "ne-resize",
            CursorKind::NwResize => "nw-resize",
            CursorKind::NResize => "n-resize",
            CursorKind::SeResize => "se-resize",
            CursorKind::SwResize => "sw-resize",
            CursorKind::SResize => "s-resize",
            CursorKind::WResize => "w-resize",
            CursorKind::EwResize => "ew-resize",
            CursorKind::NsResize => "ns-resize",
            CursorKind::NeswResize => "nesw-resize",
            CursorKind::NwseResize => "nwse-resize",
            CursorKind::ColResize => "col-resize",
            CursorKind::RowResize => "row-resize",
            CursorKind::Crosshair => "crosshair",
            CursorKind::Wait => "wait",
            CursorKind::Progress => "progress",
            CursorKind::Help => "help",
            CursorKind::ContextMenu => "context-menu",
            CursorKind::Cell => "cell",
            CursorKind::Alias => "alias",
            CursorKind::Copy => "copy",
            CursorKind::ZoomIn => "zoom-in",
            CursorKind::ZoomOut => "zoom-out",
            CursorKind::None => "none",
        }
        .into(),
        "z-index" => match style.z_index {
            ZIndexValue::Auto => "auto".into(),
            ZIndexValue::Integer(n) => n.to_string(),
        },
        "opacity" => format_num(style.opacity),
        "color" => format_computed_color4(
            style.text.color_computed.as_deref(),
            style.text.color,
            cc_resolved,
            dark,
            &eval,
        ),
        "background-color" => format_computed_color4(
            style.visual.background_color_computed.as_deref(),
            style.visual.background_color,
            cc_resolved,
            dark,
            &eval,
        ),
        "font-size" => format_px(style.text.font_size_px),
        "font-weight" => format_num(style.text.font_weight),
        "font-style" => {
            if let Some(n) = style
                .effects
                .as_ref()
                .and_then(|e| e.font_style_oblique_math.as_ref())
            {
                let a = n.serialize_computed(
                    &eval,
                    None,
                    rix_css::values_math::ValueKind::Angle,
                    Some(-90.0),
                    Some(90.0),
                );
                return if a == "14deg" {
                    "oblique".to_string()
                } else {
                    format!("oblique {a}")
                };
            }
            match style.text.font_style {
                FontStyleMode::Normal => "normal".into(),
                FontStyleMode::Italic => "italic".into(),
                FontStyleMode::Oblique(deg) => {
                    if (deg - 14.0).abs() < 1e-4 {
                        "oblique".into()
                    } else {
                        format!("oblique {}deg", format_num(deg))
                    }
                }
            }
        }
        "font-family" => style
            .text
            .font_family
            .iter()
            .map(|f| f.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        "line-height" => match style.text.line_height {
            LineHeightValue::Normal => "normal".into(),
            LineHeightValue::Number(n) => format_px(n * style.text.font_size_px),
            LineHeightValue::Length(ref lp) => {
                format_lp_used(lp, &eval, true, style.text.font_size_px).unwrap_or_else(|| {
                    format_px(lp.resolve_px(style.text.font_size_px, &eval).max(0.0))
                })
            }
        },
        "letter-spacing" => {
            let s = format_lp(&style.text.letter_spacing, &eval);
            if s == "0px" {
                "normal".into()
            } else {
                s
            }
        }
        "word-spacing" => format_lp(&style.text.word_spacing, &eval),
        "text-indent" => {
            let mut s = format_lp(&style.text.text_indent, &eval);
            if style.text.text_indent_hanging {
                s.push_str(" hanging");
            }
            if style.text.text_indent_each_line {
                s.push_str(" each-line");
            }
            s
        }
        "text-align" | "text-align-all" => match style.text.text_align {
            TextAlignMode::Start => "start",
            TextAlignMode::End => "end",
            TextAlignMode::Left => "left",
            TextAlignMode::Right => "right",
            TextAlignMode::Center => "center",
            TextAlignMode::Justify => "justify",
            TextAlignMode::MatchParent => "match-parent",
        }
        .into(),
        "text-transform" => style.text.text_transform_flags.serialize(),
        "white-space" => rix_css::text_css::serialize_white_space(
            style.text.white_space_collapse,
            style.text.text_wrap_mode,
            style.white_space_trim,
        ),
        "word-break" => match style.text.word_break {
            WordBreakMode::Normal => "normal",
            WordBreakMode::BreakAll => "break-all",
            WordBreakMode::KeepAll => "keep-all",
            WordBreakMode::Manual => "manual",
            WordBreakMode::AutoPhrase => "auto-phrase",
            WordBreakMode::BreakWord => "break-word",
        }
        .into(),
        "overflow-wrap" | "word-wrap" => match style.text.overflow_wrap {
            rix_css::OverflowWrapMode::Normal => "normal",
            rix_css::OverflowWrapMode::BreakWord => "break-word",
            rix_css::OverflowWrapMode::Anywhere => "anywhere",
        }
        .into(),
        "direction" => match style.text.direction {
            DirectionMode::Ltr => "ltr",
            DirectionMode::Rtl => "rtl",
        }
        .into(),
        "user-select" => match style.visual.user_select {
            UserSelectMode::Auto => "auto",
            UserSelectMode::None => "none",
            UserSelectMode::Text => "text",
            UserSelectMode::All => "all",
            UserSelectMode::Contain => "contain",
        }
        .into(),
        "width" => {
            if let Some(lb) = layout_box {
                if style.display != DisplayMode::Inline {
                    if let SizeValue::Length(ref lp) = style.box_model.width {
                        if let Some(s) =
                            format_lp_used(lp, &eval, true, lb.parent_content_size.width)
                        {
                            return s;
                        }
                    }
                    let w = if style.box_sizing == BoxSizingMode::BorderBox {
                        lb.border_size.width
                    } else {
                        lb.content_size.width
                    };
                    return format_px(w.max(0.0));
                }
            }
            format_size_opts(&style.box_model.width, &eval, true)
        }
        "height" => {
            if let Some(lb) = layout_box {
                if style.display != DisplayMode::Inline {
                    if let SizeValue::Length(ref lp) = style.box_model.height {
                        if let Some(s) =
                            format_lp_used(lp, &eval, true, lb.parent_content_size.height)
                        {
                            return s;
                        }
                    }
                    let h = if style.box_sizing == BoxSizingMode::BorderBox {
                        lb.border_size.height
                    } else {
                        lb.content_size.height
                    };
                    return format_px(h.max(0.0));
                }
            }
            format_size_opts(&style.box_model.height, &eval, true)
        }
        "min-width" => match &style.box_model.min_width {
            SizeValue::Auto if !parent_flex_grid => "0px".into(),
            other => format_size_opts(other, &eval, true),
        },
        "min-height" => match &style.box_model.min_height {
            SizeValue::Auto if !parent_flex_grid => "0px".into(),
            other => format_size_opts(other, &eval, true),
        },
        "max-width" => format_size_opts(&style.box_model.max_width, &eval, true),
        "max-height" => format_size_opts(&style.box_model.max_height, &eval, true),
        "margin-top" => layout_box.map_or_else(
            || format_lpa(&style.box_model.margin.top, &eval),
            |lb| match &style.box_model.margin.top {
                LengthPercentageAuto::Value(lp) => {
                    format_lp_used(lp, &eval, false, lb.parent_content_size.width)
                        .unwrap_or_else(|| format_px(lb.margin.top))
                }
                LengthPercentageAuto::Auto => format_px(lb.margin.top),
            },
        ),
        "margin-right" => layout_box.map_or_else(
            || format_lpa(&style.box_model.margin.right, &eval),
            |lb| match &style.box_model.margin.right {
                LengthPercentageAuto::Value(lp) => {
                    format_lp_used(lp, &eval, false, lb.parent_content_size.width)
                        .unwrap_or_else(|| format_px(lb.margin.right))
                }
                LengthPercentageAuto::Auto => format_px(lb.margin.right),
            },
        ),
        "margin-bottom" => layout_box.map_or_else(
            || format_lpa(&style.box_model.margin.bottom, &eval),
            |lb| match &style.box_model.margin.bottom {
                LengthPercentageAuto::Value(lp) => {
                    format_lp_used(lp, &eval, false, lb.parent_content_size.width)
                        .unwrap_or_else(|| format_px(lb.margin.bottom))
                }
                LengthPercentageAuto::Auto => format_px(lb.margin.bottom),
            },
        ),
        "margin-left" => layout_box.map_or_else(
            || format_lpa(&style.box_model.margin.left, &eval),
            |lb| match &style.box_model.margin.left {
                LengthPercentageAuto::Value(lp) => {
                    format_lp_used(lp, &eval, false, lb.parent_content_size.width)
                        .unwrap_or_else(|| format_px(lb.margin.left))
                }
                LengthPercentageAuto::Auto => format_px(lb.margin.left),
            },
        ),
        "padding-top" => layout_box.map_or_else(
            || format_lp_opts(&style.box_model.padding.top, &eval, true),
            |lb| {
                format_lp_used(
                    &style.box_model.padding.top,
                    &eval,
                    true,
                    lb.parent_content_size.width,
                )
                .unwrap_or_else(|| format_px(lb.padding.top.max(0.0)))
            },
        ),
        "padding-right" => layout_box.map_or_else(
            || format_lp_opts(&style.box_model.padding.right, &eval, true),
            |lb| {
                format_lp_used(
                    &style.box_model.padding.right,
                    &eval,
                    true,
                    lb.parent_content_size.width,
                )
                .unwrap_or_else(|| format_px(lb.padding.right.max(0.0)))
            },
        ),
        "padding-bottom" => layout_box.map_or_else(
            || format_lp_opts(&style.box_model.padding.bottom, &eval, true),
            |lb| {
                format_lp_used(
                    &style.box_model.padding.bottom,
                    &eval,
                    true,
                    lb.parent_content_size.width,
                )
                .unwrap_or_else(|| format_px(lb.padding.bottom.max(0.0)))
            },
        ),
        "padding-left" => layout_box.map_or_else(
            || format_lp_opts(&style.box_model.padding.left, &eval, true),
            |lb| {
                format_lp_used(
                    &style.box_model.padding.left,
                    &eval,
                    true,
                    lb.parent_content_size.width,
                )
                .unwrap_or_else(|| format_px(lb.padding.left.max(0.0)))
            },
        ),
        "top" => resolve_inset_computed(
            &style.box_model.inset.top,
            &style.box_model.inset.bottom,
            style.position,
            &eval,
            layout_box,
            true,
            |lb| lb.used_insets.top,
        ),
        "right" => resolve_inset_computed(
            &style.box_model.inset.right,
            &style.box_model.inset.left,
            style.position,
            &eval,
            layout_box,
            false,
            |lb| lb.used_insets.right,
        ),
        "bottom" => resolve_inset_computed(
            &style.box_model.inset.bottom,
            &style.box_model.inset.top,
            style.position,
            &eval,
            layout_box,
            true,
            |lb| lb.used_insets.bottom,
        ),
        "left" => resolve_inset_computed(
            &style.box_model.inset.left,
            &style.box_model.inset.right,
            style.position,
            &eval,
            layout_box,
            false,
            |lb| lb.used_insets.left,
        ),
        "border-top-width" => format_px(style.box_model.border_width.top),
        "border-right-width" => format_px(style.box_model.border_width.right),
        "border-bottom-width" => format_px(style.box_model.border_width.bottom),
        "border-left-width" => format_px(style.box_model.border_width.left),
        "border-top-style" => format_line_style(style.visual.border_style.top).into(),
        "border-right-style" => format_line_style(style.visual.border_style.right).into(),
        "border-bottom-style" => format_line_style(style.visual.border_style.bottom).into(),
        "border-left-style" => format_line_style(style.visual.border_style.left).into(),
        "border-top-color" => style.visual.border_color_computed[0]
            .as_deref()
            .map_or_else(
                || format_css_color_resolved(style.visual.border_color.top, cc_resolved, dark),
                |e| e.resolve(cc_resolved, dark, &eval).to_computed_css_string(),
            ),
        "border-right-color" => style.visual.border_color_computed[1]
            .as_deref()
            .map_or_else(
                || format_css_color_resolved(style.visual.border_color.right, cc_resolved, dark),
                |e| e.resolve(cc_resolved, dark, &eval).to_computed_css_string(),
            ),
        "border-bottom-color" => style.visual.border_color_computed[2]
            .as_deref()
            .map_or_else(
                || format_css_color_resolved(style.visual.border_color.bottom, cc_resolved, dark),
                |e| e.resolve(cc_resolved, dark, &eval).to_computed_css_string(),
            ),
        "border-left-color" => style.visual.border_color_computed[3]
            .as_deref()
            .map_or_else(
                || format_css_color_resolved(style.visual.border_color.left, cc_resolved, dark),
                |e| e.resolve(cc_resolved, dark, &eval).to_computed_css_string(),
            ),
        "border-top-left-radius" => {
            format_corner_radius(&style.visual.border_radius.top_left, &eval)
        }
        "border-top-right-radius" => {
            format_corner_radius(&style.visual.border_radius.top_right, &eval)
        }
        "border-bottom-right-radius" => {
            format_corner_radius(&style.visual.border_radius.bottom_right, &eval)
        }
        "border-bottom-left-radius" => {
            format_corner_radius(&style.visual.border_radius.bottom_left, &eval)
        }
        "outline-color" => style.visual.outline_color_computed.as_deref().map_or_else(
            || format_css_color_resolved(style.visual.outline_color, cc_resolved, dark),
            |e| e.resolve(cc_resolved, dark, &eval).to_computed_css_string(),
        ),
        "outline-style" => format_line_style(style.visual.outline_style).into(),
        "outline-width" => format_px(style.visual.outline_width),
        "outline-offset" => format_lp(&style.visual.outline_offset, &eval),
        "appearance" | "-webkit-appearance" | "-moz-appearance" => {
            style.visual.appearance.as_css_str().into()
        }
        "accent-color" => match style.text.accent_color {
            None => "auto".into(),
            Some(c) => format_css_color_resolved(c, cc_resolved, dark),
        },
        "vertical-align" => match style.box_model.vertical_align {
            VerticalAlignMode::Baseline => "baseline".into(),
            VerticalAlignMode::Top => "top".into(),
            VerticalAlignMode::Middle => "middle".into(),
            VerticalAlignMode::Bottom => "bottom".into(),
            VerticalAlignMode::TextTop => "text-top".into(),
            VerticalAlignMode::TextBottom => "text-bottom".into(),
            VerticalAlignMode::Sub => "sub".into(),
            VerticalAlignMode::Super => "super".into(),
            VerticalAlignMode::LengthPx(px) => match style.box_model.vertical_align_lp.as_ref() {
                Some(lp) => format_lp(lp, &eval),
                None => format_px(px),
            },
        },
        "text-shadow" => {
            if style.text.text_shadow.is_empty() {
                "none".into()
            } else {
                style
                    .text
                    .text_shadow
                    .iter()
                    .map(|sh| {
                        let c = format_css_color(sh.color, cc, dark);
                        let ox = format_lp(&sh.offset_x, &eval);
                        let oy = format_lp(&sh.offset_y, &eval);
                        let bl = format_lp_opts(&sh.blur, &eval, true);
                        format!("{c} {ox} {oy} {bl}")
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        }
        "text-decoration-inset" => rix_css::anim_css::serialize_text_decoration_inset_computed(
            &style.visual.text_decoration_inset,
            &eval,
        ),
        "text-overflow" => match style.box_model.text_overflow {
            TextOverflowMode::Clip => "clip",
            TextOverflowMode::Ellipsis => "ellipsis",
        }
        .into(),
        "border-collapse" => match style.text.border_collapse {
            BorderCollapseMode::Separate => "separate",
            BorderCollapseMode::Collapse => "collapse",
        }
        .into(),
        "border-spacing" => {
            let x = format_lp_opts(&style.text.border_spacing_x, &eval, true);
            let y = format_lp_opts(&style.text.border_spacing_y, &eval, true);
            collapse_2_sides_str(&x, &y)
        }
        "empty-cells" => match style.text.empty_cells {
            EmptyCellsMode::Show => "show",
            EmptyCellsMode::Hide => "hide",
        }
        .into(),
        "box-shadow" => match style.effects.as_ref().map(|e| e.box_shadow.as_slice()) {
            Some(list) if !list.is_empty() => list
                .iter()
                .map(|sh| {
                    let c = format_css_color(sh.color, cc, dark);
                    let ox = format_lp(&sh.offset_x, &eval);
                    let oy = format_lp(&sh.offset_y, &eval);
                    let bl = format_lp_opts(&sh.blur, &eval, true);
                    let sp = format_lp(&sh.spread, &eval);
                    if sh.inset {
                        format!("{c} {ox} {oy} {bl} {sp} inset")
                    } else {
                        format!("{c} {ox} {oy} {bl} {sp}")
                    }
                })
                .collect::<Vec<_>>()
                .join(", "),
            _ => "none".into(),
        },
        "filter" => format_computed_filter_list(
            style.effects.as_ref().map(|e| e.filter.as_slice()),
            &eval,
            cc,
            dark,
        ),
        "backdrop-filter" => format_computed_filter_list(
            style.effects.as_ref().map(|e| e.backdrop_filter.as_slice()),
            &eval,
            cc,
            dark,
        ),
        "color-interpolation-filters" => style
            .text
            .color_interpolation_filters
            .to_css_string()
            .into(),
        "table-layout" => match style.box_model.table_layout {
            TableLayoutMode::Auto => "auto",
            TableLayoutMode::Fixed => "fixed",
        }
        .into(),
        "content" => {
            if let Some(raw_img) = style
                .effects
                .as_ref()
                .and_then(|e| e.content_image_raw.as_deref())
            {
                raw_img.to_string()
            } else {
                match &style.box_model.content {
                    Some(c) => format!("\"{}\"", c.as_str()),
                    None => "normal".into(),
                }
            }
        }
        "column-rule-color" => style
            .visual
            .column_rule_color_computed
            .as_deref()
            .map_or_else(
                || format_css_color_resolved(style.visual.column_rule_color, cc_resolved, dark),
                |e| e.resolve(cc_resolved, dark, &eval).to_computed_css_string(),
            ),
        "caret-color" => style.text.caret_color_computed.as_deref().map_or_else(
            || {
                let c = style.text.caret_color.unwrap_or(CssColor::CurrentColor);
                format_css_color_resolved(c, cc_resolved, dark)
            },
            |e| e.resolve(cc_resolved, dark, &eval).to_computed_css_string(),
        ),
        "fill" => match style.text.svg_fill {
            Some(Some(c)) => format_css_color_resolved(c, cc_resolved, dark),
            Some(None) => "none".into(),
            None => "rgb(0, 0, 0)".into(),
        },
        "stroke" => match style.text.svg_stroke {
            Some(Some(c)) => format_css_color_resolved(c, cc_resolved, dark),
            Some(None) | None => "none".into(),
        },
        "fill-rule" => if style.text.svg_fill_rule_even_odd {
            "evenodd"
        } else {
            "nonzero"
        }
        .into(),
        "clip-rule" => if style.text.svg_clip_rule_even_odd {
            "evenodd"
        } else {
            "nonzero"
        }
        .into(),
        "fill-opacity" => format_num(style.text.svg_fill_opacity),
        "stroke-opacity" => format_num(style.text.svg_stroke_opacity),
        "stroke-miterlimit" => format_num(style.text.svg_stroke_miterlimit),
        "stroke-linecap" => style.text.svg_stroke_linecap.as_css_str().into(),
        "stroke-linejoin" => style.text.svg_stroke_linejoin.as_css_str().into(),
        "text-anchor" => style.text.svg_text_anchor.as_css_str().into(),
        "dominant-baseline" => style.text.svg_dominant_baseline.as_css_str().into(),
        "font-stretch" => format!("{}%", format_num(style.text.font_stretch_pct)),
        "font-size-adjust" => match style.text.font_size_adjust {
            Some(n) => format_num(n),
            None => "none".into(),
        },
        "flood-opacity" => format_num(
            style
                .effects
                .as_ref()
                .and_then(|e| e.svg_flood_opacity)
                .unwrap_or(1.0),
        ),
        "stop-opacity" => format_num(
            style
                .effects
                .as_ref()
                .and_then(|e| e.svg_stop_opacity)
                .unwrap_or(1.0),
        ),
        "text-decoration-style" => style.visual.text_decoration_style.as_css_str().into(),
        "background-attachment" => style.visual.background_attachment.as_str().to_string(),
        "background-clip" => match style.visual.background_clip {
            rix_css::BackgroundClipMode::BorderBox => "border-box",
            rix_css::BackgroundClipMode::PaddingBox => "padding-box",
            rix_css::BackgroundClipMode::ContentBox => "content-box",
            rix_css::BackgroundClipMode::Text => "text",
        }
        .into(),
        "background-origin" => match style.visual.background_origin {
            rix_css::BackgroundClipMode::BorderBox => "border-box",
            rix_css::BackgroundClipMode::PaddingBox => "padding-box",
            rix_css::BackgroundClipMode::ContentBox => "content-box",
            rix_css::BackgroundClipMode::Text => "padding-box",
        }
        .into(),
        "list-style-image" => style
            .visual
            .list_style_image_raw
            .clone()
            .unwrap_or_else(|| "none".into()),
        "border-image-source" => style
            .visual
            .border_image_source_raw
            .clone()
            .unwrap_or_else(|| "none".into()),
        "perspective-origin" => {
            let (bw, bh) = layout_box.map_or((0.0, 0.0), |lb| {
                (lb.border_size.width, lb.border_size.height)
            });
            let (ox, oy) = style
                .effects
                .as_ref()
                .and_then(|e| e.perspective_origin.as_ref())
                .map_or(
                    (
                        LengthPercentage::Percent(0.5),
                        LengthPercentage::Percent(0.5),
                    ),
                    |(x, y)| (x.clone(), y.clone()),
                );
            format!(
                "{} {}",
                format_px(ox.resolve_px(bw, &eval)),
                format_px(oy.resolve_px(bh, &eval))
            )
        }
        "flex-direction" => match style.flex_grid.as_ref().map(|f| f.flex_direction) {
            Some(FlexDirectionMode::RowReverse) => "row-reverse",
            Some(FlexDirectionMode::Column) => "column",
            Some(FlexDirectionMode::ColumnReverse) => "column-reverse",
            _ => "row",
        }
        .into(),
        "flex-wrap" => match style.flex_grid.as_ref().map(|f| f.flex_wrap) {
            Some(FlexWrapMode::Wrap) => "wrap",
            Some(FlexWrapMode::WrapReverse) => "wrap-reverse",
            _ => "nowrap",
        }
        .into(),
        "flex" => {
            let g = format_num(
                style
                    .flex_grid
                    .as_ref()
                    .map_or(0.0, |f| f.flex_grow.max(0.0)),
            );
            let s = format_num(
                style
                    .flex_grid
                    .as_ref()
                    .map_or(1.0, |f| f.flex_shrink.max(0.0)),
            );
            let b = style.flex_grid.as_ref().map_or_else(
                || "auto".to_string(),
                |f| {
                    f.flex_basis_size.as_ref().map_or_else(
                        || match &f.flex_basis {
                            LengthPercentageAuto::Auto => "auto".to_string(),
                            LengthPercentageAuto::Value(lp) => {
                                format_size_opts(&SizeValue::Length(lp.clone()), &eval, true)
                            }
                        },
                        |sz| format_size_opts(sz, &eval, true),
                    )
                },
            );
            format!("{g} {s} {b}")
        }
        "flex-flow" => {
            let d = match style.flex_grid.as_ref().map(|f| f.flex_direction) {
                Some(FlexDirectionMode::RowReverse) => "row-reverse",
                Some(FlexDirectionMode::Column) => "column",
                Some(FlexDirectionMode::ColumnReverse) => "column-reverse",
                _ => "row",
            };
            let w = match style.flex_grid.as_ref().map(|f| f.flex_wrap) {
                Some(FlexWrapMode::Wrap) => "wrap",
                Some(FlexWrapMode::WrapReverse) => "wrap-reverse",
                _ => "nowrap",
            };
            format!("{d} {w}")
        }
        "flex-grow" => format_num(
            style
                .flex_grid
                .as_ref()
                .map_or(0.0, |f| f.flex_grow.max(0.0)),
        ),
        "flex-shrink" => format_num(
            style
                .flex_grid
                .as_ref()
                .map_or(1.0, |f| f.flex_shrink.max(0.0)),
        ),
        "flex-basis" => style.flex_grid.as_ref().map_or_else(
            || "auto".into(),
            |f| {
                f.flex_basis_size.as_ref().map_or_else(
                    || match &f.flex_basis {
                        LengthPercentageAuto::Auto => "auto".to_string(),
                        LengthPercentageAuto::Value(lp) => {
                            format_size_opts(&SizeValue::Length(lp.clone()), &eval, true)
                        }
                    },
                    |s| format_size_opts(s, &eval, true),
                )
            },
        ),
        "order" => style.flex_grid.as_ref().map_or(0, |f| f.order).to_string(),
        "align-items" => format_align(
            style
                .flex_grid
                .as_ref()
                .map_or(AlignValue::Normal, |f| f.align_items),
        )
        .into(),
        "align-self" => format_align(
            style
                .flex_grid
                .as_ref()
                .map_or(AlignValue::Auto, |f| f.align_self),
        )
        .into(),
        "align-content" => format_align(
            style
                .flex_grid
                .as_ref()
                .map_or(AlignValue::Normal, |f| f.align_content),
        )
        .into(),
        "justify-content" => format_align(
            style
                .flex_grid
                .as_ref()
                .map_or(AlignValue::Normal, |f| f.justify_content),
        )
        .into(),
        "justify-items" => format_align(
            style
                .flex_grid
                .as_ref()
                .map_or(AlignValue::Normal, |f| f.justify_items),
        )
        .into(),
        "justify-self" => format_align(
            style
                .flex_grid
                .as_ref()
                .map_or(AlignValue::Auto, |f| f.justify_self),
        )
        .into(),
        "row-gap" | "grid-row-gap" => style.flex_grid.as_ref().map_or_else(
            || "normal".into(),
            |f| {
                if f.row_gap_specified {
                    format_lp_opts(&f.row_gap, &eval, true)
                } else {
                    "normal".into()
                }
            },
        ),
        "column-gap" | "grid-column-gap" => style.flex_grid.as_ref().map_or_else(
            || "normal".into(),
            |f| {
                if f.column_gap_specified {
                    format_lp_opts(&f.column_gap, &eval, true)
                } else {
                    "normal".into()
                }
            },
        ),
        "grid-auto-flow" => rix_css::grid_css::serialize_grid_auto_flow(
            style
                .flex_grid
                .as_ref()
                .map_or(GridAutoFlowMode::Row, |f| f.grid_auto_flow),
        )
        .into(),
        "mix-blend-mode" => match style.effects.as_ref().map(|e| e.mix_blend_mode) {
            Some(MixBlendMode::Multiply) => "multiply",
            Some(MixBlendMode::Screen) => "screen",
            Some(MixBlendMode::Overlay) => "overlay",
            Some(MixBlendMode::Darken) => "darken",
            Some(MixBlendMode::Lighten) => "lighten",
            Some(MixBlendMode::ColorDodge) => "color-dodge",
            Some(MixBlendMode::ColorBurn) => "color-burn",
            Some(MixBlendMode::HardLight) => "hard-light",
            Some(MixBlendMode::SoftLight) => "soft-light",
            Some(MixBlendMode::Difference) => "difference",
            Some(MixBlendMode::Exclusion) => "exclusion",
            Some(MixBlendMode::Hue) => "hue",
            Some(MixBlendMode::Saturation) => "saturation",
            Some(MixBlendMode::Color) => "color",
            Some(MixBlendMode::Luminosity) => "luminosity",
            _ => "normal",
        }
        .into(),
        "color-scheme" => if style.color_scheme_dark {
            "dark"
        } else {
            "normal"
        }
        .into(),
        "interpolate-size" => match style.text.interpolate_size {
            rix_css::InterpolateSizeMode::NumericOnly => "numeric-only",
            rix_css::InterpolateSizeMode::AllowKeywords => "allow-keywords",
        }
        .into(),
        "transition-behavior" => match style.effects.as_ref().map(|e| e.transitions.as_slice()) {
            Some(list) if !list.is_empty() => list
                .iter()
                .map(|t| match t.behavior {
                    rix_css::TransitionBehaviorMode::Normal => "normal",
                    rix_css::TransitionBehaviorMode::AllowDiscrete => "allow-discrete",
                })
                .collect::<Vec<_>>()
                .join(", "),
            _ => "normal".into(),
        },
        "animation-composition" => match style.effects.as_ref().map(|e| e.animations.as_slice()) {
            Some(list) if !list.is_empty() => list
                .iter()
                .map(|a| match a.composition {
                    rix_css::AnimationCompositionMode::Replace => "replace",
                    rix_css::AnimationCompositionMode::Add => "add",
                    rix_css::AnimationCompositionMode::Accumulate => "accumulate",
                })
                .collect::<Vec<_>>()
                .join(", "),
            _ => "replace".into(),
        },
        "transition" => match style.effects.as_ref().map(|e| e.transitions.as_slice()) {
            Some(list) if !list.is_empty() => list
                .iter()
                .map(|t| {
                    let mut parts: Vec<String> = Vec::new();
                    let dur_s = t.duration_ms / 1000.0;
                    let del_s = t.delay_ms / 1000.0;
                    let is_def_timing = t.timing == rix_css::TimingFunction::Ease;
                    let is_def_beh = t.behavior == rix_css::TransitionBehaviorMode::Normal;
                    if t.property.as_str() != "all"
                        || (dur_s == 0.0 && del_s == 0.0 && is_def_timing && is_def_beh)
                    {
                        parts.push(t.property.as_str().to_string());
                    }
                    if dur_s != 0.0 || del_s != 0.0 {
                        parts.push(format!("{}s", format_num(dur_s)));
                    }
                    if !is_def_timing {
                        parts.push(t.timing.to_css_string());
                    }
                    if del_s != 0.0 {
                        parts.push(format!("{}s", format_num(del_s)));
                    }
                    if !is_def_beh {
                        parts.push("allow-discrete".to_string());
                    }
                    parts.join(" ")
                })
                .collect::<Vec<_>>()
                .join(", "),
            _ => "all".into(),
        },
        "transition-property" => match style.effects.as_ref().map(|e| e.transitions.as_slice()) {
            Some(list) if !list.is_empty() => list
                .iter()
                .map(|t| t.property.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            _ => "all".into(),
        },
        "transition-duration" => {
            if let Some(n) = style
                .effects
                .as_ref()
                .and_then(|e| e.transition_duration_math.as_ref())
            {
                return n.serialize_computed(
                    &eval,
                    None,
                    rix_css::values_math::ValueKind::Time,
                    Some(0.0),
                    None,
                );
            }
            match style.effects.as_ref().map(|e| e.transitions.as_slice()) {
                Some(list) if !list.is_empty() => list
                    .iter()
                    .map(|t| format!("{}s", format_num(t.duration_ms / 1000.0)))
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => "0s".into(),
            }
        }
        "transition-delay" => {
            if let Some(n) = style
                .effects
                .as_ref()
                .and_then(|e| e.transition_delay_math.as_ref())
            {
                return n.serialize_computed(
                    &eval,
                    None,
                    rix_css::values_math::ValueKind::Time,
                    None,
                    None,
                );
            }
            match style.effects.as_ref().map(|e| e.transitions.as_slice()) {
                Some(list) if !list.is_empty() => list
                    .iter()
                    .map(|t| format!("{}s", format_num(t.delay_ms / 1000.0)))
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => "0s".into(),
            }
        }
        "view-transition-name" => style
            .effects
            .as_ref()
            .and_then(|e| e.view_transition_name.as_ref())
            .map_or_else(|| "none".to_string(), |s| s.as_str().to_string()),
        "animation" => match style.effects.as_ref().map(|e| e.animations.as_slice()) {
            Some(list) if !list.is_empty() => rix_css::anim_css::serialize_animation_list(list),
            _ => "none".into(),
        },
        "animation-name" => match style.effects.as_ref().map(|e| e.animations.as_slice()) {
            Some(list) if !list.is_empty() => list
                .iter()
                .map(|a| rix_css::anim_css::serialize_animation_name_token(a.name.as_str()))
                .collect::<Vec<_>>()
                .join(", "),
            _ => "none".into(),
        },
        "animation-direction" => match style.effects.as_ref().map(|e| e.animations.as_slice()) {
            Some(list) if !list.is_empty() => list
                .iter()
                .map(|a| match a.direction {
                    rix_css::AnimationDirectionMode::Normal => "normal",
                    rix_css::AnimationDirectionMode::Reverse => "reverse",
                    rix_css::AnimationDirectionMode::Alternate => "alternate",
                    rix_css::AnimationDirectionMode::AlternateReverse => "alternate-reverse",
                })
                .collect::<Vec<_>>()
                .join(", "),
            _ => "normal".into(),
        },
        "animation-fill-mode" => match style.effects.as_ref().map(|e| e.animations.as_slice()) {
            Some(list) if !list.is_empty() => list
                .iter()
                .map(|a| match a.fill_mode {
                    rix_css::AnimationFillModeKind::None => "none",
                    rix_css::AnimationFillModeKind::Forwards => "forwards",
                    rix_css::AnimationFillModeKind::Backwards => "backwards",
                    rix_css::AnimationFillModeKind::Both => "both",
                })
                .collect::<Vec<_>>()
                .join(", "),
            _ => "none".into(),
        },
        "animation-play-state" => match style.effects.as_ref().map(|e| e.animations.as_slice()) {
            Some(list) if !list.is_empty() => list
                .iter()
                .map(|a| match a.play_state {
                    rix_css::AnimationPlayStateMode::Running => "running",
                    rix_css::AnimationPlayStateMode::Paused => "paused",
                })
                .collect::<Vec<_>>()
                .join(", "),
            _ => "running".into(),
        },
        "animation-timing-function" => {
            match style.effects.as_ref().map(|e| e.animations.as_slice()) {
                Some(list) if !list.is_empty() => list
                    .iter()
                    .map(|a| a.timing.to_css_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => "ease".into(),
            }
        }
        "animation-timeline" => {
            match style
                .effects
                .as_ref()
                .map(|e| e.animation_timelines.as_slice())
            {
                Some(list) if !list.is_empty() => list
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => "auto".into(),
            }
        }
        "animation-range-start" => {
            match style
                .effects
                .as_ref()
                .map(|e| e.animation_range_starts.as_slice())
            {
                Some(list) if !list.is_empty() => list
                    .iter()
                    .map(|pt| pt.serialize(false, Some(&eval)))
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => "normal".into(),
            }
        }
        "animation-range-end" => {
            match style
                .effects
                .as_ref()
                .map(|e| e.animation_range_ends.as_slice())
            {
                Some(list) if !list.is_empty() => list
                    .iter()
                    .map(|pt| pt.serialize(true, Some(&eval)))
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => "normal".into(),
            }
        }
        "animation-range" => {
            let starts = style
                .effects
                .as_ref()
                .map(|e| e.animation_range_starts.as_slice())
                .unwrap_or(&[]);
            let ends = style
                .effects
                .as_ref()
                .map(|e| e.animation_range_ends.as_slice())
                .unwrap_or(&[]);
            rix_css::anim_css::serialize_animation_range_shorthand(starts, ends, Some(&eval))
        }
        "background-image" => {
            if let Some(raw) = style.visual.background_image_raw.as_deref() {
                rix_css::substitution::parse_and_serialize_bg_image_fn_with_scheme(raw, dark, &eval)
                    .unwrap_or_else(|| raw.to_string())
            } else if !style.visual.background_layers.is_empty() {
                style
                    .visual
                    .background_layers
                    .iter()
                    .map(|l| match &l.image {
                        rix_css::BackgroundImageValue::None => "none".to_string(),
                        rix_css::BackgroundImageValue::Url(u) => {
                            format!(
                                "url({})",
                                rix_css::substitution::quote_css_string(u.as_str())
                            )
                        }
                        rix_css::BackgroundImageValue::LinearGradient { angle_deg, stops } => {
                            let mut parts = Vec::with_capacity(stops.len() + 1);
                            if (*angle_deg - 180.0).abs() > 0.01 {
                                parts.push(format!("{}deg", format_num(*angle_deg)));
                            }
                            for st in stops {
                                let col = format_css_color(st.color, style.text.color, dark);
                                if let Some(ref pos) = st.position {
                                    parts.push(format!("{col} {}", format_lp(pos, &eval)));
                                } else {
                                    parts.push(col);
                                }
                            }
                            format!("linear-gradient({})", parts.join(", "))
                        }
                        rix_css::BackgroundImageValue::RadialGradient {
                            shape_circle,
                            cx,
                            cy,
                            stops,
                        } => {
                            let mut parts: Vec<String> = Vec::with_capacity(stops.len() + 1);
                            let is_default_pos = matches!(cx, LengthPercentage::Percent(p) if (*p - 0.5).abs() < 1e-4)
                                && matches!(cy, LengthPercentage::Percent(p) if (*p - 0.5).abs() < 1e-4);
                            let mut prefix = String::new();
                            if *shape_circle {
                                prefix.push_str("circle");
                            }
                            if !is_default_pos {
                                if !prefix.is_empty() {
                                    prefix.push(' ');
                                }
                                prefix.push_str(&format!(
                                    "at {} {}",
                                    format_lp(cx, &eval),
                                    format_lp(cy, &eval)
                                ));
                            }
                            if !prefix.is_empty() {
                                parts.push(prefix);
                            }
                            for st in stops {
                                let col = format_css_color(st.color, style.text.color, dark);
                                if let Some(ref pos) = st.position {
                                    parts.push(format!("{col} {}", format_lp(pos, &eval)));
                                } else {
                                    parts.push(col);
                                }
                            }
                            format!("radial-gradient({})", parts.join(", "))
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                "none".into()
            }
        }
        "animation-duration" => {
            if let Some(n) = style
                .effects
                .as_ref()
                .and_then(|e| e.animation_duration_math.as_ref())
            {
                return n.serialize_computed(
                    &eval,
                    None,
                    rix_css::values_math::ValueKind::Time,
                    Some(0.0),
                    None,
                );
            }
            let timelines = style
                .effects
                .as_ref()
                .map(|e| e.animation_timelines.as_slice())
                .unwrap_or(&[]);
            // CSSWG #6530: `animation-duration: auto` resolves to `0s` when `animation-timeline`
            // is the initial single `auto` timeline; otherwise it stays `auto`.
            let single_auto_timeline = timelines.is_empty()
                || (timelines.len() == 1 && timelines[0].as_str().eq_ignore_ascii_case("auto"));
            match style.effects.as_ref().map(|e| e.animations.as_slice()) {
                Some(list) if !list.is_empty() => list
                    .iter()
                    .map(|a| {
                        if a.duration_auto && !single_auto_timeline {
                            "auto".to_string()
                        } else {
                            format!("{}s", format_num(a.duration_ms / 1000.0))
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => "0s".into(),
            }
        }
        "animation-delay" => {
            if let Some(n) = style
                .effects
                .as_ref()
                .and_then(|e| e.animation_delay_math.as_ref())
            {
                return n.serialize_computed(
                    &eval,
                    None,
                    rix_css::values_math::ValueKind::Time,
                    None,
                    None,
                );
            }
            match style.effects.as_ref().map(|e| e.animations.as_slice()) {
                Some(list) if !list.is_empty() => list
                    .iter()
                    .map(|a| format!("{}s", format_num(a.delay_ms / 1000.0)))
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => "0s".into(),
            }
        }
        "animation-iteration-count" => {
            if let Some(n) = style
                .effects
                .as_ref()
                .and_then(|e| e.animation_iteration_count_math.as_ref())
            {
                return n.serialize_computed(
                    &eval,
                    None,
                    rix_css::values_math::ValueKind::Number,
                    Some(0.0),
                    None,
                );
            }
            match style.effects.as_ref().map(|e| e.animations.as_slice()) {
                Some(list) if !list.is_empty() => list
                    .iter()
                    .map(|a| {
                        if a.iteration_count.is_infinite() {
                            "infinite".to_string()
                        } else {
                            format_num(a.iteration_count)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => "1".into(),
            }
        }
        "scale" => style
            .effects
            .as_ref()
            .map_or_else(|| "none".into(), |e| e.scale.serialize_computed(&eval)),
        "rotate" => style
            .effects
            .as_ref()
            .map_or_else(|| "none".into(), |e| e.rotate.serialize_computed(&eval)),
        "translate" => style
            .effects
            .as_ref()
            .map_or_else(|| "none".into(), |e| e.translate.serialize_computed(&eval)),
        "image-resolution" => style
            .effects
            .as_ref()
            .and_then(|e| e.image_resolution.as_ref())
            .map_or_else(
                || "1dppx".into(),
                |n| {
                    n.serialize_computed(
                        &eval,
                        None,
                        rix_css::values_math::ValueKind::Resolution,
                        Some(0.0),
                        None,
                    )
                },
            ),
        "background-position" => {
            if !style.visual.background_positions.is_empty() {
                style
                    .visual
                    .background_positions
                    .iter()
                    .map(|(x, y)| format!("{} {}", format_lp(x, &eval), format_lp(y, &eval)))
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                format!(
                    "{} {}",
                    format_lp(&style.visual.background_position.0, &eval),
                    format_lp(&style.visual.background_position.1, &eval),
                )
            }
        }
        "background-size" => {
            let fmt_size =
                |sz: &rix_css::BackgroundSizeValue, cover: bool, contain: bool| -> String {
                    if cover {
                        return "cover".to_string();
                    }
                    if contain {
                        return "contain".to_string();
                    }
                    match sz {
                        rix_css::BackgroundSizeValue::Auto => "auto".to_string(),
                        rix_css::BackgroundSizeValue::Cover => "cover".to_string(),
                        rix_css::BackgroundSizeValue::Contain => "contain".to_string(),
                        rix_css::BackgroundSizeValue::Explicit(w, h) => {
                            let ws = w.as_ref().map_or_else(
                                || "auto".to_string(),
                                |lp| format_lp_opts(lp, &eval, true),
                            );
                            let hs = h.as_ref().map_or_else(
                                || "auto".to_string(),
                                |lp| format_lp_opts(lp, &eval, true),
                            );
                            if ws == "auto" && hs == "auto" {
                                "auto".to_string()
                            } else {
                                format!("{ws} {hs}")
                            }
                        }
                    }
                };
            if !style.visual.background_sizes.is_empty() {
                style
                    .visual
                    .background_sizes
                    .iter()
                    .map(|sz| fmt_size(sz, false, false))
                    .collect::<Vec<_>>()
                    .join(", ")
            } else if let Some(ref sz) = style.visual.background_size {
                fmt_size(sz, false, false)
            } else if !style.visual.background_layers.is_empty() {
                style
                    .visual
                    .background_layers
                    .iter()
                    .map(|l| fmt_size(&l.size, l.size_cover, l.size_contain))
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                "auto".to_string()
            }
        }
        "background-repeat" => {
            if !style.visual.background_repeats.is_empty() {
                style
                    .visual
                    .background_repeats
                    .iter()
                    .map(|r| r.to_css_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            } else if !style.visual.background_layers.is_empty() {
                let fmt_rep = |rx: bool, ry: bool| -> &'static str {
                    match (rx, ry) {
                        (true, true) => "repeat",
                        (true, false) => "repeat-x",
                        (false, true) => "repeat-y",
                        (false, false) => "no-repeat",
                    }
                };
                style
                    .visual
                    .background_layers
                    .iter()
                    .map(|l| fmt_rep(l.repeat_x, l.repeat_y))
                    .collect::<Vec<_>>()
                    .join(", ")
            } else {
                "repeat".to_string()
            }
        }
        "object-position" => style.visual.object_position.as_deref().map_or_else(
            || "50% 50%".to_string(),
            |pos| pos.serialize_computed(&eval, style.text.writing_mode, style.text.direction),
        ),
        "aspect-ratio" => style
            .effects
            .as_ref()
            .and_then(|e| {
                e.aspect_ratio_math
                    .as_ref()
                    .map(|m| (e.aspect_ratio_auto, m))
            })
            .map_or_else(
                || "auto".to_string(),
                |(has_auto, (a, b))| {
                    let sa = a.serialize_computed(
                        &eval,
                        None,
                        rix_css::values_math::ValueKind::Number,
                        Some(0.0),
                        None,
                    );
                    let ratio = match b {
                        Some(bn) => format!(
                            "{sa} / {}",
                            bn.serialize_computed(
                                &eval,
                                None,
                                rix_css::values_math::ValueKind::Number,
                                Some(0.0),
                                None,
                            )
                        ),
                        None => format!("{sa} / 1"),
                    };
                    if has_auto {
                        format!("auto {ratio}")
                    } else {
                        ratio
                    }
                },
            ),
        "offset-distance"
        | "shape-margin"
        | "stroke-width"
        | "stroke-dashoffset"
        | "stroke-dasharray"
        | "background-position-x"
        | "background-position-y"
        | "scroll-padding-top"
        | "scroll-padding-right"
        | "scroll-padding-bottom"
        | "scroll-padding-left"
        | "cx"
        | "cy"
        | "rx"
        | "ry"
        | "x"
        | "y"
        | "border-image-slice" => {
            if let Some(kind) = PropertyIdKind::from_name(effective) {
                if let Some(node) = style
                    .effects
                    .as_ref()
                    .and_then(|e| e.extra_math_props.get(&kind))
                {
                    if let Some((vk, nonneg)) = rix_css::properties::values4_property_kind(kind) {
                        return node.serialize_computed(
                            &eval,
                            None,
                            vk,
                            nonneg.then_some(0.0),
                            None,
                        );
                    }
                }
            }
            match effective {
                "stroke-width" => "1px",
                "stroke-dasharray" => "none",
                "background-position-x" | "background-position-y" => "0%",
                "scroll-padding-top"
                | "scroll-padding-right"
                | "scroll-padding-bottom"
                | "scroll-padding-left"
                | "rx"
                | "ry" => "auto",
                "border-image-slice" => "100%",
                _ => "0px",
            }
            .into()
        }
        "transform" => {
            if let Some(raw) = style
                .effects
                .as_ref()
                .and_then(|e| e.transform_raw.as_ref())
            {
                rix_css::values_math::compute_transform_matrix_css(raw, &eval)
                    .or_else(|| {
                        rix_css::values_math::parse_and_serialize_transform(raw, Some(&eval))
                    })
                    .unwrap_or_else(|| "none".into())
            } else if let Some(ops) = style
                .effects
                .as_ref()
                .map(|e| e.transform.as_slice())
                .filter(|s| !s.is_empty())
            {
                let (bw, bh) = layout_box.map_or((0.0, 0.0), |lb| {
                    (lb.border_size.width, lb.border_size.height)
                });
                format_transform_ops_matrix(ops, &eval, bw, bh)
            } else {
                "none".into()
            }
        }
        "orphans" => style.text.orphans.to_string(),
        "widows" => style.text.widows.to_string(),
        "tab-size" => match &style.text.tab_size_len {
            Some(lp) => format_lp_opts(lp, &eval, true),
            None => format_num(style.text.tab_size_num),
        },
        "column-count" => match style.box_model.column_count {
            Some(n) => n.to_string(),
            None => "auto".into(),
        },
        "grid-template-rows" => {
            if let Some(rgt) = layout_box
                .as_ref()
                .and_then(|lb| lb.resolved_grid_tracks.as_ref())
            {
                return rgt.rows.clone();
            }
            style.flex_grid.as_ref().map_or_else(
                || "none".into(),
                |f| {
                    let is_grid = matches!(
                        style.display,
                        DisplayMode::Grid
                            | DisplayMode::InlineGrid
                            | DisplayMode::GridLanes
                            | DisplayMode::InlineGridLanes
                            | DisplayMode::InlineGridLanesMulti
                    );
                    if !is_grid && f.grid_template_rows_list.has_intrinsic_auto_repeat() {
                        "none".into()
                    } else {
                        rix_css::grid_css::serialize_track_list(
                            &f.grid_template_rows_list,
                            Some(&eval),
                        )
                    }
                },
            )
        }
        "grid-template-columns" => {
            if let Some(rgt) = layout_box
                .as_ref()
                .and_then(|lb| lb.resolved_grid_tracks.as_ref())
            {
                return rgt.columns.clone();
            }
            style.flex_grid.as_ref().map_or_else(
                || "none".into(),
                |f| {
                    let is_grid = matches!(
                        style.display,
                        DisplayMode::Grid
                            | DisplayMode::InlineGrid
                            | DisplayMode::GridLanes
                            | DisplayMode::InlineGridLanes
                            | DisplayMode::InlineGridLanesMulti
                    );
                    if !is_grid && f.grid_template_columns_list.has_intrinsic_auto_repeat() {
                        "none".into()
                    } else {
                        rix_css::grid_css::serialize_track_list(
                            &f.grid_template_columns_list,
                            Some(&eval),
                        )
                    }
                },
            )
        }
        "grid-template-areas" => style.flex_grid.as_ref().map_or_else(
            || "none".into(),
            |f| rix_css::grid_css::serialize_grid_areas(&f.grid_template_areas),
        ),
        "grid-auto-columns" => style.flex_grid.as_ref().map_or_else(
            || "auto".into(),
            |f| rix_css::grid_css::serialize_auto_tracks(&f.grid_auto_columns, Some(&eval)),
        ),
        "grid-auto-rows" => style.flex_grid.as_ref().map_or_else(
            || "auto".into(),
            |f| rix_css::grid_css::serialize_auto_tracks(&f.grid_auto_rows, Some(&eval)),
        ),
        "grid-row-start" => style.flex_grid.as_ref().map_or_else(
            || "auto".into(),
            |f| rix_css::grid_css::serialize_grid_line(&f.grid_row_start, Some(&eval)),
        ),
        "grid-row-end" => style.flex_grid.as_ref().map_or_else(
            || "auto".into(),
            |f| rix_css::grid_css::serialize_grid_line(&f.grid_row_end, Some(&eval)),
        ),
        "grid-column-start" => style.flex_grid.as_ref().map_or_else(
            || "auto".into(),
            |f| rix_css::grid_css::serialize_grid_line(&f.grid_column_start, Some(&eval)),
        ),
        "grid-column-end" => style.flex_grid.as_ref().map_or_else(
            || "auto".into(),
            |f| rix_css::grid_css::serialize_grid_line(&f.grid_column_end, Some(&eval)),
        ),
        "grid-row" => style.flex_grid.as_ref().map_or_else(
            || "auto".into(),
            |f| {
                rix_css::grid_css::serialize_grid_row_or_col_shortest(
                    &f.grid_row_start,
                    &f.grid_row_end,
                    Some(&eval),
                )
            },
        ),
        "grid-column" => style.flex_grid.as_ref().map_or_else(
            || "auto".into(),
            |f| {
                rix_css::grid_css::serialize_grid_row_or_col_shortest(
                    &f.grid_column_start,
                    &f.grid_column_end,
                    Some(&eval),
                )
            },
        ),
        "grid-area" => style.flex_grid.as_ref().map_or_else(
            || "auto".into(),
            |f| {
                rix_css::grid_css::serialize_grid_area_shortest(
                    &f.grid_row_start,
                    &f.grid_column_start,
                    &f.grid_row_end,
                    &f.grid_column_end,
                    Some(&eval),
                )
            },
        ),
        "grid-template" => style.flex_grid.as_ref().map_or_else(
            || "none".into(),
            |f| {
                rix_css::grid_css::serialize_grid_template_from_values(
                    &f.grid_template_rows_list,
                    &f.grid_template_columns_list,
                    &f.grid_template_areas,
                )
                .unwrap_or_default()
            },
        ),
        "grid" => style.flex_grid.as_ref().map_or_else(
            || "none".into(),
            |f| {
                rix_css::grid_css::serialize_grid_from_values(
                    &f.grid_template_rows_list,
                    &f.grid_template_columns_list,
                    &f.grid_template_areas,
                    &f.grid_auto_rows,
                    &f.grid_auto_columns,
                    f.grid_auto_flow,
                )
                .unwrap_or_default()
            },
        ),
        "fit-tolerance" => style.flex_grid.as_ref().map_or_else(
            || "normal".into(),
            |f| rix_css::grid_css::serialize_fit_tolerance(&f.fit_tolerance, Some(&eval)),
        ),
        "grid-lanes-direction" => style.flex_grid.as_ref().map_or_else(
            || "normal".into(),
            |f| rix_css::grid_css::serialize_grid_lanes_direction_computed(f.grid_lanes_direction),
        ),
        "grid-lanes-pack" => rix_css::grid_css::serialize_grid_lanes_pack(
            style
                .flex_grid
                .as_ref()
                .map_or_else(Default::default, |f| f.grid_lanes_pack),
        )
        .into(),
        "grid-lanes" => style.flex_grid.as_ref().map_or_else(
            || "normal".into(),
            |f| {
                rix_css::grid_css::serialize_grid_lanes_from_values(
                    &f.grid_template_columns_list,
                    &f.grid_template_rows_list,
                    &f.grid_template_areas,
                    f.grid_lanes_direction,
                    Some(&eval),
                )
                .unwrap_or_default()
            },
        ),
        "gap" => {
            let r = style
                .flex_grid
                .as_ref()
                .map_or_else(|| "normal".into(), |f| format_lp(&f.row_gap, &eval));
            let c = style
                .flex_grid
                .as_ref()
                .map_or_else(|| "normal".into(), |f| format_lp(&f.column_gap, &eval));
            if r == c {
                r
            } else {
                format!("{r} {c}")
            }
        }
        "place-content" => fmt_pair("align-content", "justify-content"),
        "place-items" => fmt_pair("align-items", "justify-items"),
        "place-self" => fmt_pair("align-self", "justify-self"),
        "text-decoration" => format_computed_property_resolved(
            style,
            "text-decoration-line",
            layout_box,
            parent_flex_grid,
            ch_px,
        ),
        "text-decoration-line" => {
            let l = style.visual.text_decoration_line;
            let mut parts = Vec::new();
            if l.contains(rix_css::TextDecorationLine::UNDERLINE) {
                parts.push("underline");
            }
            if l.contains(rix_css::TextDecorationLine::OVERLINE) {
                parts.push("overline");
            }
            if l.contains(rix_css::TextDecorationLine::LINE_THROUGH) {
                parts.push("line-through");
            }
            if parts.is_empty() {
                "none".into()
            } else {
                parts.join(" ")
            }
        }
        "text-decoration-color" => style
            .visual
            .text_decoration_color_computed
            .as_deref()
            .map_or_else(
                || {
                    style.visual.text_decoration_color.map_or_else(
                        || {
                            format_computed_color4(
                                style.text.color_computed.as_deref(),
                                style.text.color,
                                cc_resolved,
                                dark,
                                &eval,
                            )
                        },
                        |c| format_css_color_resolved(c, cc_resolved, dark),
                    )
                },
                |e| e.resolve(cc_resolved, dark, &eval).to_computed_css_string(),
            ),
        "transform-origin" => {
            let orig = style
                .effects
                .as_ref()
                .map(|e| e.transform_origin.clone())
                .unwrap_or_default();
            let (bw, bh) = layout_box.map_or((0.0, 0.0), |lb| {
                (lb.border_size.width, lb.border_size.height)
            });
            let xs = if layout_box.is_some() {
                format_px(orig.x.resolve_px(bw, &eval))
            } else {
                format_lp(&orig.x, &eval)
            };
            let ys = if layout_box.is_some() {
                format_px(orig.y.resolve_px(bh, &eval))
            } else {
                format_lp(&orig.y, &eval)
            };
            if orig.z.abs() > 1e-4 {
                format!("{xs} {ys} {}", format_px(orig.z))
            } else {
                format!("{xs} {ys}")
            }
        }
        "clip-path" => match style.effects.as_ref().map(|e| &e.clip_path) {
            Some(rix_css::ClipPathValue::Inset {
                top,
                right,
                bottom,
                left,
                ..
            }) => format!(
                "inset({} {} {} {})",
                format_lp(top, &eval),
                format_lp(right, &eval),
                format_lp(bottom, &eval),
                format_lp(left, &eval),
            ),
            Some(rix_css::ClipPathValue::Circle { radius, cx, cy }) => {
                let rs = match radius {
                    rix_css::ClipRadius::Length(l) => format_lp(l, &eval),
                    rix_css::ClipRadius::ClosestSide => "closest-side".to_string(),
                    rix_css::ClipRadius::FarthestSide => "farthest-side".to_string(),
                };
                format!(
                    "circle({rs} at {} {})",
                    format_lp(cx, &eval),
                    format_lp(cy, &eval)
                )
            }
            Some(rix_css::ClipPathValue::Polygon { fill_odd, points }) => {
                let pts: Vec<String> = points
                    .iter()
                    .map(|(x, y)| format!("{} {}", format_lp(x, &eval), format_lp(y, &eval)))
                    .collect();
                if *fill_odd {
                    format!("polygon(evenodd, {})", pts.join(", "))
                } else {
                    format!("polygon({})", pts.join(", "))
                }
            }
            _ => "none".into(),
        },
        "transition-timing-function" => {
            match style.effects.as_ref().map(|e| e.transitions.as_slice()) {
                Some(list) if !list.is_empty() => list
                    .iter()
                    .map(|t| t.timing.to_css_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => "ease".into(),
            }
        }
        _ => String::new(),
    }
}

#[rustfmt::skip]
fn format_transform_ops_matrix(ops: &[rix_css::TransformOp], eval: &EvalContext, ref_w: f32, ref_h: f32) -> String {
    use rix_css::values_math::format_math_f64;
    use rix_css::TransformOp as T;
    // Column-major 4x4 matrix multiplication (CSS Transforms Level 2).
    let mul4 = |a: [f64; 16], b: [f64; 16]| -> [f64; 16] {
        let mut r = [0.0; 16];
        for col in 0..4 {
            for row in 0..4 {
                r[col * 4 + row] = a[row] * b[col * 4]
                    + a[4 + row] * b[col * 4 + 1]
                    + a[8 + row] * b[col * 4 + 2]
                    + a[12 + row] * b[col * 4 + 3];
            }
        }
        r
    };
    let mut m: [f64; 16] = [
        1.0, 0.0, 0.0, 0.0,
        0.0, 1.0, 0.0, 0.0,
        0.0, 0.0, 1.0, 0.0,
        0.0, 0.0, 0.0, 1.0,
    ];
    for op in ops {
        let step: [f64; 16] = match op {
            T::Translate(x, y) => [
                1.0, 0.0, 0.0, 0.0,
                0.0, 1.0, 0.0, 0.0,
                0.0, 0.0, 1.0, 0.0,
                f64::from(x.resolve_px(ref_w, eval)), f64::from(y.resolve_px(ref_h, eval)), 0.0, 1.0,
            ],
            T::Translate3d(x, y, z) => [
                1.0, 0.0, 0.0, 0.0,
                0.0, 1.0, 0.0, 0.0,
                0.0, 0.0, 1.0, 0.0,
                f64::from(x.resolve_px(ref_w, eval)), f64::from(y.resolve_px(ref_h, eval)), f64::from(*z), 1.0,
            ],
            T::TranslateZ(z) => [
                1.0, 0.0, 0.0, 0.0,
                0.0, 1.0, 0.0, 0.0,
                0.0, 0.0, 1.0, 0.0,
                0.0, 0.0, f64::from(*z), 1.0,
            ],
            T::Scale(sx, sy) => [
                f64::from(*sx), 0.0, 0.0, 0.0,
                0.0, f64::from(*sy), 0.0, 0.0,
                0.0, 0.0, 1.0, 0.0,
                0.0, 0.0, 0.0, 1.0,
            ],
            T::Scale3d(sx, sy, sz) => [
                f64::from(*sx), 0.0, 0.0, 0.0,
                0.0, f64::from(*sy), 0.0, 0.0,
                0.0, 0.0, f64::from(*sz), 0.0,
                0.0, 0.0, 0.0, 1.0,
            ],
            T::ScaleZ(sz) => [
                1.0, 0.0, 0.0, 0.0,
                0.0, 1.0, 0.0, 0.0,
                0.0, 0.0, f64::from(*sz), 0.0,
                0.0, 0.0, 0.0, 1.0,
            ],
            T::RotateDeg(d) => {
                let r = f64::from(*d).to_radians();
                let (s, c) = (r.sin(), r.cos());
                [c, s, 0.0, 0.0, -s, c, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0]
            }
            T::RotateXDeg(d) => {
                let r = f64::from(*d).to_radians();
                let (s, c) = (r.sin(), r.cos());
                [1.0, 0.0, 0.0, 0.0, 0.0, c, s, 0.0, 0.0, -s, c, 0.0, 0.0, 0.0, 0.0, 1.0]
            }
            T::RotateYDeg(d) => {
                let r = f64::from(*d).to_radians();
                let (s, c) = (r.sin(), r.cos());
                [c, 0.0, -s, 0.0, 0.0, 1.0, 0.0, 0.0, s, 0.0, c, 0.0, 0.0, 0.0, 0.0, 1.0]
            }
            T::Rotate3d(x, y, z, deg) => {
                let (x, y, z, deg) = (f64::from(*x), f64::from(*y), f64::from(*z), f64::from(*deg));
                let len = (x * x + y * y + z * z).sqrt();
                if len > 0.0 && deg.is_finite() {
                    let (ux, uy, uz) = (x / len, y / len, z / len);
                    let (s, c) = (deg.to_radians().sin(), deg.to_radians().cos());
                    let t = 1.0 - c;
                    [
                        t * ux * ux + c, t * ux * uy + uz * s, t * ux * uz - uy * s, 0.0,
                        t * ux * uy - uz * s, t * uy * uy + c, t * uy * uz + ux * s, 0.0,
                        t * ux * uz + uy * s, t * uy * uz - ux * s, t * uz * uz + c, 0.0,
                        0.0, 0.0, 0.0, 1.0,
                    ]
                } else {
                    [1.0,0.0,0.0,0.0, 0.0,1.0,0.0,0.0, 0.0,0.0,1.0,0.0, 0.0,0.0,0.0,1.0]
                }
            }
            T::SkewDeg(ax, ay) => [
                1.0, f64::from(*ay).to_radians().tan(), 0.0, 0.0,
                f64::from(*ax).to_radians().tan(), 1.0, 0.0, 0.0,
                0.0, 0.0, 1.0, 0.0,
                0.0, 0.0, 0.0, 1.0,
            ],
            T::Perspective(d) => {
                let p = if *d > 0.0 && d.is_finite() { -1.0 / f64::from(*d) } else { 0.0 };
                [1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, p, 0.0, 0.0, 0.0, 1.0]
            }
            T::Matrix(v) => [
                f64::from(v[0]), f64::from(v[1]), 0.0, 0.0,
                f64::from(v[2]), f64::from(v[3]), 0.0, 0.0,
                0.0, 0.0, 1.0, 0.0,
                f64::from(v[4]), f64::from(v[5]), 0.0, 1.0,
            ],
            T::Matrix3d(v) => [
                f64::from(v[0]), f64::from(v[1]), f64::from(v[2]), f64::from(v[3]),
                f64::from(v[4]), f64::from(v[5]), f64::from(v[6]), f64::from(v[7]),
                f64::from(v[8]), f64::from(v[9]), f64::from(v[10]), f64::from(v[11]),
                f64::from(v[12]), f64::from(v[13]), f64::from(v[14]), f64::from(v[15]),
            ],
        };
        m = mul4(m, step);
    }
    let clean = |v: f64| if !v.is_finite() || v.abs() < 1e-6 { 0.0 } else { v };
    let is_2d = m[2].abs() < 1e-6 && m[3].abs() < 1e-6
        && m[6].abs() < 1e-6 && m[7].abs() < 1e-6
        && m[8].abs() < 1e-6 && m[9].abs() < 1e-6
        && (m[10] - 1.0).abs() < 1e-6 && m[11].abs() < 1e-6
        && m[14].abs() < 1e-6 && (m[15] - 1.0).abs() < 1e-6;
    if is_2d {
        format!(
            "matrix({}, {}, {}, {}, {}, {})",
            format_math_f64(clean(m[0])), format_math_f64(clean(m[1])),
            format_math_f64(clean(m[4])), format_math_f64(clean(m[5])),
            format_math_f64(clean(m[12])), format_math_f64(clean(m[13])),
        )
    } else {
        let parts: Vec<String> = m.iter().map(|&v| format_math_f64(clean(v))).collect();
        format!("matrix3d({})", parts.join(", "))
    }
}

// ---------------------------------------------------------------------------
// CSS Typed OM (`CSSUnitValue` & `computedStyleMap`)
// ---------------------------------------------------------------------------

fn install_web_animations_and_typed_om(vm: &mut JsVm, host: &SharedHost) {
    let elem_proto = host.borrow().prototypes.element_proto;

    let unit_proto = vm.alloc_ordinary_object();
    let unit_ctor = vm.alloc_host_fn("CSSUnitValue", move |vm, _, args| {
        let obj = vm.alloc_ordinary_object();
        vm.heap
            .object_set_prototype(obj, Some(unit_proto), &mut vm.shapes);
        let v = arg(args, 0).to_number_f64().unwrap_or(0.0);
        let u = vm.value_to_rust_string(arg(args, 1));
        let kv = vm.heap.intern_key("value");
        vm.heap.object_define_data(
            obj,
            kv,
            JsValue::from_f64(v),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        let ku = vm.heap.intern_key("unit");
        let us = vm.heap.alloc_string(u);
        vm.heap.object_define_data(
            obj,
            ku,
            JsValue::from_string(us),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        Ok(JsValue::from_object(obj))
    });
    let k_proto = vm.heap.intern_key("prototype");
    vm.heap.object_define_data(
        unit_ctor,
        k_proto,
        JsValue::from_object(unit_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    define_global(vm, "CSSUnitValue", JsValue::from_object(unit_ctor));

    // CSS Typed OM `CSSNumericValue.parse(cssText)` (used by Web Animations CSSNumberish times).
    let num_proto = vm.alloc_ordinary_object();
    vm.heap
        .object_set_prototype(unit_proto, Some(num_proto), &mut vm.shapes);
    let num_ctor = vm.alloc_host_fn("CSSNumericValue", |vm, _, _| {
        Err(vm.type_error("Illegal constructor"))
    });
    let num_parse = vm.alloc_host_fn("parse", move |vm, _, args| {
        let raw = vm.to_js_string(arg(args, 0))?;
        let Some((val, unit)) = split_css_unit_value(&raw) else {
            return Err(create_dom_exception(
                vm,
                "SyntaxError",
                &format!("Invalid CSSNumericValue '{raw}'"),
            ));
        };
        let obj = vm.alloc_ordinary_object();
        vm.heap
            .object_set_prototype(obj, Some(unit_proto), &mut vm.shapes);
        let kv = vm.heap.intern_key("value");
        vm.heap.object_define_data(
            obj,
            kv,
            JsValue::from_f64(val),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        let ku = vm.heap.intern_key("unit");
        let us = vm.heap.alloc_string(unit);
        vm.heap.object_define_data(
            obj,
            ku,
            JsValue::from_string(us),
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        let km = vm.heap.intern_key("__rix_css_unit_value__");
        vm.heap.object_define_data(
            obj,
            km,
            JsValue::TRUE,
            PropertyAttrs::empty(),
            &mut vm.shapes,
        );
        Ok(JsValue::from_object(obj))
    });
    let k_parse = vm.heap.intern_key("parse");
    vm.heap.object_define_data(
        num_ctor,
        k_parse,
        JsValue::from_object(num_parse),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
    let k_nproto = vm.heap.intern_key("prototype");
    vm.heap.object_define_data(
        num_ctor,
        k_nproto,
        JsValue::from_object(num_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    define_global(vm, "CSSNumericValue", JsValue::from_object(num_ctor));

    let h_map = Rc::clone(host);
    define_host_method(vm, elem_proto, "computedStyleMap", move |vm, this, _| {
        let node = require_node(vm, &h_map, this)?;
        let map_obj = vm.alloc_ordinary_object();
        let h_get = Rc::clone(&h_map);
        define_host_method(vm, map_obj, "get", move |vm, _, args| {
            let prop = vm.value_to_rust_string(arg(args, 0));
            let (style, pfg) = {
                let mut h = h_get.borrow_mut();
                (
                    h.computed_style_for_node(node),
                    h.parent_is_flex_or_grid(node),
                )
            };
            let Some(style) = style else {
                return Ok(JsValue::null());
            };
            let ch_px = Some(h_get.borrow_mut().ch_advance_px(&style));
            let lower_prop = prop.to_ascii_lowercase();
            // CSS Typed OM / CSS Color 5: `computedStyleMap()` returns the computed value (where
            // `currentcolor`-dependent relative/mixed colors preserve `currentcolor` unresolved
            // against `color`, unlike `getComputedStyle()` which returns the resolved value).
            let computed_color_override = match lower_prop.as_str() {
                "background-color" => style
                    .visual
                    .background_color_computed
                    .as_deref()
                    .filter(|e| e.contains_current_color())
                    .map(|e| e.to_specified_css_string()),
                "border-top-color" => style.visual.border_color_computed[0]
                    .as_deref()
                    .filter(|e| e.contains_current_color())
                    .map(|e| e.to_specified_css_string()),
                "border-right-color" => style.visual.border_color_computed[1]
                    .as_deref()
                    .filter(|e| e.contains_current_color())
                    .map(|e| e.to_specified_css_string()),
                "border-bottom-color" => style.visual.border_color_computed[2]
                    .as_deref()
                    .filter(|e| e.contains_current_color())
                    .map(|e| e.to_specified_css_string()),
                "border-left-color" => style.visual.border_color_computed[3]
                    .as_deref()
                    .filter(|e| e.contains_current_color())
                    .map(|e| e.to_specified_css_string()),
                "outline-color" => style
                    .visual
                    .outline_color_computed
                    .as_deref()
                    .filter(|e| e.contains_current_color())
                    .map(|e| e.to_specified_css_string()),
                "text-decoration-color" => style
                    .visual
                    .text_decoration_color_computed
                    .as_deref()
                    .filter(|e| e.contains_current_color())
                    .map(|e| e.to_specified_css_string()),
                _ => None,
            };
            let val_str = computed_color_override.unwrap_or_else(|| {
                format_computed_property_resolved(&style, &prop, None, pfg, ch_px)
            });
            if val_str.is_empty() {
                return Ok(JsValue::null());
            }
            let css_val = vm.alloc_ordinary_object();
            if let Some((num, unit)) = split_css_unit_value(&val_str) {
                vm.heap
                    .object_set_prototype(css_val, Some(unit_proto), &mut vm.shapes);
                let kv = vm.heap.intern_key("value");
                vm.heap.object_define_data(
                    css_val,
                    kv,
                    JsValue::from_f64(num),
                    PropertyAttrs::DEFAULT,
                    &mut vm.shapes,
                );
                let ku = vm.heap.intern_key("unit");
                let us = vm.heap.alloc_string(unit);
                vm.heap.object_define_data(
                    css_val,
                    ku,
                    JsValue::from_string(us),
                    PropertyAttrs::DEFAULT,
                    &mut vm.shapes,
                );
            }
            define_host_method(vm, css_val, "toString", move |vm, _, _| {
                Ok(JsValue::from_string(vm.heap.alloc_string(val_str.clone())))
            });
            Ok(JsValue::from_object(css_val))
        });
        Ok(JsValue::from_object(map_obj))
    });
}

fn format_computed_filter_list(
    list: Option<&[FilterOp]>,
    eval: &EvalContext,
    cc: ColorRgba,
    dark: bool,
) -> String {
    match list {
        Some(items) if !items.is_empty() => items
            .iter()
            .map(|op| match op {
                FilterOp::Blur(r) => format!("blur({})", format_lp_opts(r, eval, true)),
                FilterOp::Brightness(v) => format!("brightness({})", format_num(*v)),
                FilterOp::Contrast(v) => format!("contrast({})", format_num(*v)),
                FilterOp::Grayscale(v) => format!("grayscale({})", format_num(*v)),
                FilterOp::Opacity(v) => format!("opacity({})", format_num(*v)),
                FilterOp::Saturate(v) => format!("saturate({})", format_num(*v)),
                FilterOp::Invert(v) => format!("invert({})", format_num(*v)),
                FilterOp::Sepia(v) => format!("sepia({})", format_num(*v)),
                FilterOp::HueRotateDeg(d) => format!("hue-rotate({}deg)", format_num(*d)),
                FilterOp::DropShadow {
                    offset_x,
                    offset_y,
                    blur,
                    color,
                } => format!(
                    "drop-shadow({} {} {} {})",
                    format_css_color(*color, cc, dark),
                    format_lp(offset_x, eval),
                    format_lp(offset_y, eval),
                    format_lp_opts(blur, eval, true),
                ),
                FilterOp::Url(u) => {
                    let mut escaped = String::with_capacity(u.as_str().len() + 7);
                    escaped.push_str("url(\"");
                    for ch in u.as_str().chars() {
                        match ch {
                            '"' => escaped.push_str("\\\""),
                            '\\' => escaped.push_str("\\\\"),
                            _ => escaped.push(ch),
                        }
                    }
                    escaped.push_str("\")");
                    escaped
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
        _ => "none".into(),
    }
}

fn split_css_unit_value(s: &str) -> Option<(f64, String)> {
    let t = s.trim();
    if t.is_empty() || t.contains(char::is_whitespace) || t.contains('(') {
        return None;
    }
    let idx = t
        .find(|c: char| !matches!(c, '0'..='9' | '.' | '+' | '-' | 'e' | 'E'))
        .unwrap_or(t.len());
    let num = t[..idx].parse::<f64>().ok()?;
    let unit = match &t[idx..] {
        "" => "number".to_string(),
        "%" => "percent".to_string(),
        other => other.to_string(),
    };
    Some((num, unit))
}

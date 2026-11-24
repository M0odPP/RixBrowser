//! W3C CSSOM (`CSS`, `CSSStyleSheet`, `CSSRuleList`, `CSSStyleRule`,
//! `CSSStyleDeclaration`, `document.styleSheets`, `document.adoptedStyleSheets`,
//! `<style>.sheet`, `getComputedStyle(el, pseudo)`, and validated inline `el.style`).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use rix_css::{
    evaluate_supports_condition_str, is_logical_property, parse_and_expand_declaration,
    parse_and_serialize_declaration, parse_cssom_rules, parse_pseudo_element_str,
    parse_selector_list_with_namespaces, parse_single_cssom_rule_with_namespaces,
    resolve_logical_to_physical, serialize_css_tokens, serialize_selector_list,
    strip_outer_css_comments, AlignValue, BorderCollapseMode, BoxSizingMode, CalcExpr, ClearMode,
    ColorRgba, ComputedStyle, CssAtom, CssColor, CssomRuleKind, CursorKind, DirectionMode,
    DisplayMode, EvalContext, FilterOp, FlexDirectionMode, FlexWrapMode, FloatMode, FontStyleMode,
    GridAutoFlowMode, LengthPercentage, LengthPercentageAuto, LineHeightValue, LineStyle,
    MixBlendMode, OverflowMode, PointerEventsMode, PositionMode, PropertyIdKind, RixPseudoElement,
    SelectorNamespaceMap, SizeValue, TableLayoutMode, TextAlignMode, TextOverflowMode,
    TextTransformMode, UserSelectMode, VerticalAlignMode, VisibilityMode, WhiteSpaceMode,
    WordBreakMode, ZIndexValue, COMPUTED_STYLE_LONGHANDS,
};
use rix_dom::{NodeData, NodeId};
use rix_js::promise::{alloc_promise, resolve_promise};
use rix_js::{HeapHandle, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};

use crate::dom_bridge::{
    arg, define_global, define_host_accessor, define_host_method, get_attr_string, require_node,
    set_element_attr, wrap_node, ResolvedLayoutBox, SharedHost,
};
use crate::web_apis::create_dom_exception;

/// Mutable backing state for a live CSSOM `CSSStyleSheet` (either `<style>.sheet` or constructed).
#[derive(Debug, Clone)]
pub struct CssStyleSheetState {
    pub owner_node: Option<NodeId>,
    pub rules: Vec<CssomRuleKind>,
    pub disabled: bool,
    pub is_constructed: bool,
    pub source_hash: u64,
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
pub fn install_cssom(vm: &mut JsVm, host: &SharedHost) {
    install_css_namespace(vm, host);
    install_css_rule_ctors(vm);
    install_css_stylesheet_ctor(vm, host);
    install_style_element_sheet_accessor(vm, host);
    install_document_stylesheets(vm, host);
    install_get_computed_style(vm, host);
}

// ---------------------------------------------------------------------------
// Global `CSS` namespace (`CSS.supports`, `CSS.escape`, `CSS.registerProperty`)
// ---------------------------------------------------------------------------

fn install_css_namespace(vm: &mut JsVm, host: &SharedHost) {
    let css_obj = vm.alloc_ordinary_object();

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
        st.mark_sheets_dirty();
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

fn install_css_rule_ctors(vm: &mut JsVm) {
    let rule_proto = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(rule_proto).unwrap().is_prototype = true;
    let rule_ctor = vm.alloc_host_fn("CSSRule", |vm, _, _| {
        Err(vm.type_error("Illegal constructor"))
    });
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
    let kp = vm.heap.intern_key("prototype");
    let kc = vm.heap.intern_key("constructor");
    vm.heap.object_define_data(
        rule_ctor,
        kp,
        JsValue::from_object(rule_proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        rule_proto,
        kc,
        JsValue::from_object(rule_ctor),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
    define_global(vm, "CSSRule", JsValue::from_object(rule_ctor));

    for name in [
        "CSSStyleRule",
        "CSSGroupingRule",
        "CSSMediaRule",
        "CSSSupportsRule",
        "CSSLayerBlockRule",
        "CSSLayerStatementRule",
        "CSSScopeRule",
        "CSSKeyframesRule",
        "CSSPropertyRule",
        "CSSStyleDeclaration",
    ] {
        let p = vm.alloc_ordinary_object();
        vm.heap
            .object_set_prototype(p, Some(rule_proto), &mut vm.shapes);
        vm.heap.get_object_mut(p).unwrap().is_prototype = true;
        let c = vm.alloc_host_fn(name, |vm, _, _| Err(vm.type_error("Illegal constructor")));
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
    }
}

fn install_css_stylesheet_ctor(vm: &mut JsVm, host: &SharedHost) {
    let proto = vm.alloc_ordinary_object();
    vm.heap.get_object_mut(proto).unwrap().is_prototype = true;
    let h = Rc::clone(host);
    let ctor = vm.alloc_host_fn("CSSStyleSheet", move |vm, _, _args| {
        let sheet_state = Rc::new(RefCell::new(CssStyleSheetState::from_css("", None, true)));
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
    let proto = proto_override
        .or_else(|| host.borrow().css_stylesheet_proto)
        .unwrap_or(vm.object_proto);
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    host.borrow_mut()
        .stylesheet_handle_map
        .insert(obj, Rc::clone(&state));

    // Live `cssRules` / `rules` Proxy so `const {cssRules} = sheet; sheet.insertRule(...)` stays live!
    let live_rules = create_live_css_rule_list(vm, host, Rc::clone(&state), obj, Vec::new());
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

    // `ownerNode`, `href`, `title`, `type`
    let st_owner = Rc::clone(&state);
    let h_owner = Rc::clone(host);
    let g_owner = vm.alloc_host_fn("get ownerNode", move |vm, _, _| {
        let owner = st_owner.borrow().owner_node;
        Ok(owner.map_or(JsValue::NULL, |nid| wrap_node(vm, &h_owner, nid)))
    });
    define_host_accessor(vm, obj, "ownerNode", Some(g_owner), None);

    let k_href = vm.heap.intern_key("href");
    vm.heap.object_define_data(
        obj,
        k_href,
        JsValue::NULL,
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );
    let k_type = vm.heap.intern_key("type");
    let v_type = JsValue::from_string(vm.heap.alloc_string("text/css"));
    vm.heap
        .object_define_data(obj, k_type, v_type, PropertyAttrs::DEFAULT, &mut vm.shapes);

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

    // `insertRule(rule, index = 0)` and `deleteRule(index)`
    attach_rule_list_mutators(vm, obj, host, Rc::clone(&state), Vec::new());

    // `replaceSync(text)` & `replace(text)`
    let st_rs = Rc::clone(&state);
    let h_rs = Rc::clone(host);
    define_host_method(vm, obj, "replaceSync", move |vm, _, args| {
        let text = vm.value_to_rust_string(arg(args, 0));
        {
            let mut st = st_rs.borrow_mut();
            st.rules = parse_cssom_rules(&text);
            st.source_hash = simple_hash(text.as_bytes());
        }
        sync_sheet_to_dom(&h_rs, &st_rs);
        Ok(JsValue::UNDEFINED)
    });

    let st_rep = Rc::clone(&state);
    let h_rep = Rc::clone(host);
    define_host_method(vm, obj, "replace", move |vm, this_val, args| {
        let text = vm.value_to_rust_string(arg(args, 0));
        {
            let mut st = st_rep.borrow_mut();
            st.rules = parse_cssom_rules(&text);
            st.source_hash = simple_hash(text.as_bytes());
        }
        sync_sheet_to_dom(&h_rep, &st_rep);
        let p = alloc_promise(vm);
        resolve_promise(vm, p, this_val);
        Ok(JsValue::from_object(p))
    });

    obj
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
        let ns = st_ins.borrow().active_namespaces();
        let ns_opt = (!ns.is_empty()).then_some(&ns);
        let parsed = parse_single_cssom_rule_with_namespaces(&rule_str, ns_opt)
            .map_err(|e| create_dom_exception(vm, "SyntaxError", &e))?;
        {
            let mut st = st_ins.borrow_mut();
            let Some(list) = st.rule_list_mut(&p_ins) else {
                return Err(create_dom_exception(
                    vm,
                    "IndexSizeError",
                    "Parent grouping rule no longer exists",
                ));
            };
            if index > list.len() {
                return Err(create_dom_exception(
                    vm,
                    "IndexSizeError",
                    "Rule index exceeds cssRules.length",
                ));
            }
            list.insert(index, parsed);
        }
        sync_sheet_to_dom(&h_ins, &st_ins);
        Ok(JsValue::from_i32(index as i32))
    });

    let st_del = Rc::clone(&state);
    let h_del = Rc::clone(host);
    let p_del = list_path;
    define_host_method(vm, target_obj, "deleteRule", move |vm, _, args| {
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
        {
            let mut st = st_del.borrow_mut();
            let Some(list) = st.rule_list_mut(&p_del) else {
                return Err(create_dom_exception(
                    vm,
                    "IndexSizeError",
                    "Parent grouping rule no longer exists",
                ));
            };
            if index >= list.len() {
                return Err(create_dom_exception(
                    vm,
                    "IndexSizeError",
                    "Rule index out of bounds",
                ));
            }
            list.remove(index);
        }
        sync_sheet_to_dom(&h_del, &st_del);
        Ok(JsValue::UNDEFINED)
    });
}

fn create_live_css_rule_list(
    vm: &mut JsVm,
    host: &SharedHost,
    state: SharedStyleSheet,
    parent_sheet: HeapHandle,
    list_path: Vec<usize>,
) -> JsValue {
    let target = vm.alloc_ordinary_object();
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
        )))
    });

    let handler = vm.alloc_ordinary_object();
    let st_get = Rc::clone(&state);
    let h_get = Rc::clone(host);
    let p_get = list_path;
    define_host_method(vm, handler, "get", move |vm, _, args| {
        let prop = vm.value_to_rust_string(arg(args, 1));
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
) -> HeapHandle {
    let obj = vm.alloc_ordinary_object();
    let (rule_type, is_style, is_grouping) = {
        let st = state.borrow();
        match st.rule_at(&rule_path) {
            Some(CssomRuleKind::Style { .. }) => (1i32, true, false),
            Some(CssomRuleKind::Media { .. }) => (4, false, true),
            Some(CssomRuleKind::Keyframes { .. }) => (7, false, false),
            Some(CssomRuleKind::Namespace { .. }) => (10, false, false),
            Some(CssomRuleKind::Supports { .. }) => (12, false, true),
            Some(CssomRuleKind::LayerBlock { .. } | CssomRuleKind::Scope { .. }) => {
                (0, false, true)
            }
            _ => (0, false, false),
        }
    };
    let k_type = vm.heap.intern_key("type");
    vm.heap.object_define_data(
        obj,
        k_type,
        JsValue::from_i32(rule_type),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    let k_ps = vm.heap.intern_key("parentStyleSheet");
    vm.heap.object_define_data(
        obj,
        k_ps,
        JsValue::from_object(parent_sheet),
        PropertyAttrs::DEFAULT,
        &mut vm.shapes,
    );

    // `cssText`
    let st_ct = Rc::clone(&state);
    let p_ct = rule_path.clone();
    let g_ct = vm.alloc_host_fn("get cssText", move |vm, _, _| {
        let text = st_ct
            .borrow()
            .rule_at(&p_ct)
            .map(CssomRuleKind::to_css_text)
            .unwrap_or_default();
        Ok(JsValue::from_string(vm.heap.alloc_string(text)))
    });
    define_host_accessor(vm, obj, "cssText", Some(g_ct), None);

    // `selectorText` (for CSSStyleRule)
    let st_s1 = Rc::clone(&state);
    let p_s1 = rule_path.clone();
    let g_sel = vm.alloc_host_fn("get selectorText", move |vm, _, _| {
        let sel = match st_s1.borrow().rule_at(&p_s1) {
            Some(CssomRuleKind::Style { selector_text, .. }) => selector_text.clone(),
            _ => String::new(),
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(sel)))
    });
    let st_s2 = Rc::clone(&state);
    let h_s2 = Rc::clone(host);
    let p_s2 = rule_path.clone();
    let s_sel = vm.alloc_host_fn("set selectorText", move |vm, _, args| {
        let raw = vm.value_to_rust_string(arg(args, 0));
        let ns = st_s2.borrow().active_namespaces();
        let ns_opt = (!ns.is_empty()).then_some(&ns);
        if let Ok(parsed) = parse_selector_list_with_namespaces(&raw, ns_opt) {
            let canonical = serialize_selector_list(&parsed);
            if let Some(CssomRuleKind::Style { selector_text, .. }) =
                st_s2.borrow_mut().rule_at_mut(&p_s2)
            {
                *selector_text = canonical;
            }
            sync_sheet_to_dom(&h_s2, &st_s2);
        }
        Ok(JsValue::UNDEFINED)
    });
    define_host_accessor(vm, obj, "selectorText", Some(g_sel), Some(s_sel));

    // `style` (`CSSStyleDeclaration` with `[PutForwards=cssText]` per CSSOM §6.4.3)
    if is_style {
        let style_decl = create_rule_style_proxy(vm, host, Rc::clone(&state), rule_path.clone());
        let g_style = vm.alloc_host_fn("get style", move |_, _, _| Ok(style_decl));
        let st_rs = Rc::clone(&state);
        let h_rs = Rc::clone(host);
        let p_rs = rule_path.clone();
        let s_style = vm.alloc_host_fn("set style", move |vm, _, args| {
            let val = vm.value_to_rust_string(arg(args, 0));
            let validated = canonicalize_css_decls(&val);
            if let Some(CssomRuleKind::Style {
                declarations_text, ..
            }) = st_rs.borrow_mut().rule_at_mut(&p_rs)
            {
                *declarations_text = serialize_style_decls_with_priority(&validated);
            }
            sync_sheet_to_dom(&h_rs, &st_rs);
            Ok(JsValue::UNDEFINED)
        });
        define_host_accessor(vm, obj, "style", Some(g_style), Some(s_style));
    }

    // `CSSGroupingRule` (`@media`, `@supports`, `@layer`, `@scope`): live `cssRules`, `insertRule`, `deleteRule`
    if is_grouping {
        let live_child_rules =
            create_live_css_rule_list(vm, host, Rc::clone(&state), parent_sheet, rule_path.clone());
        let k_cr = vm.heap.intern_key("cssRules");
        vm.heap.object_define_data(
            obj,
            k_cr,
            live_child_rules,
            PropertyAttrs::DEFAULT,
            &mut vm.shapes,
        );
        attach_rule_list_mutators(vm, obj, host, Rc::clone(&state), rule_path.clone());
    }

    // `conditionText` / `name` / `namespaceURI` / `prefix`
    let st_cond = Rc::clone(&state);
    let p_cond = rule_path.clone();
    let g_cond = vm.alloc_host_fn("get conditionText", move |vm, _, _| {
        let txt = match st_cond.borrow().rule_at(&p_cond) {
            Some(
                CssomRuleKind::Media { condition_text, .. }
                | CssomRuleKind::Supports { condition_text, .. },
            ) => condition_text.clone(),
            _ => String::new(),
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(txt)))
    });
    define_host_accessor(vm, obj, "conditionText", Some(g_cond), None);

    let st_name = Rc::clone(&state);
    let p_name = rule_path.clone();
    let g_name = vm.alloc_host_fn("get name", move |vm, _, _| {
        let txt = match st_name.borrow().rule_at(&p_name) {
            Some(
                CssomRuleKind::LayerBlock { name, .. }
                | CssomRuleKind::Keyframes { name, .. }
                | CssomRuleKind::Property { name, .. },
            ) => name.clone(),
            _ => String::new(),
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(txt)))
    });
    define_host_accessor(vm, obj, "name", Some(g_name), None);

    let st_ns = Rc::clone(&state);
    let p_ns = rule_path.clone();
    let g_ns = vm.alloc_host_fn("get namespaceURI", move |vm, _, _| {
        let txt = match st_ns.borrow().rule_at(&p_ns) {
            Some(CssomRuleKind::Namespace { url, .. }) => url.clone(),
            _ => String::new(),
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(txt)))
    });
    define_host_accessor(vm, obj, "namespaceURI", Some(g_ns), None);

    let st_pref = Rc::clone(&state);
    let p_pref = rule_path;
    let g_pref = vm.alloc_host_fn("get prefix", move |vm, _, _| {
        let txt = match st_pref.borrow().rule_at(&p_pref) {
            Some(CssomRuleKind::Namespace { prefix, .. }) => prefix.clone().unwrap_or_default(),
            _ => String::new(),
        };
        Ok(JsValue::from_string(vm.heap.alloc_string(txt)))
    });
    define_host_accessor(vm, obj, "prefix", Some(g_pref), None);

    obj
}

fn create_rule_style_proxy(
    vm: &mut JsVm,
    host: &SharedHost,
    state: SharedStyleSheet,
    rule_path: Vec<usize>,
) -> JsValue {
    let target = vm.alloc_ordinary_object();
    let st1 = Rc::clone(&state);
    let p1 = rule_path.clone();
    define_host_method(vm, target, "getPropertyValue", move |vm, _, args| {
        let prop = normalize_css_prop_name(&vm.value_to_rust_string(arg(args, 0)));
        let raw = match st1.borrow().rule_at(&p1) {
            Some(CssomRuleKind::Style {
                declarations_text, ..
            }) => declarations_text.clone(),
            _ => String::new(),
        };
        let decls = parse_style_decls_with_priority(&raw);
        let val = lookup_decl_or_shorthand(&decls, &prop);
        Ok(JsValue::from_string(vm.heap.alloc_string(val)))
    });
    let st_prio = Rc::clone(&state);
    let p_prio = rule_path.clone();
    define_host_method(vm, target, "getPropertyPriority", move |vm, _, args| {
        let prop = normalize_css_prop_name(&vm.value_to_rust_string(arg(args, 0)));
        let raw = match st_prio.borrow().rule_at(&p_prio) {
            Some(CssomRuleKind::Style {
                declarations_text, ..
            }) => declarations_text.clone(),
            _ => String::new(),
        };
        let decls = parse_style_decls_with_priority(&raw);
        let imp = decls
            .iter()
            .find(|(k, _, _)| k == &prop)
            .is_some_and(|(_, _, i)| *i);
        let s = if imp { "important" } else { "" };
        Ok(JsValue::from_string(vm.heap.alloc_string(s)))
    });
    let st_item = Rc::clone(&state);
    let p_item = rule_path.clone();
    define_host_method(vm, target, "item", move |vm, _, args| {
        let idx = vm.to_number(arg(args, 0)).unwrap_or(-1.0);
        if idx < 0.0 || !idx.is_finite() {
            return Ok(JsValue::from_string(vm.heap.alloc_string("")));
        }
        let raw = match st_item.borrow().rule_at(&p_item) {
            Some(CssomRuleKind::Style {
                declarations_text, ..
            }) => declarations_text.clone(),
            _ => String::new(),
        };
        let decls = parse_style_decls_with_priority(&raw);
        let name = decls
            .get(idx as usize)
            .map(|(k, _, _)| k.as_str())
            .unwrap_or("");
        Ok(JsValue::from_string(vm.heap.alloc_string(name)))
    });
    let st2 = Rc::clone(&state);
    let h2 = Rc::clone(host);
    let p2 = rule_path.clone();
    define_host_method(vm, target, "setProperty", move |vm, _, args| {
        let prop = normalize_css_prop_name(&vm.value_to_rust_string(arg(args, 0)));
        let val_arg = arg(args, 1);
        let raw_val = if val_arg.is_null() || val_arg.is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(val_arg)
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
        let cur = match st2.borrow().rule_at(&p2) {
            Some(CssomRuleKind::Style {
                declarations_text, ..
            }) => declarations_text.clone(),
            _ => return Ok(JsValue::UNDEFINED),
        };
        let mut decls = parse_style_decls_with_priority(&cur);
        if !set_decl_expanded(&mut decls, &prop, &raw_val, important, false) {
            return Ok(JsValue::UNDEFINED);
        }
        if let Some(CssomRuleKind::Style {
            declarations_text, ..
        }) = st2.borrow_mut().rule_at_mut(&p2)
        {
            *declarations_text = serialize_style_decls_with_priority(&decls);
        }
        sync_sheet_to_dom(&h2, &st2);
        Ok(JsValue::UNDEFINED)
    });
    let st3 = Rc::clone(&state);
    let h3 = Rc::clone(host);
    let p3 = rule_path.clone();
    define_host_method(vm, target, "removeProperty", move |vm, _, args| {
        let prop = normalize_css_prop_name(&vm.value_to_rust_string(arg(args, 0)));
        let cur = match st3.borrow().rule_at(&p3) {
            Some(CssomRuleKind::Style {
                declarations_text, ..
            }) => declarations_text.clone(),
            _ => String::new(),
        };
        let mut decls = parse_style_decls_with_priority(&cur);
        let old = remove_decl_or_shorthand(&mut decls, &prop);
        if let Some(CssomRuleKind::Style {
            declarations_text, ..
        }) = st3.borrow_mut().rule_at_mut(&p3)
        {
            *declarations_text = serialize_style_decls_with_priority(&decls);
        }
        sync_sheet_to_dom(&h3, &st3);
        Ok(JsValue::from_string(vm.heap.alloc_string(old)))
    });

    let handler = vm.alloc_ordinary_object();
    let st_get = Rc::clone(&state);
    let p_get = rule_path.clone();
    define_host_method(vm, handler, "get", move |vm, _, args| {
        let prop = vm.value_to_rust_string(arg(args, 1));
        let k = vm.heap.intern_key(&prop);
        if let Some(t_h) = arg(args, 0).as_object() {
            if vm.heap.object_get_own(t_h, k, &vm.shapes).is_some() {
                return vm.get_property_by_key(t_h, k);
            }
        }
        let raw = match st_get.borrow().rule_at(&p_get) {
            Some(CssomRuleKind::Style {
                declarations_text, ..
            }) => declarations_text.clone(),
            _ => String::new(),
        };
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
            if let Some(CssomRuleKind::Style {
                declarations_text, ..
            }) = st_set.borrow_mut().rule_at_mut(&p_set)
            {
                *declarations_text = serialize_style_decls_with_priority(&validated);
            }
            sync_sheet_to_dom(&h_set, &st_set);
            return Ok(JsValue::TRUE);
        }
        let css_key = camel_to_kebab(&prop);
        let cur = match st_set.borrow().rule_at(&p_set) {
            Some(CssomRuleKind::Style {
                declarations_text, ..
            }) => declarations_text.clone(),
            _ => return Ok(JsValue::TRUE),
        };
        let mut decls = parse_style_decls_with_priority(&cur);
        if !set_decl_expanded(&mut decls, &css_key, &raw_val, false, false) {
            return Ok(JsValue::TRUE);
        }
        if let Some(CssomRuleKind::Style {
            declarations_text, ..
        }) = st_set.borrow_mut().rule_at_mut(&p_set)
        {
            *declarations_text = serialize_style_decls_with_priority(&decls);
        }
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
    let (is_style, is_connected, css_text) = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let doc = doc_rc.borrow();
        let Some(NodeData::Element(el)) = doc.get(nid) else {
            return None;
        };
        if !el.name.local.as_str().eq_ignore_ascii_case("style") {
            return None;
        }
        let connected = doc.is_connected(nid);
        let text = doc.inner_text(nid);
        (true, connected, text)
    };
    if !is_style {
        return None;
    }
    if !is_connected {
        // Per HTML spec, disconnected `<style>` elements have `sheet === null`.
        return None;
    }
    let mut st = host.borrow_mut();
    let sheet = st
        .style_element_sheets
        .entry(nid)
        .or_insert_with(|| {
            Rc::new(RefCell::new(CssStyleSheetState::from_css(
                &css_text,
                Some(nid),
                false,
            )))
        })
        .clone();
    drop(st);
    sheet.borrow_mut().sync_from_text_if_changed(&css_text);
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

fn install_document_stylesheets(vm: &mut JsVm, host: &SharedHost) {
    let dp = host.borrow().prototypes.document_proto;

    // `document.styleSheets`
    let h = Rc::clone(host);
    let g = vm.alloc_host_fn("get styleSheets", move |vm, _, _| {
        let style_nodes: Vec<NodeId> = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            doc.descendants(doc.root())
                .filter(|&n| {
                    matches!(
                        doc.get(n),
                        Some(NodeData::Element(el)) if el.name.local.as_str().eq_ignore_ascii_case("style")
                    )
                })
                .collect()
        };
        let arr = vm.alloc_array();
        for nid in style_nodes {
            if let Some(sheet_state) = get_or_create_style_element_sheet(&h, nid) {
                let obj = wrap_stylesheet(vm, &h, sheet_state, None);
                vm.heap.object_push_element(arr, JsValue::from_object(obj));
            }
        }
        Ok(JsValue::from_object(arr))
    });
    define_host_accessor(vm, dp, "styleSheets", Some(g), None);

    // `document.adoptedStyleSheets`
    let h1 = Rc::clone(host);
    let g_ad = vm.alloc_host_fn("get adoptedStyleSheets", move |vm, _, _| {
        let sheets: Vec<SharedStyleSheet> = h1.borrow().adopted_stylesheets.clone();
        let arr = vm.alloc_array();
        for sh in sheets {
            let obj = wrap_stylesheet(vm, &h1, sh, None);
            vm.heap.object_push_element(arr, JsValue::from_object(obj));
        }
        Ok(JsValue::from_object(arr))
    });
    let h2 = Rc::clone(host);
    let s_ad = vm.alloc_host_fn("set adoptedStyleSheets", move |vm, _, args| {
        let mut new_sheets = Vec::new();
        if let Some(arr_h) = arg(args, 0).as_object() {
            let k_len = vm.heap.intern_key("length");
            let len = vm
                .get_property_by_key(arr_h, k_len)
                .ok()
                .and_then(|v| vm.to_number(v).ok())
                .unwrap_or(0.0) as usize;
            for i in 0..len {
                let ki = vm.heap.intern_key(&i.to_string());
                if let Ok(v) = vm.get_property_by_key(arr_h, ki) {
                    if let Some(sh_h) = v.as_object() {
                        if let Some(st) = h2.borrow().stylesheet_handle_map.get(&sh_h).cloned() {
                            new_sheets.push(st);
                        }
                    }
                }
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

fn install_get_computed_style(vm: &mut JsVm, host: &SharedHost) {
    let h = Rc::clone(host);
    let gcs = vm.alloc_host_fn("getComputedStyle", move |vm, _, args| {
        let nid = require_node(vm, &h, arg(args, 0))?;
        let pseudo = if args.len() >= 2 && !arg(args, 1).is_undefined() && !arg(args, 1).is_null() {
            let raw = vm.value_to_rust_string(arg(args, 1));
            parse_pseudo_element_str(&raw)
        } else {
            None
        };
        Ok(create_computed_style_proxy(vm, Rc::clone(&h), nid, pseudo))
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

fn normalize_css_prop_name(input: &str) -> String {
    if input.starts_with("--") {
        // CSSOM §6.7.2: custom property names are case-sensitive and must NOT be trimmed
        // (so `setProperty("--foo ", "bar")` is rejected as invalid).
        input.to_string()
    } else {
        input.trim().to_ascii_lowercase()
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

fn serialize_style_decls_with_priority(decls: &[(String, String, bool)]) -> String {
    decls
        .iter()
        .map(|(k, v, imp)| {
            let sep = if k.starts_with("--") && v.starts_with(' ') {
                ":"
            } else {
                ": "
            };
            if *imp {
                if v.is_empty() {
                    format!("{k}{sep}!important;")
                } else {
                    format!("{k}{sep}{v} !important;")
                }
            } else {
                format!("{k}{sep}{v};")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
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

fn has_var_ref_ci(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.contains("var(") || lower.contains("env(")
}

fn lookup_decl_or_shorthand(decls: &[(String, String, bool)], prop: &str) -> String {
    if let Some((_, v, _)) = decls.iter().find(|(k, _, _)| k == prop) {
        return v.clone();
    }
    let get = |name: &str| -> Option<(&str, bool)> {
        decls
            .iter()
            .find(|(k, _, _)| k == name)
            .and_then(|(_, v, imp)| {
                (!v.is_empty() && !has_var_ref_ci(v)).then_some((v.as_str(), *imp))
            })
    };
    let get_pair = |a: &str, b: &str| -> Option<String> {
        let (va, ia) = get(a)?;
        let (vb, ib) = get(b)?;
        if ia != ib {
            return None;
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
        Some(collapse_4_sides_str(vt, vr, vb, vl))
    };
    let get_border_side = |prefix: &str| -> Option<String> {
        let (w, iw) = get(&format!("{prefix}-width"))?;
        let (s, is_) = get(&format!("{prefix}-style"))?;
        let (c, ic) = get(&format!("{prefix}-color"))?;
        if iw != is_ || is_ != ic {
            return None;
        }
        Some(format_border_triple(w, s, c))
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
        "border" => get_border_side("border-top").and_then(|t| {
            let r = get_border_side("border-right")?;
            let b = get_border_side("border-bottom")?;
            let l = get_border_side("border-left")?;
            (t == r && r == b && b == l).then_some(t)
        }),
        "border-radius" => get_quad(
            "border-top-left-radius",
            "border-top-right-radius",
            "border-bottom-right-radius",
            "border-bottom-left-radius",
        ),
        _ => None,
    };
    res.unwrap_or_default()
}

fn remove_decl_or_shorthand(decls: &mut Vec<(String, String, bool)>, prop: &str) -> String {
    let old = lookup_decl_or_shorthand(decls, prop);
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
                // longhands unless protected by `!important` during parsing.
                let names: Vec<&str> = longhands.iter().map(|k| k.css_name()).collect();
                decls.retain(|(k, _, i)| {
                    !names.contains(&k.as_str()) || (respect_important_cascade && !important && *i)
                });
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
    let validated = canonicalize_css_decls(val);
    set_element_attr(
        vm,
        host,
        nid,
        "style",
        &serialize_style_decls_with_priority(&validated),
    );
}

/// Creates a CSSOM-validated inline `CSSStyleDeclaration` Proxy for `element.style`.
pub(crate) fn create_inline_style_proxy(vm: &mut JsVm, host: &SharedHost, nid: NodeId) -> JsValue {
    let target = vm.alloc_ordinary_object();
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
        let val_arg = arg(args, 1);
        let raw_val = if val_arg.is_null() || val_arg.is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(val_arg)
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
        let old = remove_decl_or_shorthand(&mut decls, &prop);
        set_element_attr(
            vm,
            &h,
            nid,
            "style",
            &serialize_style_decls_with_priority(&decls),
        );
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
        let css_key = camel_to_kebab(&prop_name);
        if PropertyIdKind::from_name(&css_key).is_some() {
            return Ok(JsValue::TRUE);
        }
        let _ = &h_has;
        Ok(JsValue::FALSE)
    });
    let h_get = Rc::clone(host);
    define_host_method(vm, handler, "get", move |vm, _, args| {
        let prop_name = vm.value_to_rust_string(arg(args, 1));
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
        let css_key = camel_to_kebab(&prop_name);
        let val = lookup_decl_or_shorthand(&decls, &css_key);
        Ok(JsValue::from_string(vm.heap.alloc_string(val)))
    });
    let h_set = Rc::clone(host);
    define_host_method(vm, handler, "set", move |vm, _, args| {
        let prop_name = vm.value_to_rust_string(arg(args, 1));
        let val_arg = arg(args, 2);
        let val = if val_arg.is_null() || val_arg.is_undefined() {
            String::new()
        } else {
            vm.value_to_rust_string(val_arg)
        };
        if prop_name == "cssText" {
            set_inline_style_css_text(vm, &h_set, nid, &val);
            return Ok(JsValue::TRUE);
        }
        let css_key = camel_to_kebab(&prop_name);
        let raw = get_attr_string(&h_set, nid, "style").unwrap_or_default();
        let mut decls = parse_style_decls_with_priority(&raw);
        if !set_decl_expanded(&mut decls, &css_key, &val, false, false) {
            return Ok(JsValue::TRUE);
        }
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
        .entries
        .keys()
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
    let style = live_style_for(host, nid, pseudo);
    let layout_box = if pseudo.is_none() && prop_needs_layout_resolution(prop) {
        host.borrow_mut().resolved_layout_box_for_node(nid)
    } else {
        None
    };
    let parent_flex_grid = if pseudo.is_none() && prop_needs_parent_flex_check(prop) {
        host.borrow_mut().parent_is_flex_or_grid(nid)
    } else {
        false
    };
    format_computed_property_resolved(&style, prop, layout_box, parent_flex_grid)
}

pub(crate) fn create_computed_style_proxy(
    vm: &mut JsVm,
    host: SharedHost,
    nid: NodeId,
    pseudo: Option<RixPseudoElement>,
) -> JsValue {
    let target = vm.alloc_ordinary_object();
    let h1 = Rc::clone(&host);
    define_host_method(vm, target, "getPropertyValue", move |vm, _, args| {
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
            let total = COMPUTED_STYLE_LONGHANDS.len() + style.custom_properties.entries.len();
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
                let total = COMPUTED_STYLE_LONGHANDS.len() + style.custom_properties.entries.len();
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
    define_host_method(vm, handler, "get", move |vm, _, args| {
        let prop = vm.value_to_rust_string(arg(args, 1));
        if prop == "length" {
            let style = live_style_for(&h_get, nid, pseudo);
            let total = COMPUTED_STYLE_LONGHANDS.len() + style.custom_properties.entries.len();
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
    if c.a == 255 {
        format!("rgb({}, {}, {})", c.r, c.g, c.b)
    } else if c.a == 0 {
        format!("rgba({}, {}, {}, 0)", c.r, c.g, c.b)
    } else {
        let a = (f64::from(c.a) / 255.0 * 1000.0).round() / 1000.0;
        format!("rgba({}, {}, {}, {a})", c.r, c.g, c.b)
    }
}

fn format_css_color(c: CssColor, current_color: ColorRgba, dark: bool) -> String {
    format_rgba(c.resolve(current_color, dark))
}

fn format_num(v: f32) -> String {
    if v == 0.0 {
        return "0".to_string();
    }
    if (v - v.round()).abs() < 1e-4 {
        format!("{}", v.round() as i64)
    } else {
        format!("{v}")
    }
}

fn format_px(v: f32) -> String {
    format!("{}px", format_num(v))
}

fn eval_ctx_for_style(style: &ComputedStyle) -> EvalContext {
    EvalContext {
        font_size_px: style.text.font_size_px,
        root_font_size_px: 16.0,
        viewport_width_px: 800.0,
        viewport_height_px: 600.0,
        ..EvalContext::default()
    }
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

fn format_size_opts(s: &SizeValue, eval: &EvalContext, nonneg: bool) -> String {
    match s {
        SizeValue::Auto => "auto".into(),
        SizeValue::None => "none".into(),
        SizeValue::MinContent => "min-content".into(),
        SizeValue::MaxContent => "max-content".into(),
        SizeValue::FitContent(_) => "fit-content".into(),
        SizeValue::Stretch => "stretch".into(),
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
    match a {
        AlignValue::Auto => "auto",
        AlignValue::Normal => "normal",
        AlignValue::Stretch => "stretch",
        AlignValue::Center => "center",
        AlignValue::Start => "start",
        AlignValue::End => "end",
        AlignValue::FlexStart => "flex-start",
        AlignValue::FlexEnd => "flex-end",
        AlignValue::Baseline => "baseline",
        AlignValue::SpaceBetween => "space-between",
        AlignValue::SpaceAround => "space-around",
        AlignValue::SpaceEvenly => "space-evenly",
    }
}

/// Serializes any computed CSS property from [`ComputedStyle`] according to CSSOM.
#[must_use]
pub fn format_computed_property(style: &ComputedStyle, prop: &str) -> String {
    format_computed_property_resolved(style, prop, None, false)
}

fn format_computed_property_resolved(
    style: &ComputedStyle,
    prop: &str,
    layout_box: Option<ResolvedLayoutBox>,
    parent_flex_grid: bool,
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
    let dark = style.color_scheme_dark;
    let eval = eval_ctx_for_style(style);

    // Computed shorthand serialization (both logical and physical)
    let fmt_pair = |a: &str, b: &str| -> String {
        let va = format_computed_property_resolved(style, a, layout_box, parent_flex_grid);
        let vb = format_computed_property_resolved(style, b, layout_box, parent_flex_grid);
        collapse_2_sides_str(&va, &vb)
    };
    let fmt_quad = |t: &str, r: &str, b: &str, l: &str| -> String {
        let vt = format_computed_property_resolved(style, t, layout_box, parent_flex_grid);
        let vr = format_computed_property_resolved(style, r, layout_box, parent_flex_grid);
        let vb = format_computed_property_resolved(style, b, layout_box, parent_flex_grid);
        let vl = format_computed_property_resolved(style, l, layout_box, parent_flex_grid);
        collapse_4_sides_str(&vt, &vr, &vb, &vl)
    };
    let fmt_border_side = |prefix: &str| -> String {
        let w = format_computed_property_resolved(
            style,
            &format!("{prefix}-width"),
            layout_box,
            parent_flex_grid,
        );
        let s = format_computed_property_resolved(
            style,
            &format!("{prefix}-style"),
            layout_box,
            parent_flex_grid,
        );
        let c = format_computed_property_resolved(
            style,
            &format!("{prefix}-color"),
            layout_box,
            parent_flex_grid,
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
        "display" => match style.display {
            DisplayMode::None => "none",
            DisplayMode::Block => "block",
            DisplayMode::Inline => "inline",
            DisplayMode::InlineBlock => "inline-block",
            DisplayMode::Flex => "flex",
            DisplayMode::InlineFlex => "inline-flex",
            DisplayMode::Grid => "grid",
            DisplayMode::InlineGrid => "inline-grid",
            DisplayMode::Table => "table",
            DisplayMode::InlineTable => "inline-table",
            DisplayMode::TableRow => "table-row",
            DisplayMode::TableCell => "table-cell",
            DisplayMode::ListItem => "list-item",
            DisplayMode::Contents => "contents",
            DisplayMode::FlowRoot => "flow-root",
        }
        .into(),
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
            CursorKind::Move => "move",
            CursorKind::NotAllowed => "not-allowed",
            CursorKind::Grab => "grab",
            CursorKind::Grabbing => "grabbing",
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
            CursorKind::None => "none",
        }
        .into(),
        "z-index" => match style.z_index {
            ZIndexValue::Auto => "auto".into(),
            ZIndexValue::Integer(n) => n.to_string(),
        },
        "opacity" => format_num(style.opacity),
        "color" => format_rgba(style.text.color),
        "background-color" => format_rgba(style.visual.background_color),
        "font-size" => format_px(style.text.font_size_px),
        "font-weight" => format_num(style.text.font_weight),
        "font-style" => match style.text.font_style {
            FontStyleMode::Normal => "normal",
            FontStyleMode::Italic => "italic",
            FontStyleMode::Oblique(_) => "oblique",
        }
        .into(),
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
            LineHeightValue::Length(ref lp) => format_lp(lp, &eval),
        },
        "letter-spacing" => match style.text.letter_spacing {
            LengthPercentage::Zero => "normal".into(),
            ref lp => format_lp(lp, &eval),
        },
        "word-spacing" => format_lp(&style.text.word_spacing, &eval),
        "text-indent" => format_lp(&style.text.text_indent, &eval),
        "text-align" => match style.text.text_align {
            TextAlignMode::Start => "start",
            TextAlignMode::End => "end",
            TextAlignMode::Left => "left",
            TextAlignMode::Right => "right",
            TextAlignMode::Center => "center",
            TextAlignMode::Justify => "justify",
            TextAlignMode::MatchParent => "match-parent",
        }
        .into(),
        "text-transform" => match style.text.text_transform {
            TextTransformMode::None => "none",
            TextTransformMode::Uppercase => "uppercase",
            TextTransformMode::Lowercase => "lowercase",
            TextTransformMode::Capitalize => "capitalize",
        }
        .into(),
        "white-space" => match style.text.white_space {
            WhiteSpaceMode::Normal => "normal",
            WhiteSpaceMode::Nowrap => "nowrap",
            WhiteSpaceMode::Pre => "pre",
            WhiteSpaceMode::PreWrap => "pre-wrap",
            WhiteSpaceMode::PreLine => "pre-line",
            WhiteSpaceMode::BreakSpaces => "break-spaces",
        }
        .into(),
        "word-break" => match style.text.word_break {
            WordBreakMode::Normal => "normal",
            WordBreakMode::BreakAll => "break-all",
            WordBreakMode::KeepAll => "keep-all",
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
            |lb| format_px(lb.margin.top),
        ),
        "margin-right" => layout_box.map_or_else(
            || format_lpa(&style.box_model.margin.right, &eval),
            |lb| format_px(lb.margin.right),
        ),
        "margin-bottom" => layout_box.map_or_else(
            || format_lpa(&style.box_model.margin.bottom, &eval),
            |lb| format_px(lb.margin.bottom),
        ),
        "margin-left" => layout_box.map_or_else(
            || format_lpa(&style.box_model.margin.left, &eval),
            |lb| format_px(lb.margin.left),
        ),
        "padding-top" => layout_box.map_or_else(
            || format_lp_opts(&style.box_model.padding.top, &eval, true),
            |lb| format_px(lb.padding.top.max(0.0)),
        ),
        "padding-right" => layout_box.map_or_else(
            || format_lp_opts(&style.box_model.padding.right, &eval, true),
            |lb| format_px(lb.padding.right.max(0.0)),
        ),
        "padding-bottom" => layout_box.map_or_else(
            || format_lp_opts(&style.box_model.padding.bottom, &eval, true),
            |lb| format_px(lb.padding.bottom.max(0.0)),
        ),
        "padding-left" => layout_box.map_or_else(
            || format_lp_opts(&style.box_model.padding.left, &eval, true),
            |lb| format_px(lb.padding.left.max(0.0)),
        ),
        "top" => format_lpa(&style.box_model.inset.top, &eval),
        "right" => format_lpa(&style.box_model.inset.right, &eval),
        "bottom" => format_lpa(&style.box_model.inset.bottom, &eval),
        "left" => format_lpa(&style.box_model.inset.left, &eval),
        "border-top-width" => format_px(style.box_model.border_width.top),
        "border-right-width" => format_px(style.box_model.border_width.right),
        "border-bottom-width" => format_px(style.box_model.border_width.bottom),
        "border-left-width" => format_px(style.box_model.border_width.left),
        "border-top-style" => format_line_style(style.visual.border_style.top).into(),
        "border-right-style" => format_line_style(style.visual.border_style.right).into(),
        "border-bottom-style" => format_line_style(style.visual.border_style.bottom).into(),
        "border-left-style" => format_line_style(style.visual.border_style.left).into(),
        "border-top-color" => format_css_color(style.visual.border_color.top, cc, dark),
        "border-right-color" => format_css_color(style.visual.border_color.right, cc, dark),
        "border-bottom-color" => format_css_color(style.visual.border_color.bottom, cc, dark),
        "border-left-color" => format_css_color(style.visual.border_color.left, cc, dark),
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
        "outline-color" => format_css_color(style.visual.outline_color, cc, dark),
        "outline-style" => format_line_style(style.visual.outline_style).into(),
        "outline-width" => format_px(style.visual.outline_width),
        "outline-offset" => format_lp(&style.visual.outline_offset, &eval),
        "vertical-align" => match style.box_model.vertical_align {
            VerticalAlignMode::Baseline => "baseline".into(),
            VerticalAlignMode::Top => "top".into(),
            VerticalAlignMode::Middle => "middle".into(),
            VerticalAlignMode::Bottom => "bottom".into(),
            VerticalAlignMode::TextTop => "text-top".into(),
            VerticalAlignMode::TextBottom => "text-bottom".into(),
            VerticalAlignMode::Sub => "sub".into(),
            VerticalAlignMode::Super => "super".into(),
            VerticalAlignMode::LengthPx(px) => format_px(px),
        },
        "text-overflow" => match style.box_model.text_overflow {
            TextOverflowMode::Clip => "clip",
            TextOverflowMode::Ellipsis => "ellipsis",
        }
        .into(),
        "border-collapse" => match style.box_model.border_collapse {
            BorderCollapseMode::Separate => "separate",
            BorderCollapseMode::Collapse => "collapse",
        }
        .into(),
        "border-spacing" => {
            let x = format_lp_opts(&style.box_model.border_spacing_x, &eval, true);
            let y = format_lp_opts(&style.box_model.border_spacing_y, &eval, true);
            collapse_2_sides_str(&x, &y)
        }
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
        "filter" => match style.effects.as_ref().map(|e| e.filter.as_slice()) {
            Some(list) if !list.is_empty() => list
                .iter()
                .map(|op| match op {
                    FilterOp::Blur(r) => format!("blur({})", format_lp_opts(r, &eval, true)),
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
                        format_lp(offset_x, &eval),
                        format_lp(offset_y, &eval),
                        format_lp_opts(blur, &eval, true),
                    ),
                })
                .collect::<Vec<_>>()
                .join(" "),
            _ => "none".into(),
        },
        "table-layout" => match style.box_model.table_layout {
            TableLayoutMode::Auto => "auto",
            TableLayoutMode::Fixed => "fixed",
        }
        .into(),
        "content" => match &style.box_model.content {
            Some(c) => format!("\"{}\"", c.as_str()),
            None => "normal".into(),
        },
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
        "flex-grow" => format_num(style.flex_grid.as_ref().map_or(0.0, |f| f.flex_grow)),
        "flex-shrink" => format_num(style.flex_grid.as_ref().map_or(1.0, |f| f.flex_shrink)),
        "flex-basis" => style
            .flex_grid
            .as_ref()
            .map_or_else(|| "auto".into(), |f| format_lpa(&f.flex_basis, &eval)),
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
        "row-gap" => style
            .flex_grid
            .as_ref()
            .map_or_else(|| "normal".into(), |f| format_lp(&f.row_gap, &eval)),
        "column-gap" => style
            .flex_grid
            .as_ref()
            .map_or_else(|| "normal".into(), |f| format_lp(&f.column_gap, &eval)),
        "grid-auto-flow" => match style.flex_grid.as_ref().map(|f| f.grid_auto_flow) {
            Some(GridAutoFlowMode::Column) => "column",
            Some(GridAutoFlowMode::RowDense) => "row dense",
            Some(GridAutoFlowMode::ColumnDense) => "column dense",
            _ => "row",
        }
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
        "transition-property" => match style.effects.as_ref().map(|e| e.transitions.as_slice()) {
            Some(list) if !list.is_empty() => list
                .iter()
                .map(|t| t.property.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            _ => "all".into(),
        },
        "transition-duration" => match style.effects.as_ref().map(|e| e.transitions.as_slice()) {
            Some(list) if !list.is_empty() => list
                .iter()
                .map(|t| format!("{}s", format_num(t.duration_ms / 1000.0)))
                .collect::<Vec<_>>()
                .join(", "),
            _ => "0s".into(),
        },
        "transition-delay" => match style.effects.as_ref().map(|e| e.transitions.as_slice()) {
            Some(list) if !list.is_empty() => list
                .iter()
                .map(|t| format!("{}s", format_num(t.delay_ms / 1000.0)))
                .collect::<Vec<_>>()
                .join(", "),
            _ => "0s".into(),
        },
        "transition-timing-function" => {
            match style.effects.as_ref().map(|e| e.transitions.as_slice()) {
                Some(list) if !list.is_empty() => list
                    .iter()
                    .map(|t| match t.timing {
                        rix_css::TimingFunction::Linear => "linear".to_string(),
                        rix_css::TimingFunction::Ease => "ease".to_string(),
                        rix_css::TimingFunction::EaseIn => "ease-in".to_string(),
                        rix_css::TimingFunction::EaseOut => "ease-out".to_string(),
                        rix_css::TimingFunction::EaseInOut => "ease-in-out".to_string(),
                        rix_css::TimingFunction::CubicBezier(a, b, c, d) => {
                            format!(
                                "cubic-bezier({}, {}, {}, {})",
                                format_num(a),
                                format_num(b),
                                format_num(c),
                                format_num(d)
                            )
                        }
                        rix_css::TimingFunction::Steps(n, start) => {
                            if start {
                                format!("steps({n}, start)")
                            } else {
                                format!("steps({n})")
                            }
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => "ease".into(),
            }
        }
        _ => String::new(),
    }
}

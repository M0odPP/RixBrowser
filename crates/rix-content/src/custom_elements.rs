//! WHATWG HTML §4.13 Custom Elements, Scoped `CustomElementRegistry`,
//! `[CEReactions]` Reaction Queue, `HTMLElement` Construction Stack,
//! `ElementInternals`, `CustomStateSet`, `ARIAMixin`, and Form-Associated
//! Custom Elements.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use rix_dom::{
    is_valid_custom_element_name, CustomElementState, Document, ElementState, NodeData, NodeId,
};
use rix_js::promise::{alloc_promise, reject_promise, resolve_promise};
use rix_js::{HeapHandle, JsException, JsObject, JsValue, JsVm, ObjectKind, PropertyAttrs};

use crate::dom_bridge::{
    arg, define_global, define_host_accessor, define_host_method, require_node, wrap_node,
    DomMutation, SharedHost,
};
use crate::dom_interfaces::{html_tag_interface_name, make_proto, HTML_NS};
use crate::event_loop::SharedEventLoop;
use crate::events::SharedEvents;
use crate::web_apis::create_dom_exception;

/// Identifier for a `CustomElementRegistry` (`RegistryId::GLOBAL` is `window.customElements`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegistryId(pub u64);

impl RegistryId {
    pub const GLOBAL: Self = Self(0);
}

/// Entry on a custom element definition's WHATWG §4.13.2 construction stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstructionEntry {
    Element(NodeId),
    AlreadyConstructed,
}

/// Complete WHATWG HTML §4.13.4 custom element definition.
#[derive(Debug, Clone)]
pub struct CustomElementDefinition {
    pub name: String,
    pub local_name: String,
    pub constructor: HeapHandle,
    pub prototype: HeapHandle,
    pub connected_cb: Option<HeapHandle>,
    pub disconnected_cb: Option<HeapHandle>,
    pub connected_move_cb: Option<HeapHandle>,
    pub adopted_cb: Option<HeapHandle>,
    pub attr_changed_cb: Option<HeapHandle>,
    pub form_associated_cb: Option<HeapHandle>,
    pub form_disabled_cb: Option<HeapHandle>,
    pub form_reset_cb: Option<HeapHandle>,
    pub form_state_restore_cb: Option<HeapHandle>,
    pub observed_attributes: Vec<String>,
    pub disable_shadow: bool,
    pub disable_internals: bool,
    pub form_associated: bool,
    pub construction_stack: Vec<ConstructionEntry>,
}

/// Per-registry storage (`window.customElements` or `new CustomElementRegistry()`).
#[derive(Debug)]
pub struct CustomElementRegistryData {
    pub id: RegistryId,
    pub is_scoped: bool,
    pub js_handle: HeapHandle,
    pub by_name: HashMap<String, CustomElementDefinition>,
    pub by_ctor: HashMap<HeapHandle, String>,
    pub when_defined_waiters: HashMap<String, Vec<HeapHandle>>,
    pub definition_running: bool,
}

/// A single queued custom element reaction (WHATWG HTML §4.13.6).
#[derive(Debug, Clone)]
pub enum CeReaction {
    Upgrade(NodeId),
    Connected(NodeId),
    Disconnected(NodeId),
    ConnectedMove(NodeId),
    Adopted {
        node: NodeId,
        old_doc: NodeId,
        new_doc: NodeId,
    },
    AttributeChanged {
        node: NodeId,
        name: String,
        old_value: Option<String>,
        new_value: Option<String>,
        namespace: Option<String>,
    },
    FormAssociated {
        node: NodeId,
        form: Option<NodeId>,
    },
    FormDisabled {
        node: NodeId,
        disabled: bool,
    },
    FormReset(NodeId),
}

/// Constraint validation flags for `ElementInternals.validity`.
#[derive(Debug, Clone, Default)]
pub struct ValidityFlags {
    pub value_missing: bool,
    pub type_mismatch: bool,
    pub pattern_mismatch: bool,
    pub too_long: bool,
    pub too_short: bool,
    pub range_underflow: bool,
    pub range_overflow: bool,
    pub step_mismatch: bool,
    pub bad_input: bool,
    pub custom_error: bool,
}

impl ValidityFlags {
    #[must_use]
    pub fn is_valid(&self) -> bool {
        !(self.value_missing
            || self.type_mismatch
            || self.pattern_mismatch
            || self.too_long
            || self.too_short
            || self.range_underflow
            || self.range_overflow
            || self.step_mismatch
            || self.bad_input
            || self.custom_error)
    }
}

/// Per-element `ElementInternals` backing state.
#[derive(Debug, Clone)]
pub struct ElementInternalsData {
    pub js_handle: HeapHandle,
    pub states_handle: HeapHandle,
    pub state_entries: Vec<(u64, String)>,
    pub next_state_seq: u64,
    pub role: Option<String>,
    pub aria_strings: HashMap<String, Option<String>>,
    pub aria_elements: HashMap<String, Option<Vec<NodeId>>>,
    pub form_value: Option<JsValue>,
    pub form_state: Option<JsValue>,
    pub validity: ValidityFlags,
    pub validation_message: String,
    pub validation_anchor: Option<NodeId>,
}

/// Central custom elements & scoped registry state owned via `RefCell` on `HostState`.
#[derive(Default)]
pub struct CustomElementsState {
    pub next_registry_id: u64,
    pub global_registry_handle: Option<HeapHandle>,
    pub registry_proto: Option<HeapHandle>,
    pub internals_proto: Option<HeapHandle>,
    pub state_set_proto: Option<HeapHandle>,
    pub registries: HashMap<RegistryId, CustomElementRegistryData>,
    pub handle_to_registry: HashMap<HeapHandle, RegistryId>,
    /// Explicit registry per `Document`. Absent means: `Some(GLOBAL)` for main doc, `None` for inert docs.
    pub doc_registries: HashMap<NodeId, Option<RegistryId>>,
    /// Explicit registry per `ShadowRoot`. Absent means: `None` if `null_registry`, else inherit host.
    pub shadow_registries: HashMap<NodeId, Option<RegistryId>>,
    /// Explicit registry per `Element`. Absent means: inherit from tree root (or `GLOBAL` for main doc).
    pub element_registries: HashMap<NodeId, Option<RegistryId>>,
    /// Resolved `(RegistryId, definition_name)` for each upgraded/constructed custom element.
    pub element_definitions: HashMap<NodeId, (RegistryId, String)>,
    pub internals_attached: HashSet<NodeId>,
    pub internals_data: HashMap<NodeId, ElementInternalsData>,
    pub internals_handle_to_node: HashMap<HeapHandle, NodeId>,
    pub state_set_handle_to_node: HashMap<HeapHandle, NodeId>,
    pub reactions_stack: Vec<Vec<CeReaction>>,
    pub backup_queue: Vec<CeReaction>,
    pub processing_backup: bool,
    /// Active `(RegistryId, def_name, node_id)` stack during upgrades so `super()` resolves scoped definitions.
    pub active_upgrade_stack: Vec<(RegistryId, String, NodeId)>,
    /// Active constructor-invocation registry hint (for `createElement` with explicit scoped registry).
    pub active_construct_registry: Vec<RegistryId>,
}

impl CustomElementsState {
    /// Traces every JS heap handle held strongly by registries, definitions, waiters, and internals.
    pub fn collect_roots(&self, out: &mut Vec<JsValue>) {
        for h in [
            self.global_registry_handle,
            self.registry_proto,
            self.internals_proto,
            self.state_set_proto,
        ]
        .into_iter()
        .flatten()
        {
            out.push(JsValue::from_object(h));
        }
        for reg in self.registries.values() {
            out.push(JsValue::from_object(reg.js_handle));
            for def in reg.by_name.values() {
                out.push(JsValue::from_object(def.constructor));
                out.push(JsValue::from_object(def.prototype));
                for cb in [
                    def.connected_cb,
                    def.disconnected_cb,
                    def.connected_move_cb,
                    def.adopted_cb,
                    def.attr_changed_cb,
                    def.form_associated_cb,
                    def.form_disabled_cb,
                    def.form_reset_cb,
                    def.form_state_restore_cb,
                ]
                .into_iter()
                .flatten()
                {
                    out.push(JsValue::from_object(cb));
                }
            }
            for waiters in reg.when_defined_waiters.values() {
                for &p in waiters {
                    out.push(JsValue::from_object(p));
                }
            }
        }
        for data in self.internals_data.values() {
            out.push(JsValue::from_object(data.js_handle));
            out.push(JsValue::from_object(data.states_handle));
            if let Some(v) = data.form_value {
                out.push(v);
            }
            if let Some(v) = data.form_state {
                out.push(v);
            }
        }
    }

    pub fn has_any_definitions(&self) -> bool {
        self.registries.values().any(|r| !r.by_name.is_empty())
    }
}

/// All W3C ARIA 1.3 `ARIAMixin` string properties `(idl_name, content_attr_name)`.
pub const ARIA_STRING_ATTRS: &[(&str, &str)] = &[
    ("role", "role"),
    ("ariaAtomic", "aria-atomic"),
    ("ariaAutoComplete", "aria-autocomplete"),
    ("ariaBrailleLabel", "aria-braillelabel"),
    ("ariaBrailleRoleDescription", "aria-brailleroledescription"),
    ("ariaBusy", "aria-busy"),
    ("ariaChecked", "aria-checked"),
    ("ariaColCount", "aria-colcount"),
    ("ariaColIndex", "aria-colindex"),
    ("ariaColIndexText", "aria-colindextext"),
    ("ariaColSpan", "aria-colspan"),
    ("ariaCurrent", "aria-current"),
    ("ariaDescription", "aria-description"),
    ("ariaDisabled", "aria-disabled"),
    ("ariaExpanded", "aria-expanded"),
    ("ariaHasPopup", "aria-haspopup"),
    ("ariaHidden", "aria-hidden"),
    ("ariaInvalid", "aria-invalid"),
    ("ariaKeyShortcuts", "aria-keyshortcuts"),
    ("ariaLabel", "aria-label"),
    ("ariaLevel", "aria-level"),
    ("ariaLive", "aria-live"),
    ("ariaModal", "aria-modal"),
    ("ariaMultiLine", "aria-multiline"),
    ("ariaMultiSelectable", "aria-multiselectable"),
    ("ariaOrientation", "aria-orientation"),
    ("ariaPlaceholder", "aria-placeholder"),
    ("ariaPosInSet", "aria-posinset"),
    ("ariaPressed", "aria-pressed"),
    ("ariaReadOnly", "aria-readonly"),
    ("ariaRelevant", "aria-relevant"),
    ("ariaRequired", "aria-required"),
    ("ariaRoleDescription", "aria-roledescription"),
    ("ariaRowCount", "aria-rowcount"),
    ("ariaRowIndex", "aria-rowindex"),
    ("ariaRowIndexText", "aria-rowindextext"),
    ("ariaRowSpan", "aria-rowspan"),
    ("ariaSelected", "aria-selected"),
    ("ariaSetSize", "aria-setsize"),
    ("ariaSort", "aria-sort"),
    ("ariaValueMax", "aria-valuemax"),
    ("ariaValueMin", "aria-valuemin"),
    ("ariaValueNow", "aria-valuenow"),
    ("ariaValueText", "aria-valuetext"),
];

/// W3C ARIA element-reflection properties `(idl_name, content_attr_name, is_list)`.
pub const ARIA_ELEMENT_ATTRS: &[(&str, &str, bool)] = &[
    (
        "ariaActiveDescendantElement",
        "aria-activedescendant",
        false,
    ),
    ("ariaControlsElements", "aria-controls", true),
    ("ariaDescribedByElements", "aria-describedby", true),
    ("ariaDetailsElements", "aria-details", true),
    ("ariaErrorMessageElements", "aria-errormessage", true),
    ("ariaFlowToElements", "aria-flowto", true),
    ("ariaLabelledByElements", "aria-labelledby", true),
    ("ariaOwnsElements", "aria-owns", true),
];

// ---------------------------------------------------------------------------
// Registry resolution helpers
// ---------------------------------------------------------------------------

/// Returns the effective `CustomElementRegistry` for a `Document`.
pub(crate) fn registry_for_document(host: &SharedHost, doc_id: NodeId) -> Option<RegistryId> {
    let hb = host.borrow();
    let ce = hb.custom_elements_state.borrow();
    if let Some(&reg) = ce.doc_registries.get(&doc_id) {
        return reg;
    }
    let main_root = hb.document.borrow().root();
    if doc_id == main_root
        || hb
            .doc_meta
            .get(&doc_id)
            .is_some_and(|m| m.default_view.is_some())
    {
        Some(RegistryId::GLOBAL)
    } else {
        None
    }
}

/// Returns the effective `CustomElementRegistry` for a `ShadowRoot`.
pub(crate) fn registry_for_shadow_root(host: &SharedHost, sr_id: NodeId) -> Option<RegistryId> {
    let hb = host.borrow();
    let ce = hb.custom_elements_state.borrow();
    if let Some(&reg) = ce.shadow_registries.get(&sr_id) {
        return reg;
    }
    let doc = hb.document.borrow();
    match doc.get(sr_id) {
        Some(NodeData::ShadowRoot {
            null_registry: true,
            ..
        }) => None,
        Some(NodeData::ShadowRoot { host: host_el, .. }) => {
            let host_el = *host_el;
            drop(doc);
            drop(ce);
            drop(hb);
            effective_registry_for_element(host, host_el)
        }
        _ => None,
    }
}

/// Returns the effective `CustomElementRegistry` for an `Element`.
pub(crate) fn effective_registry_for_element(host: &SharedHost, nid: NodeId) -> Option<RegistryId> {
    let (explicit, root_info) = {
        let hb = host.borrow();
        let ce = hb.custom_elements_state.borrow();
        if let Some(&reg) = ce.element_registries.get(&nid) {
            return reg;
        }
        let doc = hb.document.borrow();
        let root = crate::shadow_dom::tree_root(&doc, nid);
        let info = match doc.get(root) {
            Some(NodeData::ShadowRoot { .. }) => Some((true, root)),
            Some(NodeData::Document) => Some((false, root)),
            _ => Some((false, hb.owner_doc_of(nid))),
        };
        (None::<Option<RegistryId>>, info)
    };
    if let Some(reg) = explicit {
        return reg;
    }
    match root_info {
        Some((true, sr)) => registry_for_shadow_root(host, sr),
        Some((false, doc_id)) => registry_for_document(host, doc_id),
        None => None,
    }
}

/// Sets the explicit custom element registry on a `ShadowRoot`.
pub(crate) fn set_shadow_root_registry(host: &SharedHost, sr_id: NodeId, reg: Option<RegistryId>) {
    host.borrow()
        .custom_elements_state
        .borrow_mut()
        .shadow_registries
        .insert(sr_id, reg);
}

/// Sets the explicit custom element registry on a `Document`.
pub(crate) fn set_document_registry(host: &SharedHost, doc_id: NodeId, reg: Option<RegistryId>) {
    host.borrow()
        .custom_elements_state
        .borrow_mut()
        .doc_registries
        .insert(doc_id, reg);
}

/// Sets the explicit custom element registry on an `Element`.
pub(crate) fn set_element_registry(host: &SharedHost, nid: NodeId, reg: Option<RegistryId>) {
    host.borrow()
        .custom_elements_state
        .borrow_mut()
        .element_registries
        .insert(nid, reg);
}

/// Resolves an optional `customElementRegistry` property from an options dictionary
/// (`createElement`, `createElementNS`, `attachShadow`, `importNode`).
pub(crate) fn resolve_registry_option(
    vm: &mut JsVm,
    host: &SharedHost,
    opts_h: HeapHandle,
    default_reg: Option<RegistryId>,
) -> Result<(bool, Option<RegistryId>), JsException> {
    let k = vm.heap.intern_key("customElementRegistry");
    let v = vm.get_property_by_key(opts_h, k)?;
    if v.is_undefined() {
        return Ok((false, default_reg));
    }
    if v.is_null() {
        return Ok((true, None));
    }
    if let Some(oh) = v.as_object() {
        if let Some(&reg_id) = host
            .borrow()
            .custom_elements_state
            .borrow()
            .handle_to_registry
            .get(&oh)
        {
            return Ok((true, Some(reg_id)));
        }
    }
    Err(vm.type_error("customElementRegistry must be a CustomElementRegistry or null"))
}

/// Returns `true` if `host_id` is a custom element whose definition disables `attachShadow`.
pub(crate) fn is_shadow_disabled_for_element(host: &SharedHost, host_id: NodeId) -> bool {
    let hb = host.borrow();
    let ce = hb.custom_elements_state.borrow();
    if let Some((reg_id, name)) = ce.element_definitions.get(&host_id) {
        if let Some(def) = ce.registries.get(reg_id).and_then(|r| r.by_name.get(name)) {
            return def.disable_shadow;
        }
    }
    drop(ce);
    // Also check un-upgraded definition in effective registry per WHATWG DOM §4.9 step 7.
    let elem_info = {
        let doc = hb.document.borrow();
        match doc.get(host_id) {
            Some(NodeData::Element(e)) if &*e.name.ns == HTML_NS => {
                Some((e.name.local.to_string(), e.is_value.clone()))
            }
            _ => None,
        }
    };
    drop(hb);
    if let Some((local, is_val)) = elem_info {
        let reg = effective_registry_for_element(host, host_id);
        if let Some((reg_id, def_name)) =
            lookup_definition(host, reg, HTML_NS, &local, is_val.as_deref())
        {
            let hb = host.borrow();
            let ce = hb.custom_elements_state.borrow();
            if let Some(def) = ce
                .registries
                .get(&reg_id)
                .and_then(|r| r.by_name.get(&def_name))
            {
                return def.disable_shadow;
            }
        }
    }
    false
}

/// WHATWG DOM §4.9 "look up a custom element definition(registry, namespace, localName, is)".
pub(crate) fn lookup_definition(
    host: &SharedHost,
    reg_opt: Option<RegistryId>,
    ns: &str,
    local_name: &str,
    is_value: Option<&str>,
) -> Option<(RegistryId, String)> {
    if ns != HTML_NS {
        return None;
    }
    let reg_id = reg_opt?;
    let hb = host.borrow();
    let ce = hb.custom_elements_state.borrow();
    let reg = ce.registries.get(&reg_id)?;
    // 1. Autonomous custom element: definition whose name == local_name && local_name == local_name.
    if let Some(def) = reg.by_name.get(local_name) {
        if def.local_name == local_name {
            return Some((reg_id, def.name.clone()));
        }
    }
    // 2. Customized built-in: definition whose name == is && local_name == local_name.
    if let Some(is_name) = is_value {
        if let Some(def) = reg.by_name.get(is_name) {
            if def.local_name == local_name {
                return Some((reg_id, def.name.clone()));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Reaction queue (`[CEReactions]`)
// ---------------------------------------------------------------------------

/// Runs `f` inside a WHATWG §4.13.6 `[CEReactions]` element queue scope, invoking all
/// enqueued reactions in FIFO order before returning.
pub(crate) fn with_ce_reactions<T, F>(
    vm: &mut JsVm,
    host: &SharedHost,
    f: F,
) -> Result<T, JsException>
where
    F: FnOnce(&mut JsVm) -> Result<T, JsException>,
{
    host.borrow()
        .custom_elements_state
        .borrow_mut()
        .reactions_stack
        .push(Vec::new());
    let res = f(vm);
    let queue = host
        .borrow()
        .custom_elements_state
        .borrow_mut()
        .reactions_stack
        .pop()
        .unwrap_or_default();
    invoke_reaction_queue(vm, host, queue);
    res
}

/// Enqueues a custom element reaction onto the current `[CEReactions]` queue (or backup queue).
pub(crate) fn enqueue_reaction(host: &SharedHost, reaction: CeReaction) {
    let hb = host.borrow();
    let mut ce = hb.custom_elements_state.borrow_mut();
    if let Some(top) = ce.reactions_stack.last_mut() {
        top.push(reaction);
    } else {
        ce.backup_queue.push(reaction);
    }
}

/// Drains the backup element queue (invoked at microtask checkpoints and reaction boundaries).
pub(crate) fn drain_backup_queue(vm: &mut JsVm, host: &SharedHost) {
    loop {
        let queue = {
            let hb = host.borrow();
            let mut ce = hb.custom_elements_state.borrow_mut();
            if ce.processing_backup || ce.backup_queue.is_empty() {
                return;
            }
            ce.processing_backup = true;
            std::mem::take(&mut ce.backup_queue)
        };
        invoke_reaction_queue(vm, host, queue);
        let more = {
            let hb = host.borrow();
            let mut ce = hb.custom_elements_state.borrow_mut();
            ce.processing_backup = false;
            !ce.backup_queue.is_empty()
        };
        if !more {
            break;
        }
    }
}

/// Invokes a list of queued reactions in FIFO order. Exceptions in callbacks are reported
/// to the global error handler and do NOT abort subsequent reactions (WHATWG HTML §4.13.6).
fn invoke_reaction_queue(vm: &mut JsVm, host: &SharedHost, queue: Vec<CeReaction>) {
    for reaction in queue {
        match reaction {
            CeReaction::Upgrade(nid) => {
                upgrade_element_now(vm, host, nid);
            }
            CeReaction::Connected(nid) => {
                invoke_def_callback(vm, host, nid, |d| d.connected_cb, &[]);
            }
            CeReaction::Disconnected(nid) => {
                invoke_def_callback(vm, host, nid, |d| d.disconnected_cb, &[]);
            }
            CeReaction::ConnectedMove(nid) => {
                let has_move =
                    lookup_element_def(host, nid).is_some_and(|d| d.connected_move_cb.is_some());
                if has_move {
                    invoke_def_callback(vm, host, nid, |d| d.connected_move_cb, &[]);
                } else {
                    invoke_def_callback(vm, host, nid, |d| d.disconnected_cb, &[]);
                    invoke_def_callback(vm, host, nid, |d| d.connected_cb, &[]);
                }
            }
            CeReaction::Adopted {
                node,
                old_doc,
                new_doc,
            } => {
                let old_v = wrap_node(vm, host, old_doc);
                let new_v = wrap_node(vm, host, new_doc);
                invoke_def_callback(vm, host, node, |d| d.adopted_cb, &[old_v, new_v]);
            }
            CeReaction::AttributeChanged {
                node,
                name,
                old_value,
                new_value,
                namespace,
            } => {
                let should_fire = lookup_element_def(host, node).is_some_and(|d| {
                    d.attr_changed_cb.is_some() && d.observed_attributes.iter().any(|a| a == &name)
                });
                if should_fire {
                    let n_v = JsValue::from_string(vm.heap.alloc_string(name));
                    let o_v = old_value.map_or(JsValue::NULL, |s| {
                        JsValue::from_string(vm.heap.alloc_string(s))
                    });
                    let v_v = new_value.map_or(JsValue::NULL, |s| {
                        JsValue::from_string(vm.heap.alloc_string(s))
                    });
                    let ns_v = namespace.map_or(JsValue::NULL, |s| {
                        JsValue::from_string(vm.heap.alloc_string(s))
                    });
                    invoke_def_callback(
                        vm,
                        host,
                        node,
                        |d| d.attr_changed_cb,
                        &[n_v, o_v, v_v, ns_v],
                    );
                }
            }
            CeReaction::FormAssociated { node, form } => {
                let form_v = form.map_or(JsValue::NULL, |f| wrap_node(vm, host, f));
                invoke_def_callback(vm, host, node, |d| d.form_associated_cb, &[form_v]);
            }
            CeReaction::FormDisabled { node, disabled } => {
                invoke_def_callback(
                    vm,
                    host,
                    node,
                    |d| d.form_disabled_cb,
                    &[JsValue::from_bool(disabled)],
                );
            }
            CeReaction::FormReset(node) => {
                invoke_def_callback(vm, host, node, |d| d.form_reset_cb, &[]);
            }
        }
    }
}

fn lookup_element_def(host: &SharedHost, nid: NodeId) -> Option<CustomElementDefinition> {
    let hb = host.borrow();
    let ce = hb.custom_elements_state.borrow();
    let (reg_id, name) = ce.element_definitions.get(&nid)?;
    ce.registries.get(reg_id)?.by_name.get(name).cloned()
}

fn invoke_def_callback<F>(vm: &mut JsVm, host: &SharedHost, nid: NodeId, pick: F, args: &[JsValue])
where
    F: FnOnce(&CustomElementDefinition) -> Option<HeapHandle>,
{
    let Some(def) = lookup_element_def(host, nid) else {
        return;
    };
    let Some(cb) = pick(&def) else {
        return;
    };
    let wrapper = wrap_node(vm, host, nid);
    if let Err(exc) = vm.call_value(JsValue::from_object(cb), wrapper, args) {
        report_ce_exception(vm, exc);
    }
}

/// Reports an uncaught exception from custom element construction/callbacks via `window.onerror`
/// / `ErrorEvent` per WHATWG HTML §8.1.4.7.
pub(crate) fn report_ce_exception(vm: &mut JsVm, exc: JsException) {
    let msg = vm.format_exception(exc.value);
    let k_onerror = vm.heap.intern_key("onerror");
    if let Ok(handler) = vm.get_property_by_key(vm.global, k_onerror) {
        if handler
            .as_object()
            .is_some_and(|h| vm.is_callable_handle(h))
        {
            let m_v = JsValue::from_string(vm.heap.alloc_string(&msg));
            let s_v = JsValue::from_string(vm.heap.alloc_string(""));
            let _ = vm.call_value(
                handler,
                JsValue::from_object(vm.global),
                &[
                    m_v,
                    s_v,
                    JsValue::from_i32(0),
                    JsValue::from_i32(0),
                    exc.value,
                ],
            );
            return;
        }
    }
    // Also dispatch an `error` ErrorEvent on `window` if listeners are attached.
    let k_dispatch = vm.heap.intern_key("dispatchEvent");
    let k_ee = vm.heap.intern_key("ErrorEvent");
    if let (Ok(df), Ok(ee_ctor)) = (
        vm.get_property_by_key(vm.global, k_dispatch),
        vm.get_property_by_key(vm.global, k_ee),
    ) {
        if df.as_object().is_some_and(|h| vm.is_callable_handle(h))
            && ee_ctor
                .as_object()
                .is_some_and(|h| vm.is_constructor_handle(h))
        {
            let init = vm.alloc_ordinary_object();
            let k_msg = vm.heap.intern_key("message");
            let k_err = vm.heap.intern_key("error");
            let m_v = JsValue::from_string(vm.heap.alloc_string(&msg));
            vm.heap
                .object_define_data(init, k_msg, m_v, PropertyAttrs::DEFAULT, &mut vm.shapes);
            vm.heap.object_define_data(
                init,
                k_err,
                exc.value,
                PropertyAttrs::DEFAULT,
                &mut vm.shapes,
            );
            let ty = JsValue::from_string(vm.heap.alloc_string("error"));
            if let Ok(ev) = vm.construct_value(ee_ctor, &[ty, JsValue::from_object(init)], ee_ctor)
            {
                let _ = vm.call_value(df, JsValue::from_object(vm.global), &[ev]);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// WHATWG §4.13.5 Upgrade an element
// ---------------------------------------------------------------------------

/// Upgrades `nid` synchronously per WHATWG HTML §4.13.5 if it is `Undefined` or `Uncustomized`
/// and matches a definition in its effective `CustomElementRegistry`.
pub(crate) fn upgrade_element_now(vm: &mut JsVm, host: &SharedHost, nid: NodeId) {
    let elem_meta = {
        let hb = host.borrow();
        let doc = hb.document.borrow();
        match doc.get(nid) {
            Some(NodeData::Element(e)) => {
                if !matches!(
                    e.custom_element_state,
                    CustomElementState::Undefined | CustomElementState::Uncustomized
                ) {
                    return;
                }
                if &*e.name.ns != HTML_NS {
                    return;
                }
                let attrs: Vec<(String, String, Option<String>)> = e
                    .attributes
                    .iter()
                    .map(|(qn, v)| {
                        let ns = if qn.ns.is_empty() {
                            None
                        } else {
                            Some(qn.ns.to_string())
                        };
                        (qn.local.to_string(), v.clone(), ns)
                    })
                    .collect();
                Some((
                    e.name.local.to_string(),
                    e.is_value.clone(),
                    attrs,
                    doc.is_connected(nid),
                ))
            }
            _ => return,
        }
    };
    let Some((local, is_val, attrs, is_connected)) = elem_meta else {
        return;
    };
    let reg_opt = effective_registry_for_element(host, nid);
    let Some((reg_id, def_name)) =
        lookup_definition(host, reg_opt, HTML_NS, &local, is_val.as_deref())
    else {
        return;
    };
    let def = {
        let hb = host.borrow();
        let ce = hb.custom_elements_state.borrow();
        match ce
            .registries
            .get(&reg_id)
            .and_then(|r| r.by_name.get(&def_name))
        {
            Some(d) => d.clone(),
            None => return,
        }
    };

    // Step 2: Set custom element state to "precustomized" and record definition.
    {
        let doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = doc_rc.borrow_mut();
        if let Some(NodeData::Element(e)) = doc.get_mut(nid) {
            e.set_custom_element_state(CustomElementState::Precustomized);
        }
    }
    {
        let hb = host.borrow();
        let mut ce = hb.custom_elements_state.borrow_mut();
        ce.element_definitions
            .insert(nid, (reg_id, def_name.clone()));
        ce.active_upgrade_stack
            .push((reg_id, def_name.clone(), nid));
        if let Some(d) = ce
            .registries
            .get_mut(&reg_id)
            .and_then(|r| r.by_name.get_mut(&def_name))
        {
            d.construction_stack.push(ConstructionEntry::Element(nid));
        }
    }

    // Step 3-4: Enqueue attributeChangedCallback for existing observed attributes and connectedCallback
    // BEFORE invoking the constructor (on an isolated queue for this upgrade per §4.13.5!).
    let mut pre_reactions = Vec::new();
    for (aname, aval, ans) in attrs {
        if def.observed_attributes.iter().any(|a| a == &aname) {
            pre_reactions.push(CeReaction::AttributeChanged {
                node: nid,
                name: aname,
                old_value: None,
                new_value: Some(aval),
                namespace: ans,
            });
        }
    }
    if is_connected {
        pre_reactions.push(CeReaction::Connected(nid));
    }

    let ctor_val = JsValue::from_object(def.constructor);
    let wrapper = wrap_node(vm, host, nid);
    let construct_result = vm.construct_value(ctor_val, &[], ctor_val);

    // Pop construction stack & active upgrade stack.
    {
        let hb = host.borrow();
        let mut ce = hb.custom_elements_state.borrow_mut();
        ce.active_upgrade_stack.pop();
        if let Some(d) = ce
            .registries
            .get_mut(&reg_id)
            .and_then(|r| r.by_name.get_mut(&def_name))
        {
            d.construction_stack.pop();
        }
    }

    let failed = match construct_result {
        Ok(res_val) => {
            if res_val != wrapper {
                let err =
                    vm.type_error("Custom element upgrade constructor returned a different object");
                report_ce_exception(vm, err);
                true
            } else {
                false
            }
        }
        Err(exc) => {
            report_ce_exception(vm, exc);
            true
        }
    };

    if failed {
        {
            let doc_rc = Rc::clone(&host.borrow().document);
            let mut doc = doc_rc.borrow_mut();
            if let Some(NodeData::Element(e)) = doc.get_mut(nid) {
                e.set_custom_element_state(CustomElementState::Failed);
            }
        }
        let mut hb = host.borrow_mut();
        hb.custom_elements_state
            .borrow_mut()
            .element_definitions
            .remove(&nid);
        hb.invalidate_element_state(nid, ElementState::DEFINED);
        return;
    }

    // Success -> state = Custom (`:defined` matches!).
    {
        let doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = doc_rc.borrow_mut();
        if let Some(NodeData::Element(e)) = doc.get_mut(nid) {
            e.set_custom_element_state(CustomElementState::Custom);
        }
    }
    host.borrow_mut()
        .invalidate_element_state(nid, ElementState::DEFINED);
    // Run the upgrade's enqueued attributeChanged / connected callbacks immediately!
    invoke_reaction_queue(vm, host, pre_reactions);
}

// ---------------------------------------------------------------------------
// WHATWG §4.13.2 `HTMLElement` / `HTML*Element` constructor (`super()`)
// ---------------------------------------------------------------------------

/// Handles `super()` or direct `new MyElement()` on any HTML built-in element constructor.
pub(crate) fn handle_builtin_html_constructor(
    vm: &mut JsVm,
    host: &SharedHost,
    active_ctor_name: &str,
    active_ctor_handle: HeapHandle,
) -> Result<JsValue, JsException> {
    let Some(new_target) = vm.native_new_target.as_object() else {
        return Err(vm.type_error(format!("Illegal constructor: {active_ctor_name}")));
    };
    // Step 2: If NewTarget == active function object -> TypeError!
    if new_target == active_ctor_handle {
        return Err(vm.type_error(format!("Illegal constructor: {active_ctor_name}")));
    }

    // Step 3: Look up definition by NewTarget.
    // Check active upgrade stack first (handles scoped registries during upgrade!),
    // then active construct registry hint, then global registry!
    let found: Option<(RegistryId, String, String, Option<ConstructionEntry>)> = {
        let hb = host.borrow();
        let ce = hb.custom_elements_state.borrow();
        let mut res = None;
        if let Some((reg_id, def_name, _)) = ce.active_upgrade_stack.last() {
            if let Some(def) = ce
                .registries
                .get(reg_id)
                .and_then(|r| r.by_name.get(def_name))
            {
                if def.constructor == new_target {
                    res = Some((
                        *reg_id,
                        def.name.clone(),
                        def.local_name.clone(),
                        def.construction_stack.last().copied(),
                    ));
                }
            }
        }
        if res.is_none() {
            if let Some(&hint_reg) = ce.active_construct_registry.last() {
                if let Some(reg) = ce.registries.get(&hint_reg) {
                    if let Some(def_name) = reg.by_ctor.get(&new_target) {
                        if let Some(def) = reg.by_name.get(def_name) {
                            res = Some((
                                hint_reg,
                                def.name.clone(),
                                def.local_name.clone(),
                                def.construction_stack.last().copied(),
                            ));
                        }
                    }
                }
            }
        }
        if res.is_none() {
            if let Some(reg) = ce.registries.get(&RegistryId::GLOBAL) {
                if let Some(def_name) = reg.by_ctor.get(&new_target) {
                    if let Some(def) = reg.by_name.get(def_name) {
                        res = Some((
                            RegistryId::GLOBAL,
                            def.name.clone(),
                            def.local_name.clone(),
                            def.construction_stack.last().copied(),
                        ));
                    }
                }
            }
        }
        res
    };

    let Some((reg_id, def_name, local_name, stack_top)) = found else {
        return Err(vm.type_error("Illegal constructor"));
    };

    // Step 4-5: Verify active built-in constructor matches `def.local_name` BEFORE getting `.prototype`!
    let expected_iface = html_tag_interface_name(&local_name);
    if expected_iface != active_ctor_name {
        return Err(vm.type_error(format!(
            "Custom element '{def_name}' extends '{local_name}' ({expected_iface}), not {active_ctor_name}"
        )));
    }

    // Step 6: Get `prototype` from `NewTarget` EXACTLY ONCE!
    let proto_val = vm.get_property_by_key(new_target, vm.key_prototype)?;
    let resolved_proto = proto_val.as_object().unwrap_or_else(|| {
        let hb = host.borrow();
        hb.interface_protos
            .get(active_ctor_name)
            .copied()
            .unwrap_or(hb.prototypes.html_element_proto)
    });

    // Step 7-9: Construction stack check.
    match stack_top {
        Some(ConstructionEntry::AlreadyConstructed) => {
            Err(vm.type_error("Custom element has already been constructed"))
        }
        Some(ConstructionEntry::Element(nid)) => {
            // Replace stack top with AlreadyConstructedMarker.
            {
                let hb = host.borrow();
                let mut ce = hb.custom_elements_state.borrow_mut();
                if let Some(def) = ce
                    .registries
                    .get_mut(&reg_id)
                    .and_then(|r| r.by_name.get_mut(&def_name))
                {
                    if let Some(last) = def.construction_stack.last_mut() {
                        *last = ConstructionEntry::AlreadyConstructed;
                    }
                }
            }
            let wrapper = wrap_node(vm, host, nid);
            if let Some(wh) = wrapper.as_object() {
                vm.heap
                    .object_set_prototype(wh, Some(resolved_proto), &mut vm.shapes);
            }
            Ok(wrapper)
        }
        None => {
            // Direct `new MyCustomElement()` construction!
            let is_val = if def_name != local_name {
                Some(def_name.clone())
            } else {
                None
            };
            let nid = {
                let doc_rc = Rc::clone(&host.borrow().document);
                let mut doc = doc_rc.borrow_mut();
                let n = doc.create_element_ns(HTML_NS, None, &local_name);
                if let Some(NodeData::Element(e)) = doc.get_mut(n) {
                    e.is_value = is_val;
                    e.set_custom_element_state(CustomElementState::Custom);
                }
                n
            };
            {
                let mut hb = host.borrow_mut();
                let root = hb.document.borrow().root();
                hb.set_owner_doc(nid, root);
                let mut ce = hb.custom_elements_state.borrow_mut();
                ce.element_registries.insert(nid, Some(reg_id));
                ce.element_definitions.insert(nid, (reg_id, def_name));
            }
            let wrapper = wrap_node(vm, host, nid);
            if let Some(wh) = wrapper.as_object() {
                vm.heap
                    .object_set_prototype(wh, Some(resolved_proto), &mut vm.shapes);
            }
            Ok(wrapper)
        }
    }
}

// ---------------------------------------------------------------------------
// WHATWG DOM §4.5 / §4.9 `createElement` / `createElementNS` with Custom Elements
// ---------------------------------------------------------------------------

/// Creates an element per WHATWG DOM §4.9 "create an element" with full support for
/// scoped registries, autonomous synchronous construction (including return-value validation
/// and post-construction prefix assignment), and customized built-ins.
#[allow(clippy::too_many_arguments)]
pub(crate) fn create_element_with_ce(
    vm: &mut JsVm,
    host: &SharedHost,
    doc_id: NodeId,
    ns: &str,
    prefix: Option<&str>,
    local: &str,
    is_opt: Option<&str>,
    explicit_reg: Option<Option<RegistryId>>,
    synchronous_custom_elements: bool,
) -> Result<JsValue, JsException> {
    let reg_opt = match explicit_reg {
        Some(r) => r,
        None => registry_for_document(host, doc_id),
    };
    let def_match = lookup_definition(host, reg_opt, ns, local, is_opt);

    if let Some((reg_id, def_name)) = def_match {
        let def = {
            let hb = host.borrow();
            let ce = hb.custom_elements_state.borrow();
            ce.registries
                .get(&reg_id)
                .and_then(|r| r.by_name.get(&def_name))
                .cloned()
        };
        if let Some(def) = def {
            let is_autonomous = def.name == def.local_name;
            if is_autonomous && synchronous_custom_elements {
                // WHATWG DOM §4.9 step 5.1 (autonomous custom element synchronous construct):
                host.borrow()
                    .custom_elements_state
                    .borrow_mut()
                    .active_construct_registry
                    .push(reg_id);
                let ctor_val = JsValue::from_object(def.constructor);
                let construct_res = vm.construct_value(ctor_val, &[], ctor_val);
                host.borrow()
                    .custom_elements_state
                    .borrow_mut()
                    .active_construct_registry
                    .pop();

                let valid_nid: Result<NodeId, JsException> = match construct_res {
                    Err(exc) => Err(exc),
                    Ok(val) => {
                        let nid_opt = host.borrow().node_for_value(vm, val);
                        if let Some(nid) = nid_opt {
                            let check_err = {
                                let hb = host.borrow();
                                let doc = hb.document.borrow();
                                match doc.get(nid) {
                                    Some(NodeData::Element(e)) => {
                                        if !e.attributes.is_empty() {
                                            Some("Constructed custom element must not have attributes")
                                        } else if doc.first_child(nid).is_some() {
                                            Some(
                                                "Constructed custom element must not have children",
                                            )
                                        } else if doc.parent(nid).is_some() {
                                            Some(
                                                "Constructed custom element must not have a parent",
                                            )
                                        } else if hb.owner_doc_of(nid) != doc_id {
                                            Some("Constructed custom element has wrong ownerDocument")
                                        } else if &*e.name.ns != HTML_NS
                                            || e.name.local.as_str() != local
                                        {
                                            Some("Constructed custom element has wrong localName")
                                        } else {
                                            None
                                        }
                                    }
                                    _ => Some("Constructed result is not an Element"),
                                }
                            };
                            if let Some(msg) = check_err {
                                Err(create_dom_exception(vm, "NotSupportedError", msg))
                            } else {
                                Ok(nid)
                            }
                        } else {
                            Err(vm.type_error(
                                "Custom element constructor did not return an HTMLElement",
                            ))
                        }
                    }
                };

                match valid_nid {
                    Ok(nid) => {
                        // Step 5.1.3.9: Set namespace prefix AFTER constructor returns!
                        if let Some(pfx) = prefix {
                            let doc_rc = Rc::clone(&host.borrow().document);
                            let mut doc = doc_rc.borrow_mut();
                            if let Some(NodeData::Element(e)) = doc.get_mut(nid) {
                                e.name.prefix = Some(
                                    rix_dom::QualName::new(
                                        Some(pfx.into()),
                                        e.name.ns.clone(),
                                        e.name.local.clone(),
                                    )
                                    .prefix
                                    .unwrap(),
                                );
                            }
                        }
                        if let Some(is_v) = is_opt {
                            let doc_rc = Rc::clone(&host.borrow().document);
                            let mut doc = doc_rc.borrow_mut();
                            if let Some(NodeData::Element(e)) = doc.get_mut(nid) {
                                e.is_value = Some(is_v.to_string());
                            }
                        }
                        set_element_registry(host, nid, reg_opt);
                        return Ok(wrap_node(vm, host, nid));
                    }
                    Err(exc) => {
                        report_ce_exception(vm, exc);
                        // Fallback: HTMLUnknownElement with custom element state = "failed".
                        let nid = {
                            let doc_rc = Rc::clone(&host.borrow().document);
                            let mut doc = doc_rc.borrow_mut();
                            let n = doc.create_element_ns(ns, prefix, local);
                            if let Some(NodeData::Element(e)) = doc.get_mut(n) {
                                e.set_custom_element_state(CustomElementState::Failed);
                                if let Some(is_v) = is_opt {
                                    e.is_value = Some(is_v.to_string());
                                }
                            }
                            n
                        };
                        {
                            let mut hb = host.borrow_mut();
                            hb.set_owner_doc(nid, doc_id);
                            hb.custom_elements_state
                                .borrow_mut()
                                .element_registries
                                .insert(nid, reg_opt);
                        }
                        let wrapper = wrap_node(vm, host, nid);
                        if let Some(wh) = wrapper.as_object() {
                            if let Some(&unk_proto) =
                                host.borrow().interface_protos.get("HTMLUnknownElement")
                            {
                                vm.heap
                                    .object_set_prototype(wh, Some(unk_proto), &mut vm.shapes);
                            }
                        }
                        return Ok(wrapper);
                    }
                }
            } else if !is_autonomous {
                // Customized built-in element: create built-in, set `is_value`, then upgrade!
                let nid = {
                    let doc_rc = Rc::clone(&host.borrow().document);
                    let mut doc = doc_rc.borrow_mut();
                    let n = doc.create_element_ns(ns, prefix, local);
                    if let Some(NodeData::Element(e)) = doc.get_mut(n) {
                        e.is_value = is_opt.map(str::to_string);
                        e.set_custom_element_state(CustomElementState::Undefined);
                    }
                    n
                };
                {
                    let mut hb = host.borrow_mut();
                    hb.set_owner_doc(nid, doc_id);
                    hb.custom_elements_state
                        .borrow_mut()
                        .element_registries
                        .insert(nid, reg_opt);
                }
                if synchronous_custom_elements {
                    upgrade_element_now(vm, host, nid);
                } else {
                    enqueue_reaction(host, CeReaction::Upgrade(nid));
                }
                return Ok(wrap_node(vm, host, nid));
            }
        }
    }

    // No matching definition (or non-sync autonomous): create normal / undefined element.
    let nid = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let mut doc = doc_rc.borrow_mut();
        let n = doc.create_element_ns(ns, prefix, local);
        if let Some(NodeData::Element(e)) = doc.get_mut(n) {
            if let Some(is_v) = is_opt {
                e.is_value = Some(is_v.to_string());
                if ns == HTML_NS && is_valid_custom_element_name(is_v) {
                    e.set_custom_element_state(CustomElementState::Undefined);
                }
            } else if ns == HTML_NS && is_valid_custom_element_name(local) {
                e.set_custom_element_state(CustomElementState::Undefined);
            }
        }
        n
    };
    {
        let mut hb = host.borrow_mut();
        hb.set_owner_doc(nid, doc_id);
        hb.custom_elements_state
            .borrow_mut()
            .element_registries
            .insert(nid, reg_opt);
    }
    Ok(wrap_node(vm, host, nid))
}

// ---------------------------------------------------------------------------
// Shadow-including subtree lifecycle helpers
// ---------------------------------------------------------------------------

/// Collects all `Element` nodes in shadow-including tree order starting at `root`.
pub(crate) fn collect_shadow_including_elements(
    doc: &Document,
    root: NodeId,
    include_root: bool,
) -> Vec<NodeId> {
    let mut out = Vec::new();
    fn walk(doc: &Document, nid: NodeId, include_self: bool, out: &mut Vec<NodeId>) {
        if include_self && matches!(doc.get(nid), Some(NodeData::Element(_))) {
            out.push(nid);
        }
        if let Some(NodeData::Element(e)) = doc.get(nid) {
            if let Some(sr) = e.shadow_root {
                for sc in doc.children(sr) {
                    walk(doc, sc, true, out);
                }
            }
        }
        for c in doc.children(nid) {
            walk(doc, c, true, out);
        }
    }
    walk(doc, root, include_root, &mut out);
    out
}

/// Upgrades all matching custom elements in shadow-including tree order under `root`
/// (used after HTML parser chunks, `innerHTML`, `cloneNode`, `importNode`, `upgrade()`).
pub(crate) fn upgrade_subtree(vm: &mut JsVm, host: &SharedHost, root: NodeId, include_root: bool) {
    if !host
        .borrow()
        .custom_elements_state
        .borrow()
        .has_any_definitions()
    {
        return;
    }
    let elements = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let doc = doc_rc.borrow();
        collect_shadow_including_elements(&doc, root, include_root)
    };
    let _ = with_ce_reactions(vm, host, |_vm| {
        for nid in elements {
            enqueue_reaction(host, CeReaction::Upgrade(nid));
        }
        Ok(())
    });
}

/// Enqueues `Upgrade` + `Connected` (or `Disconnected`) for all custom elements in the
/// shadow-including subtree rooted at `root`.
pub(crate) fn notify_subtree_connected(
    vm: &mut JsVm,
    host: &SharedHost,
    root: NodeId,
    connected: bool,
) {
    if !host
        .borrow()
        .custom_elements_state
        .borrow()
        .has_any_definitions()
    {
        return;
    }
    let elements = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let doc = doc_rc.borrow();
        collect_shadow_including_elements(&doc, root, true)
    };
    let _ = with_ce_reactions(vm, host, |_vm| {
        for nid in elements {
            let state = {
                let hb = host.borrow();
                let doc = hb.document.borrow();
                match doc.get(nid) {
                    Some(NodeData::Element(e)) => Some(e.custom_element_state),
                    _ => None,
                }
            };
            match state {
                Some(CustomElementState::Custom) => {
                    if connected {
                        enqueue_reaction(host, CeReaction::Connected(nid));
                    } else {
                        enqueue_reaction(host, CeReaction::Disconnected(nid));
                    }
                }
                Some(CustomElementState::Undefined) if connected => {
                    enqueue_reaction(host, CeReaction::Upgrade(nid));
                }
                _ => {}
            }
        }
        Ok(())
    });
}

/// Enqueues `Adopted(old_doc, new_doc)` for all custom elements in the shadow-including
/// subtree rooted at `root` when `old_doc != new_doc`.
pub(crate) fn notify_subtree_adopted(
    vm: &mut JsVm,
    host: &SharedHost,
    root: NodeId,
    old_doc: NodeId,
    new_doc: NodeId,
) {
    if old_doc == new_doc
        || !host
            .borrow()
            .custom_elements_state
            .borrow()
            .has_any_definitions()
    {
        return;
    }
    let elements = {
        let doc_rc = Rc::clone(&host.borrow().document);
        let doc = doc_rc.borrow();
        collect_shadow_including_elements(&doc, root, true)
    };
    let _ = with_ce_reactions(vm, host, |_vm| {
        for nid in elements {
            let is_custom = {
                let hb = host.borrow();
                let doc = hb.document.borrow();
                matches!(
                    doc.get(nid),
                    Some(NodeData::Element(e)) if e.custom_element_state == CustomElementState::Custom
                )
            };
            if is_custom {
                enqueue_reaction(
                    host,
                    CeReaction::Adopted {
                        node: nid,
                        old_doc,
                        new_doc,
                    },
                );
            }
        }
        Ok(())
    });
}

// ---------------------------------------------------------------------------
// Installation of JS APIs (`CustomElementRegistry`)
// ---------------------------------------------------------------------------

/// Installs `CustomElementRegistry`, `window.customElements`, scoped registry accessors,
/// `ElementInternals`, `CustomStateSet`, and `ARIAMixin`.
pub fn install_custom_elements(
    vm: &mut JsVm,
    host: &SharedHost,
    _events: &SharedEvents,
    event_loop: &SharedEventLoop,
) {
    // 1. Register microtask checkpoint hook to drain any backup CE reaction queue.
    {
        let wh = Rc::downgrade(host);
        event_loop
            .borrow_mut()
            .add_checkpoint_hook(Rc::new(RefCell::new(Box::new(move |vm: &mut JsVm| {
                if let Some(h) = wh.upgrade() {
                    drain_backup_queue(vm, &h);
                }
            }))));
    }

    // 2. Register DOM mutation sink so attribute mutations emitted from `Attr.value`,
    //    `NamedNodeMap`, `DOMTokenList`, `CSSStyleDeclaration`, `DOMStringMap`, etc.
    //    enqueue `AttributeChanged` reactions automatically!
    {
        let wh = Rc::downgrade(host);
        host.borrow_mut()
            .mutation_sinks
            .push(Rc::new(RefCell::new(Box::new(
                move |mutation: &DomMutation| {
                    let Some(h) = wh.upgrade() else {
                        return;
                    };
                    if let DomMutation::Attributes {
                        target,
                        attribute_name,
                        attribute_namespace,
                        old_value,
                    } = mutation
                    {
                        let hb = h.borrow();
                        let ce = hb.custom_elements_state.borrow();
                        if !ce.element_definitions.contains_key(target) {
                            return;
                        }
                        let new_value = {
                            let doc = hb.document.borrow();
                            match doc.get(*target) {
                                Some(NodeData::Element(e)) => e
                                    .get_attribute_ns(
                                        attribute_namespace.as_deref().unwrap_or(""),
                                        attribute_name,
                                    )
                                    .map(str::to_string),
                                _ => None,
                            }
                        };
                        drop(ce);
                        drop(hb);
                        // Only enqueue onto the backup/active queue if NOT already inside
                        // `set_element_attr` / `remove_element_attr` which call `notify_attribute_changed`
                        // directly; wait: to avoid double firing, we route `set_element_attr` and
                        // `remove_element_attr` purely through this sink + `drain_backup_queue`!
                        enqueue_reaction(
                            &h,
                            CeReaction::AttributeChanged {
                                node: *target,
                                name: attribute_name.clone(),
                                old_value: old_value.clone(),
                                new_value,
                                namespace: attribute_namespace.clone(),
                            },
                        );
                    }
                },
            ))));
    }

    install_custom_element_registry(vm, host);
}

fn alloc_registry_instance(
    vm: &mut JsVm,
    host: &SharedHost,
    proto: HeapHandle,
    is_scoped: bool,
) -> (RegistryId, HeapHandle) {
    let obj = vm
        .heap
        .alloc_object(JsObject::with_kind(ObjectKind::Ordinary, Some(proto)));
    let hb = host.borrow();
    let mut ce = hb.custom_elements_state.borrow_mut();
    let reg_id = if !is_scoped && !ce.registries.contains_key(&RegistryId::GLOBAL) {
        RegistryId::GLOBAL
    } else {
        ce.next_registry_id = ce.next_registry_id.max(1);
        let id = RegistryId(ce.next_registry_id);
        ce.next_registry_id += 1;
        id
    };
    ce.registries.insert(
        reg_id,
        CustomElementRegistryData {
            id: reg_id,
            is_scoped,
            js_handle: obj,
            by_name: HashMap::new(),
            by_ctor: HashMap::new(),
            when_defined_waiters: HashMap::new(),
            definition_running: false,
        },
    );
    ce.handle_to_registry.insert(obj, reg_id);
    if reg_id == RegistryId::GLOBAL {
        ce.global_registry_handle = Some(obj);
    }
    (reg_id, obj)
}

fn require_registry_id(
    vm: &mut JsVm,
    host: &SharedHost,
    this_val: JsValue,
) -> Result<RegistryId, JsException> {
    let Some(h) = this_val.as_object() else {
        return Err(vm.type_error("Illegal invocation: expected CustomElementRegistry"));
    };
    host.borrow()
        .custom_elements_state
        .borrow()
        .handle_to_registry
        .get(&h)
        .copied()
        .ok_or_else(|| vm.type_error("Illegal invocation: expected CustomElementRegistry"))
}

fn get_optional_callable_prop(
    vm: &mut JsVm,
    obj: HeapHandle,
    prop: &str,
) -> Result<Option<HeapHandle>, JsException> {
    let k = vm.heap.intern_key(prop);
    let val = vm.get_property_by_key(obj, k)?;
    if val.is_undefined() {
        return Ok(None);
    }
    if let Some(h) = val.as_object() {
        if vm.is_callable_handle(h) {
            return Ok(Some(h));
        }
    }
    Err(vm.type_error(format!("'{prop}' is not a function")))
}

fn install_custom_element_registry(vm: &mut JsVm, host: &SharedHost) {
    let proto = make_proto(vm, vm.object_proto);
    let h_ctor = Rc::clone(host);
    let ctor = vm.alloc_host_fn("CustomElementRegistry", move |vm, _this, _args| {
        if vm.native_new_target.is_undefined() {
            return Err(vm.type_error("CustomElementRegistry constructor requires 'new'"));
        }
        let target_proto = vm
            .native_new_target
            .as_object()
            .and_then(|nt| vm.get_property_by_key(nt, vm.key_prototype).ok())
            .and_then(JsValue::as_object)
            .unwrap_or(proto);
        let (_, obj) = alloc_registry_instance(vm, &h_ctor, target_proto, true);
        Ok(JsValue::from_object(obj))
    });
    vm.mark_constructor(ctor);
    vm.heap.object_define_data(
        ctor,
        vm.key_prototype,
        JsValue::from_object(proto),
        PropertyAttrs::empty(),
        &mut vm.shapes,
    );
    vm.heap.object_define_data(
        proto,
        vm.key_constructor,
        JsValue::from_object(ctor),
        PropertyAttrs::METHOD,
        &mut vm.shapes,
    );
    let tag_val = JsValue::from_string(vm.heap.alloc_string("CustomElementRegistry"));
    vm.heap.object_define_data(
        proto,
        vm.key_symbol_to_string_tag,
        tag_val,
        PropertyAttrs::CONFIGURABLE,
        &mut vm.shapes,
    );
    define_global(vm, "CustomElementRegistry", JsValue::from_object(ctor));

    host.borrow()
        .custom_elements_state
        .borrow_mut()
        .registry_proto = Some(proto);

    // `CustomElementRegistry.prototype.define(name, constructor, options)`
    let h = Rc::clone(host);
    define_host_method(vm, proto, "define", move |vm, t, args| {
        let reg_id = require_registry_id(vm, &h, t)?;
        if args.len() < 2 {
            return Err(vm.type_error("CustomElementRegistry.define requires 2 arguments"));
        }
        let name = crate::dom_parsing::to_dom_string(vm, arg(args, 0))?;
        // Step 1: IsConstructor(constructor) BEFORE validating `name`!
        let Some(ctor_h) = arg(args, 1)
            .as_object()
            .filter(|&ch| vm.is_constructor_handle(ch))
        else {
            return Err(vm.type_error("customElements.define requires a constructor function"));
        };
        // Step 2: Validate custom element name -> SyntaxError.
        if !is_valid_custom_element_name(&name) {
            return Err(create_dom_exception(
                vm,
                "SyntaxError",
                &format!("'{name}' is not a valid custom element name"),
            ));
        }
        // Step 3-4: Check duplicate name & duplicate constructor BEFORE checking definition_running!
        let is_scoped = {
            let hb = h.borrow();
            let ce = hb.custom_elements_state.borrow();
            let Some(reg) = ce.registries.get(&reg_id) else {
                return Err(vm.type_error("Invalid CustomElementRegistry"));
            };
            if reg.by_name.contains_key(&name) {
                return Err(create_dom_exception(
                    vm,
                    "NotSupportedError",
                    &format!("Custom element '{name}' is already defined"),
                ));
            }
            if reg.by_ctor.contains_key(&ctor_h) {
                return Err(create_dom_exception(
                    vm,
                    "NotSupportedError",
                    "This constructor has already been used with this registry",
                ));
            }
            reg.is_scoped
        };

        // Step 5-8: Parse optional `{ extends }` dictionary.
        let opts_val = arg(args, 2);
        let extends_opt: Option<String> = if opts_val.is_undefined() || opts_val.is_null() {
            None
        } else if let Some(oh) = opts_val.as_object() {
            let k_ext = vm.heap.intern_key("extends");
            let ev = vm.get_property_by_key(oh, k_ext)?;
            if ev.is_undefined() {
                None
            } else {
                Some(crate::dom_parsing::to_dom_string(vm, ev)?)
            }
        } else {
            return Err(vm.type_error("Options argument to define() must be an object"));
        };

        let local_name = if let Some(ref ext) = extends_opt {
            if is_scoped {
                return Err(create_dom_exception(
                    vm,
                    "NotSupportedError",
                    "Scoped CustomElementRegistry does not support customized built-in elements",
                ));
            }
            if is_valid_custom_element_name(ext)
                || html_tag_interface_name(ext) == "HTMLUnknownElement"
            {
                return Err(create_dom_exception(
                    vm,
                    "NotSupportedError",
                    &format!("'{ext}' is not a valid HTML element to extend"),
                ));
            }
            ext.clone()
        } else {
            name.clone()
        };

        // Step 10-11: Check & set `element_definition_is_running` flag.
        {
            let hb = h.borrow();
            let mut ce = hb.custom_elements_state.borrow_mut();
            let Some(reg) = ce.registries.get_mut(&reg_id) else {
                return Err(vm.type_error("Invalid CustomElementRegistry"));
            };
            if reg.definition_running {
                return Err(create_dom_exception(
                    vm,
                    "NotSupportedError",
                    "Element definition is already running on this registry",
                ));
            }
            reg.definition_running = true;
        }

        // Steps 12-16: Read prototype, lifecycle callbacks, observedAttributes, disabledFeatures, formAssociated.
        let extract_result: Result<CustomElementDefinition, JsException> = (|| {
            let proto_val = vm.get_property_by_key(ctor_h, vm.key_prototype)?;
            let Some(proto_h) = proto_val.as_object() else {
                return Err(vm.type_error("Custom element constructor prototype must be an object"));
            };
            let connected_cb = get_optional_callable_prop(vm, proto_h, "connectedCallback")?;
            let disconnected_cb = get_optional_callable_prop(vm, proto_h, "disconnectedCallback")?;
            let connected_move_cb =
                get_optional_callable_prop(vm, proto_h, "connectedMoveCallback")?;
            let adopted_cb = get_optional_callable_prop(vm, proto_h, "adoptedCallback")?;
            let attr_changed_cb =
                get_optional_callable_prop(vm, proto_h, "attributeChangedCallback")?;

            let mut observed_attributes = Vec::new();
            if attr_changed_cb.is_some() {
                let k_obs = vm.heap.intern_key("observedAttributes");
                let obs_val = vm.get_property_by_key(ctor_h, k_obs)?;
                if !obs_val.is_undefined() {
                    let items = vm.collect_iterable(obs_val)?;
                    for item in items {
                        observed_attributes.push(crate::dom_parsing::to_dom_string(vm, item)?);
                    }
                }
            }

            let mut disable_shadow = false;
            let mut disable_internals = false;
            let k_df = vm.heap.intern_key("disabledFeatures");
            let df_val = vm.get_property_by_key(ctor_h, k_df)?;
            if !df_val.is_undefined() {
                let items = vm.collect_iterable(df_val)?;
                for item in items {
                    let s = crate::dom_parsing::to_dom_string(vm, item)?;
                    if s == "shadow" {
                        disable_shadow = true;
                    } else if s == "internals" {
                        disable_internals = true;
                    }
                }
            }

            let k_fa = vm.heap.intern_key("formAssociated");
            let fa_val = vm.get_property_by_key(ctor_h, k_fa)?;
            let form_associated = vm.to_boolean(fa_val);
            let (form_associated_cb, form_disabled_cb, form_reset_cb, form_state_restore_cb) =
                if form_associated {
                    (
                        get_optional_callable_prop(vm, proto_h, "formAssociatedCallback")?,
                        get_optional_callable_prop(vm, proto_h, "formDisabledCallback")?,
                        get_optional_callable_prop(vm, proto_h, "formResetCallback")?,
                        get_optional_callable_prop(vm, proto_h, "formStateRestoreCallback")?,
                    )
                } else {
                    (None, None, None, None)
                };

            Ok(CustomElementDefinition {
                name: name.clone(),
                local_name: local_name.clone(),
                constructor: ctor_h,
                prototype: proto_h,
                connected_cb,
                disconnected_cb,
                connected_move_cb,
                adopted_cb,
                attr_changed_cb,
                form_associated_cb,
                form_disabled_cb,
                form_reset_cb,
                form_state_restore_cb,
                observed_attributes,
                disable_shadow,
                disable_internals,
                form_associated,
                construction_stack: Vec::new(),
            })
        })();

        // Always reset `definition_running`.
        {
            let hb = h.borrow();
            let mut ce = hb.custom_elements_state.borrow_mut();
            if let Some(reg) = ce.registries.get_mut(&reg_id) {
                reg.definition_running = false;
            }
        }
        let def = extract_result?;

        let waiters = {
            let hb = h.borrow();
            let mut ce = hb.custom_elements_state.borrow_mut();
            if let Some(reg) = ce.registries.get_mut(&reg_id) {
                reg.by_ctor.insert(ctor_h, name.clone());
                reg.by_name.insert(name.clone(), def);
                reg.when_defined_waiters.remove(&name).unwrap_or_default()
            } else {
                Vec::new()
            }
        };

        // Step 19-20: Upgrade shadow-including descendants of the main document matching this registry.
        let main_root = h.borrow().document.borrow().root();
        let all_elements = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            collect_shadow_including_elements(&doc, main_root, false)
        };
        let mut candidates = Vec::new();
        for nid in all_elements {
            let matches_def = {
                let hb = h.borrow();
                let doc = hb.document.borrow();
                match doc.get(nid) {
                    Some(NodeData::Element(e))
                        if &*e.name.ns == HTML_NS
                            && e.name.local.as_str() == local_name
                            && matches!(
                                e.custom_element_state,
                                CustomElementState::Undefined | CustomElementState::Uncustomized
                            ) =>
                    {
                        if extends_opt.is_some() {
                            e.is_value.as_deref() == Some(name.as_str())
                        } else {
                            true
                        }
                    }
                    _ => false,
                }
            };
            if matches_def && effective_registry_for_element(&h, nid) == Some(reg_id) {
                candidates.push(nid);
            }
        }
        let _ = with_ce_reactions(vm, &h, |_vm| {
            for nid in candidates {
                enqueue_reaction(&h, CeReaction::Upgrade(nid));
            }
            Ok(())
        });

        // Step 21: Resolve `whenDefined(name)` promises with `constructor`.
        for p in waiters {
            resolve_promise(vm, p, JsValue::from_object(ctor_h));
        }

        Ok(JsValue::UNDEFINED)
    });

    // `CustomElementRegistry.prototype.get(name)`
    let h = Rc::clone(host);
    define_host_method(vm, proto, "get", move |vm, t, args| {
        let reg_id = require_registry_id(vm, &h, t)?;
        let name = crate::dom_parsing::to_dom_string(vm, arg(args, 0))?;
        let hb = h.borrow();
        let ce = hb.custom_elements_state.borrow();
        Ok(ce
            .registries
            .get(&reg_id)
            .and_then(|r| r.by_name.get(&name))
            .map_or(JsValue::UNDEFINED, |d| JsValue::from_object(d.constructor)))
    });

    // `CustomElementRegistry.prototype.getName(constructor)`
    let h = Rc::clone(host);
    define_host_method(vm, proto, "getName", move |vm, t, args| {
        let reg_id = require_registry_id(vm, &h, t)?;
        let Some(ch) = arg(args, 0).as_object() else {
            return Ok(JsValue::NULL);
        };
        let found = h
            .borrow()
            .custom_elements_state
            .borrow()
            .registries
            .get(&reg_id)
            .and_then(|r| r.by_ctor.get(&ch).cloned());
        Ok(found.map_or(JsValue::NULL, |s| {
            JsValue::from_string(vm.heap.alloc_string(s))
        }))
    });

    // `CustomElementRegistry.prototype.whenDefined(name)`
    let h = Rc::clone(host);
    define_host_method(vm, proto, "whenDefined", move |vm, t, args| {
        let reg_id = require_registry_id(vm, &h, t)?;
        let p = alloc_promise(vm);
        let name = match crate::dom_parsing::to_dom_string(vm, arg(args, 0)) {
            Ok(s) => s,
            Err(exc) => {
                reject_promise(vm, p, exc.value);
                return Ok(JsValue::from_object(p));
            }
        };
        if !is_valid_custom_element_name(&name) {
            let err = create_dom_exception(
                vm,
                "SyntaxError",
                &format!("'{name}' is not a valid custom element name"),
            );
            reject_promise(vm, p, err.value);
            return Ok(JsValue::from_object(p));
        }
        let existing = h
            .borrow()
            .custom_elements_state
            .borrow()
            .registries
            .get(&reg_id)
            .and_then(|r| r.by_name.get(&name))
            .map(|d| d.constructor);
        if let Some(c) = existing {
            resolve_promise(vm, p, JsValue::from_object(c));
        } else if let Some(reg) = h
            .borrow()
            .custom_elements_state
            .borrow_mut()
            .registries
            .get_mut(&reg_id)
        {
            reg.when_defined_waiters.entry(name).or_default().push(p);
        }
        Ok(JsValue::from_object(p))
    });

    // `CustomElementRegistry.prototype.upgrade(root)`
    let h = Rc::clone(host);
    define_host_method(vm, proto, "upgrade", move |vm, t, args| {
        let _reg_id = require_registry_id(vm, &h, t)?;
        let root_id = require_node(vm, &h, arg(args, 0))?;
        upgrade_subtree(vm, &h, root_id, true);
        Ok(JsValue::UNDEFINED)
    });

    // `CustomElementRegistry.prototype.initialize(root)` (Scoped Custom Element Registries)
    let h = Rc::clone(host);
    define_host_method(vm, proto, "initialize", move |vm, t, args| {
        let reg_id = require_registry_id(vm, &h, t)?;
        let root_id = require_node(vm, &h, arg(args, 0))?;
        let is_scoped = h
            .borrow()
            .custom_elements_state
            .borrow()
            .registries
            .get(&reg_id)
            .is_some_and(|r| r.is_scoped);

        let root_is_doc_or_sr = {
            let hb = h.borrow();
            let doc = hb.document.borrow();
            match doc.get(root_id) {
                Some(NodeData::Document) => {
                    if is_scoped {
                        return Err(create_dom_exception(
                            vm,
                            "NotSupportedError",
                            "Scoped CustomElementRegistry cannot initialize a Document",
                        ));
                    }
                    1u8
                }
                Some(NodeData::ShadowRoot { .. }) => 2u8,
                Some(NodeData::Element(_) | NodeData::DocumentFragment) => 3u8,
                _ => 0u8,
            }
        };
        if root_is_doc_or_sr == 0 {
            return Ok(JsValue::UNDEFINED);
        }

        // Associate `root_id` (if ShadowRoot/Document with null registry) and all null-registry
        // shadow-including descendants in tree order.
        if root_is_doc_or_sr == 2 && registry_for_shadow_root(&h, root_id).is_none() {
            set_shadow_root_registry(&h, root_id, Some(reg_id));
        } else if root_is_doc_or_sr == 1 && registry_for_document(&h, root_id).is_none() {
            set_document_registry(&h, root_id, Some(reg_id));
        }

        let elements = {
            let doc_rc = Rc::clone(&h.borrow().document);
            let doc = doc_rc.borrow();
            collect_shadow_including_elements(&doc, root_id, true)
        };
        let mut to_upgrade = Vec::new();
        for nid in elements {
            let cur_reg = effective_registry_for_element(&h, nid);
            if cur_reg.is_none() {
                set_element_registry(&h, nid, Some(reg_id));
                let is_connected = h.borrow().document.borrow().is_connected(nid);
                if is_connected {
                    to_upgrade.push(nid);
                }
            }
        }
        let _ = with_ce_reactions(vm, &h, |_vm| {
            for nid in to_upgrade {
                enqueue_reaction(&h, CeReaction::Upgrade(nid));
            }
            Ok(())
        });
        Ok(JsValue::UNDEFINED)
    });

    // Allocate global `window.customElements` (`RegistryId::GLOBAL`).
    let (_, global_reg) = alloc_registry_instance(vm, host, proto, false);
    define_global(vm, "customElements", JsValue::from_object(global_reg));

    // Install `.customElementRegistry` getter on `Document`, `ShadowRoot`, and `Element`.
    let dp = host.borrow().prototypes.document_proto;
    let srp = host.borrow().prototypes.shadow_root_proto;
    let ep = host.borrow().prototypes.element_proto;

    let h = Rc::clone(host);
    let g_doc = vm.alloc_host_fn("get customElementRegistry", move |vm, t, _| {
        let doc_id = require_node(vm, &h, t)?;
        let reg_opt = registry_for_document(&h, doc_id);
        Ok(reg_id_to_js(&h, reg_opt))
    });
    define_host_accessor(vm, dp, "customElementRegistry", Some(g_doc), None);

    let h = Rc::clone(host);
    let g_sr = vm.alloc_host_fn("get customElementRegistry", move |vm, t, _| {
        let sr_id = require_node(vm, &h, t)?;
        let reg_opt = registry_for_shadow_root(&h, sr_id);
        Ok(reg_id_to_js(&h, reg_opt))
    });
    define_host_accessor(vm, srp, "customElementRegistry", Some(g_sr), None);

    let h = Rc::clone(host);
    let g_el = vm.alloc_host_fn("get customElementRegistry", move |vm, t, _| {
        let nid = require_node(vm, &h, t)?;
        let reg_opt = effective_registry_for_element(&h, nid);
        Ok(reg_id_to_js(&h, reg_opt))
    });
    define_host_accessor(vm, ep, "customElementRegistry", Some(g_el), None);
}

fn reg_id_to_js(host: &SharedHost, reg_opt: Option<RegistryId>) -> JsValue {
    let Some(rid) = reg_opt else {
        return JsValue::NULL;
    };
    host.borrow()
        .custom_elements_state
        .borrow()
        .registries
        .get(&rid)
        .map_or(JsValue::NULL, |r| JsValue::from_object(r.js_handle))
}

//! Clean-room WHATWG HTML5 Living Standard §13.2.6 tree builder for `rix-html`.
//!
//! Consumes tokens directly from `html5ever::tokenizer::Tokenizer` via [`TokenSink`]
//! and emits DOM mutations to any [`TreeSink`], implementing modern WHATWG tree
//! construction rules (customizable `<select>`, `<search>` special tag, `<template>`
//! `frameset-ok` and form-pointer rules, empty DOCTYPE system-ID quirks, and SVG `<script>`).

use std::borrow::Cow::Borrowed;
use std::cell::{Cell, Ref, RefCell};
use std::collections::VecDeque;
use std::iter::{Enumerate, Rev};
use std::slice;

use html5ever::interface::tree_builder::{
    create_element, create_element_with_flags, ElemName, NodeOrText, QuirksMode, TreeSink,
};
use html5ever::interface::NoQuirks;
use html5ever::interface::NodeOrText::{AppendNode, AppendText};
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::{self as tok_state, RawKind};
use html5ever::tokenizer::{self, Doctype, EndTag, StartTag, Tag, TokenSink, TokenSinkResult};
use markup5ever::{
    expanded_name, local_name, ns, Attribute, ExpandedName, LocalName, Namespace, QualName,
};

pub mod data;
pub mod foreign;
#[macro_use]
pub mod tag_sets;
mod rules;
pub mod types;

use self::tag_sets::{
    button_scope, cursory_implied_end, default_scope, mathml_text_integration_point, special_tag,
    svg_html_integration_point, td_th,
};
use self::types::{
    FormatEntry, InsertionMode, InsertionPoint, ProcessResult, PushFlag, SplitStatus, Token,
};

/// Configuration options for [`TreeBuilder`].
#[derive(Copy, Clone, Debug)]
pub struct TreeBuilderOpts {
    pub scripting_enabled: bool,
    pub iframe_srcdoc: bool,
    pub drop_doctype: bool,
    pub quirks_mode: QuirksMode,
}

impl Default for TreeBuilderOpts {
    fn default() -> Self {
        Self {
            scripting_enabled: true,
            iframe_srcdoc: false,
            drop_doctype: false,
            quirks_mode: NoQuirks,
        }
    }
}

/// WHATWG HTML5 tree builder implementing [`TokenSink`].
pub struct TreeBuilder<Handle, Sink> {
    opts: TreeBuilderOpts,
    pub sink: Sink,
    mode: Cell<InsertionMode>,
    orig_mode: Cell<Option<InsertionMode>>,
    template_modes: RefCell<Vec<InsertionMode>>,
    pending_table_text: RefCell<Vec<(SplitStatus, StrTendril)>>,
    quirks_mode: Cell<QuirksMode>,
    doc_handle: Handle,
    open_elems: RefCell<Vec<Handle>>,
    active_formatting: RefCell<Vec<FormatEntry<Handle>>>,
    head_elem: RefCell<Option<Handle>>,
    form_elem: RefCell<Option<Handle>>,
    frameset_ok: Cell<bool>,
    ignore_lf: Cell<bool>,
    foster_parenting: Cell<bool>,
    context_elem: RefCell<Option<Handle>>,
    current_line: Cell<u64>,
}

impl<Handle, Sink> TreeBuilder<Handle, Sink>
where
    Handle: Clone,
    Sink: TreeSink<Handle = Handle>,
{
    pub fn new(sink: Sink, opts: TreeBuilderOpts) -> Self {
        let doc_handle = sink.get_document();
        Self {
            opts,
            sink,
            mode: Cell::new(InsertionMode::Initial),
            orig_mode: Cell::new(None),
            template_modes: RefCell::new(Vec::new()),
            pending_table_text: RefCell::new(Vec::new()),
            quirks_mode: Cell::new(opts.quirks_mode),
            doc_handle,
            open_elems: RefCell::new(Vec::new()),
            active_formatting: RefCell::new(Vec::new()),
            head_elem: RefCell::new(None),
            form_elem: RefCell::new(None),
            frameset_ok: Cell::new(true),
            ignore_lf: Cell::new(false),
            foster_parenting: Cell::new(false),
            context_elem: RefCell::new(None),
            current_line: Cell::new(1),
        }
    }

    pub fn new_for_fragment(
        sink: Sink,
        context_elem: Handle,
        form_elem: Option<Handle>,
        opts: TreeBuilderOpts,
    ) -> Self {
        let doc_handle = sink.get_document();
        let context_is_template =
            sink.elem_name(&context_elem).expanded() == expanded_name!(html "template");
        let template_modes = if context_is_template {
            RefCell::new(vec![InsertionMode::InTemplate])
        } else {
            RefCell::new(Vec::new())
        };

        let tb = Self {
            opts,
            sink,
            mode: Cell::new(InsertionMode::Initial),
            orig_mode: Cell::new(None),
            template_modes,
            pending_table_text: RefCell::new(Vec::new()),
            quirks_mode: Cell::new(opts.quirks_mode),
            doc_handle,
            open_elems: RefCell::new(Vec::new()),
            active_formatting: RefCell::new(Vec::new()),
            head_elem: RefCell::new(None),
            form_elem: RefCell::new(form_elem),
            frameset_ok: Cell::new(true),
            ignore_lf: Cell::new(false),
            foster_parenting: Cell::new(false),
            context_elem: RefCell::new(Some(context_elem)),
            current_line: Cell::new(1),
        };

        tb.create_root(Vec::new());
        let initial_mode = tb.reset_insertion_mode();
        tb.mode.set(initial_mode);
        tb
    }

    pub fn tokenizer_state_for_context_elem(
        &self,
        context_element_allows_scripting: bool,
    ) -> tok_state::State {
        let context_elem = self.context_elem.borrow();
        let elem = context_elem.as_ref().expect("no context element");
        let elem_name = self.sink.elem_name(elem);
        let name = match elem_name.expanded() {
            ExpandedName {
                ns: &ns!(html),
                local,
            } => local,
            _ => return tok_state::Data,
        };
        match *name {
            local_name!("title") | local_name!("textarea") => tok_state::RawData(tok_state::Rcdata),
            local_name!("style")
            | local_name!("xmp")
            | local_name!("iframe")
            | local_name!("noembed")
            | local_name!("noframes") => tok_state::RawData(tok_state::Rawtext),
            local_name!("script") => tok_state::RawData(tok_state::ScriptData),
            local_name!("noscript") => {
                if context_element_allows_scripting {
                    tok_state::RawData(tok_state::Rawtext)
                } else {
                    tok_state::Data
                }
            }
            local_name!("plaintext") => tok_state::Plaintext,
            _ => tok_state::Data,
        }
    }

    pub fn is_fragment(&self) -> bool {
        self.context_elem.borrow().is_some()
    }

    /// Returns true if a `<template>` element is on the stack of open elements OR if
    /// parsing a fragment whose context element is `<template>` (active template mode stack).
    pub(crate) fn in_template(&self) -> bool {
        self.in_html_elem_named(local_name!("template")) || !self.template_modes.borrow().is_empty()
    }

    fn process_to_completion(&self, mut token: Token) -> TokenSinkResult<Handle> {
        let mut more_tokens = VecDeque::new();
        loop {
            let should_ack = matches!(
                token,
                Token::Tag(Tag {
                    self_closing: true,
                    kind: StartTag,
                    ..
                })
            );
            let result = if self.is_foreign(&token) {
                self.step_foreign(token)
            } else {
                let mode = self.mode.get();
                self.step(mode, token)
            };
            match result {
                ProcessResult::Done => {
                    if should_ack {
                        self.sink
                            .parse_error(Borrowed("Unacknowledged self-closing tag"));
                    }
                    let Some(next) = more_tokens.pop_front() else {
                        return TokenSinkResult::Continue;
                    };
                    token = next;
                }
                ProcessResult::DoneAckSelfClosing => {
                    let Some(next) = more_tokens.pop_front() else {
                        return TokenSinkResult::Continue;
                    };
                    token = next;
                }
                ProcessResult::Reprocess(m, t) => {
                    self.mode.set(m);
                    token = t;
                }
                ProcessResult::SplitWhitespace(mut buf) => {
                    let Some((first, is_ws)) = buf.pop_front_char_run(|c| c.is_ascii_whitespace())
                    else {
                        return TokenSinkResult::Continue;
                    };
                    let status = if is_ws {
                        SplitStatus::Whitespace
                    } else {
                        SplitStatus::NotWhitespace
                    };
                    token = Token::Characters(status, first);
                    if buf.len32() > 0 {
                        more_tokens.push_back(Token::Characters(SplitStatus::NotSplit, buf));
                    }
                }
                ProcessResult::Script(node) => {
                    return TokenSinkResult::Script(node);
                }
                ProcessResult::ToPlaintext => return TokenSinkResult::Plaintext,
                ProcessResult::ToRawData(k) => return TokenSinkResult::RawData(k),
                ProcessResult::EncodingIndicator(enc) => {
                    return TokenSinkResult::EncodingIndicator(enc);
                }
            }
        }
    }

    fn appropriate_place_for_insertion(
        &self,
        override_target: Option<Handle>,
    ) -> InsertionPoint<Handle> {
        declare_tag_set!(foster_target = "table" "tbody" "tfoot" "thead" "tr");
        let target = override_target.unwrap_or_else(|| self.current_node().clone());
        if !(self.foster_parenting.get() && self.elem_in(&target, foster_target)) {
            if self.html_elem_named(&target, local_name!("template")) {
                let contents = self.sink.get_template_contents(&target);
                return InsertionPoint::LastChild(contents);
            }
            return InsertionPoint::LastChild(target);
        }

        let open_elems = self.open_elems.borrow();
        let mut iter = open_elems.iter().rev().peekable();
        while let Some(elem) = iter.next() {
            if self.html_elem_named(elem, local_name!("template")) {
                let contents = self.sink.get_template_contents(elem);
                return InsertionPoint::LastChild(contents);
            }
            if self.html_elem_named(elem, local_name!("table")) {
                if let Some(prev) = iter.peek() {
                    return InsertionPoint::TableFosterParenting {
                        element: elem.clone(),
                        prev_element: (*prev).clone(),
                    };
                }
            }
        }
        InsertionPoint::LastChild(open_elems[0].clone())
    }

    fn insert_at(&self, insertion_point: InsertionPoint<Handle>, child: NodeOrText<Handle>) {
        match insertion_point {
            InsertionPoint::LastChild(parent) => self.sink.append(&parent, child),
            InsertionPoint::TableFosterParenting {
                element,
                prev_element,
            } => self
                .sink
                .append_based_on_parent_node(&element, &prev_element, child),
        }
    }
}

impl<Handle, Sink> TokenSink for TreeBuilder<Handle, Sink>
where
    Handle: Clone,
    Sink: TreeSink<Handle = Handle>,
{
    type Handle = Handle;

    fn process_token(&self, token: tokenizer::Token, line_number: u64) -> TokenSinkResult<Handle> {
        if line_number != self.current_line.get() {
            self.current_line.set(line_number);
            self.sink.set_current_line(line_number);
        }
        let ignore_lf = self.ignore_lf.take();

        let token = match token {
            tokenizer::ParseError(e) => {
                self.sink.parse_error(e);
                return TokenSinkResult::Continue;
            }
            tokenizer::DoctypeToken(dt) => {
                if self.mode.get() == InsertionMode::Initial {
                    let (err, quirk) = data::doctype_error_and_quirks(&dt, self.opts.iframe_srcdoc);
                    if err {
                        self.sink.parse_error(Borrowed("Bad DOCTYPE"));
                    }
                    let Doctype {
                        name,
                        public_id,
                        system_id,
                        ..
                    } = dt;
                    if !self.opts.drop_doctype {
                        self.sink.append_doctype_to_document(
                            name.unwrap_or_default(),
                            public_id.unwrap_or_default(),
                            system_id.unwrap_or_default(),
                        );
                    }
                    self.set_quirks_mode(quirk);
                    self.mode.set(InsertionMode::BeforeHtml);
                } else {
                    self.sink.parse_error(Borrowed("DOCTYPE in body"));
                }
                return TokenSinkResult::Continue;
            }
            tokenizer::TagToken(x) => Token::Tag(x),
            tokenizer::CommentToken(x) => Token::Comment(x),
            tokenizer::NullCharacterToken => Token::NullCharacter,
            tokenizer::EOFToken => Token::Eof,
            tokenizer::CharacterTokens(mut x) => {
                if ignore_lf && x.starts_with('\n') {
                    x.pop_front(1);
                }
                if x.is_empty() {
                    return TokenSinkResult::Continue;
                }
                Token::Characters(SplitStatus::NotSplit, x)
            }
        };

        self.process_to_completion(token)
    }

    fn end(&self) {
        let popped: Vec<Handle> = self.open_elems.borrow_mut().drain(..).rev().collect();
        for elem in popped {
            self.sink.pop(&elem);
        }
    }

    fn adjusted_current_node_present_but_not_in_html_namespace(&self) -> bool {
        !self.open_elems.borrow().is_empty()
            && *self.sink.elem_name(&self.adjusted_current_node()).ns() != ns!(html)
    }
}

pub(crate) fn html_elem<Handle>(open_elems: &[Handle]) -> &Handle {
    &open_elems[0]
}

pub(crate) fn tag_attr(tag: &Tag, name: &LocalName) -> Option<StrTendril> {
    tag.attrs
        .iter()
        .find(|a| a.name.ns == *ns!() && a.name.local == *name)
        .map(|a| a.value.clone())
}

struct ActiveFormattingView<'a, Handle: 'a> {
    data: Ref<'a, Vec<FormatEntry<Handle>>>,
}

impl<'a, Handle: 'a> ActiveFormattingView<'a, Handle> {
    fn iter(&'a self) -> impl Iterator<Item = (usize, &'a Handle, &'a Tag)> + 'a {
        ActiveFormattingIter {
            iter: self.data.iter().enumerate().rev(),
        }
    }
}

struct ActiveFormattingIter<'a, Handle: 'a> {
    iter: Rev<Enumerate<slice::Iter<'a, FormatEntry<Handle>>>>,
}

impl<'a, Handle> Iterator for ActiveFormattingIter<'a, Handle> {
    type Item = (usize, &'a Handle, &'a Tag);
    fn next(&mut self) -> Option<Self::Item> {
        match self.iter.next() {
            None | Some((_, &FormatEntry::Marker)) => None,
            Some((i, FormatEntry::Element(h, t))) => Some((i, h, t)),
        }
    }
}

enum Bookmark<Handle> {
    Replace(Handle),
    InsertAfter(Handle),
}

impl<Handle, Sink> TreeBuilder<Handle, Sink>
where
    Handle: Clone,
    Sink: TreeSink<Handle = Handle>,
{
    pub(crate) fn unexpected<T>(&self, _thing: &T) -> ProcessResult<Handle> {
        self.sink.parse_error(Borrowed("Unexpected token"));
        ProcessResult::Done
    }

    pub(crate) fn assert_named(&self, node: &Handle, name: LocalName) {
        debug_assert!(self.html_elem_named(node, name));
    }

    fn active_formatting_end_to_marker(&self) -> ActiveFormattingView<'_, Handle> {
        ActiveFormattingView {
            data: self.active_formatting.borrow(),
        }
    }

    fn position_in_active_formatting(&self, element: &Handle) -> Option<usize> {
        self.active_formatting
            .borrow()
            .iter()
            .position(|n| match n {
                FormatEntry::Marker => false,
                FormatEntry::Element(handle, _) => self.sink.same_node(handle, element),
            })
    }

    pub(crate) fn set_quirks_mode(&self, mode: QuirksMode) {
        self.quirks_mode.set(mode);
        self.sink.set_quirks_mode(mode);
    }

    pub(crate) fn stop_parsing(&self) -> ProcessResult<Handle> {
        ProcessResult::Done
    }

    pub(crate) fn to_raw_text_mode(&self, k: RawKind) -> ProcessResult<Handle> {
        self.orig_mode.set(Some(self.mode.get()));
        self.mode.set(InsertionMode::Text);
        ProcessResult::ToRawData(k)
    }

    pub(crate) fn parse_raw_data(&self, tag: Tag, k: RawKind) -> ProcessResult<Handle> {
        self.insert_element_for(tag);
        self.to_raw_text_mode(k)
    }

    pub(crate) fn current_node(&self) -> Ref<'_, Handle> {
        Ref::map(self.open_elems.borrow(), |elems| {
            elems.last().expect("no current element")
        })
    }

    pub(crate) fn adjusted_current_node(&self) -> Ref<'_, Handle> {
        if self.open_elems.borrow().len() == 1 {
            let context_elem = self.context_elem.borrow();
            if let Ok(ctx) = Ref::filter_map(context_elem, |e| e.as_ref()) {
                return ctx;
            }
        }
        self.current_node()
    }

    pub(crate) fn current_node_in<TagSet>(&self, set: TagSet) -> bool
    where
        TagSet: Fn(ExpandedName) -> bool,
    {
        set(self.sink.elem_name(&self.current_node()).expanded())
    }

    pub(crate) fn insert_appropriately(
        &self,
        child: NodeOrText<Handle>,
        override_target: Option<Handle>,
    ) {
        let insertion_point = self.appropriate_place_for_insertion(override_target);
        self.insert_at(insertion_point, child);
    }

    pub(crate) fn adoption_agency(&self, subject: LocalName) {
        if self.current_node_named(subject.clone())
            && self
                .position_in_active_formatting(&self.current_node())
                .is_none()
        {
            self.pop();
            return;
        }

        for _ in 0..8 {
            let maybe_fmt_entry = self
                .active_formatting_end_to_marker()
                .iter()
                .find(|&(_, _, tag)| tag.name == subject)
                .map(|(i, h, t)| (i, h.clone(), t.clone()));

            let Some((fmt_elem_index, fmt_elem, fmt_elem_tag)) = maybe_fmt_entry else {
                self.process_end_tag_in_body(Tag {
                    kind: EndTag,
                    name: subject,
                    self_closing: false,
                    attrs: Vec::new(),
                    had_duplicate_attributes: false,
                });
                return;
            };

            let Some(fmt_elem_stack_index) = self
                .open_elems
                .borrow()
                .iter()
                .rposition(|n| self.sink.same_node(n, &fmt_elem))
            else {
                self.sink
                    .parse_error(Borrowed("Formatting element not open"));
                self.active_formatting.borrow_mut().remove(fmt_elem_index);
                return;
            };

            if !self.in_scope(default_scope, |n| self.sink.same_node(&n, &fmt_elem)) {
                self.sink
                    .parse_error(Borrowed("Formatting element not in scope"));
                return;
            }

            if !self.sink.same_node(&self.current_node(), &fmt_elem) {
                self.sink
                    .parse_error(Borrowed("Formatting element not current node"));
            }

            let maybe_furthest_block = self
                .open_elems
                .borrow()
                .iter()
                .enumerate()
                .skip(fmt_elem_stack_index)
                .find(|&(_, open_element)| self.elem_in(open_element, special_tag))
                .map(|(i, h)| (i, h.clone()));

            let Some((furthest_block_index, furthest_block)) = maybe_furthest_block else {
                self.open_elems.borrow_mut().truncate(fmt_elem_stack_index);
                self.active_formatting.borrow_mut().remove(fmt_elem_index);
                return;
            };

            let common_ancestor = self.open_elems.borrow()[fmt_elem_stack_index - 1].clone();
            let mut bookmark = Bookmark::Replace(fmt_elem.clone());

            let mut node_index = furthest_block_index;
            let mut last_node = furthest_block.clone();
            let mut inner_counter = 0usize;

            loop {
                inner_counter += 1;
                node_index -= 1;
                let mut node = self.open_elems.borrow()[node_index].clone();

                if self.sink.same_node(&node, &fmt_elem) {
                    break;
                }

                if inner_counter > 3 {
                    if let Some(pos) = self.position_in_active_formatting(&node) {
                        self.active_formatting.borrow_mut().remove(pos);
                    }
                    self.open_elems.borrow_mut().remove(node_index);
                    continue;
                }

                let Some(node_formatting_index) = self.position_in_active_formatting(&node) else {
                    self.open_elems.borrow_mut().remove(node_index);
                    continue;
                };

                let tag = match self.active_formatting.borrow()[node_formatting_index] {
                    FormatEntry::Element(_, ref t) => t.clone(),
                    FormatEntry::Marker => unreachable!("marker during adoption agency"),
                };
                let new_element = create_element_with_flags(
                    &self.sink,
                    QualName::new(None, ns!(html), tag.name.clone()),
                    tag.attrs.clone(),
                    tag.had_duplicate_attributes,
                );
                self.open_elems.borrow_mut()[node_index] = new_element.clone();
                self.active_formatting.borrow_mut()[node_formatting_index] =
                    FormatEntry::Element(new_element.clone(), tag);
                node = new_element;

                if self.sink.same_node(&last_node, &furthest_block) {
                    bookmark = Bookmark::InsertAfter(node.clone());
                }

                self.sink.remove_from_parent(&last_node);
                self.sink.append(&node, AppendNode(last_node.clone()));
                last_node = node;
            }

            self.sink.remove_from_parent(&last_node);
            self.insert_appropriately(AppendNode(last_node.clone()), Some(common_ancestor));

            let new_element = create_element_with_flags(
                &self.sink,
                QualName::new(None, ns!(html), fmt_elem_tag.name.clone()),
                fmt_elem_tag.attrs.clone(),
                fmt_elem_tag.had_duplicate_attributes,
            );
            let new_entry = FormatEntry::Element(new_element.clone(), fmt_elem_tag);

            self.sink.reparent_children(&furthest_block, &new_element);
            self.sink
                .append(&furthest_block, AppendNode(new_element.clone()));

            match bookmark {
                Bookmark::Replace(to_replace) => {
                    if let Some(index) = self.position_in_active_formatting(&to_replace) {
                        self.active_formatting.borrow_mut()[index] = new_entry;
                    }
                }
                Bookmark::InsertAfter(previous) => {
                    if let Some(prev_idx) = self.position_in_active_formatting(&previous) {
                        self.active_formatting
                            .borrow_mut()
                            .insert(prev_idx + 1, new_entry);
                    }
                    if let Some(old_idx) = self.position_in_active_formatting(&fmt_elem) {
                        self.active_formatting.borrow_mut().remove(old_idx);
                    }
                }
            }

            self.remove_from_stack(&fmt_elem);
            let new_fb_idx = self
                .open_elems
                .borrow()
                .iter()
                .position(|n| self.sink.same_node(n, &furthest_block));
            if let Some(new_fb_idx) = new_fb_idx {
                self.open_elems
                    .borrow_mut()
                    .insert(new_fb_idx + 1, new_element);
            }
        }
    }

    pub(crate) fn push(&self, elem: &Handle) {
        self.open_elems.borrow_mut().push(elem.clone());
    }

    pub(crate) fn pop(&self) -> Handle {
        let elem = self
            .open_elems
            .borrow_mut()
            .pop()
            .expect("no current element");
        self.sink.pop(&elem);
        elem
    }

    pub(crate) fn remove_from_stack(&self, elem: &Handle) {
        let position = self
            .open_elems
            .borrow()
            .iter()
            .rposition(|x| self.sink.same_node(elem, x));
        if let Some(position) = position {
            self.open_elems.borrow_mut().remove(position);
            self.sink.pop(elem);
        }
    }

    fn is_marker_or_open(&self, entry: &FormatEntry<Handle>) -> bool {
        match entry {
            FormatEntry::Marker => true,
            FormatEntry::Element(node, _) => self
                .open_elems
                .borrow()
                .iter()
                .rev()
                .any(|n| self.sink.same_node(n, node)),
        }
    }

    pub(crate) fn reconstruct_active_formatting_elements(&self) {
        {
            let active_formatting = self.active_formatting.borrow();
            let Some(last) = active_formatting.last() else {
                return;
            };
            if self.is_marker_or_open(last) {
                return;
            }
        }

        let mut entry_index = self.active_formatting.borrow().len() - 1;
        loop {
            if entry_index == 0 {
                break;
            }
            entry_index -= 1;
            if self.is_marker_or_open(&self.active_formatting.borrow()[entry_index]) {
                entry_index += 1;
                break;
            }
        }

        loop {
            let tag = match self.active_formatting.borrow()[entry_index] {
                FormatEntry::Element(_, ref t) => t.clone(),
                FormatEntry::Marker => unreachable!("marker during formatting reconstruction"),
            };
            let new_element = self.insert_element(
                PushFlag::Push,
                ns!(html),
                tag.name.clone(),
                tag.attrs.clone(),
                tag.had_duplicate_attributes,
            );
            self.active_formatting.borrow_mut()[entry_index] =
                FormatEntry::Element(new_element, tag);
            if entry_index == self.active_formatting.borrow().len() - 1 {
                break;
            }
            entry_index += 1;
        }
    }

    pub(crate) fn body_elem(&self) -> Option<Ref<'_, Handle>> {
        if self.open_elems.borrow().len() <= 1 {
            return None;
        }
        let node = Ref::map(self.open_elems.borrow(), |elems| &elems[1]);
        if self.html_elem_named(&node, local_name!("body")) {
            Some(node)
        } else {
            None
        }
    }

    pub(crate) fn check_body_end(&self) {
        declare_tag_set!(body_end_ok =
            "dd" "dt" "li" "optgroup" "option" "p" "rp" "rt" "tbody" "td" "tfoot" "th"
            "thead" "tr" "body" "html");
        for elem in self.open_elems.borrow().iter() {
            let bad = !body_end_ok(self.sink.elem_name(elem).expanded());
            if bad {
                self.sink
                    .parse_error(Borrowed("Unexpected open tag at end of body"));
                return;
            }
        }
    }

    pub(crate) fn in_scope<TagSet, Pred>(&self, scope: TagSet, pred: Pred) -> bool
    where
        TagSet: Fn(ExpandedName) -> bool,
        Pred: Fn(Handle) -> bool,
    {
        for node in self.open_elems.borrow().iter().rev() {
            if pred(node.clone()) {
                return true;
            }
            if scope(self.sink.elem_name(node).expanded()) {
                return false;
            }
        }
        false
    }

    pub(crate) fn elem_in<TagSet>(&self, elem: &Handle, set: TagSet) -> bool
    where
        TagSet: Fn(ExpandedName) -> bool,
    {
        set(self.sink.elem_name(elem).expanded())
    }

    pub(crate) fn html_elem_named(&self, elem: &Handle, name: LocalName) -> bool {
        let elem_name = self.sink.elem_name(elem);
        *elem_name.ns() == ns!(html) && *elem_name.local_name() == name
    }

    pub(crate) fn in_html_elem_named(&self, name: LocalName) -> bool {
        self.open_elems
            .borrow()
            .iter()
            .any(|elem| self.html_elem_named(elem, name.clone()))
    }

    pub(crate) fn current_node_named(&self, name: LocalName) -> bool {
        self.html_elem_named(&self.current_node(), name)
    }

    pub(crate) fn in_scope_named<TagSet>(&self, scope: TagSet, name: LocalName) -> bool
    where
        TagSet: Fn(ExpandedName) -> bool,
    {
        self.in_scope(scope, |elem| self.html_elem_named(&elem, name.clone()))
    }

    pub(crate) fn generate_implied_end_tags<TagSet>(&self, set: TagSet)
    where
        TagSet: Fn(ExpandedName) -> bool,
    {
        loop {
            {
                let open_elems = self.open_elems.borrow();
                let Some(elem) = open_elems.last() else {
                    return;
                };
                if !set(self.sink.elem_name(elem).expanded()) {
                    return;
                }
            }
            self.pop();
        }
    }

    pub(crate) fn generate_implied_end_except(&self, except: LocalName) {
        self.generate_implied_end_tags(|p| {
            if *p.ns == ns!(html) && *p.local == except {
                false
            } else {
                cursory_implied_end(p)
            }
        });
    }

    pub(crate) fn pop_until_current<TagSet>(&self, tag_set: TagSet)
    where
        TagSet: Fn(ExpandedName) -> bool,
    {
        while !self.current_node_in(&tag_set) {
            self.open_elems.borrow_mut().pop();
        }
    }

    pub(crate) fn pop_until<P>(&self, pred: P) -> usize
    where
        P: Fn(ExpandedName) -> bool,
    {
        let mut n = 0usize;
        loop {
            n += 1;
            match self.open_elems.borrow_mut().pop() {
                None => break,
                Some(elem) => {
                    if pred(self.sink.elem_name(&elem).expanded()) {
                        break;
                    }
                }
            }
        }
        n
    }

    pub(crate) fn pop_until_named(&self, name: LocalName) -> usize {
        self.pop_until(|p| *p.ns == ns!(html) && *p.local == name)
    }

    pub(crate) fn expect_to_close(&self, name: LocalName) {
        if self.pop_until_named(name) != 1 {
            self.sink.parse_error(Borrowed("Unexpected open element"));
        }
    }

    pub(crate) fn close_p_element(&self) {
        declare_tag_set!(implied = [cursory_implied_end] - "p");
        self.generate_implied_end_tags(implied);
        self.expect_to_close(local_name!("p"));
    }

    pub(crate) fn close_p_element_in_button_scope(&self) {
        if self.in_scope_named(button_scope, local_name!("p")) {
            self.close_p_element();
        }
    }

    pub(crate) fn is_type_hidden(&self, tag: &Tag) -> bool {
        tag.attrs
            .iter()
            .find(|at| at.name.expanded() == expanded_name!("", "type"))
            .is_some_and(|at| at.value.eq_ignore_ascii_case("hidden"))
    }

    pub(crate) fn foster_parent_in_body(&self, token: Token) -> ProcessResult<Handle> {
        self.foster_parenting.set(true);
        let res = self.step(InsertionMode::InBody, token);
        self.foster_parenting.set(false);
        res
    }

    pub(crate) fn process_chars_in_table(&self, token: Token) -> ProcessResult<Handle> {
        declare_tag_set!(table_outer = "table" "tbody" "tfoot" "thead" "tr");
        if self.current_node_in(table_outer) {
            self.orig_mode.set(Some(self.mode.get()));
            ProcessResult::Reprocess(InsertionMode::InTableText, token)
        } else {
            self.sink
                .parse_error(Borrowed("Unexpected characters in table"));
            self.foster_parent_in_body(token)
        }
    }

    pub(crate) fn reset_insertion_mode(&self) -> InsertionMode {
        let open_elems = self.open_elems.borrow();
        let context_elem = self.context_elem.borrow();
        for (i, mut node) in open_elems.iter().enumerate().rev() {
            let last = i == 0;
            if let (true, Some(ctx)) = (last, context_elem.as_ref()) {
                node = ctx;
            }
            let elem_name = self.sink.elem_name(node);
            let name = match elem_name.expanded() {
                ExpandedName {
                    ns: &ns!(html),
                    local,
                } => local,
                _ => continue,
            };
            match *name {
                local_name!("td") | local_name!("th") if !last => return InsertionMode::InCell,
                local_name!("tr") => return InsertionMode::InRow,
                local_name!("tbody") | local_name!("thead") | local_name!("tfoot") => {
                    return InsertionMode::InTableBody;
                }
                local_name!("caption") => return InsertionMode::InCaption,
                local_name!("colgroup") => return InsertionMode::InColumnGroup,
                local_name!("table") => return InsertionMode::InTable,
                local_name!("template") => {
                    return *self
                        .template_modes
                        .borrow()
                        .last()
                        .unwrap_or(&InsertionMode::InTemplate);
                }
                local_name!("head") if !last => return InsertionMode::InHead,
                local_name!("body") => return InsertionMode::InBody,
                local_name!("frameset") => return InsertionMode::InFrameset,
                local_name!("html") => {
                    return match *self.head_elem.borrow() {
                        None => InsertionMode::BeforeHead,
                        Some(_) => InsertionMode::AfterHead,
                    };
                }
                _ => {}
            }
        }
        InsertionMode::InBody
    }

    pub(crate) fn close_the_cell(&self) {
        self.generate_implied_end_tags(cursory_implied_end);
        if self.pop_until(td_th) != 1 {
            self.sink
                .parse_error(Borrowed("expected to close <td> or <th> with cell"));
        }
        self.clear_active_formatting_to_marker();
    }

    pub(crate) fn append_text(&self, text: StrTendril) -> ProcessResult<Handle> {
        self.insert_appropriately(AppendText(text), None);
        ProcessResult::Done
    }

    pub(crate) fn append_comment(&self, text: StrTendril) -> ProcessResult<Handle> {
        let comment = self.sink.create_comment(text);
        self.insert_appropriately(AppendNode(comment), None);
        ProcessResult::Done
    }

    pub(crate) fn append_comment_to_doc(&self, text: StrTendril) -> ProcessResult<Handle> {
        let comment = self.sink.create_comment(text);
        self.sink.append(&self.doc_handle, AppendNode(comment));
        ProcessResult::Done
    }

    pub(crate) fn append_comment_to_html(&self, text: StrTendril) -> ProcessResult<Handle> {
        let open_elems = self.open_elems.borrow();
        let target = html_elem(&open_elems);
        let comment = self.sink.create_comment(text);
        self.sink.append(target, AppendNode(comment));
        ProcessResult::Done
    }

    pub(crate) fn create_root(&self, attrs: Vec<Attribute>) {
        let elem = create_element(
            &self.sink,
            QualName::new(None, ns!(html), local_name!("html")),
            attrs,
        );
        self.push(&elem);
        self.sink.append(&self.doc_handle, AppendNode(elem));
    }

    pub(crate) fn insert_element(
        &self,
        push: PushFlag,
        ns: Namespace,
        name: LocalName,
        attrs: Vec<Attribute>,
        had_duplicate_attributes: bool,
    ) -> Handle {
        declare_tag_set!(form_associatable =
            "button" "fieldset" "input" "object" "output" "select" "textarea" "img");
        declare_tag_set!(listed = [form_associatable] - "img");

        let insertion_point = self.appropriate_place_for_insertion(None);
        let (node1, node2) = match insertion_point {
            InsertionPoint::LastChild(ref p) => (p.clone(), None),
            InsertionPoint::TableFosterParenting {
                ref element,
                ref prev_element,
            } => (element.clone(), Some(prev_element.clone())),
        };

        let qname = QualName::new(None, ns, name);
        let form_is_associatable = form_associatable(qname.expanded())
            && self.form_elem.borrow().is_some()
            && !self.in_template()
            && !(listed(qname.expanded())
                && attrs
                    .iter()
                    .any(|a| a.name.expanded() == expanded_name!("", "form")));

        let elem = create_element_with_flags(&self.sink, qname, attrs, had_duplicate_attributes);

        if form_is_associatable {
            let form = self.form_elem.borrow().as_ref().cloned();
            if let Some(form) = form {
                self.sink
                    .associate_with_form(&elem, &form, (&node1, node2.as_ref()));
            }
        }

        self.insert_at(insertion_point, AppendNode(elem.clone()));

        if matches!(push, PushFlag::Push) {
            self.push(&elem);
        }
        elem
    }

    pub(crate) fn insert_element_for(&self, tag: Tag) -> Handle {
        self.insert_element(
            PushFlag::Push,
            ns!(html),
            tag.name,
            tag.attrs,
            tag.had_duplicate_attributes,
        )
    }

    pub(crate) fn insert_and_pop_element_for(&self, tag: Tag) -> Handle {
        self.insert_element(
            PushFlag::NoPush,
            ns!(html),
            tag.name,
            tag.attrs,
            tag.had_duplicate_attributes,
        )
    }

    pub(crate) fn insert_phantom(&self, name: LocalName) -> Handle {
        self.insert_element(PushFlag::Push, ns!(html), name, Vec::new(), false)
    }

    pub(crate) fn insert_foreign_element(
        &self,
        tag: Tag,
        ns: Namespace,
        only_add_to_element_stack: bool,
    ) -> Handle {
        let adjusted_insertion_location = self.appropriate_place_for_insertion(None);
        let qname = QualName::new(None, ns, tag.name);
        let elem =
            create_element_with_flags(&self.sink, qname, tag.attrs, tag.had_duplicate_attributes);
        if !only_add_to_element_stack {
            self.insert_at(adjusted_insertion_location, AppendNode(elem.clone()));
        }
        self.push(&elem);
        elem
    }

    pub(crate) fn should_attach_declarative_shadow(&self, tag: &Tag) -> bool {
        let adjusted_insertion_location = self.appropriate_place_for_insertion(None);
        let intended_parent = match adjusted_insertion_location {
            InsertionPoint::LastChild(p) => p,
            InsertionPoint::TableFosterParenting { element, .. } => element,
        };
        let is_shadow_root_mode = tag.attrs.iter().any(|attr| {
            attr.name.local == local_name!("shadowrootmode")
                && (attr.value.eq_ignore_ascii_case("open")
                    || attr.value.eq_ignore_ascii_case("closed"))
        });
        let allow = self.sink.allow_declarative_shadow_roots(&intended_parent);
        let adjusted_not_topmost = match self.open_elems.borrow().first() {
            Some(_) => self.open_elems.borrow().len() > 1,
            None => true,
        };
        is_shadow_root_mode && allow && adjusted_not_topmost
    }

    pub(crate) fn attach_declarative_shadow(
        &self,
        tag: &Tag,
        shadow_host: &Handle,
        template: &Handle,
    ) -> bool {
        self.sink
            .attach_declarative_shadow(shadow_host, template, &tag.attrs)
    }

    pub(crate) fn create_formatting_element_for(&self, tag: Tag) -> Handle {
        let mut first_match = None;
        let mut matches = 0usize;
        for (i, _, old_tag) in self.active_formatting_end_to_marker().iter() {
            if tag.equiv_modulo_attr_order(old_tag) {
                first_match = Some(i);
                matches += 1;
            }
        }
        if matches >= 3 {
            if let Some(idx) = first_match {
                self.active_formatting.borrow_mut().remove(idx);
            }
        }

        let elem = self.insert_element(
            PushFlag::Push,
            ns!(html),
            tag.name.clone(),
            tag.attrs.clone(),
            tag.had_duplicate_attributes,
        );
        self.active_formatting
            .borrow_mut()
            .push(FormatEntry::Element(elem.clone(), tag));
        elem
    }

    pub(crate) fn clear_active_formatting_to_marker(&self) {
        loop {
            match self.active_formatting.borrow_mut().pop() {
                None | Some(FormatEntry::Marker) => break,
                _ => {}
            }
        }
    }

    pub(crate) fn process_end_tag_in_body(&self, tag: Tag) {
        let mut match_idx = None;
        for (i, elem) in self.open_elems.borrow().iter().enumerate().rev() {
            if self.html_elem_named(elem, tag.name.clone()) {
                match_idx = Some(i);
                break;
            }
            if self.elem_in(elem, special_tag) {
                self.sink
                    .parse_error(Borrowed("Found special tag while closing generic tag"));
                return;
            }
        }

        let Some(match_idx) = match_idx else {
            self.unexpected(&tag);
            return;
        };

        self.generate_implied_end_except(tag.name.clone());
        if match_idx != self.open_elems.borrow().len() - 1 {
            self.unexpected(&tag);
        }
        self.open_elems.borrow_mut().truncate(match_idx);
    }

    pub(crate) fn handle_misnested_a_tags(&self, tag: &Tag) {
        let Some(node) = self
            .active_formatting_end_to_marker()
            .iter()
            .find(|&(_, n, _)| self.html_elem_named(n, local_name!("a")))
            .map(|(_, n, _)| n.clone())
        else {
            return;
        };
        self.unexpected(tag);
        self.adoption_agency(local_name!("a"));
        if let Some(index) = self.position_in_active_formatting(&node) {
            self.active_formatting.borrow_mut().remove(index);
        }
        self.remove_from_stack(&node);
    }

    fn is_foreign(&self, token: &Token) -> bool {
        if matches!(token, Token::Eof) || self.open_elems.borrow().is_empty() {
            return false;
        }
        let current = self.adjusted_current_node();
        let elem_name = self.sink.elem_name(&current);
        let name = elem_name.expanded();
        if let ns!(html) = *name.ns {
            return false;
        }

        if mathml_text_integration_point(name) {
            match token {
                Token::Characters(..) | Token::NullCharacter => return false,
                Token::Tag(Tag {
                    kind: StartTag,
                    name,
                    ..
                }) if !matches!(*name, local_name!("mglyph") | local_name!("malignmark")) => {
                    return false;
                }
                _ => {}
            }
        }

        if svg_html_integration_point(name) {
            match token {
                Token::Characters(..)
                | Token::NullCharacter
                | Token::Tag(Tag { kind: StartTag, .. }) => return false,
                _ => {}
            }
        }

        if let expanded_name!(mathml "annotation-xml") = name {
            match token {
                Token::Tag(Tag {
                    kind: StartTag,
                    name: local_name!("svg"),
                    ..
                }) => return false,
                Token::Characters(..)
                | Token::NullCharacter
                | Token::Tag(Tag { kind: StartTag, .. }) => {
                    return !self
                        .sink
                        .is_mathml_annotation_xml_integration_point(&self.adjusted_current_node());
                }
                _ => {}
            }
        }
        true
    }

    pub(crate) fn enter_foreign(&self, mut tag: Tag, ns: Namespace) -> ProcessResult<Handle> {
        match ns {
            ns!(mathml) => foreign::adjust_mathml_attributes(&mut tag),
            ns!(svg) => foreign::adjust_svg_attributes(&mut tag),
            _ => {}
        }
        foreign::adjust_foreign_attributes(&mut tag);
        if tag.self_closing {
            self.insert_element(
                PushFlag::NoPush,
                ns,
                tag.name,
                tag.attrs,
                tag.had_duplicate_attributes,
            );
            ProcessResult::DoneAckSelfClosing
        } else {
            self.insert_element(
                PushFlag::Push,
                ns,
                tag.name,
                tag.attrs,
                tag.had_duplicate_attributes,
            );
            ProcessResult::Done
        }
    }

    pub(crate) fn foreign_start_tag(&self, mut tag: Tag) -> ProcessResult<Handle> {
        let current_ns = self
            .sink
            .elem_name(&self.adjusted_current_node())
            .ns()
            .clone();
        match current_ns {
            ns!(mathml) => foreign::adjust_mathml_attributes(&mut tag),
            ns!(svg) => {
                foreign::adjust_svg_tag_name(&mut tag);
                foreign::adjust_svg_attributes(&mut tag);
            }
            _ => {}
        }
        foreign::adjust_foreign_attributes(&mut tag);
        let is_svg_script = current_ns == ns!(svg) && tag.name == local_name!("script");
        if tag.self_closing {
            let elem = self.insert_element(
                PushFlag::NoPush,
                current_ns,
                tag.name,
                tag.attrs,
                tag.had_duplicate_attributes,
            );
            if is_svg_script {
                if self.is_fragment() {
                    self.sink.mark_script_already_started(&elem);
                }
                return ProcessResult::Script(elem);
            }
            ProcessResult::DoneAckSelfClosing
        } else {
            let elem = self.insert_element(
                PushFlag::Push,
                current_ns,
                tag.name,
                tag.attrs,
                tag.had_duplicate_attributes,
            );
            if is_svg_script && self.is_fragment() {
                self.sink.mark_script_already_started(&elem);
            }
            ProcessResult::Done
        }
    }

    pub(crate) fn unexpected_start_tag_in_foreign_content(
        &self,
        tag: Tag,
    ) -> ProcessResult<Handle> {
        self.unexpected(&tag);
        while !self.current_node_in(|n| {
            *n.ns == ns!(html) || mathml_text_integration_point(n) || svg_html_integration_point(n)
        }) {
            self.pop();
        }
        self.step(self.mode.get(), Token::Tag(tag))
    }
}

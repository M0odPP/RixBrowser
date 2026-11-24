//! CSS Selectors Level 4 integration over `selectors 0.40` and `rix-dom`.
//!
//! Precomputed hashes match `rix_dom::ElementData` (`hash_ident` for classes/IDs
//! and `hash_ident_ascii_lower` for tag names) so `BloomFilter` ancestor pruning
//! works with zero hash-mismatch false negatives.

use std::fmt;

use cssparser::{
    serialize_identifier, serialize_string, CowRcStr, ParseError, Parser as CssParser, ParserInput,
    SourceLocation, ToCss,
};
use lightningcss::printer::Printer;
use lightningcss::stylesheet::PrinterOptions;
use lightningcss::traits::ToCss as LightningToCss;
use precomputed_hash::PrecomputedHash;
use rix_core::{intern_str, InternedStr};
use rix_dom::{
    fold_hash_u32, hash_ident, hash_ident_ascii_lower, Document, ElementData, ElementState,
    NodeData, NodeId,
};
use selectors::attr::{AttrSelectorOperation, CaseSensitivity, NamespaceConstraint};
use selectors::bloom::BloomFilter;
use selectors::context::{
    MatchingContext, MatchingForInvalidation, MatchingMode, NeedsSelectorFlags, QuirksMode,
    SelectorCaches,
};
use selectors::matching::{matches_selector, ElementSelectorFlags};
use selectors::parser::{
    NonTSPseudoClass, ParseRelative, Parser as SelectorsParser, PseudoElement, Selector,
    SelectorImpl, SelectorList, SelectorParseErrorKind,
};
use selectors::{Element, OpaqueElement};

/// Case-sensitive interned CSS identifier with precomputed FNV-1a hash matching `rix-dom`.
#[derive(Clone, Eq)]
pub struct CssAtom {
    pub ident: InternedStr,
    pub hash64: u64,
    pub hash32: u32,
}

impl CssAtom {
    #[must_use]
    pub fn new(s: &str) -> Self {
        let hash64 = hash_ident(s);
        Self {
            ident: intern_str(s),
            hash64,
            hash32: fold_hash_u32(hash64),
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        self.ident.as_str()
    }
}

impl Default for CssAtom {
    fn default() -> Self {
        Self::new("")
    }
}

impl PartialEq for CssAtom {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.hash64 == other.hash64 && self.ident == other.ident
    }
}

impl std::hash::Hash for CssAtom {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.hash64.hash(state);
    }
}

impl fmt::Debug for CssAtom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.ident.as_str())
    }
}

impl From<&str> for CssAtom {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl PrecomputedHash for CssAtom {
    #[inline]
    fn precomputed_hash(&self) -> u32 {
        self.hash32
    }
}

impl ToCss for CssAtom {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        serialize_identifier(self.ident.as_str(), dest)
    }
}

impl AsRef<str> for CssAtom {
    #[inline]
    fn as_ref(&self) -> &str {
        self.ident.as_str()
    }
}

/// CSS attribute selector value serialized as a CSSOM quoted string (`[attr="val"]`).
#[derive(Clone, Eq, PartialEq, Hash, Default)]
pub struct CssAttrValue(pub CssAtom);

impl CssAttrValue {
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for CssAttrValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0.as_str())
    }
}

impl From<&str> for CssAttrValue {
    fn from(s: &str) -> Self {
        Self(CssAtom::new(s))
    }
}

impl AsRef<str> for CssAttrValue {
    #[inline]
    fn as_ref(&self) -> &str {
        self.0.as_str()
    }
}

impl ToCss for CssAttrValue {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        serialize_string(self.0.as_str(), dest)
    }
}

/// ASCII-lowercased CSS local name (tags and attribute names) matching `ElementData::tag_hash`.
#[derive(Clone, Eq)]
pub struct CssLocalName {
    pub ident: InternedStr,
    pub hash64: u64,
    pub hash32: u32,
}

impl CssLocalName {
    #[must_use]
    pub fn new(s: &str) -> Self {
        let lower = s.to_ascii_lowercase();
        let hash64 = hash_ident_ascii_lower(&lower);
        Self {
            ident: intern_str(&lower),
            hash64,
            hash32: fold_hash_u32(hash64),
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        self.ident.as_str()
    }
}

impl Default for CssLocalName {
    fn default() -> Self {
        Self::new("")
    }
}

impl PartialEq for CssLocalName {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.hash64 == other.hash64 && self.ident == other.ident
    }
}

impl std::hash::Hash for CssLocalName {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.hash64.hash(state);
    }
}

impl fmt::Debug for CssLocalName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.ident.as_str())
    }
}

impl From<&str> for CssLocalName {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl PrecomputedHash for CssLocalName {
    #[inline]
    fn precomputed_hash(&self) -> u32 {
        self.hash32
    }
}

impl ToCss for CssLocalName {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        serialize_identifier(self.ident.as_str(), dest)
    }
}

/// Non-tree-structural pseudo-classes supported by RixBrowser (Selectors Level 4/5).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RixPseudoClass {
    Hover,
    Active,
    Focus,
    FocusVisible,
    FocusWithin,
    Enabled,
    Disabled,
    Checked,
    Indeterminate,
    ReadOnly,
    ReadWrite,
    Link,
    Visited,
    AnyLink,
    PlaceholderShown,
    Default,
    Valid,
    Invalid,
    UserValid,
    UserInvalid,
    InRange,
    OutOfRange,
    Required,
    Optional,
    Defined,
    Target,
    Modal,
    PopoverOpen,
    Open,
    Fullscreen,
    Autofill,
    Blank,
    Playing,
    Paused,
    Muted,
    Buffering,
    Stalled,
    Seeking,
    VolumeLocked,
    HasSlotted,
    Heading,
    HeadingList(Vec<i32>),
    Dir(CssAtom),
    Lang(Vec<CssAtom>),
    State(CssAtom),
}

impl ToCss for RixPseudoClass {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        match self {
            Self::Hover => dest.write_str(":hover"),
            Self::Active => dest.write_str(":active"),
            Self::Focus => dest.write_str(":focus"),
            Self::FocusVisible => dest.write_str(":focus-visible"),
            Self::FocusWithin => dest.write_str(":focus-within"),
            Self::Enabled => dest.write_str(":enabled"),
            Self::Disabled => dest.write_str(":disabled"),
            Self::Checked => dest.write_str(":checked"),
            Self::Indeterminate => dest.write_str(":indeterminate"),
            Self::ReadOnly => dest.write_str(":read-only"),
            Self::ReadWrite => dest.write_str(":read-write"),
            Self::Link => dest.write_str(":link"),
            Self::Visited => dest.write_str(":visited"),
            Self::AnyLink => dest.write_str(":any-link"),
            Self::PlaceholderShown => dest.write_str(":placeholder-shown"),
            Self::Default => dest.write_str(":default"),
            Self::Valid => dest.write_str(":valid"),
            Self::Invalid => dest.write_str(":invalid"),
            Self::UserValid => dest.write_str(":user-valid"),
            Self::UserInvalid => dest.write_str(":user-invalid"),
            Self::InRange => dest.write_str(":in-range"),
            Self::OutOfRange => dest.write_str(":out-of-range"),
            Self::Required => dest.write_str(":required"),
            Self::Optional => dest.write_str(":optional"),
            Self::Defined => dest.write_str(":defined"),
            Self::Target => dest.write_str(":target"),
            Self::Modal => dest.write_str(":modal"),
            Self::PopoverOpen => dest.write_str(":popover-open"),
            Self::Open => dest.write_str(":open"),
            Self::Fullscreen => dest.write_str(":fullscreen"),
            Self::Autofill => dest.write_str(":autofill"),
            Self::Blank => dest.write_str(":blank"),
            Self::Playing => dest.write_str(":playing"),
            Self::Paused => dest.write_str(":paused"),
            Self::Muted => dest.write_str(":muted"),
            Self::Buffering => dest.write_str(":buffering"),
            Self::Stalled => dest.write_str(":stalled"),
            Self::Seeking => dest.write_str(":seeking"),
            Self::VolumeLocked => dest.write_str(":volume-locked"),
            Self::HasSlotted => dest.write_str(":has-slotted"),
            Self::Heading => dest.write_str(":heading"),
            Self::HeadingList(levels) => {
                dest.write_str(":heading(")?;
                for (i, lvl) in levels.iter().enumerate() {
                    if i > 0 {
                        dest.write_str(", ")?;
                    }
                    write!(dest, "{lvl}")?;
                }
                dest.write_char(')')
            }
            Self::Dir(dir) => {
                dest.write_str(":dir(")?;
                serialize_identifier(dir.as_str(), dest)?;
                dest.write_char(')')
            }
            Self::Lang(langs) => {
                dest.write_str(":lang(")?;
                for (i, lang) in langs.iter().enumerate() {
                    if i > 0 {
                        dest.write_str(", ")?;
                    }
                    if lang.as_str().contains('*') || lang.as_str().is_empty() {
                        serialize_string(lang.as_str(), dest)?;
                    } else {
                        serialize_identifier(lang.as_str(), dest)?;
                    }
                }
                dest.write_char(')')
            }
            Self::State(st) => {
                dest.write_str(":state(")?;
                serialize_identifier(st.as_str(), dest)?;
                dest.write_char(')')
            }
        }
    }
}

impl NonTSPseudoClass for RixPseudoClass {
    type Impl = RixSelectorImpl;

    fn is_active_or_hover(&self) -> bool {
        matches!(self, Self::Active | Self::Hover)
    }

    fn is_user_action_state(&self) -> bool {
        matches!(
            self,
            Self::Hover | Self::Active | Self::Focus | Self::FocusVisible | Self::FocusWithin
        )
    }
}

/// Pseudo-elements supported by RixBrowser (Selectors Level 4 / CSS Pseudo-Elements).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RixPseudoElement {
    Before,
    After,
    Marker,
    Placeholder,
    Selection,
    FirstLine,
    FirstLetter,
    Backdrop,
    FileSelectorButton,
    DetailsContent,
    Cue,
    TargetText,
}

impl ToCss for RixPseudoElement {
    fn to_css<W: fmt::Write>(&self, dest: &mut W) -> fmt::Result {
        match self {
            Self::Before => dest.write_str("::before"),
            Self::After => dest.write_str("::after"),
            Self::Marker => dest.write_str("::marker"),
            Self::Placeholder => dest.write_str("::placeholder"),
            Self::Selection => dest.write_str("::selection"),
            Self::FirstLine => dest.write_str("::first-line"),
            Self::FirstLetter => dest.write_str("::first-letter"),
            Self::Backdrop => dest.write_str("::backdrop"),
            Self::FileSelectorButton => dest.write_str("::file-selector-button"),
            Self::DetailsContent => dest.write_str("::details-content"),
            Self::Cue => dest.write_str("::cue"),
            Self::TargetText => dest.write_str("::target-text"),
        }
    }
}

impl PseudoElement for RixPseudoElement {
    type Impl = RixSelectorImpl;

    fn is_before_or_after(&self) -> bool {
        matches!(self, Self::Before | Self::After)
    }

    fn valid_after_slotted(&self) -> bool {
        matches!(
            self,
            Self::Before
                | Self::After
                | Self::Marker
                | Self::Placeholder
                | Self::FileSelectorButton
                | Self::DetailsContent
        )
    }
}

/// `selectors` trait implementation for RixBrowser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RixSelectorImpl;

impl SelectorImpl for RixSelectorImpl {
    type ExtraMatchingData<'a> = ();
    type AttrValue = CssAttrValue;
    type Identifier = CssAtom;
    type LocalName = CssLocalName;
    type NamespaceUrl = CssAtom;
    type NamespacePrefix = CssAtom;
    type BorrowedNamespaceUrl = CssAtom;
    type BorrowedLocalName = CssLocalName;
    type NonTSPseudoClass = RixPseudoClass;
    type PseudoElement = RixPseudoElement;
}

/// `@namespace` declarations active while parsing a stylesheet's selectors.
#[derive(Debug, Clone, Default)]
pub struct SelectorNamespaceMap {
    pub default_namespace: Option<CssAtom>,
    pub prefixes: std::collections::HashMap<String, CssAtom>,
}

impl SelectorNamespaceMap {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.default_namespace.is_none() && self.prefixes.is_empty()
    }
}

/// Selectors Level 4 parser implementation.
pub struct RixSelectorParser<'a> {
    pub forgiving: bool,
    pub namespaces: Option<&'a SelectorNamespaceMap>,
}

impl Default for RixSelectorParser<'_> {
    fn default() -> Self {
        Self {
            forgiving: true,
            namespaces: None,
        }
    }
}

impl<'i> SelectorsParser<'i> for RixSelectorParser<'_> {
    type Impl = RixSelectorImpl;
    type Error = SelectorParseErrorKind<'i>;

    fn default_namespace(&self) -> Option<CssAtom> {
        self.namespaces.and_then(|ns| ns.default_namespace.clone())
    }

    fn namespace_for_prefix(&self, prefix: &CssAtom) -> Option<CssAtom> {
        self.namespaces
            .and_then(|ns| ns.prefixes.get(prefix.as_str()).cloned())
    }

    fn parse_slotted(&self) -> bool {
        true
    }

    fn parse_part(&self) -> bool {
        true
    }

    fn parse_nth_child_of(&self) -> bool {
        true
    }

    fn parse_is_and_where(&self) -> bool {
        true
    }

    fn parse_has(&self) -> bool {
        true
    }

    fn parse_host(&self) -> bool {
        true
    }

    fn parse_parent_selector(&self) -> bool {
        true
    }

    fn allow_forgiving_selectors(&self) -> bool {
        self.forgiving
    }

    fn parse_non_ts_pseudo_class(
        &self,
        location: SourceLocation,
        name: CowRcStr<'i>,
    ) -> Result<RixPseudoClass, ParseError<'i, Self::Error>> {
        let lower = name.to_ascii_lowercase();
        let pc = match lower.as_str() {
            "hover" => RixPseudoClass::Hover,
            "active" => RixPseudoClass::Active,
            "focus" => RixPseudoClass::Focus,
            "focus-visible" => RixPseudoClass::FocusVisible,
            "focus-within" => RixPseudoClass::FocusWithin,
            "enabled" => RixPseudoClass::Enabled,
            "disabled" => RixPseudoClass::Disabled,
            "checked" => RixPseudoClass::Checked,
            "indeterminate" => RixPseudoClass::Indeterminate,
            "read-only" => RixPseudoClass::ReadOnly,
            "read-write" => RixPseudoClass::ReadWrite,
            "link" => RixPseudoClass::Link,
            "visited" => RixPseudoClass::Visited,
            "any-link" => RixPseudoClass::AnyLink,
            "placeholder-shown" => RixPseudoClass::PlaceholderShown,
            "default" => RixPseudoClass::Default,
            "valid" => RixPseudoClass::Valid,
            "invalid" => RixPseudoClass::Invalid,
            "user-valid" => RixPseudoClass::UserValid,
            "user-invalid" => RixPseudoClass::UserInvalid,
            "in-range" => RixPseudoClass::InRange,
            "out-of-range" => RixPseudoClass::OutOfRange,
            "required" => RixPseudoClass::Required,
            "optional" => RixPseudoClass::Optional,
            "defined" => RixPseudoClass::Defined,
            "target" => RixPseudoClass::Target,
            "modal" => RixPseudoClass::Modal,
            "popover-open" => RixPseudoClass::PopoverOpen,
            "open" => RixPseudoClass::Open,
            "fullscreen" => RixPseudoClass::Fullscreen,
            "autofill" => RixPseudoClass::Autofill,
            "blank" => RixPseudoClass::Blank,
            "playing" => RixPseudoClass::Playing,
            "paused" => RixPseudoClass::Paused,
            "muted" => RixPseudoClass::Muted,
            "buffering" => RixPseudoClass::Buffering,
            "stalled" => RixPseudoClass::Stalled,
            "seeking" => RixPseudoClass::Seeking,
            "volume-locked" => RixPseudoClass::VolumeLocked,
            "has-slotted" => RixPseudoClass::HasSlotted,
            "heading" => RixPseudoClass::Heading,
            _ => {
                return Err(location.new_custom_error(
                    SelectorParseErrorKind::UnsupportedPseudoClassOrElement(name),
                ));
            }
        };
        Ok(pc)
    }

    fn parse_non_ts_functional_pseudo_class<'t>(
        &self,
        name: CowRcStr<'i>,
        parser: &mut CssParser<'i, 't>,
        _after_part: bool,
    ) -> Result<RixPseudoClass, ParseError<'i, Self::Error>> {
        if name.eq_ignore_ascii_case("lang") {
            let langs = parser.parse_comma_separated(|p| {
                let lang = p.expect_ident_or_string()?;
                Ok(CssAtom::new(lang.as_ref()))
            })?;
            return Ok(RixPseudoClass::Lang(langs));
        }
        if name.eq_ignore_ascii_case("dir") {
            let dir = parser.expect_ident()?;
            return Ok(RixPseudoClass::Dir(CssAtom::new(dir.as_ref())));
        }
        if name.eq_ignore_ascii_case("state") {
            let ident = parser.expect_ident()?;
            return Ok(RixPseudoClass::State(CssAtom::new(ident.as_ref())));
        }
        if name.eq_ignore_ascii_case("heading") {
            let levels = parser.parse_comma_separated(|p| Ok(p.expect_integer()?))?;
            return Ok(RixPseudoClass::HeadingList(levels));
        }
        Err(
            parser.new_custom_error(SelectorParseErrorKind::UnsupportedPseudoClassOrElement(
                name,
            )),
        )
    }

    fn parse_pseudo_element(
        &self,
        location: SourceLocation,
        name: CowRcStr<'i>,
    ) -> Result<RixPseudoElement, ParseError<'i, Self::Error>> {
        let lower = name.to_ascii_lowercase();
        let pe = match lower.as_str() {
            "before" => RixPseudoElement::Before,
            "after" => RixPseudoElement::After,
            "marker" => RixPseudoElement::Marker,
            "placeholder" => RixPseudoElement::Placeholder,
            "selection" => RixPseudoElement::Selection,
            "first-line" => RixPseudoElement::FirstLine,
            "first-letter" => RixPseudoElement::FirstLetter,
            "backdrop" => RixPseudoElement::Backdrop,
            "file-selector-button" => RixPseudoElement::FileSelectorButton,
            "details-content" => RixPseudoElement::DetailsContent,
            "cue" => RixPseudoElement::Cue,
            "target-text" => RixPseudoElement::TargetText,
            _ => {
                return Err(location.new_custom_error(
                    SelectorParseErrorKind::UnsupportedPseudoClassOrElement(name),
                ));
            }
        };
        Ok(pe)
    }
}

/// Parses a CSS selector list string into a Servo `SelectorList`.
#[must_use]
pub fn parse_selector_list(input: &str) -> Option<SelectorList<RixSelectorImpl>> {
    parse_selector_list_strict(input).ok()
}

/// Parses a CSS selector list string strictly, returning a human-readable syntax error on failure.
///
/// Used by DOM `querySelector`/`matches`/`closest` and `CSSStyleSheet.insertRule`.
pub fn parse_selector_list_strict(input: &str) -> Result<SelectorList<RixSelectorImpl>, String> {
    parse_selector_list_with_forgiving(input, true)
}

/// Parses a CSS selector list strictly with active `@namespace` bindings.
pub fn parse_selector_list_with_namespaces(
    input: &str,
    namespaces: Option<&SelectorNamespaceMap>,
) -> Result<SelectorList<RixSelectorImpl>, String> {
    parse_selector_list_internal(input, true, namespaces, ParseRelative::No)
}

/// Parses a CSS selector list without forgiving selector-list recovery (for `CSS.supports(selector(...))`).
pub fn parse_selector_list_non_forgiving(
    input: &str,
) -> Result<SelectorList<RixSelectorImpl>, String> {
    parse_selector_list_internal(input, false, None, ParseRelative::No)
}

/// Parses a CSS selector list allowing relative selector syntax (`ForNesting` or `ForScope`).
pub fn parse_selector_list_relative(
    input: &str,
    forgiving: bool,
    namespaces: Option<&SelectorNamespaceMap>,
    relative: ParseRelative,
) -> Result<SelectorList<RixSelectorImpl>, String> {
    parse_selector_list_internal(input, forgiving, namespaces, relative)
}

fn parse_selector_list_with_forgiving(
    input: &str,
    forgiving: bool,
) -> Result<SelectorList<RixSelectorImpl>, String> {
    parse_selector_list_internal(input, forgiving, None, ParseRelative::No)
}

fn parse_selector_list_internal(
    input: &str,
    forgiving: bool,
    namespaces: Option<&SelectorNamespaceMap>,
    relative: ParseRelative,
) -> Result<SelectorList<RixSelectorImpl>, String> {
    if input.trim().is_empty() {
        return Err("Empty CSS selector".to_string());
    }
    let mut parser_input = ParserInput::new(input);
    let mut parser = CssParser::new(&mut parser_input);
    let sel_parser = RixSelectorParser {
        forgiving,
        namespaces,
    };
    let list = SelectorList::parse(&sel_parser, &mut parser, relative)
        .map_err(|e| format!("Invalid CSS selector '{input}': {e:?}"))?;
    if !parser.is_exhausted() {
        return Err(format!("Trailing tokens in CSS selector '{input}'"));
    }
    Ok(list)
}

/// Serializes a compiled Servo `SelectorList` using canonical CSSOM `ToCss` formatting.
#[must_use]
pub fn serialize_selector_list(list: &SelectorList<RixSelectorImpl>) -> String {
    let mut out = String::new();
    let _ = list.to_css(&mut out);
    out
}

/// Parses a pseudo-element argument (e.g. `"::before"`, `":before"`, `"::after"`) for `getComputedStyle`.
#[must_use]
pub fn parse_pseudo_element_str(raw: &str) -> Option<RixPseudoElement> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let name = trimmed
        .strip_prefix("::")
        .or_else(|| trimmed.strip_prefix(':'))?;
    match name.to_ascii_lowercase().as_str() {
        "before" => Some(RixPseudoElement::Before),
        "after" => Some(RixPseudoElement::After),
        "marker" => Some(RixPseudoElement::Marker),
        "placeholder" => Some(RixPseudoElement::Placeholder),
        "selection" => Some(RixPseudoElement::Selection),
        "first-line" => Some(RixPseudoElement::FirstLine),
        "first-letter" => Some(RixPseudoElement::FirstLetter),
        "backdrop" => Some(RixPseudoElement::Backdrop),
        "file-selector-button" => Some(RixPseudoElement::FileSelectorButton),
        "details-content" => Some(RixPseudoElement::DetailsContent),
        "cue" => Some(RixPseudoElement::Cue),
        "target-text" => Some(RixPseudoElement::TargetText),
        _ => None,
    }
}

/// Compiles a `lightningcss` selector list into a Servo `SelectorList` using a reusable buffer.
#[must_use]
pub fn compile_lightning_selector_list(
    selectors: &lightningcss::selector::SelectorList<'_>,
    scratch: &mut String,
) -> Option<SelectorList<RixSelectorImpl>> {
    compile_lightning_selector_list_with_namespaces(selectors, scratch, None)
}

/// Compiles a `lightningcss` selector list with active `@namespace` declarations.
#[must_use]
pub fn compile_lightning_selector_list_with_namespaces(
    selectors: &lightningcss::selector::SelectorList<'_>,
    scratch: &mut String,
    namespaces: Option<&SelectorNamespaceMap>,
) -> Option<SelectorList<RixSelectorImpl>> {
    scratch.clear();
    {
        let mut printer = Printer::new(scratch, PrinterOptions::default());
        if LightningToCss::to_css(selectors, &mut printer).is_err() {
            return None;
        }
    }
    parse_selector_list_with_namespaces(scratch, namespaces).ok()
}

/// Lightweight handle adapting a `rix_dom::Document` element for `selectors::Element`.
#[derive(Clone, Copy)]
pub struct DomElement<'a> {
    pub doc: &'a Document,
    pub node_id: NodeId,
    pub target_pseudo: Option<RixPseudoElement>,
}

impl<'a> DomElement<'a> {
    #[must_use]
    pub const fn new(doc: &'a Document, node_id: NodeId) -> Self {
        Self {
            doc,
            node_id,
            target_pseudo: None,
        }
    }

    #[must_use]
    pub const fn with_pseudo(doc: &'a Document, node_id: NodeId, pseudo: RixPseudoElement) -> Self {
        Self {
            doc,
            node_id,
            target_pseudo: Some(pseudo),
        }
    }

    #[inline]
    fn element_data(&self) -> Option<&'a ElementData> {
        match self.doc.get(self.node_id)? {
            NodeData::Element(data) => Some(data),
            _ => None,
        }
    }

    fn attr_value(&self, name: &str) -> Option<&'a str> {
        let elem = self.element_data()?;
        elem.attributes
            .iter()
            .find(|(q, _)| q.local.eq_str_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl fmt::Debug for DomElement<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DomElement")
            .field("node_id", &self.node_id)
            .field("target_pseudo", &self.target_pseudo)
            .finish()
    }
}

impl<'a> Element for DomElement<'a> {
    type Impl = RixSelectorImpl;

    #[inline]
    fn opaque(&self) -> OpaqueElement {
        match self.doc.get(self.node_id) {
            Some(node) => OpaqueElement::new(node),
            None => OpaqueElement::new(self.doc),
        }
    }

    fn parent_element(&self) -> Option<Self> {
        let parent_id = self.doc.parent(self.node_id)?;
        match self.doc.get(parent_id)? {
            NodeData::Element(_) => Some(Self::new(self.doc, parent_id)),
            _ => None,
        }
    }

    fn parent_node_is_shadow_root(&self) -> bool {
        self.doc
            .parent(self.node_id)
            .and_then(|p| self.doc.get(p))
            .is_some_and(|n| matches!(n, NodeData::ShadowRoot { .. }))
    }

    fn containing_shadow_host(&self) -> Option<Self> {
        for anc in self.doc.ancestors(self.node_id) {
            if let Some(NodeData::ShadowRoot { host, .. }) = self.doc.get(anc) {
                return Some(Self::new(self.doc, *host));
            }
        }
        None
    }

    fn is_pseudo_element(&self) -> bool {
        self.target_pseudo.is_some()
    }

    fn pseudo_element_originating_element(&self) -> Option<Self> {
        if self.target_pseudo.is_some() {
            Some(Self::new(self.doc, self.node_id))
        } else {
            None
        }
    }

    fn prev_sibling_element(&self) -> Option<Self> {
        let parent = self.doc.parent(self.node_id)?;
        let mut prev_elem = None;
        for child in self.doc.children(parent) {
            if child == self.node_id {
                return prev_elem.map(|id| Self::new(self.doc, id));
            }
            if matches!(self.doc.get(child), Some(NodeData::Element(_))) {
                prev_elem = Some(child);
            }
        }
        None
    }

    fn next_sibling_element(&self) -> Option<Self> {
        let parent = self.doc.parent(self.node_id)?;
        let mut seen_self = false;
        for child in self.doc.children(parent) {
            if seen_self {
                if matches!(self.doc.get(child), Some(NodeData::Element(_))) {
                    return Some(Self::new(self.doc, child));
                }
            } else if child == self.node_id {
                seen_self = true;
            }
        }
        None
    }

    fn first_element_child(&self) -> Option<Self> {
        self.doc
            .children(self.node_id)
            .into_iter()
            .find(|&c| matches!(self.doc.get(c), Some(NodeData::Element(_))))
            .map(|id| Self::new(self.doc, id))
    }

    fn is_html_element_in_html_document(&self) -> bool {
        self.element_data()
            .is_some_and(|e| &*e.name.ns == "http://www.w3.org/1999/xhtml")
    }

    #[inline]
    fn has_local_name(&self, local_name: &CssLocalName) -> bool {
        let Some(elem) = self.element_data() else {
            return false;
        };
        if elem.tag_hash != local_name.hash64 {
            return false;
        }
        if self.is_html_element_in_html_document() {
            elem.name
                .local
                .eq_str_ignore_ascii_case(local_name.as_str())
        } else {
            &*elem.name.local == local_name.as_str()
        }
    }

    #[inline]
    fn has_namespace(&self, ns: &CssAtom) -> bool {
        self.element_data()
            .is_some_and(|e| &*e.name.ns == ns.as_str())
    }

    fn is_same_type(&self, other: &Self) -> bool {
        match (self.element_data(), other.element_data()) {
            (Some(a), Some(b)) => a.tag_hash == b.tag_hash && a.name.local == b.name.local,
            _ => false,
        }
    }

    fn attr_matches(
        &self,
        ns: &NamespaceConstraint<&CssAtom>,
        local_name: &CssLocalName,
        operation: &AttrSelectorOperation<&CssAttrValue>,
    ) -> bool {
        let Some(elem) = self.element_data() else {
            return false;
        };
        for (qname, val) in &elem.attributes {
            let ns_ok = match ns {
                NamespaceConstraint::Any => true,
                NamespaceConstraint::Specific(url) => {
                    let qns: &str = &qname.ns;
                    (url.as_str().is_empty() && qns.is_empty()) || qns == url.as_str()
                }
            };
            // Servo `selectors` already selects `local_name_lower` when
            // `is_html_element_in_html_document()` is true, so the element's
            // attribute local name must match `local_name` case-sensitively
            // (e.g. `setAttributeNS(null, "FOO", "bar")` must NOT match `[foo]`).
            if !ns_ok || &*qname.local != local_name.as_str() {
                continue;
            }
            if operation.eval_str(val.as_str()) {
                return true;
            }
        }
        false
    }

    fn match_non_ts_pseudo_class(
        &self,
        pc: &RixPseudoClass,
        _context: &mut MatchingContext<Self::Impl>,
    ) -> bool {
        let Some(elem) = self.element_data() else {
            return false;
        };
        let st = elem.pseudo_state;
        let tag = elem.name.local.as_str();
        match pc {
            RixPseudoClass::Hover => st.contains(ElementState::HOVER),
            RixPseudoClass::Active => st.contains(ElementState::ACTIVE),
            RixPseudoClass::Focus => st.contains(ElementState::FOCUS),
            RixPseudoClass::FocusVisible => st.contains(ElementState::FOCUS_VISIBLE),
            RixPseudoClass::FocusWithin => st.contains(ElementState::FOCUS_WITHIN),
            RixPseudoClass::Disabled => {
                st.contains(ElementState::DISABLED) || self.attr_value("disabled").is_some()
            }
            RixPseudoClass::Enabled => {
                !st.contains(ElementState::DISABLED) && self.attr_value("disabled").is_none()
            }
            RixPseudoClass::Checked => {
                st.contains(ElementState::CHECKED)
                    || self.attr_value("checked").is_some()
                    || (tag.eq_ignore_ascii_case("option") && self.attr_value("selected").is_some())
            }
            RixPseudoClass::Indeterminate => st.contains(ElementState::INDETERMINATE),
            RixPseudoClass::ReadOnly => !self.is_read_write(),
            RixPseudoClass::ReadWrite => self.is_read_write(),
            RixPseudoClass::Link => self.is_link() && !st.contains(ElementState::VISITED),
            RixPseudoClass::Visited => self.is_link() && st.contains(ElementState::VISITED),
            RixPseudoClass::AnyLink => self.is_link(),
            RixPseudoClass::PlaceholderShown => {
                self.attr_value("placeholder").is_some()
                    && self.attr_value("value").unwrap_or("").is_empty()
            }
            RixPseudoClass::Default => {
                self.attr_value("checked").is_some() || self.attr_value("selected").is_some()
            }
            RixPseudoClass::Required => self.attr_value("required").is_some(),
            RixPseudoClass::Optional => self.attr_value("required").is_none(),
            RixPseudoClass::Valid => !st.contains(ElementState::INVALID),
            RixPseudoClass::Invalid => st.contains(ElementState::INVALID),
            RixPseudoClass::UserValid => st.contains(ElementState::USER_VALID),
            RixPseudoClass::UserInvalid => st.contains(ElementState::USER_INVALID),
            RixPseudoClass::InRange => {
                self.attr_value("min").is_some() || self.attr_value("max").is_some()
            }
            RixPseudoClass::OutOfRange => false,
            RixPseudoClass::Defined => !tag.contains('-') || st.contains(ElementState::DEFINED),
            RixPseudoClass::Target => st.contains(ElementState::TARGET),
            RixPseudoClass::Modal => st.contains(ElementState::MODAL),
            RixPseudoClass::PopoverOpen => st.contains(ElementState::POPOVER_OPEN),
            RixPseudoClass::Open => {
                st.contains(ElementState::OPEN)
                    || ((tag.eq_ignore_ascii_case("details") || tag.eq_ignore_ascii_case("dialog"))
                        && self.attr_value("open").is_some())
            }
            RixPseudoClass::Fullscreen => st.contains(ElementState::FULLSCREEN),
            RixPseudoClass::Autofill => st.contains(ElementState::AUTOFILL),
            RixPseudoClass::Blank => self.is_empty(),
            RixPseudoClass::Playing
            | RixPseudoClass::Paused
            | RixPseudoClass::Muted
            | RixPseudoClass::Buffering
            | RixPseudoClass::Stalled
            | RixPseudoClass::Seeking
            | RixPseudoClass::VolumeLocked => false,
            RixPseudoClass::HasSlotted => {
                tag.eq_ignore_ascii_case("slot") && self.has_assigned_slotted_nodes()
            }
            RixPseudoClass::Heading => self.heading_level().is_some(),
            RixPseudoClass::HeadingList(levels) => self
                .heading_level()
                .is_some_and(|lvl| levels.contains(&lvl)),
            RixPseudoClass::Dir(expected) => self.matches_dir(expected.as_str()),
            RixPseudoClass::Lang(expected_list) => expected_list
                .iter()
                .any(|lang| self.matches_lang(lang.as_str())),
            RixPseudoClass::State(_) => false,
        }
    }

    fn match_pseudo_element(
        &self,
        pe: &RixPseudoElement,
        _context: &mut MatchingContext<Self::Impl>,
    ) -> bool {
        self.target_pseudo == Some(*pe)
    }

    fn apply_selector_flags(&self, _flags: ElementSelectorFlags) {}

    fn is_link(&self) -> bool {
        let Some(elem) = self.element_data() else {
            return false;
        };
        let tag: &str = elem.name.local.as_ref();
        (tag.eq_ignore_ascii_case("a") || tag.eq_ignore_ascii_case("area"))
            && self.attr_value("href").is_some()
    }

    fn is_html_slot_element(&self) -> bool {
        self.element_data()
            .is_some_and(|e| e.name.local.eq_str_ignore_ascii_case("slot"))
    }

    fn assigned_slot(&self) -> Option<Self> {
        let parent_id = self.doc.parent(self.node_id)?;
        let Some(NodeData::Element(host)) = self.doc.get(parent_id) else {
            return None;
        };
        let shadow_root = host.shadow_root?;
        let desired_name = self.attr_value("slot").unwrap_or("");
        find_slot_in_subtree(self.doc, shadow_root, desired_name).map(|id| Self::new(self.doc, id))
    }

    fn has_id(&self, id: &CssAtom, case_sensitivity: CaseSensitivity) -> bool {
        let Some(elem) = self.element_data() else {
            return false;
        };
        let Some(ref cached) = elem.cached_id else {
            return false;
        };
        case_sensitivity.eq(cached.as_str().as_bytes(), id.as_str().as_bytes())
    }

    fn has_class(&self, name: &CssAtom, case_sensitivity: CaseSensitivity) -> bool {
        let Some(elem) = self.element_data() else {
            return false;
        };
        match case_sensitivity {
            CaseSensitivity::CaseSensitive => elem.has_class_hash(name.hash64),
            CaseSensitivity::AsciiCaseInsensitive => self.attr_value("class").is_some_and(|cls| {
                cls.split_ascii_whitespace()
                    .any(|token| token.eq_ignore_ascii_case(name.as_str()))
            }),
        }
    }

    fn has_custom_state(&self, _name: &CssAtom) -> bool {
        false
    }

    fn imported_part(&self, _name: &CssAtom) -> Option<CssAtom> {
        None
    }

    fn is_part(&self, name: &CssAtom) -> bool {
        self.attr_value("part")
            .is_some_and(|parts| parts.split_ascii_whitespace().any(|p| p == name.as_str()))
    }

    fn is_empty(&self) -> bool {
        for child in self.doc.children(self.node_id) {
            match self.doc.get(child) {
                Some(NodeData::Element(_)) => return false,
                Some(NodeData::Text(t)) if !t.is_empty() => return false,
                _ => {}
            }
        }
        true
    }

    fn is_root(&self) -> bool {
        self.doc
            .parent(self.node_id)
            .and_then(|p| self.doc.get(p))
            .is_some_and(|n| matches!(n, NodeData::Document))
    }

    fn add_element_unique_hashes(&self, filter: &mut BloomFilter) -> bool {
        let Some(elem) = self.element_data() else {
            return false;
        };
        filter.insert_hash(fold_hash_u32(elem.tag_hash));
        if let Some(ref id) = elem.cached_id {
            filter.insert_hash(fold_hash_u32(hash_ident(id.as_str())));
        }
        for &cls_hash in &elem.class_hashes {
            filter.insert_hash(fold_hash_u32(cls_hash));
        }
        true
    }
}

impl DomElement<'_> {
    fn is_read_write(&self) -> bool {
        let Some(elem) = self.element_data() else {
            return false;
        };
        if let Some(ce) = self.attr_value("contenteditable") {
            if ce.is_empty()
                || ce.eq_ignore_ascii_case("true")
                || ce.eq_ignore_ascii_case("plaintext-only")
            {
                return true;
            }
        }
        let tag = elem.name.local.as_str();
        if tag.eq_ignore_ascii_case("textarea") || tag.eq_ignore_ascii_case("input") {
            return self.attr_value("readonly").is_none() && self.attr_value("disabled").is_none();
        }
        false
    }

    fn heading_level(&self) -> Option<i32> {
        let tag = self.element_data()?.name.local.as_str();
        match tag.to_ascii_lowercase().as_str() {
            "h1" => Some(1),
            "h2" => Some(2),
            "h3" => Some(3),
            "h4" => Some(4),
            "h5" => Some(5),
            "h6" => Some(6),
            _ => None,
        }
    }

    fn has_assigned_slotted_nodes(&self) -> bool {
        // Walk up to ShadowRoot host and check if any host light-DOM child assigns to this slot.
        let slot_name = self.attr_value("name").unwrap_or("");
        for anc in self.doc.ancestors(self.node_id) {
            if let Some(NodeData::ShadowRoot { host, .. }) = self.doc.get(anc) {
                for child in self.doc.children(*host) {
                    match self.doc.get(child) {
                        Some(NodeData::Element(e)) => {
                            let target = e.get_attribute("slot").unwrap_or("");
                            if target == slot_name {
                                return true;
                            }
                        }
                        Some(NodeData::Text(t)) if !t.is_empty() && slot_name.is_empty() => {
                            return true;
                        }
                        _ => {}
                    }
                }
                return false;
            }
        }
        false
    }

    /// HTML Living Standard §3.2.6.4 directionality resolution for `:dir(ltr)` / `:dir(rtl)`.
    fn resolved_directionality(&self) -> &'static str {
        let mut current = Some(self.node_id);
        while let Some(id) = current {
            let elem = DomElement::new(self.doc, id);
            if let Some(data) = elem.element_data() {
                let tag = data.name.local.as_str();
                if let Some(dir) = elem.attr_value("dir") {
                    if dir.eq_ignore_ascii_case("ltr") {
                        return "ltr";
                    }
                    if dir.eq_ignore_ascii_case("rtl") {
                        return "rtl";
                    }
                    if dir.eq_ignore_ascii_case("auto") {
                        return elem.auto_directionality();
                    }
                }
                if tag.eq_ignore_ascii_case("bdi") {
                    return elem.auto_directionality();
                }
                if tag.eq_ignore_ascii_case("input")
                    && elem
                        .attr_value("type")
                        .is_some_and(|t| t.eq_ignore_ascii_case("tel"))
                {
                    return "ltr";
                }
            }
            current = self.doc.parent(id);
        }
        "ltr"
    }

    fn auto_directionality(&self) -> &'static str {
        // Check `value` attribute on `<input>` / `<textarea>`, otherwise inspect text descendants
        // (skipping `<script>`, `<style>`, `<textarea>`, `<bdi>`, and elements with valid `dir`).
        if let Some(data) = self.element_data() {
            let tag = data.name.local.as_str();
            if tag.eq_ignore_ascii_case("input") {
                if let Some(val) = self.attr_value("value") {
                    if let Some(d) = first_strong_bidi_dir(val) {
                        return d;
                    }
                }
                return "ltr";
            }
        }
        if let Some(d) = find_first_strong_dir_in_subtree(self.doc, self.node_id, true) {
            return d;
        }
        "ltr"
    }

    fn matches_dir(&self, expected: &str) -> bool {
        if !expected.eq_ignore_ascii_case("ltr") && !expected.eq_ignore_ascii_case("rtl") {
            return false;
        }
        self.resolved_directionality()
            .eq_ignore_ascii_case(expected)
    }

    fn matches_lang(&self, expected: &str) -> bool {
        let mut current = Some(self.node_id);
        while let Some(id) = current {
            let elem = DomElement::new(self.doc, id);
            if let Some(lang) = elem.attr_value("lang") {
                return lang_matches_extended_range(lang, expected);
            }
            current = self.doc.parent(id);
        }
        false
    }
}

fn first_strong_bidi_dir(text: &str) -> Option<&'static str> {
    for ch in text.chars() {
        let cp = ch as u32;
        // Hebrew, Arabic, Syriac, Thaana, NKo, Samaritan, Mandaic, Arabic Presentation Forms
        if (0x0590..=0x08FF).contains(&cp)
            || (0xFB1D..=0xFDFF).contains(&cp)
            || (0xFE70..=0xFEFF).contains(&cp)
            || (0x10800..=0x10FFF).contains(&cp)
            || (0x1E800..=0x1EFFF).contains(&cp)
        {
            return Some("rtl");
        }
        if ch.is_alphabetic() {
            return Some("ltr");
        }
    }
    None
}

fn find_first_strong_dir_in_subtree(
    doc: &Document,
    node: NodeId,
    is_root: bool,
) -> Option<&'static str> {
    for child in doc.children(node) {
        match doc.get(child) {
            Some(NodeData::Text(t)) => {
                if let Some(d) = first_strong_bidi_dir(t) {
                    return Some(d);
                }
            }
            Some(NodeData::Element(e)) => {
                let tag = e.name.local.as_str();
                if tag.eq_ignore_ascii_case("script")
                    || tag.eq_ignore_ascii_case("style")
                    || tag.eq_ignore_ascii_case("textarea")
                    || tag.eq_ignore_ascii_case("bdi")
                {
                    continue;
                }
                if let Some(dir) = e.get_attribute("dir") {
                    if dir.eq_ignore_ascii_case("ltr")
                        || dir.eq_ignore_ascii_case("rtl")
                        || dir.eq_ignore_ascii_case("auto")
                    {
                        continue;
                    }
                }
                if let Some(d) = find_first_strong_dir_in_subtree(doc, child, false) {
                    return Some(d);
                }
            }
            _ => {}
        }
    }
    let _ = is_root;
    None
}

/// RFC 4647 §3.3.2 Extended Filtering for CSS Selectors Level 4 `:lang()`.
fn lang_matches_extended_range(tag: &str, range: &str) -> bool {
    let range = range.trim();
    if range == "*" {
        return !tag.trim().is_empty();
    }
    let tag_subs: Vec<&str> = tag.trim().split('-').filter(|s| !s.is_empty()).collect();
    let range_subs: Vec<&str> = range.split('-').filter(|s| !s.is_empty()).collect();
    if tag_subs.is_empty() || range_subs.is_empty() {
        return false;
    }
    let mut ti = 0usize;
    let mut ri = 0usize;
    // First subtag must match (unless wildcard `*`).
    if range_subs[0] != "*" && !tag_subs[0].eq_ignore_ascii_case(range_subs[0]) {
        return false;
    }
    ti += 1;
    ri += 1;
    while ri < range_subs.len() {
        if range_subs[ri] == "*" {
            ri += 1;
            continue;
        }
        if ti >= tag_subs.len() {
            return false;
        }
        if tag_subs[ti].eq_ignore_ascii_case(range_subs[ri]) {
            ti += 1;
            ri += 1;
        } else if tag_subs[ti].len() == 1 {
            // Singleton subtag blocks skipping further.
            return false;
        } else {
            ti += 1;
        }
    }
    true
}

fn find_slot_in_subtree(doc: &Document, root: NodeId, slot_name: &str) -> Option<NodeId> {
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if let Some(NodeData::Element(el)) = doc.get(id) {
            if el.name.local.eq_str_ignore_ascii_case("slot") {
                let name = el
                    .attributes
                    .iter()
                    .find(|(q, _)| q.local.eq_str_ignore_ascii_case("name"))
                    .map(|(_, v)| v.as_str())
                    .unwrap_or("");
                if name == slot_name {
                    return Some(id);
                }
            }
        }
        let children: Vec<NodeId> = doc.children(id).collect();
        for child in children.into_iter().rev() {
            stack.push(child);
        }
    }
    None
}

/// Convenience helper for matching a parsed `SelectorList` against a `DomElement`.
#[must_use]
pub fn matches_selector_list_for_element(
    list: &SelectorList<RixSelectorImpl>,
    element: &DomElement<'_>,
    bloom: Option<&BloomFilter>,
    caches: &mut SelectorCaches,
    shadow_host: Option<OpaqueElement>,
) -> bool {
    matches_selector_list_with_scope(list, element, None, bloom, caches, shadow_host)
}

/// Matches a parsed `SelectorList` against `element` with an optional `:scope` element.
#[must_use]
pub fn matches_selector_list_with_scope(
    list: &SelectorList<RixSelectorImpl>,
    element: &DomElement<'_>,
    scope: Option<OpaqueElement>,
    bloom: Option<&BloomFilter>,
    caches: &mut SelectorCaches,
    shadow_host: Option<OpaqueElement>,
) -> bool {
    let mut ctx = MatchingContext::new(
        MatchingMode::Normal,
        bloom,
        caches,
        QuirksMode::NoQuirks,
        NeedsSelectorFlags::No,
        MatchingForInvalidation::No,
    );
    ctx.current_host = shadow_host;
    ctx.scope_element = scope;
    list.slice()
        .iter()
        .any(|sel| matches_selector(sel, 0, None, element, &mut ctx))
}

/// Tests whether a single `node` matches `list` with an optional `:scope` element.
#[must_use]
pub fn matches_element_with_scope(
    doc: &Document,
    node: NodeId,
    list: &SelectorList<RixSelectorImpl>,
    scope_node: Option<NodeId>,
) -> bool {
    let el = DomElement::new(doc, node);
    let scope_opaque = scope_node.map(|n| DomElement::new(doc, n).opaque());
    let mut caches = SelectorCaches::default();
    matches_selector_list_with_scope(list, &el, scope_opaque, None, &mut caches, None)
}

/// Finds the first descendant element of `root` matching `list` (Selectores Level 4).
#[must_use]
pub fn query_selector_in_subtree(
    doc: &Document,
    root: NodeId,
    list: &SelectorList<RixSelectorImpl>,
    scope_node: Option<NodeId>,
    shadow_host: Option<NodeId>,
) -> Option<NodeId> {
    let scope_opaque = scope_node.map(|n| DomElement::new(doc, n).opaque());
    let host_opaque = shadow_host.map(|n| DomElement::new(doc, n).opaque());
    let mut caches = SelectorCaches::default();
    for node in doc.descendants(root) {
        if node == root {
            continue;
        }
        if !matches!(doc.get(node), Some(NodeData::Element(_))) {
            continue;
        }
        let el = DomElement::new(doc, node);
        if matches_selector_list_with_scope(list, &el, scope_opaque, None, &mut caches, host_opaque)
        {
            return Some(node);
        }
    }
    None
}

/// Finds all descendant elements of `root` matching `list` in tree order (Selectors Level 4).
#[must_use]
pub fn query_selector_all_in_subtree(
    doc: &Document,
    root: NodeId,
    list: &SelectorList<RixSelectorImpl>,
    scope_node: Option<NodeId>,
    shadow_host: Option<NodeId>,
) -> Vec<NodeId> {
    let scope_opaque = scope_node.map(|n| DomElement::new(doc, n).opaque());
    let host_opaque = shadow_host.map(|n| DomElement::new(doc, n).opaque());
    let mut caches = SelectorCaches::default();
    let mut out = Vec::new();
    for node in doc.descendants(root) {
        if node == root {
            continue;
        }
        if !matches!(doc.get(node), Some(NodeData::Element(_))) {
            continue;
        }
        let el = DomElement::new(doc, node);
        if matches_selector_list_with_scope(list, &el, scope_opaque, None, &mut caches, host_opaque)
        {
            out.push(node);
        }
    }
    out
}

/// Convenience helper for matching a single compiled `Selector` against a `DomElement`.
#[must_use]
pub fn matches_single_selector(
    selector: &Selector<RixSelectorImpl>,
    element: &DomElement<'_>,
    bloom: Option<&BloomFilter>,
    caches: &mut SelectorCaches,
    shadow_host: Option<OpaqueElement>,
) -> bool {
    matches_single_selector_with_scope(selector, element, None, bloom, caches, shadow_host)
}

/// Matches a single compiled `Selector` against a `DomElement` with an optional `:scope` root.
#[must_use]
pub fn matches_single_selector_with_scope(
    selector: &Selector<RixSelectorImpl>,
    element: &DomElement<'_>,
    scope: Option<OpaqueElement>,
    bloom: Option<&BloomFilter>,
    caches: &mut SelectorCaches,
    shadow_host: Option<OpaqueElement>,
) -> bool {
    if let Some(target_pseudo) = element.target_pseudo {
        if !selector.has_pseudo_element() {
            return false;
        }
        let originating = DomElement::new(element.doc, element.node_id);
        let matcher = |pe: &RixPseudoElement| *pe == target_pseudo;
        let mut ctx = MatchingContext::new(
            MatchingMode::ForStatelessPseudoElement,
            bloom,
            caches,
            QuirksMode::NoQuirks,
            NeedsSelectorFlags::No,
            MatchingForInvalidation::No,
        );
        ctx.current_host = shadow_host;
        ctx.scope_element = scope;
        ctx.pseudo_element_matching_fn = Some(&matcher);
        return matches_selector(selector, 0, None, &originating, &mut ctx);
    }

    let mut ctx = MatchingContext::new(
        MatchingMode::Normal,
        bloom,
        caches,
        QuirksMode::NoQuirks,
        NeedsSelectorFlags::No,
        MatchingForInvalidation::No,
    );
    ctx.current_host = shadow_host;
    ctx.scope_element = scope;
    matches_selector(selector, 0, None, element, &mut ctx)
}

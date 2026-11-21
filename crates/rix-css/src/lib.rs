#![deny(unsafe_code)]
//! Typed CSS stylesheet compiler, Selectors Level 4 integration, and compact
//! `Arc`-grouped `ComputedStyle` for RixBrowser.

pub mod computed;
pub mod properties;
pub mod selector;
pub mod stylesheet;
pub mod values;

pub use computed::{
    apply_declaration, resolve_element_style, resolve_var_tokens, BoxModelStyle, ComputedStyle,
    CustomPropertyMap, EffectsStyle, FlexGridStyle, InheritedTextStyle, VisualStyle,
};
pub use properties::{
    compile_declaration_block, parse_inline_style, parse_property_from_tokens, CssToken,
    GlobalKeyword, MissingCssFeature, MissingCssKind, PropertyIdKind, PropertyValue,
    SpecifiedDeclaration,
};
pub use selector::{
    compile_lightning_selector_list, matches_selector_list_for_element, matches_single_selector,
    parse_selector_list, CssAtom, CssLocalName, DomElement, RixPseudoClass, RixPseudoElement,
    RixSelectorImpl, RixSelectorParser,
};
pub use stylesheet::{
    evaluate_supports, CascadeOrigin, CompiledRule, FontFaceRule, FontFaceSource, KeyframeStep,
    KeyframesRule, LayerId, MediaContext, MediaTypeKind, Stylesheet,
};
pub use values::{
    AlignValue, BackgroundLayerValue, BorderEdge, BoxShadowValue, BoxSizingMode, CalcExpr,
    ClearMode, ColorRgba, CornerRadii, CornerRadius, CssColor, CursorKind, DirectionMode,
    DisplayMode, EdgeValues, EvalContext, FilterOp, FlexDirectionMode, FlexWrapMode, FloatMode,
    FontStyleMode, GridAutoFlowMode, GridTrackSize, LengthPercentage, LengthPercentageAuto,
    LengthUnit, LineHeightValue, LineStyle, OverflowMode, OverflowPair, OverflowWrapMode,
    PointerEventsMode, PositionMode, SizeValue, TextAlignMode, TextDecorationLine,
    TextTransformMode, TransformOp, UserSelectMode, VisibilityMode, WhiteSpaceMode, WordBreakMode,
    ZIndexValue,
};

#![deny(unsafe_code)]
//! Typed CSS values and Selectors Level 4 integration for RixBrowser.

pub mod properties;
pub mod selector;
pub mod values;

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

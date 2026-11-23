#![deny(unsafe_code)]
//! Typed CSS stylesheet compiler, Selectors Level 4 integration, and compact
//! `Arc`-grouped `ComputedStyle` for RixBrowser.

pub mod cascade;
pub mod computed;
pub mod properties;
pub mod rule_index;
pub mod selector;
pub mod stylesheet;
pub mod ua;
pub mod values;

pub use cascade::{RestyleReport, StyleContext, StyleMemo};
pub use computed::{
    apply_declaration, resolve_element_style, resolve_var_tokens, BoxModelStyle, ComputedStyle,
    CustomPropertyMap, EffectsStyle, FlexGridStyle, InheritedTextStyle, VisualStyle,
};
pub use properties::{
    compile_declaration_block, parse_and_serialize_declaration, parse_inline_style,
    parse_property_from_tokens, serialize_css_tokens, CssToken, GlobalKeyword, MissingCssFeature,
    MissingCssKind, PropertyIdKind, PropertyValue, SpecifiedDeclaration,
};
pub use rule_index::{InvalidationMap, InvalidationScope, RuleBucketEntry, RuleIndex};
pub use selector::{
    compile_lightning_selector_list, matches_element_with_scope, matches_selector_list_for_element,
    matches_selector_list_with_scope, matches_single_selector, parse_pseudo_element_str,
    parse_selector_list, parse_selector_list_strict, query_selector_all_in_subtree,
    query_selector_in_subtree, serialize_selector_list, CssAtom, CssLocalName, DomElement,
    RixPseudoClass, RixPseudoElement, RixSelectorImpl, RixSelectorParser,
};
pub use stylesheet::{
    evaluate_supports, evaluate_supports_condition_str, parse_cssom_rules, parse_single_cssom_rule,
    CascadeOrigin, CompiledRule, CssomRuleKind, FontFaceRule, FontFaceSource, KeyframeStep,
    KeyframesRule, LayerId, MediaContext, MediaTypeKind, Stylesheet,
};
pub use ua::{default_ua_stylesheet, UA_CSS};
pub use values::{
    AlignValue, AnimationDirectionMode, AnimationFillModeKind, AnimationPlayStateMode,
    AnimationSpec, BackgroundImageValue, BackgroundLayerValue, BorderCollapseMode, BorderEdge,
    BoxShadowValue, BoxSizingMode, CalcExpr, ClearMode, ClipPathValue, ClipRadius, ColorRgba,
    CornerRadii, CornerRadius, CssColor, CssGradientStop, CursorKind, DirectionMode, DisplayMode,
    EdgeValues, EvalContext, FilterOp, FlexDirectionMode, FlexWrapMode, FloatMode, FontStyleMode,
    GridAutoFlowMode, GridPlacementValue, GridTemplateAreasValue, GridTrackSize, LengthPercentage,
    LengthPercentageAuto, LengthUnit, LineHeightValue, LineStyle, MixBlendMode, OverflowMode,
    OverflowPair, OverflowWrapMode, PointerEventsMode, PositionMode, SizeValue, TableLayoutMode,
    TextAlignMode, TextDecorationLine, TextOverflowMode, TextTransformMode, TimingFunction,
    TransformOp, TransformOrigin, TransitionSpec, UserSelectMode, VerticalAlignMode,
    VisibilityMode, WhiteSpaceMode, WordBreakMode, ZIndexValue,
};

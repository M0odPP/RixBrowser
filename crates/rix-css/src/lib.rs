#![deny(unsafe_code)]
//! Typed CSS stylesheet compiler, Selectors Level 4 integration, and compact
//! `Arc`-grouped `ComputedStyle` for RixBrowser.

pub mod anim_css;
pub mod animations;
pub mod cascade;
pub mod color;
pub mod computed;
pub mod cssom_decl;
pub mod grid_css;
pub mod properties;
pub mod rule_index;
pub mod selector;
pub mod stylesheet;
pub mod substitution;
pub mod text_css;
pub mod ua;
pub mod values;
pub mod values_math;

pub use anim_css::{
    AnimationRangePoint, TextDecorationInsetValue, TextShadowSpec, TimelineRangeName,
};
pub use animations::{
    compute_effect_timing, compute_transition_eased_progress_local, ActiveTransition,
    AnimationStateSnapshot, ComputedEffectTiming, EffectPhase, EffectTimingInput,
    TransitionControlOverride, WebAnimationKeyframe, WebAnimationSpec,
};
pub use cascade::{RestyleReport, StyleContext, StyleMemo};
pub use color::{
    format_legacy_rgba, parse_and_serialize_color_specified, parse_css_color, ColorSpace,
    CssColorExpr, HueMethod, ResolvedColor, SystemColor,
};
pub use computed::{
    apply_declaration, resolve_element_style, resolve_element_style_full,
    resolve_element_style_scoped, resolve_var_tokens, BoxModelStyle, ComputedStyle,
    CustomPropertyMap, EffectsStyle, FlexGridStyle, InheritedTextStyle, MatchedRule, VisualStyle,
};
pub use cssom_decl::{
    parse_and_expand_declaration, parse_and_serialize_declaration, strip_outer_css_comments,
};
pub use grid_css::{
    FitToleranceValue, GridLanesDirectionMode, GridLanesPackMode, GridRepeatCount,
    GridSubgridLineItem, GridTemplateTrackList, GridTrackListEntry,
};
pub use properties::{
    compile_declaration_block, is_logical_property, is_sizing_property, parse_inline_style,
    parse_property_from_tokens, parse_single_timing_function, resolve_logical_to_physical,
    serialize_css_tokens, tokenize_custom_value_str, CssToken, GlobalKeyword, MissingCssFeature,
    MissingCssKind, PropertyIdKind, PropertyValue, SpecifiedDeclaration, ALL_RESETTABLE_LONGHANDS,
    COMPUTED_STYLE_LONGHANDS,
};
pub use rule_index::{InvalidationMap, InvalidationScope, RuleBucketEntry, RuleIndex};
pub use selector::{
    compile_lightning_selector_list, compile_lightning_selector_list_with_namespaces,
    matches_element_with_scope, matches_selector_list_for_element,
    matches_selector_list_with_scope, matches_single_selector, matches_single_selector_with_scope,
    parse_pseudo_element_str, parse_selector_list, parse_selector_list_non_forgiving,
    parse_selector_list_relative, parse_selector_list_strict, parse_selector_list_with_namespaces,
    query_selector_all_in_subtree, query_selector_in_subtree, serialize_selector_list, CssAtom,
    CssAttrValue, CssLocalName, DomElement, RixPseudoClass, RixPseudoElement, RixSelectorImpl,
    RixSelectorParser, SelectorNamespaceMap,
};
pub use stylesheet::{
    evaluate_supports, evaluate_supports_condition_str, normalize_nested_selector_text,
    parse_and_serialize_keyframe_selector, parse_and_serialize_page_selector,
    parse_and_serialize_selector_for_context, parse_cssom_rules, parse_single_cssom_rule,
    parse_single_cssom_rule_with_context, parse_single_cssom_rule_with_namespaces,
    parse_single_keyframe_rule, validate_and_serialize_page_descriptor, CascadeOrigin,
    CompiledRule, CompiledScope, CssomNestingContext, CssomParseError, CssomRuleKind, FontFaceRule,
    FontFaceSource, KeyframeStep, KeyframesRule, LayerId, LayerSeg, MediaContext, MediaTypeKind,
    RegisteredProperty, Stylesheet,
};
pub use text_css::{
    decompose_legacy_white_space, derive_white_space_mode, HangingPunctuation, HyphenateLimitChars,
    HyphensMode, LineBreakMode, TextAutospaceValue, TextFitMethod, TextFitValue,
    TextGroupAlignMode, TextSpacingTrimMode, TextTransformFlags, TextWrapMode, TextWrapStyle,
    WhiteSpaceCollapseMode, WhiteSpaceTrim, WordSpaceTransformMode, WrapInsideMode,
};
pub use ua::{default_ua_stylesheet, UA_CSS};
pub use values::{
    AlignValue, AnimationCompositionMode, AnimationDirectionMode, AnimationFillModeKind,
    AnimationPlayStateMode, AnimationSpec, BackgroundImageValue, BackgroundLayerValue,
    BorderCollapseMode, BorderEdge, BoxShadowValue, BoxSizingMode, CalcExpr, CaptionSideMode,
    ClearMode, ClipPathValue, ClipRadius, ColorInterpolationFiltersMode, ColorRgba,
    ContentVisibilityMode, CornerRadii, CornerRadius, CssColor, CssGradientStop, CursorKind,
    DirectionMode, DisplayMode, EdgeValues, EmptyCellsMode, EvalContext, FilterOp, FilterScalar,
    FilterSpecOp, FlexDirectionMode, FlexWrapMode, FloatMode, FontStyleMode, GridAutoFlowMode,
    GridPlacementValue, GridTemplateAreasValue, GridTrackSize, InterpolateSizeMode,
    LengthPercentage, LengthPercentageAuto, LengthUnit, LineHeightValue, LineStyle,
    ListStylePositionMode, ListStyleTypeMode, MixBlendMode, OverflowMode, OverflowPair,
    OverflowWrapMode, OverlayMode, OverscrollBehaviorMode, PointerEventsMode, PositionMode,
    ReadingFlowMode, ResizeMode, ScrollBehaviorMode, SizeValue, StepPositionMode, TableLayoutMode,
    TextAlignLastMode, TextAlignMode, TextDecorationLine, TextJustifyMode, TextOrientationMode,
    TextOverflowMode, TextTransformMode, TimingFunction, TransformOp, TransformOrigin,
    TransitionBehaviorMode, TransitionSpec, UserSelectMode, VerticalAlignMode, VisibilityMode,
    WhiteSpaceMode, WordBreakMode, WritingMode, ZIndexValue,
};

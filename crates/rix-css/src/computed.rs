//! Compact `Arc`-grouped `ComputedStyle`, CSS variable substitution, and
//! Cascade Level 5 property resolution.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use rix_core::InternedStr;
use smallvec::SmallVec;

use crate::properties::{
    is_logical_property, resolve_logical_to_physical, serialize_css_tokens, CssToken,
    GlobalKeyword, PropertyIdKind, PropertyValue, SpecifiedDeclaration,
};
use crate::stylesheet::{CascadeOrigin, CompiledRule, LayerId, MediaContext, RegisteredProperty};
use crate::values::{
    AlignValue, AnimationCompositionMode, AnimationDirectionMode, AnimationFillModeKind,
    AnimationPlayStateMode, AnimationSpec, BackgroundLayerValue, BorderCollapseMode,
    BoxShadowValue, BoxSizingMode, CaptionSideMode, ClearMode, ClipPathValue, ColorRgba,
    CornerRadii, CssColor, CursorKind, DirectionMode, DisplayMode, EdgeValues, EmptyCellsMode,
    EvalContext, FilterOp, FlexDirectionMode, FlexWrapMode, FloatMode, FontStyleMode,
    GridAutoFlowMode, GridPlacementValue, GridTemplateAreasValue, GridTrackSize,
    InterpolateSizeMode, LengthPercentage, LengthPercentageAuto, LineHeightValue, LineStyle,
    MixBlendMode, OverflowMode, OverflowPair, OverflowWrapMode, OverlayMode,
    OverscrollBehaviorMode, PointerEventsMode, PositionMode, ReadingFlowMode, ResizeMode,
    SizeValue, TableLayoutMode, TextAlignLastMode, TextAlignMode, TextDecorationLine,
    TextJustifyMode, TextOrientationMode, TextOverflowMode, TextTransformMode, TimingFunction,
    TransformOp, TransformOrigin, TransitionBehaviorMode, TransitionSpec, UserSelectMode,
    VerticalAlignMode, VisibilityMode, WhiteSpaceMode, WordBreakMode, WritingMode, ZIndexValue,
};

const MAX_VAR_DEPTH: u8 = 64;
const MAX_VAR_EXPANDED_TOKENS: usize = 8_192;
const MAX_VAR_EXPANDED_BYTES: usize = 65_536;

/// Non-inherited box-model geometry shared via copy-on-write `Arc`.
#[derive(Debug, Clone, PartialEq)]
pub struct BoxModelStyle {
    pub width: SizeValue,
    pub height: SizeValue,
    pub min_width: SizeValue,
    pub min_height: SizeValue,
    pub max_width: SizeValue,
    pub max_height: SizeValue,
    pub margin: EdgeValues<LengthPercentageAuto>,
    pub padding: EdgeValues<LengthPercentage>,
    pub inset: EdgeValues<LengthPercentageAuto>,
    pub border_width: EdgeValues<f32>,
    pub vertical_align: VerticalAlignMode,
    pub text_overflow: TextOverflowMode,
    pub table_layout: TableLayoutMode,
    pub content: Option<InternedStr>,
    pub column_count: Option<i32>,
}

impl Default for BoxModelStyle {
    fn default() -> Self {
        Self {
            width: SizeValue::Auto,
            height: SizeValue::Auto,
            min_width: SizeValue::Auto,
            min_height: SizeValue::Auto,
            max_width: SizeValue::None,
            max_height: SizeValue::None,
            margin: EdgeValues::all(LengthPercentageAuto::Value(LengthPercentage::Zero)),
            padding: EdgeValues::all(LengthPercentage::Zero),
            inset: EdgeValues::all(LengthPercentageAuto::Auto),
            border_width: EdgeValues::all(0.0),
            vertical_align: VerticalAlignMode::Baseline,
            text_overflow: TextOverflowMode::Clip,
            table_layout: TableLayoutMode::Auto,
            content: None,
            column_count: None,
        }
    }
}

/// Non-inherited visual/paint properties shared via copy-on-write `Arc`.
#[derive(Debug, Clone, PartialEq)]
pub struct VisualStyle {
    pub background_color: ColorRgba,
    pub background_color_computed: Option<Arc<crate::color::CssColorExpr>>,
    pub background_layers: Vec<BackgroundLayerValue>,
    pub background_image_raw: Option<String>,
    pub background_position: (LengthPercentage, LengthPercentage),
    pub object_position: Option<Box<crate::values_math::PositionSpec>>,
    pub border_style: EdgeValues<LineStyle>,
    pub border_color: EdgeValues<CssColor>,
    pub border_color_computed: [Option<Arc<crate::color::CssColorExpr>>; 4],
    pub border_radius: CornerRadii,
    pub outline_color: CssColor,
    pub outline_color_computed: Option<Arc<crate::color::CssColorExpr>>,
    pub outline_style: LineStyle,
    pub outline_width: f32,
    pub outline_offset: LengthPercentage,
    pub text_decoration_line: TextDecorationLine,
    pub text_decoration_color: Option<CssColor>,
    pub text_decoration_color_computed: Option<Arc<crate::color::CssColorExpr>>,
    pub user_select: UserSelectMode,
    pub resize: ResizeMode,
    pub overscroll_behavior_x: OverscrollBehaviorMode,
    pub overscroll_behavior_y: OverscrollBehaviorMode,
}

impl Default for VisualStyle {
    fn default() -> Self {
        Self {
            background_color: ColorRgba::TRANSPARENT,
            background_color_computed: None,
            background_layers: Vec::new(),
            background_image_raw: None,
            background_position: (
                LengthPercentage::Percent(0.0),
                LengthPercentage::Percent(0.0),
            ),
            object_position: None,
            border_style: EdgeValues::all(LineStyle::None),
            border_color: EdgeValues::all(CssColor::CurrentColor),
            border_color_computed: [None, None, None, None],
            border_radius: CornerRadii::default(),
            outline_color: CssColor::CurrentColor,
            outline_color_computed: None,
            outline_style: LineStyle::None,
            outline_width: 0.0,
            outline_offset: LengthPercentage::Zero,
            text_decoration_line: TextDecorationLine::NONE,
            text_decoration_color: None,
            text_decoration_color_computed: None,
            user_select: UserSelectMode::Auto,
            resize: ResizeMode::None,
            overscroll_behavior_x: OverscrollBehaviorMode::Auto,
            overscroll_behavior_y: OverscrollBehaviorMode::Auto,
        }
    }
}

/// Inherited typography and text properties shared across subtrees via `Arc`.
#[derive(Debug, Clone, PartialEq)]
pub struct InheritedTextStyle {
    pub color: ColorRgba,
    pub color_computed: Option<Arc<crate::color::CssColorExpr>>,
    /// CSS Color Adjustment L1 `color-scheme`: 0=normal, 1=light, 2=dark, 3=light dark.
    pub color_scheme_mode: u8,
    pub font_size_px: f32,
    pub font_weight: f32,
    pub font_style: FontStyleMode,
    pub font_family: SmallVec<[InternedStr; 4]>,
    pub line_height: LineHeightValue,
    pub letter_spacing: LengthPercentage,
    pub word_spacing: LengthPercentage,
    pub text_indent: LengthPercentage,
    pub text_align: TextAlignMode,
    pub text_transform: TextTransformMode,
    pub white_space: WhiteSpaceMode,
    pub word_break: WordBreakMode,
    pub overflow_wrap: OverflowWrapMode,
    pub direction: DirectionMode,
    pub writing_mode: WritingMode,
    pub text_orientation: TextOrientationMode,
    pub caption_side: CaptionSideMode,
    pub border_collapse: BorderCollapseMode,
    pub border_spacing_x: LengthPercentage,
    pub border_spacing_y: LengthPercentage,
    pub empty_cells: EmptyCellsMode,
    pub orphans: i32,
    pub widows: i32,
    pub tab_size_num: f32,
    pub tab_size_len: Option<LengthPercentage>,
    pub text_justify: TextJustifyMode,
    pub text_align_last: TextAlignLastMode,
    pub hyphenate_character: Option<InternedStr>,
    pub hyphenate_limit_lines: Option<i32>,
    pub interpolate_size: InterpolateSizeMode,
    /// Full CSS Values L4/L5 evaluation context snapshot for this element (`em`/`rem`/`lh`/`rlh`/viewport/container/sibling).
    pub eval_ctx: EvalContext,
}

impl Default for InheritedTextStyle {
    fn default() -> Self {
        Self {
            color: ColorRgba::BLACK,
            color_computed: None,
            color_scheme_mode: 0,
            font_size_px: 16.0,
            font_weight: 400.0,
            font_style: FontStyleMode::Normal,
            font_family: SmallVec::new(),
            line_height: LineHeightValue::Normal,
            letter_spacing: LengthPercentage::Zero,
            word_spacing: LengthPercentage::Zero,
            text_indent: LengthPercentage::Zero,
            text_align: TextAlignMode::Start,
            text_transform: TextTransformMode::None,
            white_space: WhiteSpaceMode::Normal,
            word_break: WordBreakMode::Normal,
            overflow_wrap: OverflowWrapMode::Normal,
            direction: DirectionMode::Ltr,
            writing_mode: WritingMode::HorizontalTb,
            text_orientation: TextOrientationMode::Mixed,
            caption_side: CaptionSideMode::Top,
            border_collapse: BorderCollapseMode::Separate,
            border_spacing_x: LengthPercentage::Zero,
            border_spacing_y: LengthPercentage::Zero,
            empty_cells: EmptyCellsMode::Show,
            orphans: 2,
            widows: 2,
            tab_size_num: 8.0,
            tab_size_len: None,
            text_justify: TextJustifyMode::Auto,
            text_align_last: TextAlignLastMode::Auto,
            hyphenate_character: None,
            hyphenate_limit_lines: None,
            interpolate_size: InterpolateSizeMode::NumericOnly,
            eval_ctx: EvalContext::default(),
        }
    }
}

impl InheritedTextStyle {
    /// Compares only layout-affecting typography fields (excluding `color`).
    #[must_use]
    pub fn layout_eq(&self, other: &Self) -> bool {
        self.font_size_px == other.font_size_px
            && self.font_weight == other.font_weight
            && self.font_style == other.font_style
            && self.font_family == other.font_family
            && self.line_height == other.line_height
            && self.letter_spacing == other.letter_spacing
            && self.word_spacing == other.word_spacing
            && self.text_indent == other.text_indent
            && self.text_align == other.text_align
            && self.text_transform == other.text_transform
            && self.white_space == other.white_space
            && self.word_break == other.word_break
            && self.overflow_wrap == other.overflow_wrap
            && self.direction == other.direction
            && self.writing_mode == other.writing_mode
            && self.text_orientation == other.text_orientation
            && self.caption_side == other.caption_side
            && self.border_collapse == other.border_collapse
            && self.border_spacing_x == other.border_spacing_x
            && self.border_spacing_y == other.border_spacing_y
            && self.text_justify == other.text_justify
            && self.text_align_last == other.text_align_last
            && self.hyphenate_character == other.hyphenate_character
            && self.hyphenate_limit_lines == other.hyphenate_limit_lines
    }
}

/// Optional flexbox & grid layout properties allocated only when set.
#[derive(Debug, Clone, PartialEq)]
pub struct FlexGridStyle {
    pub flex_direction: FlexDirectionMode,
    pub flex_wrap: FlexWrapMode,
    pub flex_grow: f32,
    pub flex_shrink: f32,
    pub flex_basis: LengthPercentageAuto,
    pub flex_basis_size: Option<SizeValue>,
    pub order: i32,
    pub align_items: AlignValue,
    pub align_self: AlignValue,
    pub align_content: AlignValue,
    pub justify_content: AlignValue,
    pub justify_items: AlignValue,
    pub justify_self: AlignValue,
    pub row_gap: LengthPercentage,
    pub column_gap: LengthPercentage,
    pub grid_template_columns: Vec<GridTrackSize>,
    pub grid_template_rows: Vec<GridTrackSize>,
    pub grid_template_columns_list: crate::grid_css::GridTemplateTrackList,
    pub grid_template_rows_list: crate::grid_css::GridTemplateTrackList,
    pub grid_template_areas: GridTemplateAreasValue,
    pub grid_auto_columns: Vec<GridTrackSize>,
    pub grid_auto_rows: Vec<GridTrackSize>,
    pub grid_auto_flow: GridAutoFlowMode,
    pub grid_row_start: GridPlacementValue,
    pub grid_row_end: GridPlacementValue,
    pub grid_column_start: GridPlacementValue,
    pub grid_column_end: GridPlacementValue,
    pub fit_tolerance: crate::grid_css::FitToleranceValue,
    pub grid_lanes_direction: crate::grid_css::GridLanesDirectionMode,
    pub grid_lanes_pack: crate::grid_css::GridLanesPackMode,
}

impl Default for FlexGridStyle {
    fn default() -> Self {
        Self {
            flex_direction: FlexDirectionMode::Row,
            flex_wrap: FlexWrapMode::NoWrap,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: LengthPercentageAuto::Auto,
            flex_basis_size: None,
            order: 0,
            align_items: AlignValue::Normal,
            align_self: AlignValue::Auto,
            align_content: AlignValue::Normal,
            justify_content: AlignValue::Normal,
            justify_items: AlignValue::Normal,
            justify_self: AlignValue::Auto,
            row_gap: LengthPercentage::Zero,
            column_gap: LengthPercentage::Zero,
            grid_template_columns: Vec::new(),
            grid_template_rows: Vec::new(),
            grid_template_columns_list: crate::grid_css::GridTemplateTrackList::None,
            grid_template_rows_list: crate::grid_css::GridTemplateTrackList::None,
            grid_template_areas: GridTemplateAreasValue::default(),
            grid_auto_columns: Vec::new(),
            grid_auto_rows: Vec::new(),
            grid_auto_flow: GridAutoFlowMode::Row,
            grid_row_start: GridPlacementValue::Auto,
            grid_row_end: GridPlacementValue::Auto,
            grid_column_start: GridPlacementValue::Auto,
            grid_column_end: GridPlacementValue::Auto,
            fit_tolerance: crate::grid_css::FitToleranceValue::Normal,
            grid_lanes_direction: crate::grid_css::GridLanesDirectionMode::Normal,
            grid_lanes_pack: crate::grid_css::GridLanesPackMode::Normal,
        }
    }
}

/// Optional paint effects (`box-shadow`, `transform`, `transform-origin`, `filter`,
/// `clip-path`, `mix-blend-mode`, `transition`, `animation`) allocated only when set.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EffectsStyle {
    pub box_shadow: Vec<BoxShadowValue>,
    pub transform: Vec<TransformOp>,
    pub transform_raw: Option<String>,
    pub transform_origin: TransformOrigin,
    pub scale: crate::values_math::ScaleSpec,
    pub rotate: crate::values_math::RotateSpec,
    pub translate: crate::values_math::TranslateSpec,
    pub image_resolution: Option<crate::values_math::MathNode>,
    pub filter: Vec<FilterOp>,
    pub clip_path: ClipPathValue,
    pub mix_blend_mode: MixBlendMode,
    pub transitions: Vec<TransitionSpec>,
    pub transition_delay_math: Option<crate::values_math::MathNode>,
    pub transition_duration_math: Option<crate::values_math::MathNode>,
    pub animations: Vec<AnimationSpec>,
    pub animation_delay_math: Option<crate::values_math::MathNode>,
    pub animation_duration_math: Option<crate::values_math::MathNode>,
    pub animation_iteration_count_math: Option<crate::values_math::MathNode>,
    pub extra_math_props: HashMap<PropertyIdKind, crate::values_math::MathNode>,
    pub aspect_ratio_math: Option<(
        crate::values_math::MathNode,
        Option<crate::values_math::MathNode>,
    )>,
    pub view_transition_name: Option<InternedStr>,
    pub font_style_oblique_math: Option<crate::values_math::MathNode>,
    pub reading_flow: ReadingFlowMode,
    pub reading_order: i32,
    /// CSS Containment L2 bitmask: 0x01=size, 0x02=inline-size, 0x04=layout, 0x08=style, 0x10=paint.
    pub contain: u8,
}

/// Inherited map of CSS custom properties (`--*`) stored as owned token streams,
/// together with token-level `attr()`-tainted byte ranges (CSS Values L5 §9.4.1).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CustomPropertyMap {
    pub entries: HashMap<InternedStr, Vec<CssToken>>,
    pub tainted_ranges: HashMap<InternedStr, Vec<(usize, usize)>>,
}

impl CustomPropertyMap {
    #[must_use]
    pub fn get(&self, name: &InternedStr) -> Option<&[CssToken]> {
        self.entries.get(name).map(Vec::as_slice)
    }

    pub fn insert(&mut self, name: InternedStr, value: Vec<CssToken>) {
        self.entries.insert(name, value);
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn default_box_model() -> &'static Arc<BoxModelStyle> {
    static DEFAULT: OnceLock<Arc<BoxModelStyle>> = OnceLock::new();
    DEFAULT.get_or_init(|| Arc::new(BoxModelStyle::default()))
}

fn default_visual() -> &'static Arc<VisualStyle> {
    static DEFAULT: OnceLock<Arc<VisualStyle>> = OnceLock::new();
    DEFAULT.get_or_init(|| Arc::new(VisualStyle::default()))
}

fn default_text() -> &'static Arc<InheritedTextStyle> {
    static DEFAULT: OnceLock<Arc<InheritedTextStyle>> = OnceLock::new();
    DEFAULT.get_or_init(|| Arc::new(InheritedTextStyle::default()))
}

fn default_custom_props() -> &'static Arc<CustomPropertyMap> {
    static DEFAULT: OnceLock<Arc<CustomPropertyMap>> = OnceLock::new();
    DEFAULT.get_or_init(|| Arc::new(CustomPropertyMap::default()))
}

/// Compact `Arc`-grouped computed style (`size_of::<ComputedStyle>() <= 96`).
#[derive(Debug, Clone, PartialEq)]
pub struct ComputedStyle {
    pub display: DisplayMode,
    pub position: PositionMode,
    pub float_mode: FloatMode,
    pub clear_mode: ClearMode,
    pub box_sizing: BoxSizingMode,
    pub overflow: OverflowPair,
    pub visibility: VisibilityMode,
    pub pointer_events: PointerEventsMode,
    pub cursor: CursorKind,
    pub overlay: OverlayMode,
    pub z_index: ZIndexValue,
    pub opacity: f32,
    pub color_scheme_dark: bool,
    pub box_model: Arc<BoxModelStyle>,
    pub visual: Arc<VisualStyle>,
    pub text: Arc<InheritedTextStyle>,
    pub flex_grid: Option<Arc<FlexGridStyle>>,
    pub effects: Option<Arc<EffectsStyle>>,
    pub custom_properties: Arc<CustomPropertyMap>,
}

impl Default for ComputedStyle {
    fn default() -> Self {
        Self {
            display: DisplayMode::Inline,
            position: PositionMode::Static,
            float_mode: FloatMode::None,
            clear_mode: ClearMode::None,
            box_sizing: BoxSizingMode::ContentBox,
            overflow: OverflowPair::default(),
            visibility: VisibilityMode::Visible,
            pointer_events: PointerEventsMode::Auto,
            cursor: CursorKind::Auto,
            overlay: OverlayMode::None,
            z_index: ZIndexValue::Auto,
            opacity: 1.0,
            color_scheme_dark: false,
            box_model: Arc::clone(default_box_model()),
            visual: Arc::clone(default_visual()),
            text: Arc::clone(default_text()),
            flex_grid: None,
            effects: None,
            custom_properties: Arc::clone(default_custom_props()),
        }
    }
}

impl ComputedStyle {
    /// Creates an initial style inheriting all CSS-inherited fields from `parent` in O(1).
    #[must_use]
    pub fn inherit_from(parent: &Self) -> Self {
        Self {
            display: DisplayMode::Inline,
            position: PositionMode::Static,
            float_mode: FloatMode::None,
            clear_mode: ClearMode::None,
            box_sizing: BoxSizingMode::ContentBox,
            overflow: OverflowPair::default(),
            visibility: parent.visibility,
            pointer_events: parent.pointer_events,
            cursor: parent.cursor,
            overlay: OverlayMode::None,
            z_index: ZIndexValue::Auto,
            opacity: 1.0,
            color_scheme_dark: parent.color_scheme_dark,
            box_model: Arc::clone(default_box_model()),
            visual: Arc::clone(default_visual()),
            text: Arc::clone(&parent.text),
            flex_grid: None,
            effects: None,
            custom_properties: Arc::clone(&parent.custom_properties),
        }
    }

    /// Zero-allocation comparison of layout-affecting fields.
    ///
    /// Paint-only changes (`color`, `background-color`, `border-color`,
    /// `border-radius`, `box-shadow`, `opacity`, `visibility`, `cursor`,
    /// `pointer-events`, `transform`, `filter`) return `true` so layout can be skipped.
    #[must_use]
    pub fn layout_eq(&self, other: &Self) -> bool {
        if self.display != other.display
            || self.position != other.position
            || self.float_mode != other.float_mode
            || self.clear_mode != other.clear_mode
            || self.box_sizing != other.box_sizing
            || self.overflow != other.overflow
        {
            return false;
        }
        if !Arc::ptr_eq(&self.box_model, &other.box_model) && self.box_model != other.box_model {
            return false;
        }
        if !Arc::ptr_eq(&self.text, &other.text) && !self.text.layout_eq(&other.text) {
            return false;
        }
        match (&self.flex_grid, &other.flex_grid) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b) || a == b,
            _ => false,
        }
    }

    pub(crate) fn flex_grid_mut(&mut self) -> &mut FlexGridStyle {
        let arc = self
            .flex_grid
            .get_or_insert_with(|| Arc::new(FlexGridStyle::default()));
        Arc::make_mut(arc)
    }

    pub(crate) fn effects_mut(&mut self) -> &mut EffectsStyle {
        let arc = self
            .effects
            .get_or_insert_with(|| Arc::new(EffectsStyle::default()));
        Arc::make_mut(arc)
    }
}

/// Expands `var()` references in `tokens` against `custom_props`, detecting cycles
/// and enforcing strict token/byte limits to prevent exponential expansion OOM.
///
/// Returns `None` if an undefined variable has no fallback, a cycle is encountered,
/// or the expansion exceeds the spec-sanctioned size cap (invalid at computed-value time).
#[must_use]
pub fn resolve_var_tokens(
    tokens: &[CssToken],
    custom_props: &CustomPropertyMap,
) -> Option<Vec<CssToken>> {
    let mut active = SmallVec::<[InternedStr; 8]>::new();
    let mut out = Vec::with_capacity(tokens.len());
    let mut byte_budget = MAX_VAR_EXPANDED_BYTES;
    expand_var_tokens(
        tokens,
        custom_props,
        0,
        &mut active,
        &mut out,
        &mut byte_budget,
    )?;
    Some(out)
}

#[inline]
fn token_approx_bytes(tok: &CssToken) -> usize {
    match tok {
        CssToken::Ident(s) | CssToken::String(s) | CssToken::Url(s) | CssToken::Hash(s) => {
            s.as_str().len().max(1)
        }
        CssToken::Dimension(_, u) => 8 + u.as_str().len(),
        CssToken::Function { name, args } => name.as_str().len() + 2 + args.len() * 4,
        CssToken::Var { name, .. } => name.as_str().len() + 6,
        _ => 4,
    }
}

fn expand_var_tokens(
    tokens: &[CssToken],
    custom_props: &CustomPropertyMap,
    depth: u8,
    active: &mut SmallVec<[InternedStr; 8]>,
    out: &mut Vec<CssToken>,
    byte_budget: &mut usize,
) -> Option<()> {
    if depth > MAX_VAR_DEPTH || out.len() > MAX_VAR_EXPANDED_TOKENS {
        return None;
    }
    for tok in tokens {
        match tok {
            CssToken::Var { name, fallback } => {
                if active.contains(name) {
                    return None;
                }
                if let Some(val_tokens) = custom_props.get(name) {
                    active.push(name.clone());
                    let saved_len = out.len();
                    let saved_budget = *byte_budget;
                    let res = expand_var_tokens(
                        val_tokens,
                        custom_props,
                        depth + 1,
                        active,
                        out,
                        byte_budget,
                    );
                    active.pop();
                    if res.is_some() {
                        continue;
                    }
                    // Roll back partial output before trying fallback.
                    out.truncate(saved_len);
                    *byte_budget = saved_budget;
                    // If the referenced custom property failed due to an expansion budget overflow
                    // or cycle involving `active`, propagate failure instead of using fallback.
                    if *byte_budget == 0 {
                        return None;
                    }
                }
                let fb = fallback.as_deref()?;
                expand_var_tokens(fb, custom_props, depth + 1, active, out, byte_budget)?;
            }
            CssToken::Function { name, args } if name.as_str().eq_ignore_ascii_case("var") => {
                // CSS Variables L2 §3: `var(<declaration-value> [, <declaration-value>?]?)`
                // where the name argument is itself expanded (e.g. `var(var(--myvar))`, `var({--other})`).
                let (name_args, fallback) = split_var_fn_args(args);
                let mut resolved_name_toks = Vec::new();
                let mut name_budget = *byte_budget;
                let resolved_name = expand_var_tokens(
                    name_args,
                    custom_props,
                    depth + 1,
                    active,
                    &mut resolved_name_toks,
                    &mut name_budget,
                )
                .and_then(|()| extract_single_custom_prop_name(&resolved_name_toks));
                if let Some(target_name) = resolved_name {
                    if !active.contains(&target_name) {
                        if let Some(val_tokens) = custom_props.get(&target_name) {
                            active.push(target_name);
                            let saved_len = out.len();
                            let saved_budget = *byte_budget;
                            let res = expand_var_tokens(
                                val_tokens,
                                custom_props,
                                depth + 1,
                                active,
                                out,
                                byte_budget,
                            );
                            active.pop();
                            if res.is_some() {
                                continue;
                            }
                            out.truncate(saved_len);
                            *byte_budget = saved_budget;
                            if *byte_budget == 0 {
                                return None;
                            }
                        }
                    } else {
                        return None;
                    }
                }
                let fb = fallback?;
                expand_var_tokens(fb, custom_props, depth + 1, active, out, byte_budget)?;
            }
            CssToken::Function { name, args } => {
                let cost = name.as_str().len() + 2;
                if cost > *byte_budget || out.len() >= MAX_VAR_EXPANDED_TOKENS {
                    *byte_budget = 0;
                    return None;
                }
                *byte_budget -= cost;
                let mut resolved_args = Vec::with_capacity(args.len());
                expand_var_tokens(
                    args,
                    custom_props,
                    depth + 1,
                    active,
                    &mut resolved_args,
                    byte_budget,
                )?;
                out.push(CssToken::Function {
                    name: name.clone(),
                    args: resolved_args,
                });
            }
            other => {
                let cost = token_approx_bytes(other);
                if cost > *byte_budget || out.len() >= MAX_VAR_EXPANDED_TOKENS {
                    *byte_budget = 0;
                    return None;
                }
                *byte_budget -= cost;
                out.push(other.clone());
            }
        }
    }
    Some(())
}

/// A matched compiled rule paired with its `@scope` proximity (ancestor hops from element
/// to scoping root; `u32::MAX` for unscoped rules).
#[derive(Debug, Clone, Copy)]
pub struct MatchedRule<'a> {
    pub rule: &'a CompiledRule,
    pub scope_proximity: u32,
}

/// Cascade Level 5/6 ordering key (strictly ascending priority; highest wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CascadeKey {
    band: u8,
    layer_rank: u32,
    specificity: u32,
    scope_prox_rank: u32,
    rule_order: u32,
    decl_order: u32,
}

impl CascadeKey {
    #[allow(clippy::too_many_arguments)]
    fn new(
        origin: CascadeOrigin,
        is_inline: bool,
        important: bool,
        layer_id: LayerId,
        specificity: u32,
        scope_proximity: u32,
        rule_order: u32,
        decl_order: u32,
    ) -> Self {
        let band = match (important, is_inline, origin) {
            (false, false, CascadeOrigin::UserAgent) => 0,
            (false, false, CascadeOrigin::User) => 1,
            // band 2 is reserved for HTML5 §15.3 presentational hints (Author origin for `revert`,
            // independent pre-author layer for `revert-layer` per CSS Cascade L5 §6.4.4).
            (false, false, CascadeOrigin::Author) => 3,
            (false, true, _) => 4,
            (true, false, CascadeOrigin::Author) => 5,
            (true, true, _) => 6,
            (true, false, CascadeOrigin::User) => 7,
            (true, false, CascadeOrigin::UserAgent) => 8,
        };
        let layer_rank = if !important {
            layer_id.0
        } else if layer_id.is_unlayered() {
            0
        } else {
            u32::MAX - layer_id.0
        };
        let scope_prox_rank = if scope_proximity == u32::MAX {
            0
        } else {
            u32::MAX - scope_proximity
        };
        Self {
            band,
            layer_rank,
            specificity,
            scope_prox_rank,
            rule_order,
            decl_order,
        }
    }
}

struct CandidateDecl<'a> {
    key: CascadeKey,
    decl: &'a SpecifiedDeclaration,
}

/// Resolves an element's [`ComputedStyle`] from matched rules, inline declarations,
/// parent style, root font size, and media context according to CSS Cascade Level 5.
#[must_use]
pub fn resolve_element_style(
    matched_rules: &[&CompiledRule],
    inline_decls: &[SpecifiedDeclaration],
    parent_style: Option<&ComputedStyle>,
    root_font_size_px: f32,
    media: &MediaContext,
) -> ComputedStyle {
    resolve_element_style_with_hints(
        matched_rules,
        &[],
        inline_decls,
        parent_style,
        root_font_size_px,
        media,
    )
}

/// Resolves an element's [`ComputedStyle`] including zero-specificity HTML5 §15.3
/// presentational hint declarations (`bgcolor`, `width`, `height`, `align`, `cellspacing`, etc.).
#[must_use]
pub fn resolve_element_style_with_hints(
    matched_rules: &[&CompiledRule],
    pres_hint_decls: &[SpecifiedDeclaration],
    inline_decls: &[SpecifiedDeclaration],
    parent_style: Option<&ComputedStyle>,
    root_font_size_px: f32,
    media: &MediaContext,
) -> ComputedStyle {
    resolve_element_style_full(
        matched_rules,
        pres_hint_decls,
        inline_decls,
        parent_style,
        root_font_size_px,
        media,
        None,
    )
}

/// Resolves an element's [`ComputedStyle`] with presentational hints and optional
/// `@property` / `CSS.registerProperty` custom property registrations.
#[must_use]
pub fn resolve_element_style_full(
    matched_rules: &[&CompiledRule],
    pres_hint_decls: &[SpecifiedDeclaration],
    inline_decls: &[SpecifiedDeclaration],
    parent_style: Option<&ComputedStyle>,
    root_font_size_px: f32,
    media: &MediaContext,
    registered_props: Option<&HashMap<InternedStr, RegisteredProperty>>,
) -> ComputedStyle {
    let scoped: SmallVec<[MatchedRule<'_>; 16]> = matched_rules
        .iter()
        .map(|&rule| MatchedRule {
            rule,
            scope_proximity: u32::MAX,
        })
        .collect();
    resolve_element_style_scoped(
        &scoped,
        pres_hint_decls,
        inline_decls,
        parent_style,
        root_font_size_px,
        media,
        registered_props,
    )
}

fn compute_line_height_px(lh: &LineHeightValue, font_size_px: f32, eval: &EvalContext) -> f32 {
    match lh {
        LineHeightValue::Normal => (font_size_px * 1.2).max(0.0),
        LineHeightValue::Number(n) => (n * font_size_px).max(0.0),
        LineHeightValue::Length(lp) => lp.resolve_px(font_size_px, eval).max(0.0),
    }
}

/// Resolves an element's [`ComputedStyle`] with `@scope` proximity, presentational hints,
/// and `@property` / `CSS.registerProperty` custom property registrations.
#[must_use]
pub fn resolve_element_style_scoped(
    matched_rules: &[MatchedRule<'_>],
    pres_hint_decls: &[SpecifiedDeclaration],
    inline_decls: &[SpecifiedDeclaration],
    parent_style: Option<&ComputedStyle>,
    root_font_size_px: f32,
    media: &MediaContext,
    registered_props: Option<&HashMap<InternedStr, RegisteredProperty>>,
) -> ComputedStyle {
    resolve_element_style_with_siblings(
        matched_rules,
        pres_hint_decls,
        inline_decls,
        parent_style,
        root_font_size_px,
        media,
        registered_props,
        1,
        1,
    )
}

/// Resolves an element's [`ComputedStyle`] with full CSS Values L4/L5 context including
/// 1-based `sibling_index` and `sibling_count` among parent element children.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn resolve_element_style_with_siblings(
    matched_rules: &[MatchedRule<'_>],
    pres_hint_decls: &[SpecifiedDeclaration],
    inline_decls: &[SpecifiedDeclaration],
    parent_style: Option<&ComputedStyle>,
    root_font_size_px: f32,
    media: &MediaContext,
    registered_props: Option<&HashMap<InternedStr, RegisteredProperty>>,
    sibling_index: u32,
    sibling_count: u32,
) -> ComputedStyle {
    resolve_element_style_with_context(
        matched_rules,
        pres_hint_decls,
        inline_decls,
        parent_style,
        root_font_size_px,
        media,
        registered_props,
        sibling_index,
        sibling_count,
        0,
        EvalContext::default().random_doc_seed,
        &[],
    )
}

/// Resolves an element's [`ComputedStyle`] with sibling indices, `random()` seeds, and live DOM attributes.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn resolve_element_style_with_context(
    matched_rules: &[MatchedRule<'_>],
    pres_hint_decls: &[SpecifiedDeclaration],
    inline_decls: &[SpecifiedDeclaration],
    parent_style: Option<&ComputedStyle>,
    root_font_size_px: f32,
    media: &MediaContext,
    registered_props: Option<&HashMap<InternedStr, RegisteredProperty>>,
    sibling_index: u32,
    sibling_count: u32,
    random_node_id: u64,
    random_doc_seed: u64,
    elem_attrs: &[(rix_dom::QualName, String)],
) -> ComputedStyle {
    let mut style = match parent_style {
        Some(p) => ComputedStyle::inherit_from(p),
        None => ComputedStyle::default(),
    };
    let has_registered = registered_props.is_some_and(|m| !m.is_empty());

    let mut candidates: Vec<CandidateDecl<'_>> =
        Vec::with_capacity(matched_rules.len() * 4 + pres_hint_decls.len() + inline_decls.len());
    for decl in pres_hint_decls {
        // HTML5 §15.3 Presentational Hints: override UA/User normal (bands 0..1) while losing
        // to every Author rule (band 3+); treated as Author for `revert` and separate for `revert-layer`.
        candidates.push(CandidateDecl {
            key: CascadeKey {
                band: 2,
                layer_rank: 0,
                specificity: 0,
                scope_prox_rank: 0,
                rule_order: 0,
                decl_order: decl.order,
            },
            decl,
        });
    }
    for (rule_idx, mr) in matched_rules.iter().enumerate() {
        let rule = mr.rule;
        let r_order = (rule.source_order).max(rule_idx as u32);
        for decl in rule.declarations.iter() {
            candidates.push(CandidateDecl {
                key: CascadeKey::new(
                    rule.origin,
                    false,
                    decl.important,
                    rule.layer_id,
                    rule.specificity,
                    mr.scope_proximity,
                    r_order,
                    decl.order,
                ),
                decl,
            });
        }
    }
    for decl in inline_decls {
        candidates.push(CandidateDecl {
            key: CascadeKey::new(
                CascadeOrigin::Author,
                true,
                decl.important,
                LayerId::UNLAYERED,
                u32::MAX,
                u32::MAX,
                u32::MAX,
                decl.order,
            ),
            decl,
        });
    }

    if candidates.is_empty() && !has_registered {
        let mut ctx = media.eval_context(style.text.font_size_px, root_font_size_px);
        if let Some(p) = parent_style {
            ctx.root_line_height_px = p.text.eval_ctx.root_line_height_px;
        }
        ctx.line_height_px =
            compute_line_height_px(&style.text.line_height, style.text.font_size_px, &ctx);
        if parent_style.is_none() {
            ctx.root_line_height_px = ctx.line_height_px;
        }
        ctx.sibling_index = sibling_index;
        ctx.sibling_count = sibling_count;
        ctx.random_node_id = random_node_id;
        ctx.random_doc_seed = random_doc_seed;
        if style.text.eval_ctx != ctx {
            Arc::make_mut(&mut style.text).eval_ctx = ctx;
        }
        return style;
    }

    candidates.sort_by_key(|c| c.key);

    // Preliminary evaluation context for Pass 1/Pass 2 CSS Values L5 substitution functions
    // (`if()`, `attr()`, `random-item()`, `ident()`, `inherit()`).
    let prelim_parent_fs = parent_style.map_or(16.0, |p| p.text.font_size_px);
    let mut prelim_parent_eval = media.eval_context(prelim_parent_fs, root_font_size_px);
    prelim_parent_eval.line_height_px =
        parent_style.map_or(19.2, |p| p.text.eval_ctx.line_height_px);
    prelim_parent_eval.root_line_height_px =
        parent_style.map_or(19.2, |p| p.text.eval_ctx.root_line_height_px);
    prelim_parent_eval.sibling_index = sibling_index;
    prelim_parent_eval.sibling_count = sibling_count;
    prelim_parent_eval.random_node_id = random_node_id;
    prelim_parent_eval.random_doc_seed = random_doc_seed;
    let prelim_self_fs = candidates
        .iter()
        .rev()
        .find(|c| c.decl.property == PropertyIdKind::FontSize)
        .and_then(|c| match &c.decl.value {
            PropertyValue::Length(lp) => Some(
                lp.resolve_px(prelim_parent_fs, &prelim_parent_eval)
                    .max(0.0),
            ),
            PropertyValue::MathVal(n) => {
                let px = n.eval_scalar(
                    &prelim_parent_eval,
                    Some((f64::from(prelim_parent_fs), false)),
                );
                Some(if px.is_nan() {
                    0.0
                } else {
                    px.clamp(0.0, crate::values_math::MAX_CSS_PX) as f32
                })
            }
            _ => None,
        })
        .unwrap_or(prelim_parent_fs);
    let mut prelim_eval = prelim_parent_eval;
    prelim_eval.font_size_px = prelim_self_fs;
    if parent_style.is_none() {
        prelim_eval.root_font_size_px = prelim_self_fs;
    }
    let subst_env = crate::substitution::SubstEnv {
        elem_attrs,
        parent_style,
        registered_props,
        media,
        eval: &prelim_eval,
    };

    // Pass 1: Resolve CSS custom properties (`--*`) & `@property` registrations.
    let mut raw_custom: HashMap<InternedStr, Vec<(CascadeKey, &SpecifiedDeclaration)>> =
        HashMap::new();
    for c in &candidates {
        if let (PropertyIdKind::Custom, Some(name)) = (c.decl.property, &c.decl.custom_name) {
            raw_custom
                .entry(name.clone())
                .or_default()
                .push((c.key, c.decl));
        }
    }
    if !raw_custom.is_empty() || has_registered {
        let mut custom_map = (*style.custom_properties).clone();
        if let Some(regs) = registered_props {
            for (name, reg) in regs {
                let canonical_iv = reg.initial_value.as_ref().map(|iv| {
                    if is_registered_url_syntax(&reg.syntax) {
                        canonicalize_registered_url_tokens(iv)
                    } else {
                        iv.clone()
                    }
                });
                if !reg.inherits {
                    custom_map.tainted_ranges.remove(name);
                    if let Some(iv) = canonical_iv {
                        custom_map.insert(name.clone(), iv);
                    } else {
                        custom_map.entries.remove(name);
                    }
                } else if !custom_map.entries.contains_key(name) {
                    if let Some(iv) = canonical_iv {
                        custom_map.insert(name.clone(), iv);
                    }
                }
            }
        }
        let apply_custom_kw =
            |map: &mut CustomPropertyMap, name: &InternedStr, kw: GlobalKeyword| {
                let reg = registered_props.and_then(|m| m.get(name));
                let inherit_val = || {
                    parent_style
                        .and_then(|p| {
                            p.custom_properties.get(name).map(|v| {
                                (
                                    v.to_vec(),
                                    p.custom_properties.tainted_ranges.get(name).cloned(),
                                )
                            })
                        })
                        .or_else(|| {
                            reg.and_then(|r| {
                                r.initial_value.clone().map(|v| {
                                    let v = if is_registered_url_syntax(&r.syntax) {
                                        canonicalize_registered_url_tokens(&v)
                                    } else {
                                        v
                                    };
                                    (v, None)
                                })
                            })
                        })
                };
                let initial_val = || {
                    reg.and_then(|r| {
                        r.initial_value.clone().map(|v| {
                            let v = if is_registered_url_syntax(&r.syntax) {
                                canonicalize_registered_url_tokens(&v)
                            } else {
                                v
                            };
                            (v, None)
                        })
                    })
                };
                let val = match kw {
                    GlobalKeyword::Initial => initial_val(),
                    GlobalKeyword::Inherit => inherit_val(),
                    GlobalKeyword::Unset
                    | GlobalKeyword::Revert
                    | GlobalKeyword::RevertLayer
                    | GlobalKeyword::RevertRule => {
                        if reg.is_some_and(|r| !r.inherits) {
                            initial_val()
                        } else {
                            inherit_val()
                        }
                    }
                };
                if let Some((v, t)) = val {
                    map.insert(name.clone(), v);
                    if let Some(tr) = t {
                        map.tainted_ranges.insert(name.clone(), tr);
                    } else {
                        map.tainted_ranges.remove(name);
                    }
                } else {
                    map.entries.remove(name);
                    map.tainted_ranges.remove(name);
                }
            };
        for (name, stack) in &raw_custom {
            if let Some(winner) = select_winning_candidate(stack) {
                match &winner.value {
                    PropertyValue::Custom(tokens) => {
                        custom_map.insert(name.clone(), tokens.clone());
                        custom_map.tainted_ranges.remove(name);
                    }
                    PropertyValue::Global(kw) => {
                        apply_custom_kw(&mut custom_map, name, *kw);
                    }
                    _ => {}
                }
            }
        }
        // Substitute `var()`, `inherit()`, `attr()`, `if()`, `random-item()`, `ident()`
        // inside declared custom properties using Dynamic Tarjan SCC (CSSWG #11500) + taint tracking.
        let raw_entries = custom_map.entries.clone();
        let keys: Vec<InternedStr> = raw_custom.keys().cloned().collect();
        let any_subst = keys.iter().any(|k| {
            raw_entries
                .get(k)
                .is_some_and(|t| crate::substitution::tokens_contain_subst(t))
        });
        let resolved_map = if any_subst {
            crate::substitution::resolve_custom_properties_l5(&custom_map, &keys, &subst_env)
        } else {
            HashMap::new()
        };
        for name in keys {
            let has_subst = raw_entries
                .get(&name)
                .is_some_and(|t| crate::substitution::tokens_contain_subst(t));
            let resolved_opt = if has_subst {
                resolved_map.get(&name).cloned().flatten()
            } else {
                custom_map
                    .get(&name)
                    .map(|t| crate::substitution::SubstOutput {
                        text: serialize_css_tokens(t),
                        tainted_ranges: custom_map
                            .tainted_ranges
                            .get(&name)
                            .cloned()
                            .unwrap_or_default(),
                    })
            };
            match resolved_opt {
                Some(sub_out) => {
                    let resolved = if has_subst {
                        crate::substitution::subst_output_to_tokens(&sub_out)
                    } else {
                        custom_map
                            .get(&name)
                            .map(<[CssToken]>::to_vec)
                            .unwrap_or_default()
                    };
                    if has_subst {
                        if let Some(kw) = single_global_keyword_token(&resolved) {
                            match kw {
                                GlobalKeyword::Revert
                                | GlobalKeyword::RevertLayer
                                | GlobalKeyword::RevertRule => {
                                    let reverted = raw_custom
                                        .get(&name)
                                        .and_then(|st| select_candidate_after_revert(st, kw));
                                    match reverted.map(|d| &d.value) {
                                        Some(PropertyValue::Custom(toks)) => {
                                            if let Some(sub) = resolve_var_tokens(toks, &custom_map)
                                            {
                                                custom_map.insert(name.clone(), sub);
                                                custom_map.tainted_ranges.remove(&name);
                                            } else {
                                                apply_custom_kw(
                                                    &mut custom_map,
                                                    &name,
                                                    GlobalKeyword::Unset,
                                                );
                                            }
                                        }
                                        Some(PropertyValue::Global(gkw)) => {
                                            apply_custom_kw(&mut custom_map, &name, *gkw);
                                        }
                                        _ => {
                                            apply_custom_kw(&mut custom_map, &name, kw);
                                        }
                                    }
                                    continue;
                                }
                                other_kw => {
                                    apply_custom_kw(&mut custom_map, &name, other_kw);
                                    continue;
                                }
                            }
                        }
                    }
                    if let Some(reg) = registered_props.and_then(|m| m.get(&name)) {
                        let syn_l = reg.syntax.to_ascii_lowercase();
                        let url_taint_fail = (syn_l.contains("<url>") || syn_l.contains("<image>"))
                            && crate::substitution::has_tainted_url_in_output(&sub_out);
                        if url_taint_fail
                            || !registered_value_matches_syntax(&reg.syntax, &resolved)
                        {
                            apply_custom_kw(&mut custom_map, &name, GlobalKeyword::Unset);
                            continue;
                        }
                    }
                    if has_subst {
                        custom_map.insert(name.clone(), resolved);
                        if sub_out.is_tainted() {
                            custom_map
                                .tainted_ranges
                                .insert(name, sub_out.tainted_ranges);
                        } else {
                            custom_map.tainted_ranges.remove(&name);
                        }
                    }
                }
                None => {
                    if has_subst {
                        if registered_props.and_then(|m| m.get(&name)).is_some() {
                            apply_custom_kw(&mut custom_map, &name, GlobalKeyword::Unset);
                        } else {
                            custom_map.entries.remove(&name);
                            custom_map.tainted_ranges.remove(&name);
                        }
                    }
                }
            }
        }
        style.custom_properties = Arc::new(custom_map);
    }

    // Pass 2: Expand any substitution-bearing (`var()`, `if()`, `attr()`, `random-item()`,
    // `ident()`, `inherit()`) declarations into typed declarations, preserving cascade key.
    // Per CSS Variables L1 §3 / CSS Values L5, failed substitution, attr()-tainted URLs, or
    // invalid parsed values become IACVT (behave as `unset`).
    let mut expanded_owned: Vec<(CascadeKey, SpecifiedDeclaration)> = Vec::new();
    let mut standard_candidates: Vec<(CascadeKey, &SpecifiedDeclaration)> = Vec::new();
    for c in &candidates {
        if c.decl.property == PropertyIdKind::Custom {
            continue;
        }
        if let PropertyValue::Unresolved(tokens) = &c.decl.value {
            let expanded = crate::substitution::substitute_standard_property_l5(
                tokens,
                c.decl.property,
                &style.custom_properties,
                &subst_env,
            )
            .map(|sub| {
                crate::properties::parse_property_from_str(
                    c.decl.property,
                    &sub.text,
                    c.decl.important,
                )
            })
            .unwrap_or_default();
            if !expanded.is_empty() {
                for decl in expanded {
                    expanded_owned.push((c.key, decl));
                }
            } else {
                // IACVT: emit `unset` for the property (or constituent longhands if shorthand).
                for target_prop in c.decl.property.shorthand_longhands() {
                    expanded_owned.push((
                        c.key,
                        SpecifiedDeclaration {
                            property: target_prop,
                            custom_name: None,
                            value: PropertyValue::Global(GlobalKeyword::Unset),
                            important: c.decl.important,
                            order: c.decl.order,
                        },
                    ));
                }
            }
        } else {
            standard_candidates.push((c.key, c.decl));
        }
    }

    let mut per_prop: HashMap<PropertyIdKind, Vec<(CascadeKey, &SpecifiedDeclaration)>> =
        HashMap::new();
    for (k, d) in &standard_candidates {
        per_prop.entry(d.property).or_default().push((*k, *d));
    }
    for (k, d) in &expanded_owned {
        per_prop.entry(d.property).or_default().push((*k, d));
    }

    // CSS Cascade L5 §3.1: Expand `all: <css-wide-keyword>` into every physical longhand
    // property (except `direction`, `unicode-bidi`, and custom properties), preserving `CascadeKey`.
    let mut all_expanded: Vec<(PropertyIdKind, CascadeKey, SpecifiedDeclaration)> = Vec::new();
    if let Some(all_stack) = per_prop.remove(&PropertyIdKind::All) {
        for (k, d) in all_stack {
            for &target_prop in crate::properties::ALL_RESETTABLE_LONGHANDS {
                let mut cloned = d.clone();
                cloned.property = target_prop;
                all_expanded.push((target_prop, k, cloned));
            }
        }
    }
    for (target_prop, k, decl) in &all_expanded {
        per_prop.entry(*target_prop).or_default().push((*k, decl));
    }

    // Pass 2a: Resolve `writing-mode`, `direction`, `font-size`, and `line-height` first so
    // logical property mapping and `em`/`lh`/`rlh` units on the same element use computed values.
    let parent_font_size = parent_style.map_or(16.0, |p| p.text.font_size_px);
    let parent_lh = parent_style.map_or(19.2, |p| p.text.eval_ctx.line_height_px);
    let root_lh = parent_style.map_or(19.2, |p| p.text.eval_ctx.root_line_height_px);
    let mut parent_eval = media.eval_context(parent_font_size, root_font_size_px);
    parent_eval.line_height_px = parent_lh;
    parent_eval.root_line_height_px = root_lh;
    parent_eval.sibling_index = sibling_index;
    parent_eval.sibling_count = sibling_count;
    for early_prop in [
        PropertyIdKind::WritingMode,
        PropertyIdKind::Direction,
        PropertyIdKind::FontSize,
    ] {
        if let Some(stack) = per_prop.get_mut(&early_prop) {
            stack.sort_by_key(|(k, _)| *k);
            if let Some(winner) = select_winning_candidate(stack) {
                apply_declaration(
                    &mut style,
                    winner,
                    parent_style,
                    &parent_eval,
                    media.dark_mode,
                );
            }
        }
    }
    let mut lh_eval = media.eval_context(style.text.font_size_px, root_font_size_px);
    lh_eval.line_height_px = parent_lh;
    lh_eval.root_line_height_px = root_lh;
    lh_eval.sibling_index = sibling_index;
    lh_eval.sibling_count = sibling_count;
    if let Some(stack) = per_prop.get_mut(&PropertyIdKind::LineHeight) {
        stack.sort_by_key(|(k, _)| *k);
        if let Some(winner) = select_winning_candidate(stack) {
            apply_declaration(&mut style, winner, parent_style, &lh_eval, media.dark_mode);
        }
    }
    let self_lh_px =
        compute_line_height_px(&style.text.line_height, style.text.font_size_px, &lh_eval);
    let eff_root_lh = if parent_style.is_none() {
        self_lh_px
    } else {
        root_lh
    };
    let eff_root_fs = if parent_style.is_none() {
        style.text.font_size_px
    } else {
        root_font_size_px
    };

    // Pass 2b: Map all logical property candidates onto their corresponding physical
    // property stacks using the element's resolved `writing-mode` and `direction`
    // (preserving `CascadeKey` so specificity, source order, `!important`, and `revert`
    // cascade seamlessly between logical and physical associates).
    let resolved_wm = style.text.writing_mode;
    let resolved_dir = style.text.direction;
    let logical_keys: Vec<PropertyIdKind> = per_prop
        .keys()
        .copied()
        .filter(|&p| is_logical_property(p))
        .collect();
    let mut remapped_owned: Vec<(PropertyIdKind, CascadeKey, SpecifiedDeclaration)> = Vec::new();
    for lprop in logical_keys {
        let phys = resolve_logical_to_physical(lprop, resolved_wm, resolved_dir);
        if let Some(stack) = per_prop.remove(&lprop) {
            for (k, d) in stack {
                let mut mapped = d.clone();
                mapped.property = phys;
                remapped_owned.push((phys, k, mapped));
            }
        }
    }
    for (phys, k, decl) in &remapped_owned {
        per_prop.entry(*phys).or_default().push((*k, decl));
    }

    // Pass 2c: Resolve `color-scheme` and `color` next so CanvasText & `currentColor` match.
    let mut eval_ctx = media.eval_context(style.text.font_size_px, eff_root_fs);
    eval_ctx.line_height_px = self_lh_px;
    eval_ctx.root_line_height_px = eff_root_lh;
    eval_ctx.sibling_index = sibling_index;
    eval_ctx.sibling_count = sibling_count;
    eval_ctx.random_node_id = random_node_id;
    eval_ctx.random_doc_seed = random_doc_seed;
    if let Some(p) = parent_style {
        if let SizeValue::Length(ref lp) = p.box_model.width {
            let w = lp.resolve_px(eval_ctx.viewport_width_px, &eval_ctx);
            if w > 0.0 {
                eval_ctx.container_width_px = w;
            }
        }
        if let SizeValue::Length(ref lp) = p.box_model.height {
            let h = lp.resolve_px(eval_ctx.viewport_height_px, &eval_ctx);
            if h > 0.0 {
                eval_ctx.container_height_px = h;
            }
        }
    }
    if style.text.eval_ctx != eval_ctx {
        Arc::make_mut(&mut style.text).eval_ctx = eval_ctx;
    }
    if let Some(stack) = per_prop.get_mut(&PropertyIdKind::ColorScheme) {
        stack.sort_by_key(|(k, _)| *k);
        if let Some(winner) = select_winning_candidate(stack) {
            apply_declaration(&mut style, winner, parent_style, &eval_ctx, media.dark_mode);
        }
    }
    let mut explicit_color = false;
    if let Some(stack) = per_prop.get_mut(&PropertyIdKind::Color) {
        stack.sort_by_key(|(k, _)| *k);
        if let Some(winner) = select_winning_candidate(stack) {
            apply_declaration(&mut style, winner, parent_style, &eval_ctx, media.dark_mode);
            explicit_color = true;
        }
    }
    if !explicit_color
        && media.dark_mode
        && style.color_scheme_dark
        && parent_style.is_none_or(|p| !p.color_scheme_dark)
    {
        Arc::make_mut(&mut style.text).color = ColorRgba::rgb(241, 245, 249);
    }

    // Pass 2d: Resolve all remaining physical properties.
    let mut explicit_bw = [false; 4]; // top, right, bottom, left
    for (&prop, stack) in &mut per_prop {
        if matches!(
            prop,
            PropertyIdKind::WritingMode
                | PropertyIdKind::Direction
                | PropertyIdKind::FontSize
                | PropertyIdKind::LineHeight
                | PropertyIdKind::Color
                | PropertyIdKind::ColorScheme
        ) {
            continue;
        }
        stack.sort_by_key(|(k, _)| *k);
        if let Some(winner) = select_winning_candidate(stack) {
            match prop {
                PropertyIdKind::BorderTopWidth => explicit_bw[0] = true,
                PropertyIdKind::BorderRightWidth => explicit_bw[1] = true,
                PropertyIdKind::BorderBottomWidth => explicit_bw[2] = true,
                PropertyIdKind::BorderLeftWidth => explicit_bw[3] = true,
                _ => {}
            }
            apply_declaration(&mut style, winner, parent_style, &eval_ctx, media.dark_mode);
        }
    }

    // CSS Backgrounds L3 §3.3: initial `border-*-width` is `medium` (3px), and computed
    // `border-*-width` becomes `0px` whenever the side's `border-*-style` is `none` or `hidden`.
    let bs = style.visual.border_style;
    let bw = style.box_model.border_width;
    let fix_side = |s: LineStyle, cur: f32, explicit: bool| -> f32 {
        if matches!(s, LineStyle::None | LineStyle::Hidden) {
            0.0
        } else if !explicit {
            3.0
        } else {
            cur
        }
    };
    let next_bw = EdgeValues {
        top: fix_side(bs.top, bw.top, explicit_bw[0]),
        right: fix_side(bs.right, bw.right, explicit_bw[1]),
        bottom: fix_side(bs.bottom, bw.bottom, explicit_bw[2]),
        left: fix_side(bs.left, bw.left, explicit_bw[3]),
    };
    if next_bw != bw {
        Arc::make_mut(&mut style.box_model).border_width = next_bw;
    }

    // Re-resolve any visual Color 4/5 expressions referencing `currentcolor` (including those
    // inherited via `background-color: inherit`) against this element's final `color`.
    let eff_dark = match style.text.color_scheme_mode {
        1 => false,
        2 => true,
        _ => media.dark_mode,
    };
    let self_color_res = style.text.color_computed.as_deref().map_or_else(
        || crate::color::ResolvedColor::from_rgba(style.text.color),
        |e| {
            e.resolve(
                crate::color::ResolvedColor::from_rgba(style.text.color),
                eff_dark,
                &eval_ctx,
            )
        },
    );
    if let Some(bg_expr) = style.visual.background_color_computed.clone() {
        if bg_expr.contains_current_color() {
            let resolved = bg_expr
                .resolve(self_color_res, eff_dark, &eval_ctx)
                .to_srgb_u8();
            Arc::make_mut(&mut style.visual).background_color = resolved;
        }
    }
    for side in 0..4 {
        if let Some(b_expr) = style.visual.border_color_computed[side].clone() {
            if b_expr.contains_current_color() {
                let c = CssColor::Rgba(
                    b_expr
                        .resolve(self_color_res, eff_dark, &eval_ctx)
                        .to_srgb_u8(),
                );
                let vis = Arc::make_mut(&mut style.visual);
                match side {
                    0 => vis.border_color.top = c,
                    1 => vis.border_color.right = c,
                    2 => vis.border_color.bottom = c,
                    _ => vis.border_color.left = c,
                }
            }
        }
    }
    if let Some(ol_expr) = style.visual.outline_color_computed.clone() {
        if ol_expr.contains_current_color() {
            let c = CssColor::Rgba(
                ol_expr
                    .resolve(self_color_res, eff_dark, &eval_ctx)
                    .to_srgb_u8(),
            );
            Arc::make_mut(&mut style.visual).outline_color = c;
        }
    }
    if let Some(td_expr) = style.visual.text_decoration_color_computed.clone() {
        if td_expr.contains_current_color() {
            let c = CssColor::Rgba(
                td_expr
                    .resolve(self_color_res, eff_dark, &eval_ctx)
                    .to_srgb_u8(),
            );
            Arc::make_mut(&mut style.visual).text_decoration_color = Some(c);
        }
    }

    if let Some(regs) = registered_props {
        compute_registered_custom_properties(&mut style, regs, &eval_ctx);
    }

    // CSS 2.1 §9.7 & CSS Display L3 §2.7: Blockification for root, out-of-flow, floated,
    // and flex/grid children (`position: absolute | fixed` also forces `float: none`).
    let is_root = parent_style.is_none();
    let is_abs_or_fixed = matches!(style.position, PositionMode::Absolute | PositionMode::Fixed);
    if is_abs_or_fixed {
        style.float_mode = FloatMode::None;
    }
    let is_floated = style.float_mode != FloatMode::None;
    let parent_is_flex_or_grid = parent_style.is_some_and(|p| {
        matches!(
            p.display,
            DisplayMode::Flex
                | DisplayMode::InlineFlex
                | DisplayMode::Grid
                | DisplayMode::InlineGrid
        )
    });
    if is_root && style.display == DisplayMode::Contents {
        style.display = DisplayMode::Block;
    } else if is_root || is_abs_or_fixed || is_floated || parent_is_flex_or_grid {
        style.display = style.display.blockify();
    }

    style
}

#[rustfmt::skip]
fn compute_registered_custom_properties(
    style: &mut ComputedStyle,
    regs: &HashMap<InternedStr, RegisteredProperty>,
    eval: &EvalContext,
) {
    use crate::values_math::{parse_math_value, parse_math_value_from_parser, ValueKind};
    let mut updates: Vec<(InternedStr, Vec<CssToken>)> = Vec::new();
    for (name, reg) in regs {
        let Some(toks) = style.custom_properties.get(name) else { continue; };
        let syn = reg.syntax.trim().trim_matches(|c| c == '"' || c == '\'').trim();
        let is_list = syn.ends_with('+') || syn.ends_with('#');
        let base = syn.trim_end_matches(['+', '#']).trim();
        let kind = match base {
            "<number>" => Some(ValueKind::Number),
            "<integer>" => Some(ValueKind::Integer),
            "<length>" => Some(ValueKind::Length),
            "<percentage>" => Some(ValueKind::Percentage),
            "<length-percentage>" => Some(ValueKind::LengthPercentage),
            "<angle>" => Some(ValueKind::Angle),
            "<time>" => Some(ValueKind::Time),
            "<resolution>" => Some(ValueKind::Resolution),
            _ => None,
        };
        let Some(vk) = kind else { continue; };
        let raw = serialize_css_tokens(toks);
        let trimmed = raw.trim();
        if trimmed.is_empty() { continue; }
        let mut rc = 0u32;
        let formatted = if is_list {
            let sep = if syn.ends_with('#') { ", " } else { " " };
            let mut pi = cssparser::ParserInput::new(trimmed);
            let mut p = cssparser::Parser::new(&mut pi);
            let mut parts = Vec::new();
            let mut ok = true;
            while !p.is_exhausted() {
                if let Some(mut pm) = parse_math_value_from_parser(&mut p, vk) {
                    pm.node.bind_random_property(name.as_str(), &mut rc);
                    parts.push(pm.node.serialize_computed(eval, None, vk, None, None));
                    if syn.ends_with('#') && !p.is_exhausted() && p.expect_comma().is_err() {
                        ok = false; break;
                    }
                } else { ok = false; break; }
            }
            if ok && !parts.is_empty() { Some(parts.join(sep)) } else { None }
        } else if let Some(mut pm) = parse_math_value(trimmed, vk) {
            pm.node.bind_random_property(name.as_str(), &mut rc);
            Some(pm.node.serialize_computed(eval, None, vk, None, None))
        } else {
            None
        };
        if let Some(s) = formatted {
            if s != trimmed {
                updates.push((name.clone(), crate::properties::tokenize_custom_value_str(&s)));
            }
        }
    }
    if !updates.is_empty() {
        let cmap = Arc::make_mut(&mut style.custom_properties);
        for (name, toks) in updates {
            cmap.insert(name, toks);
        }
    }
}

fn registered_value_matches_syntax(syntax: &str, tokens: &[CssToken]) -> bool {
    use crate::values_math::{parse_math_value, parse_math_value_from_parser, ValueKind};
    let syn = syntax.trim().trim_matches(|c| c == '"' || c == '\'').trim();
    if syn.is_empty() || syn == "*" {
        return true;
    }
    let raw = serialize_css_tokens(tokens);
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return false;
    }
    let check_math = |vk: ValueKind, is_list: bool, comma: bool| -> bool {
        if !is_list {
            return parse_math_value(trimmed, vk).is_some();
        }
        let mut pi = cssparser::ParserInput::new(trimmed);
        let mut p = cssparser::Parser::new(&mut pi);
        let mut count = 0usize;
        while !p.is_exhausted() {
            if parse_math_value_from_parser(&mut p, vk).is_none() {
                return false;
            }
            count += 1;
            if comma && !p.is_exhausted() && p.expect_comma().is_err() {
                return false;
            }
        }
        count > 0
    };
    for alt in syn.split('|').map(str::trim) {
        let is_list = alt.ends_with('+') || alt.ends_with('#');
        let comma = alt.ends_with('#');
        let base = alt.trim_end_matches(['+', '#']).trim();
        let ok = match base {
            "<color>" => {
                crate::cssom_decl::parse_and_serialize_declaration("color", trimmed).is_some()
            }
            "<length>" => check_math(ValueKind::Length, is_list, comma),
            "<length-percentage>" => check_math(ValueKind::LengthPercentage, is_list, comma),
            "<number>" => check_math(ValueKind::Number, is_list, comma),
            "<integer>" => check_math(ValueKind::Integer, is_list, comma),
            "<percentage>" => check_math(ValueKind::Percentage, is_list, comma),
            "<angle>" => check_math(ValueKind::Angle, is_list, comma),
            "<time>" => check_math(ValueKind::Time, is_list, comma),
            "<resolution>" => check_math(ValueKind::Resolution, is_list, comma),
            "<custom-ident>" => {
                !trimmed.contains(char::is_whitespace)
                    && GlobalKeyword::from_ident(trimmed).is_none()
            }
            "<string>" => {
                (trimmed.starts_with('"') && trimmed.ends_with('"'))
                    || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
            }
            _ if base.starts_with('<') && base.ends_with('>') => true,
            kw => trimmed.eq_ignore_ascii_case(kw),
        };
        if ok {
            return true;
        }
    }
    false
}

fn split_var_fn_args(args: &[CssToken]) -> (&[CssToken], Option<&[CssToken]>) {
    let mut depth = 0usize;
    for (i, tok) in args.iter().enumerate() {
        match tok {
            CssToken::ParenOpen => depth += 1,
            CssToken::ParenClose => depth = depth.saturating_sub(1),
            CssToken::Comma if depth == 0 => {
                return (&args[..i], Some(&args[i + 1..]));
            }
            _ => {}
        }
    }
    (args, None)
}

fn extract_single_custom_prop_name(tokens: &[CssToken]) -> Option<InternedStr> {
    let mut found: Option<&InternedStr> = None;
    for tok in tokens {
        match tok {
            CssToken::Space => continue,
            CssToken::Ident(id) if found.is_none() => found = Some(id),
            _ => return None,
        }
    }
    let id = found?;
    let s = id.as_str();
    (s.starts_with("--") && s.len() > 2).then(|| id.clone())
}

fn is_registered_url_syntax(syntax: &str) -> bool {
    syntax
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .trim()
        .eq_ignore_ascii_case("<url>")
}

fn canonicalize_registered_url_tokens(tokens: &[CssToken]) -> Vec<CssToken> {
    tokens
        .iter()
        .map(|t| match t {
            CssToken::Url(u) => CssToken::Function {
                name: rix_core::intern_str("url"),
                args: vec![CssToken::String(u.clone())],
            },
            other => other.clone(),
        })
        .collect()
}

fn single_global_keyword_token(tokens: &[CssToken]) -> Option<GlobalKeyword> {
    let mut found: Option<&str> = None;
    for tok in tokens {
        match tok {
            CssToken::Space => continue,
            CssToken::Ident(id) if found.is_none() => found = Some(id.as_str()),
            _ => return None,
        }
    }
    found.and_then(GlobalKeyword::from_ident)
}

#[inline]
const fn cascade_origin_group(band: u8) -> u8 {
    match band {
        0 | 8 => 0, // UserAgent (normal / !important)
        1 | 7 => 1, // User (normal / !important)
        2..=6 => 2, // Presentational Hints + Author (stylesheet & inline, normal & !important)
        _ => 2,
    }
}

fn select_winning_candidate<'a>(
    sorted_asc: &[(CascadeKey, &'a SpecifiedDeclaration)],
) -> Option<&'a SpecifiedDeclaration> {
    select_winning_candidate_from(sorted_asc, sorted_asc.len())
}

fn select_winning_candidate_from<'a>(
    sorted_asc: &[(CascadeKey, &'a SpecifiedDeclaration)],
    mut idx: usize,
) -> Option<&'a SpecifiedDeclaration> {
    let mut reverted_rules: SmallVec<[(u8, u32); 4]> = SmallVec::new();
    let mut reverted_imp_layer_normal: Option<u32> = None;
    while idx > 0 {
        idx -= 1;
        let (key, decl) = sorted_asc[idx];
        let rule_id = (cascade_origin_group(key.band), key.rule_order);
        if reverted_rules.contains(&rule_id) {
            continue;
        }
        match decl.value {
            PropertyValue::Global(GlobalKeyword::RevertRule) => {
                reverted_rules.push(rule_id);
                if key.band == 5 && key.layer_rank != 0 {
                    let normal_layer = u32::MAX - key.layer_rank;
                    reverted_imp_layer_normal = Some(
                        reverted_imp_layer_normal.map_or(normal_layer, |p| p.min(normal_layer)),
                    );
                }
            }
            PropertyValue::Global(GlobalKeyword::RevertLayer) => {
                // CSSWG #13916: `!important revert-rule` in an earlier layer cascading into
                // normal `revert-layer` in a later layer forms a cascade cycle -> `unset`.
                if key.band == 3
                    && reverted_imp_layer_normal.is_some_and(|imp_l| key.layer_rank > imp_l)
                {
                    return None;
                }
                while idx > 0
                    && sorted_asc[idx - 1].0.band == key.band
                    && sorted_asc[idx - 1].0.layer_rank == key.layer_rank
                {
                    idx -= 1;
                }
            }
            PropertyValue::Global(GlobalKeyword::Revert) => {
                let grp = cascade_origin_group(key.band);
                while idx > 0 && cascade_origin_group(sorted_asc[idx - 1].0.band) == grp {
                    idx -= 1;
                }
            }
            _ => return Some(decl),
        }
    }
    None
}

fn select_candidate_after_revert<'a>(
    sorted_asc: &[(CascadeKey, &'a SpecifiedDeclaration)],
    kw: GlobalKeyword,
) -> Option<&'a SpecifiedDeclaration> {
    let &(win_key, _) = sorted_asc.last()?;
    let mut idx = sorted_asc.len() - 1;
    match kw {
        GlobalKeyword::RevertRule => {
            let win_grp = cascade_origin_group(win_key.band);
            while idx > 0
                && cascade_origin_group(sorted_asc[idx - 1].0.band) == win_grp
                && sorted_asc[idx - 1].0.rule_order == win_key.rule_order
            {
                idx -= 1;
            }
        }
        GlobalKeyword::RevertLayer => {
            while idx > 0
                && sorted_asc[idx - 1].0.band == win_key.band
                && sorted_asc[idx - 1].0.layer_rank == win_key.layer_rank
            {
                idx -= 1;
            }
        }
        GlobalKeyword::Revert => {
            let grp = cascade_origin_group(win_key.band);
            while idx > 0 && cascade_origin_group(sorted_asc[idx - 1].0.band) == grp {
                idx -= 1;
            }
        }
        _ => {}
    }
    select_winning_candidate_from(sorted_asc, idx)
}

/// Applies a single winning [`SpecifiedDeclaration`] onto `style`.
pub fn apply_declaration(
    style: &mut ComputedStyle,
    decl: &SpecifiedDeclaration,
    parent: Option<&ComputedStyle>,
    eval: &EvalContext,
    dark_mode: bool,
) {
    if let PropertyValue::Global(kw) = decl.value {
        let should_inherit = match kw {
            GlobalKeyword::Inherit => true,
            GlobalKeyword::Initial => false,
            GlobalKeyword::Unset
            | GlobalKeyword::Revert
            | GlobalKeyword::RevertLayer
            | GlobalKeyword::RevertRule => decl.property.is_inherited(),
        };
        if should_inherit {
            if let Some(p) = parent {
                copy_property_from_parent(style, p, decl.property);
                return;
            }
        }
        reset_property_to_initial(style, decl.property);
        return;
    }

    let current_color = style.text.color;
    let effective_dark = match style.text.color_scheme_mode {
        1 => false,
        2 => true,
        _ => dark_mode,
    };
    let current_resolved = style.text.color_computed.as_deref().map_or_else(
        || crate::color::ResolvedColor::from_rgba(current_color),
        |expr| {
            expr.resolve(
                crate::color::ResolvedColor::from_rgba(current_color),
                effective_dark,
                eval,
            )
        },
    );
    match (decl.property, &decl.value) {
        (PropertyIdKind::Display, PropertyValue::Display(v)) => style.display = *v,
        (PropertyIdKind::Position, PropertyValue::Position(v)) => style.position = *v,
        (PropertyIdKind::Float, PropertyValue::Float(v)) => style.float_mode = *v,
        (PropertyIdKind::Clear, PropertyValue::Clear(v)) => style.clear_mode = *v,
        (PropertyIdKind::BoxSizing, PropertyValue::BoxSizing(v)) => style.box_sizing = *v,
        (PropertyIdKind::OverflowX, PropertyValue::Overflow(v)) => style.overflow.x = *v,
        (PropertyIdKind::OverflowY, PropertyValue::Overflow(v)) => style.overflow.y = *v,
        (PropertyIdKind::Visibility, PropertyValue::Visibility(v)) => style.visibility = *v,
        (PropertyIdKind::PointerEvents, PropertyValue::PointerEvents(v)) => {
            style.pointer_events = *v;
        }
        (PropertyIdKind::Cursor, PropertyValue::Cursor(v)) => style.cursor = *v,
        (PropertyIdKind::ZIndex, PropertyValue::ZIndex(v)) => style.z_index = *v,
        (PropertyIdKind::ZIndex, PropertyValue::MathVal(node)) => {
            let v = node.eval_scalar(eval, None);
            let n = if v.is_nan() {
                0
            } else if v.is_infinite() {
                if v.is_sign_positive() {
                    i32::MAX
                } else {
                    i32::MIN
                }
            } else {
                (v + 0.5)
                    .floor()
                    .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
            };
            style.z_index = ZIndexValue::Integer(n);
        }
        (PropertyIdKind::Opacity, PropertyValue::Number(v)) => style.opacity = v.clamp(0.0, 1.0),
        (PropertyIdKind::Opacity, PropertyValue::MathVal(node)) => {
            let v = node.eval_scalar(eval, Some((1.0, true)));
            let c = if v.is_nan() { 0.0 } else { v.clamp(0.0, 1.0) };
            style.opacity = c as f32;
        }
        (PropertyIdKind::ColorScheme, PropertyValue::Number(v)) => {
            let mode = (*v).round().clamp(0.0, 3.0) as u8;
            style.color_scheme_dark = matches!(mode, 2 | 3);
            Arc::make_mut(&mut style.text).color_scheme_mode = mode;
        }

        // Box model
        (PropertyIdKind::Width, PropertyValue::Size(v)) => {
            Arc::make_mut(&mut style.box_model).width = v.clone();
        }
        (PropertyIdKind::Height, PropertyValue::Size(v)) => {
            Arc::make_mut(&mut style.box_model).height = v.clone();
        }
        (PropertyIdKind::MinWidth, PropertyValue::Size(v)) => {
            Arc::make_mut(&mut style.box_model).min_width = v.clone();
        }
        (PropertyIdKind::MinHeight, PropertyValue::Size(v)) => {
            Arc::make_mut(&mut style.box_model).min_height = v.clone();
        }
        (PropertyIdKind::MaxWidth, PropertyValue::Size(v)) => {
            Arc::make_mut(&mut style.box_model).max_width = v.clone();
        }
        (PropertyIdKind::MaxHeight, PropertyValue::Size(v)) => {
            Arc::make_mut(&mut style.box_model).max_height = v.clone();
        }
        (PropertyIdKind::MarginTop, PropertyValue::LengthAuto(v)) => {
            Arc::make_mut(&mut style.box_model).margin.top = v.clone();
        }
        (PropertyIdKind::MarginRight, PropertyValue::LengthAuto(v)) => {
            Arc::make_mut(&mut style.box_model).margin.right = v.clone();
        }
        (PropertyIdKind::MarginBottom, PropertyValue::LengthAuto(v)) => {
            Arc::make_mut(&mut style.box_model).margin.bottom = v.clone();
        }
        (PropertyIdKind::MarginLeft, PropertyValue::LengthAuto(v)) => {
            Arc::make_mut(&mut style.box_model).margin.left = v.clone();
        }
        (PropertyIdKind::PaddingTop, PropertyValue::LengthAuto(LengthPercentageAuto::Value(v)))
        | (PropertyIdKind::PaddingTop, PropertyValue::Length(v)) => {
            Arc::make_mut(&mut style.box_model).padding.top = v.clone();
        }
        (
            PropertyIdKind::PaddingRight,
            PropertyValue::LengthAuto(LengthPercentageAuto::Value(v)),
        )
        | (PropertyIdKind::PaddingRight, PropertyValue::Length(v)) => {
            Arc::make_mut(&mut style.box_model).padding.right = v.clone();
        }
        (
            PropertyIdKind::PaddingBottom,
            PropertyValue::LengthAuto(LengthPercentageAuto::Value(v)),
        )
        | (PropertyIdKind::PaddingBottom, PropertyValue::Length(v)) => {
            Arc::make_mut(&mut style.box_model).padding.bottom = v.clone();
        }
        (
            PropertyIdKind::PaddingLeft,
            PropertyValue::LengthAuto(LengthPercentageAuto::Value(v)),
        )
        | (PropertyIdKind::PaddingLeft, PropertyValue::Length(v)) => {
            Arc::make_mut(&mut style.box_model).padding.left = v.clone();
        }
        (PropertyIdKind::Top, PropertyValue::LengthAuto(v)) => {
            Arc::make_mut(&mut style.box_model).inset.top = v.clone();
        }
        (PropertyIdKind::Right, PropertyValue::LengthAuto(v)) => {
            Arc::make_mut(&mut style.box_model).inset.right = v.clone();
        }
        (PropertyIdKind::Bottom, PropertyValue::LengthAuto(v)) => {
            Arc::make_mut(&mut style.box_model).inset.bottom = v.clone();
        }
        (PropertyIdKind::Left, PropertyValue::LengthAuto(v)) => {
            Arc::make_mut(&mut style.box_model).inset.left = v.clone();
        }
        (PropertyIdKind::BorderTopWidth, PropertyValue::Number(v)) => {
            Arc::make_mut(&mut style.box_model).border_width.top = v.max(0.0);
        }
        (PropertyIdKind::BorderTopWidth, PropertyValue::Length(lp)) => {
            Arc::make_mut(&mut style.box_model).border_width.top =
                lp.resolve_px(0.0, eval).max(0.0);
        }
        (PropertyIdKind::BorderRightWidth, PropertyValue::Number(v)) => {
            Arc::make_mut(&mut style.box_model).border_width.right = v.max(0.0);
        }
        (PropertyIdKind::BorderRightWidth, PropertyValue::Length(lp)) => {
            Arc::make_mut(&mut style.box_model).border_width.right =
                lp.resolve_px(0.0, eval).max(0.0);
        }
        (PropertyIdKind::BorderBottomWidth, PropertyValue::Number(v)) => {
            Arc::make_mut(&mut style.box_model).border_width.bottom = v.max(0.0);
        }
        (PropertyIdKind::BorderBottomWidth, PropertyValue::Length(lp)) => {
            Arc::make_mut(&mut style.box_model).border_width.bottom =
                lp.resolve_px(0.0, eval).max(0.0);
        }
        (PropertyIdKind::BorderLeftWidth, PropertyValue::Number(v)) => {
            Arc::make_mut(&mut style.box_model).border_width.left = v.max(0.0);
        }
        (PropertyIdKind::BorderLeftWidth, PropertyValue::Length(lp)) => {
            Arc::make_mut(&mut style.box_model).border_width.left =
                lp.resolve_px(0.0, eval).max(0.0);
        }

        // Visual
        (PropertyIdKind::BackgroundColor, PropertyValue::Color(c)) => {
            let vis = Arc::make_mut(&mut style.visual);
            vis.background_color = c.resolve(current_color, effective_dark);
            vis.background_color_computed = if *c == CssColor::CurrentColor {
                Some(Arc::new(crate::color::CssColorExpr::CurrentColor))
            } else {
                None
            };
        }
        (PropertyIdKind::BackgroundColor, PropertyValue::Color4(expr)) => {
            let res = expr.resolve(current_resolved, effective_dark, eval);
            let vis = Arc::make_mut(&mut style.visual);
            vis.background_color = res.to_srgb_u8();
            vis.background_color_computed = Some(Arc::new(if expr.contains_current_color() {
                (**expr).clone()
            } else {
                crate::color::CssColorExpr::Resolved(res)
            }));
        }
        (PropertyIdKind::BackgroundLayers, PropertyValue::BackgroundLayers(l)) => {
            let v = Arc::make_mut(&mut style.visual);
            v.background_layers = l.clone();
            v.background_image_raw = None;
        }
        (PropertyIdKind::BackgroundLayers, PropertyValue::BackgroundImageRaw(raw)) => {
            let v = Arc::make_mut(&mut style.visual);
            v.background_layers.clear();
            v.background_image_raw = Some(raw.clone());
        }
        (PropertyIdKind::ViewTransitionName, PropertyValue::ViewTransitionName(v)) => {
            style.effects_mut().view_transition_name = v.clone();
        }
        (PropertyIdKind::BorderTopStyle, PropertyValue::LineStyle(s)) => {
            Arc::make_mut(&mut style.visual).border_style.top = *s;
        }
        (PropertyIdKind::BorderRightStyle, PropertyValue::LineStyle(s)) => {
            Arc::make_mut(&mut style.visual).border_style.right = *s;
        }
        (PropertyIdKind::BorderBottomStyle, PropertyValue::LineStyle(s)) => {
            Arc::make_mut(&mut style.visual).border_style.bottom = *s;
        }
        (PropertyIdKind::BorderLeftStyle, PropertyValue::LineStyle(s)) => {
            Arc::make_mut(&mut style.visual).border_style.left = *s;
        }
        (
            PropertyIdKind::BorderTopColor
            | PropertyIdKind::BorderRightColor
            | PropertyIdKind::BorderBottomColor
            | PropertyIdKind::BorderLeftColor,
            PropertyValue::Color(c),
        ) => {
            let vis = Arc::make_mut(&mut style.visual);
            let idx = match decl.property {
                PropertyIdKind::BorderTopColor => {
                    vis.border_color.top = *c;
                    0
                }
                PropertyIdKind::BorderRightColor => {
                    vis.border_color.right = *c;
                    1
                }
                PropertyIdKind::BorderBottomColor => {
                    vis.border_color.bottom = *c;
                    2
                }
                _ => {
                    vis.border_color.left = *c;
                    3
                }
            };
            vis.border_color_computed[idx] = None;
        }
        (
            PropertyIdKind::BorderTopColor
            | PropertyIdKind::BorderRightColor
            | PropertyIdKind::BorderBottomColor
            | PropertyIdKind::BorderLeftColor,
            PropertyValue::Color4(expr),
        ) => {
            let res = expr.resolve(current_resolved, effective_dark, eval);
            let c = CssColor::Rgba(res.to_srgb_u8());
            let stored = Some(Arc::new(if expr.contains_current_color() {
                (**expr).clone()
            } else {
                crate::color::CssColorExpr::Resolved(res)
            }));
            let vis = Arc::make_mut(&mut style.visual);
            let idx = match decl.property {
                PropertyIdKind::BorderTopColor => {
                    vis.border_color.top = c;
                    0
                }
                PropertyIdKind::BorderRightColor => {
                    vis.border_color.right = c;
                    1
                }
                PropertyIdKind::BorderBottomColor => {
                    vis.border_color.bottom = c;
                    2
                }
                _ => {
                    vis.border_color.left = c;
                    3
                }
            };
            vis.border_color_computed[idx] = stored;
        }
        (PropertyIdKind::BorderTopLeftRadius, PropertyValue::CornerRadius(r)) => {
            Arc::make_mut(&mut style.visual).border_radius.top_left = r.clone();
        }
        (PropertyIdKind::BorderTopRightRadius, PropertyValue::CornerRadius(r)) => {
            Arc::make_mut(&mut style.visual).border_radius.top_right = r.clone();
        }
        (PropertyIdKind::BorderBottomRightRadius, PropertyValue::CornerRadius(r)) => {
            Arc::make_mut(&mut style.visual).border_radius.bottom_right = r.clone();
        }
        (PropertyIdKind::BorderBottomLeftRadius, PropertyValue::CornerRadius(r)) => {
            Arc::make_mut(&mut style.visual).border_radius.bottom_left = r.clone();
        }
        (PropertyIdKind::OutlineColor, PropertyValue::Color(c)) => {
            let vis = Arc::make_mut(&mut style.visual);
            vis.outline_color = *c;
            vis.outline_color_computed = None;
        }
        (PropertyIdKind::OutlineColor, PropertyValue::Color4(expr)) => {
            let res = expr.resolve(current_resolved, effective_dark, eval);
            let vis = Arc::make_mut(&mut style.visual);
            vis.outline_color = CssColor::Rgba(res.to_srgb_u8());
            vis.outline_color_computed = Some(Arc::new(if expr.contains_current_color() {
                (**expr).clone()
            } else {
                crate::color::CssColorExpr::Resolved(res)
            }));
        }
        (PropertyIdKind::OutlineStyle, PropertyValue::LineStyle(s)) => {
            Arc::make_mut(&mut style.visual).outline_style = *s;
        }
        (PropertyIdKind::OutlineWidth, PropertyValue::Number(w)) => {
            Arc::make_mut(&mut style.visual).outline_width = *w;
        }
        (PropertyIdKind::OutlineOffset, PropertyValue::Length(l)) => {
            Arc::make_mut(&mut style.visual).outline_offset = l.clone();
        }
        (PropertyIdKind::TextDecorationLine, PropertyValue::TextDecorationLine(l)) => {
            Arc::make_mut(&mut style.visual).text_decoration_line = *l;
        }
        (PropertyIdKind::TextDecorationColor, PropertyValue::Color(c)) => {
            let vis = Arc::make_mut(&mut style.visual);
            vis.text_decoration_color = Some(*c);
            vis.text_decoration_color_computed = None;
        }
        (PropertyIdKind::TextDecorationColor, PropertyValue::Color4(expr)) => {
            let res = expr.resolve(current_resolved, effective_dark, eval);
            let vis = Arc::make_mut(&mut style.visual);
            vis.text_decoration_color = Some(CssColor::Rgba(res.to_srgb_u8()));
            vis.text_decoration_color_computed = Some(Arc::new(if expr.contains_current_color() {
                (**expr).clone()
            } else {
                crate::color::CssColorExpr::Resolved(res)
            }));
        }
        (PropertyIdKind::UserSelect, PropertyValue::UserSelect(u)) => {
            Arc::make_mut(&mut style.visual).user_select = *u;
        }

        // Typography (inherited)
        (PropertyIdKind::Color, PropertyValue::Color(c)) => {
            let inherited = parent.map_or(ColorRgba::BLACK, |p| p.text.color);
            let txt = Arc::make_mut(&mut style.text);
            if *c == CssColor::CurrentColor {
                txt.color = inherited;
                txt.color_computed = parent.and_then(|p| p.text.color_computed.clone());
            } else {
                txt.color = c.resolve(inherited, effective_dark);
                txt.color_computed = None;
            }
        }
        (PropertyIdKind::Color, PropertyValue::Color4(expr)) => {
            let parent_resolved = parent.map_or(crate::color::ResolvedColor::BLACK_LEGACY, |p| {
                p.text.color_computed.as_deref().map_or_else(
                    || crate::color::ResolvedColor::from_rgba(p.text.color),
                    |pe| {
                        pe.resolve(
                            crate::color::ResolvedColor::from_rgba(p.text.color),
                            effective_dark,
                            eval,
                        )
                    },
                )
            });
            let res = expr.resolve(parent_resolved, effective_dark, eval);
            let txt = Arc::make_mut(&mut style.text);
            txt.color = res.to_srgb_u8();
            txt.color_computed = Some(Arc::new(crate::color::CssColorExpr::Resolved(res)));
        }
        (PropertyIdKind::FontSize, PropertyValue::Length(lp)) => {
            let px = lp.resolve_px(eval.font_size_px, eval).max(0.0);
            Arc::make_mut(&mut style.text).font_size_px = px;
        }
        (PropertyIdKind::FontWeight, PropertyValue::Number(w)) => {
            Arc::make_mut(&mut style.text).font_weight = *w;
        }
        (PropertyIdKind::FontStyle, PropertyValue::FontStyle(s)) => {
            Arc::make_mut(&mut style.text).font_style = *s;
            if let Some(eff) = &mut style.effects {
                Arc::make_mut(eff).font_style_oblique_math = None;
            }
        }
        (PropertyIdKind::FontStyle, PropertyValue::FontStyleObliqueMath(node)) => {
            let deg = node.eval_scalar(eval, None);
            let deg_f = if deg.is_finite() {
                deg.clamp(-90.0, 90.0) as f32
            } else {
                14.0
            };
            Arc::make_mut(&mut style.text).font_style = FontStyleMode::Oblique(deg_f);
            style.effects_mut().font_style_oblique_math = Some((**node).clone());
        }
        (PropertyIdKind::FontFamily, PropertyValue::FontFamily(f)) => {
            Arc::make_mut(&mut style.text).font_family = f.clone();
        }
        (PropertyIdKind::LineHeight, PropertyValue::LineHeight(lh)) => {
            Arc::make_mut(&mut style.text).line_height = lh.clone();
        }
        (PropertyIdKind::LetterSpacing, PropertyValue::Length(l)) => {
            Arc::make_mut(&mut style.text).letter_spacing = l.clone();
        }
        (PropertyIdKind::WordSpacing, PropertyValue::Length(l)) => {
            Arc::make_mut(&mut style.text).word_spacing = l.clone();
        }
        (PropertyIdKind::TextIndent, PropertyValue::Length(l)) => {
            Arc::make_mut(&mut style.text).text_indent = l.clone();
        }
        (PropertyIdKind::TextAlign, PropertyValue::TextAlign(ta)) => {
            Arc::make_mut(&mut style.text).text_align = *ta;
        }
        (PropertyIdKind::TextTransform, PropertyValue::TextTransform(tt)) => {
            Arc::make_mut(&mut style.text).text_transform = *tt;
        }
        (PropertyIdKind::WhiteSpace, PropertyValue::WhiteSpace(ws)) => {
            Arc::make_mut(&mut style.text).white_space = *ws;
        }
        (PropertyIdKind::WordBreak, PropertyValue::WordBreak(wb)) => {
            Arc::make_mut(&mut style.text).word_break = *wb;
        }
        (PropertyIdKind::OverflowWrap, PropertyValue::OverflowWrap(ow)) => {
            Arc::make_mut(&mut style.text).overflow_wrap = *ow;
        }
        (PropertyIdKind::Direction, PropertyValue::Direction(d)) => {
            Arc::make_mut(&mut style.text).direction = *d;
        }

        // Flex & Grid (allocated lazily)
        (PropertyIdKind::FlexDirection, PropertyValue::FlexDirection(v)) => {
            style.flex_grid_mut().flex_direction = *v;
        }
        (PropertyIdKind::FlexWrap, PropertyValue::FlexWrap(v)) => {
            style.flex_grid_mut().flex_wrap = *v;
        }
        (PropertyIdKind::FlexGrow, PropertyValue::Number(v)) => {
            style.flex_grid_mut().flex_grow = v.max(0.0);
        }
        (PropertyIdKind::FlexShrink, PropertyValue::Number(v)) => {
            style.flex_grid_mut().flex_shrink = v.max(0.0);
        }
        (PropertyIdKind::FlexBasis, PropertyValue::LengthAuto(v)) => {
            let fg = style.flex_grid_mut();
            fg.flex_basis = v.clone();
            fg.flex_basis_size = None;
        }
        (PropertyIdKind::FlexBasis, PropertyValue::Size(v)) => {
            let fg = style.flex_grid_mut();
            fg.flex_basis_size = Some(v.clone());
            fg.flex_basis = match v {
                SizeValue::Length(lp) => LengthPercentageAuto::Value(lp.clone()),
                _ => LengthPercentageAuto::Auto,
            };
        }
        (PropertyIdKind::Order, PropertyValue::Integer(v)) => {
            style.flex_grid_mut().order = *v;
        }
        (PropertyIdKind::AlignItems, PropertyValue::Align(v)) => {
            style.flex_grid_mut().align_items = *v;
        }
        (PropertyIdKind::AlignSelf, PropertyValue::Align(v)) => {
            style.flex_grid_mut().align_self = *v;
        }
        (PropertyIdKind::AlignContent, PropertyValue::Align(v)) => {
            style.flex_grid_mut().align_content = *v;
        }
        (PropertyIdKind::JustifyContent, PropertyValue::Align(v)) => {
            style.flex_grid_mut().justify_content = *v;
        }
        (PropertyIdKind::JustifyItems, PropertyValue::Align(v)) => {
            style.flex_grid_mut().justify_items = *v;
        }
        (PropertyIdKind::JustifySelf, PropertyValue::Align(v)) => {
            style.flex_grid_mut().justify_self = *v;
        }
        (PropertyIdKind::RowGap, PropertyValue::Length(v)) => {
            style.flex_grid_mut().row_gap = v.clone();
        }
        (PropertyIdKind::ColumnGap, PropertyValue::Length(v)) => {
            style.flex_grid_mut().column_gap = v.clone();
        }
        (PropertyIdKind::GridTemplateColumns, PropertyValue::GridTracks(v)) => {
            let fg = style.flex_grid_mut();
            fg.grid_template_columns = v.clone();
            fg.grid_template_columns_list =
                crate::grid_css::GridTemplateTrackList::from_simple_tracks(v.clone());
        }
        (PropertyIdKind::GridTemplateColumns, PropertyValue::GridTemplateTracks(v)) => {
            let fg = style.flex_grid_mut();
            fg.grid_template_columns = v.flatten_simple_tracks();
            fg.grid_template_columns_list = v.clone();
        }
        (PropertyIdKind::GridTemplateRows, PropertyValue::GridTracks(v)) => {
            let fg = style.flex_grid_mut();
            fg.grid_template_rows = v.clone();
            fg.grid_template_rows_list =
                crate::grid_css::GridTemplateTrackList::from_simple_tracks(v.clone());
        }
        (PropertyIdKind::GridTemplateRows, PropertyValue::GridTemplateTracks(v)) => {
            let fg = style.flex_grid_mut();
            fg.grid_template_rows = v.flatten_simple_tracks();
            fg.grid_template_rows_list = v.clone();
        }
        (PropertyIdKind::GridTemplateAreas, PropertyValue::GridTemplateAreas(v)) => {
            style.flex_grid_mut().grid_template_areas = v.clone();
        }
        (PropertyIdKind::GridAutoColumns, PropertyValue::GridTracks(v)) => {
            style.flex_grid_mut().grid_auto_columns = v.clone();
        }
        (PropertyIdKind::GridAutoRows, PropertyValue::GridTracks(v)) => {
            style.flex_grid_mut().grid_auto_rows = v.clone();
        }
        (PropertyIdKind::GridAutoFlow, PropertyValue::GridAutoFlow(v)) => {
            style.flex_grid_mut().grid_auto_flow = *v;
        }
        (PropertyIdKind::GridRowStart, PropertyValue::GridPlacement(v)) => {
            style.flex_grid_mut().grid_row_start = v.clone();
        }
        (PropertyIdKind::GridRowEnd, PropertyValue::GridPlacement(v)) => {
            style.flex_grid_mut().grid_row_end = v.clone();
        }
        (PropertyIdKind::GridColumnStart, PropertyValue::GridPlacement(v)) => {
            style.flex_grid_mut().grid_column_start = v.clone();
        }
        (PropertyIdKind::GridColumnEnd, PropertyValue::GridPlacement(v)) => {
            style.flex_grid_mut().grid_column_end = v.clone();
        }
        (PropertyIdKind::FitTolerance, PropertyValue::FitTolerance(v)) => {
            style.flex_grid_mut().fit_tolerance = v.clone();
        }
        (PropertyIdKind::GridLanesDirection, PropertyValue::GridLanesDirection(v)) => {
            style.flex_grid_mut().grid_lanes_direction = *v;
        }
        (PropertyIdKind::GridLanesPack, PropertyValue::GridLanesPack(v)) => {
            style.flex_grid_mut().grid_lanes_pack = *v;
        }

        // Effects (allocated lazily)
        (PropertyIdKind::BoxShadow, PropertyValue::BoxShadow(v)) => {
            style.effects_mut().box_shadow = v.clone();
        }
        (PropertyIdKind::Transform, PropertyValue::Transform(v)) => {
            let eff = style.effects_mut();
            eff.transform = v.clone();
            eff.transform_raw = None;
        }
        (PropertyIdKind::Transform, PropertyValue::TransformRaw(raw)) => {
            style.effects_mut().transform_raw = Some(raw.clone());
        }
        (PropertyIdKind::Scale, PropertyValue::Scale(s)) => {
            style.effects_mut().scale = s.clone();
        }
        (PropertyIdKind::Rotate, PropertyValue::Rotate(r)) => {
            style.effects_mut().rotate = r.clone();
        }
        (PropertyIdKind::Translate, PropertyValue::Translate(t)) => {
            style.effects_mut().translate = t.clone();
        }
        (PropertyIdKind::ImageResolution, PropertyValue::MathVal(n)) => {
            style.effects_mut().image_resolution = Some((**n).clone());
        }
        (PropertyIdKind::BackgroundPosition, PropertyValue::BgPosition(x, y)) => {
            Arc::make_mut(&mut style.visual).background_position = (x.clone(), y.clone());
        }
        (PropertyIdKind::ObjectPosition, PropertyValue::ObjectPos(pos)) => {
            Arc::make_mut(&mut style.visual).object_position = Some(pos.clone());
        }
        (PropertyIdKind::AspectRatio, PropertyValue::AspectRatioMath(ar)) => {
            style.effects_mut().aspect_ratio_math = ar
                .as_ref()
                .map(|(a, b)| ((**a).clone(), b.as_deref().cloned()));
        }
        (
            prop @ (PropertyIdKind::OffsetDistance
            | PropertyIdKind::ShapeMargin
            | PropertyIdKind::StrokeWidth
            | PropertyIdKind::StrokeDashoffset
            | PropertyIdKind::StrokeDasharray
            | PropertyIdKind::BackgroundPositionX
            | PropertyIdKind::BackgroundPositionY
            | PropertyIdKind::ScrollPaddingTop
            | PropertyIdKind::ScrollPaddingRight
            | PropertyIdKind::ScrollPaddingBottom
            | PropertyIdKind::ScrollPaddingLeft
            | PropertyIdKind::SvgCx
            | PropertyIdKind::SvgCy
            | PropertyIdKind::SvgRx
            | PropertyIdKind::SvgRy
            | PropertyIdKind::SvgX
            | PropertyIdKind::SvgY
            | PropertyIdKind::BorderImageSlice),
            PropertyValue::MathVal(n),
        ) => {
            style
                .effects_mut()
                .extra_math_props
                .insert(prop, (**n).clone());
        }
        (PropertyIdKind::FontSize, PropertyValue::MathVal(n)) => {
            let px = n.eval_scalar(eval, Some((f64::from(eval.font_size_px), false)));
            let px = if px.is_nan() {
                0.0
            } else {
                px.clamp(0.0, crate::values_math::MAX_CSS_PX)
            } as f32;
            Arc::make_mut(&mut style.text).font_size_px = px;
        }
        (PropertyIdKind::LineHeight, PropertyValue::MathVal(n)) => {
            let is_num = n
                .calc_type()
                .and_then(|t| t.apply_hint(crate::values_math::BaseDim::Length))
                .is_some_and(|t| t.dims == [0; 6] && t.percent == 0);
            let lh = if is_num {
                let v = n.eval_scalar(eval, Some((f64::from(eval.font_size_px), false)));
                LineHeightValue::Number(if v.is_nan() { 0.0 } else { v.max(0.0) as f32 })
            } else {
                let px = n.eval_scalar(eval, Some((f64::from(eval.font_size_px), false)));
                let px = if px.is_nan() {
                    0.0
                } else {
                    px.clamp(0.0, crate::values_math::MAX_CSS_PX)
                } as f32;
                LineHeightValue::Length(LengthPercentage::px(px))
            };
            Arc::make_mut(&mut style.text).line_height = lh;
        }
        (PropertyIdKind::Orphans | PropertyIdKind::Widows, PropertyValue::MathVal(n)) => {
            let v = n.eval_scalar(eval, None);
            let iv = if v.is_nan() {
                1
            } else if v.is_infinite() {
                if v.is_sign_positive() {
                    i32::MAX
                } else {
                    1
                }
            } else {
                (v + 0.5).floor().clamp(1.0, f64::from(i32::MAX)) as i32
            };
            if decl.property == PropertyIdKind::Orphans {
                Arc::make_mut(&mut style.text).orphans = iv;
            } else {
                Arc::make_mut(&mut style.text).widows = iv;
            }
        }
        (PropertyIdKind::ColumnCount, PropertyValue::ZIndex(ZIndexValue::Auto)) => {
            Arc::make_mut(&mut style.box_model).column_count = None;
        }
        (PropertyIdKind::ColumnCount, PropertyValue::MathVal(n)) => {
            let v = n.eval_scalar(eval, None);
            let iv = if v.is_nan() {
                1
            } else if v.is_infinite() {
                if v.is_sign_positive() {
                    i32::MAX
                } else {
                    1
                }
            } else {
                (v + 0.5).floor().clamp(1.0, f64::from(i32::MAX)) as i32
            };
            Arc::make_mut(&mut style.box_model).column_count = Some(iv);
        }
        (PropertyIdKind::TabSize, PropertyValue::MathVal(n)) => {
            let txt = Arc::make_mut(&mut style.text);
            if n.calc_type() == Some(crate::values_math::CalcType::NUMBER) {
                let v = n.eval_scalar(eval, None);
                txt.tab_size_num = if v.is_nan() { 0.0 } else { v.max(0.0) as f32 };
                txt.tab_size_len = None;
            } else {
                txt.tab_size_len = Some(LengthPercentage::Calc(Box::new(
                    crate::values::CalcExpr::Math(n.clone()),
                )));
            }
        }
        (PropertyIdKind::FontWeight, PropertyValue::MathVal(n)) => {
            let v = n.eval_scalar(eval, None);
            Arc::make_mut(&mut style.text).font_weight = if v.is_nan() {
                1.0
            } else {
                v.clamp(1.0, 1000.0) as f32
            };
        }
        (PropertyIdKind::FlexGrow, PropertyValue::MathVal(n)) => {
            let v = n.eval_scalar(eval, None);
            style.flex_grid_mut().flex_grow = if v.is_nan() { 0.0 } else { v.max(0.0) as f32 };
        }
        (PropertyIdKind::FlexShrink, PropertyValue::MathVal(n)) => {
            let v = n.eval_scalar(eval, None);
            style.flex_grid_mut().flex_shrink = if v.is_nan() { 0.0 } else { v.max(0.0) as f32 };
        }
        (PropertyIdKind::Order, PropertyValue::MathVal(n)) => {
            let v = n.eval_scalar(eval, None);
            let ord = if v.is_nan() {
                0
            } else if v.is_infinite() {
                if v.is_sign_positive() {
                    i32::MAX
                } else {
                    i32::MIN
                }
            } else {
                (v + 0.5)
                    .floor()
                    .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
            };
            style.flex_grid_mut().order = ord;
        }
        (
            PropertyIdKind::BorderTopWidth
            | PropertyIdKind::BorderRightWidth
            | PropertyIdKind::BorderBottomWidth
            | PropertyIdKind::BorderLeftWidth
            | PropertyIdKind::OutlineWidth,
            PropertyValue::MathVal(n),
        ) => {
            let px = n.resolve_px(0.0, eval).max(0.0);
            match decl.property {
                PropertyIdKind::BorderTopWidth => {
                    Arc::make_mut(&mut style.box_model).border_width.top = px
                }
                PropertyIdKind::BorderRightWidth => {
                    Arc::make_mut(&mut style.box_model).border_width.right = px
                }
                PropertyIdKind::BorderBottomWidth => {
                    Arc::make_mut(&mut style.box_model).border_width.bottom = px
                }
                PropertyIdKind::BorderLeftWidth => {
                    Arc::make_mut(&mut style.box_model).border_width.left = px
                }
                _ => Arc::make_mut(&mut style.visual).outline_width = px,
            }
        }
        (PropertyIdKind::TransitionDelay, PropertyValue::MathVal(n)) => {
            let ms = (n.eval_scalar(eval, None) * 1000.0) as f32;
            let eff = style.effects_mut();
            eff.transition_delay_math = Some((**n).clone());
            if eff.transitions.is_empty() {
                eff.transitions.push(default_transition_spec());
            }
            if ms.is_finite() {
                for slot in &mut eff.transitions {
                    slot.delay_ms = ms;
                }
            }
        }
        (PropertyIdKind::TransitionDuration, PropertyValue::MathVal(n)) => {
            let ms = (n.eval_scalar(eval, None) * 1000.0).max(0.0) as f32;
            let eff = style.effects_mut();
            eff.transition_duration_math = Some((**n).clone());
            if eff.transitions.is_empty() {
                eff.transitions.push(default_transition_spec());
            }
            if ms.is_finite() {
                for slot in &mut eff.transitions {
                    slot.duration_ms = ms;
                }
            }
        }
        (PropertyIdKind::AnimationDelay, PropertyValue::MathVal(n)) => {
            let ms = (n.eval_scalar(eval, None) * 1000.0) as f32;
            let eff = style.effects_mut();
            eff.animation_delay_math = Some((**n).clone());
            if eff.animations.is_empty() {
                eff.animations.push(default_animation_spec());
            }
            if ms.is_finite() {
                for slot in &mut eff.animations {
                    slot.delay_ms = ms;
                }
            }
        }
        (PropertyIdKind::AnimationDuration, PropertyValue::MathVal(n)) => {
            let ms = (n.eval_scalar(eval, None) * 1000.0).max(0.0) as f32;
            let eff = style.effects_mut();
            eff.animation_duration_math = Some((**n).clone());
            if eff.animations.is_empty() {
                eff.animations.push(default_animation_spec());
            }
            if ms.is_finite() {
                for slot in &mut eff.animations {
                    slot.duration_ms = ms;
                }
            }
        }
        (PropertyIdKind::AnimationIterationCount, PropertyValue::MathVal(n)) => {
            let cnt = n.eval_scalar(eval, None).max(0.0) as f32;
            let eff = style.effects_mut();
            eff.animation_iteration_count_math = Some((**n).clone());
            if eff.animations.is_empty() {
                eff.animations.push(default_animation_spec());
            }
            for slot in &mut eff.animations {
                slot.iteration_count = cnt;
            }
        }
        (PropertyIdKind::TransformOrigin, PropertyValue::TransformOrigin(v)) => {
            style.effects_mut().transform_origin = v.clone();
        }
        (PropertyIdKind::Filter, PropertyValue::Filter(v)) => {
            style.effects_mut().filter = v.clone();
        }
        (PropertyIdKind::ClipPath, PropertyValue::ClipPath(v)) => {
            style.effects_mut().clip_path = v.clone();
        }
        (PropertyIdKind::MixBlendMode, PropertyValue::MixBlendMode(v)) => {
            style.effects_mut().mix_blend_mode = *v;
        }
        (PropertyIdKind::InterpolateSize, PropertyValue::InterpolateSize(mode)) => {
            Arc::make_mut(&mut style.text).interpolate_size = *mode;
        }
        (PropertyIdKind::Transition, PropertyValue::Transitions(v)) => {
            style.effects_mut().transitions = v.clone();
        }
        (PropertyIdKind::TransitionProperty, PropertyValue::NameList(names)) => {
            let trs = &mut style.effects_mut().transitions;
            if trs.len() < names.len() {
                trs.resize_with(names.len(), default_transition_spec);
            } else if !names.is_empty() && trs.len() > names.len() {
                trs.truncate(names.len());
            }
            for (slot, name) in trs.iter_mut().zip(names.iter()) {
                slot.property = name.clone();
            }
        }
        (PropertyIdKind::TransitionDuration, PropertyValue::DurationList(ms)) => {
            let eff = style.effects_mut();
            eff.transition_duration_math = None;
            let trs = &mut eff.transitions;
            if trs.is_empty() && !ms.is_empty() {
                trs.resize_with(ms.len(), default_transition_spec);
            }
            for (i, slot) in trs.iter_mut().enumerate() {
                if !ms.is_empty() {
                    slot.duration_ms = ms[i % ms.len()];
                }
            }
        }
        (PropertyIdKind::TransitionDelay, PropertyValue::DurationList(ms)) => {
            let eff = style.effects_mut();
            eff.transition_delay_math = None;
            let trs = &mut eff.transitions;
            if trs.is_empty() && !ms.is_empty() {
                trs.resize_with(ms.len(), default_transition_spec);
            }
            for (i, slot) in trs.iter_mut().enumerate() {
                if !ms.is_empty() {
                    slot.delay_ms = ms[i % ms.len()];
                }
            }
        }
        (PropertyIdKind::TransitionTimingFunction, PropertyValue::TimingList(fns)) => {
            let trs = &mut style.effects_mut().transitions;
            if trs.is_empty() && !fns.is_empty() {
                trs.resize_with(fns.len(), default_transition_spec);
            }
            for (i, slot) in trs.iter_mut().enumerate() {
                if !fns.is_empty() {
                    slot.timing = fns[i % fns.len()];
                }
            }
        }
        (PropertyIdKind::TransitionBehavior, PropertyValue::TransitionBehaviorList(modes)) => {
            let trs = &mut style.effects_mut().transitions;
            if trs.is_empty() && !modes.is_empty() {
                trs.resize_with(modes.len(), default_transition_spec);
            }
            for (i, slot) in trs.iter_mut().enumerate() {
                if !modes.is_empty() {
                    slot.behavior = modes[i % modes.len()];
                }
            }
        }
        (PropertyIdKind::Animation, PropertyValue::Animations(v)) => {
            style.effects_mut().animations = v.clone();
        }
        (PropertyIdKind::AnimationName, PropertyValue::NameList(names)) => {
            let anims = &mut style.effects_mut().animations;
            if anims.len() < names.len() {
                anims.resize_with(names.len(), default_animation_spec);
            } else if !names.is_empty() && anims.len() > names.len() {
                anims.truncate(names.len());
            }
            for (slot, name) in anims.iter_mut().zip(names.iter()) {
                slot.name = name.clone();
            }
        }
        (PropertyIdKind::AnimationDuration, PropertyValue::DurationList(ms)) => {
            let eff = style.effects_mut();
            eff.animation_duration_math = None;
            let anims = &mut eff.animations;
            if anims.is_empty() && !ms.is_empty() {
                anims.resize_with(ms.len(), default_animation_spec);
            }
            for (i, slot) in anims.iter_mut().enumerate() {
                if !ms.is_empty() {
                    slot.duration_ms = ms[i % ms.len()];
                }
            }
        }
        (PropertyIdKind::AnimationDelay, PropertyValue::DurationList(ms)) => {
            let eff = style.effects_mut();
            eff.animation_delay_math = None;
            let anims = &mut eff.animations;
            if anims.is_empty() && !ms.is_empty() {
                anims.resize_with(ms.len(), default_animation_spec);
            }
            for (i, slot) in anims.iter_mut().enumerate() {
                if !ms.is_empty() {
                    slot.delay_ms = ms[i % ms.len()];
                }
            }
        }
        (PropertyIdKind::AnimationTimingFunction, PropertyValue::TimingList(fns)) => {
            let anims = &mut style.effects_mut().animations;
            if anims.is_empty() && !fns.is_empty() {
                anims.resize_with(fns.len(), default_animation_spec);
            }
            for (i, slot) in anims.iter_mut().enumerate() {
                if !fns.is_empty() {
                    slot.timing = fns[i % fns.len()];
                }
            }
        }
        (PropertyIdKind::AnimationIterationCount, PropertyValue::DurationList(vals)) => {
            let eff = style.effects_mut();
            eff.animation_iteration_count_math = None;
            let anims = &mut eff.animations;
            if anims.is_empty() && !vals.is_empty() {
                anims.resize_with(vals.len(), default_animation_spec);
            }
            for (i, slot) in anims.iter_mut().enumerate() {
                if !vals.is_empty() {
                    slot.iteration_count = vals[i % vals.len()];
                }
            }
        }
        (PropertyIdKind::AnimationDirection, PropertyValue::DirectionList(dirs)) => {
            let anims = &mut style.effects_mut().animations;
            if anims.is_empty() && !dirs.is_empty() {
                anims.resize_with(dirs.len(), default_animation_spec);
            }
            for (i, slot) in anims.iter_mut().enumerate() {
                if !dirs.is_empty() {
                    slot.direction = dirs[i % dirs.len()];
                }
            }
        }
        (PropertyIdKind::AnimationFillMode, PropertyValue::FillModeList(fills)) => {
            let anims = &mut style.effects_mut().animations;
            if anims.is_empty() && !fills.is_empty() {
                anims.resize_with(fills.len(), default_animation_spec);
            }
            for (i, slot) in anims.iter_mut().enumerate() {
                if !fills.is_empty() {
                    slot.fill_mode = fills[i % fills.len()];
                }
            }
        }
        (PropertyIdKind::AnimationPlayState, PropertyValue::PlayStateList(states)) => {
            let anims = &mut style.effects_mut().animations;
            if anims.is_empty() && !states.is_empty() {
                anims.resize_with(states.len(), default_animation_spec);
            }
            for (i, slot) in anims.iter_mut().enumerate() {
                if !states.is_empty() {
                    slot.play_state = states[i % states.len()];
                }
            }
        }
        (PropertyIdKind::AnimationComposition, PropertyValue::AnimationCompositionList(modes)) => {
            let anims = &mut style.effects_mut().animations;
            if anims.is_empty() && !modes.is_empty() {
                anims.resize_with(modes.len(), default_animation_spec);
            }
            for (i, slot) in anims.iter_mut().enumerate() {
                if !modes.is_empty() {
                    slot.composition = modes[i % modes.len()];
                }
            }
        }
        (PropertyIdKind::VerticalAlign, PropertyValue::VerticalAlign(v)) => {
            Arc::make_mut(&mut style.box_model).vertical_align = *v;
        }
        (PropertyIdKind::TextOverflow, PropertyValue::TextOverflow(v)) => {
            Arc::make_mut(&mut style.box_model).text_overflow = *v;
        }
        (PropertyIdKind::BorderCollapse, PropertyValue::BorderCollapse(v)) => {
            Arc::make_mut(&mut style.text).border_collapse = *v;
        }
        (PropertyIdKind::BorderSpacing, PropertyValue::BorderSpacing(x, y)) => {
            let txt = Arc::make_mut(&mut style.text);
            txt.border_spacing_x = x.clone();
            txt.border_spacing_y = y.clone();
        }
        (PropertyIdKind::EmptyCells, PropertyValue::EmptyCells(v)) => {
            Arc::make_mut(&mut style.text).empty_cells = *v;
        }
        (PropertyIdKind::TableLayout, PropertyValue::TableLayout(v)) => {
            Arc::make_mut(&mut style.box_model).table_layout = *v;
        }
        (PropertyIdKind::Content, PropertyValue::Content(v)) => {
            Arc::make_mut(&mut style.box_model).content = v.clone();
        }
        (PropertyIdKind::WritingMode, PropertyValue::WritingMode(v)) => {
            Arc::make_mut(&mut style.text).writing_mode = *v;
        }
        (PropertyIdKind::TextOrientation, PropertyValue::TextOrientation(v)) => {
            Arc::make_mut(&mut style.text).text_orientation = *v;
        }
        (PropertyIdKind::CaptionSide, PropertyValue::CaptionSide(v)) => {
            Arc::make_mut(&mut style.text).caption_side = *v;
        }
        (PropertyIdKind::Resize, PropertyValue::Resize(v)) => {
            Arc::make_mut(&mut style.visual).resize = *v;
        }
        (PropertyIdKind::OverscrollBehaviorX, PropertyValue::OverscrollBehavior(v)) => {
            Arc::make_mut(&mut style.visual).overscroll_behavior_x = *v;
        }
        (PropertyIdKind::OverscrollBehaviorY, PropertyValue::OverscrollBehavior(v)) => {
            Arc::make_mut(&mut style.visual).overscroll_behavior_y = *v;
        }
        (PropertyIdKind::Overlay, PropertyValue::Overlay(v)) => {
            style.overlay = *v;
        }
        (PropertyIdKind::ReadingFlow, PropertyValue::ReadingFlow(v)) => {
            style.effects_mut().reading_flow = *v;
        }
        (PropertyIdKind::ReadingOrder, PropertyValue::MathVal(n)) => {
            let v = n.eval_scalar(eval, None);
            if v.is_finite() {
                style.effects_mut().reading_order = (v + 0.5).floor() as i32;
            }
        }
        (PropertyIdKind::TextJustify, PropertyValue::TextJustify(v)) => {
            Arc::make_mut(&mut style.text).text_justify = *v;
        }
        (PropertyIdKind::TextAlignLast, PropertyValue::TextAlignLast(v)) => {
            Arc::make_mut(&mut style.text).text_align_last = *v;
        }
        (PropertyIdKind::HyphenateCharacter, PropertyValue::HyphenateChar(v)) => {
            Arc::make_mut(&mut style.text).hyphenate_character = v.clone();
        }
        (PropertyIdKind::HyphenateLimitLines, PropertyValue::HyphenateLimitLines(v)) => {
            Arc::make_mut(&mut style.text).hyphenate_limit_lines = *v;
        }
        (PropertyIdKind::Contain, PropertyValue::Contain(bits)) => {
            style.effects_mut().contain = *bits;
        }
        _ => {}
    }
}

fn copy_property_from_parent(
    style: &mut ComputedStyle,
    parent: &ComputedStyle,
    prop: PropertyIdKind,
) {
    match prop {
        PropertyIdKind::Display => style.display = parent.display,
        PropertyIdKind::Position => style.position = parent.position,
        PropertyIdKind::Float => style.float_mode = parent.float_mode,
        PropertyIdKind::Clear => style.clear_mode = parent.clear_mode,
        PropertyIdKind::BoxSizing => style.box_sizing = parent.box_sizing,
        PropertyIdKind::OverflowX => style.overflow.x = parent.overflow.x,
        PropertyIdKind::OverflowY => style.overflow.y = parent.overflow.y,
        PropertyIdKind::Visibility => style.visibility = parent.visibility,
        PropertyIdKind::PointerEvents => style.pointer_events = parent.pointer_events,
        PropertyIdKind::Cursor => style.cursor = parent.cursor,
        PropertyIdKind::ZIndex => style.z_index = parent.z_index,
        PropertyIdKind::Opacity => style.opacity = parent.opacity,
        PropertyIdKind::Color => {
            let txt = Arc::make_mut(&mut style.text);
            txt.color = parent.text.color;
            txt.color_computed = parent.text.color_computed.clone();
        }
        PropertyIdKind::BackgroundColor => {
            let vis = Arc::make_mut(&mut style.visual);
            vis.background_color = parent.visual.background_color;
            vis.background_color_computed = parent.visual.background_color_computed.clone();
        }
        PropertyIdKind::FontSize => {
            Arc::make_mut(&mut style.text).font_size_px = parent.text.font_size_px;
        }
        PropertyIdKind::FontWeight => {
            Arc::make_mut(&mut style.text).font_weight = parent.text.font_weight;
        }
        PropertyIdKind::FontStyle => {
            Arc::make_mut(&mut style.text).font_style = parent.text.font_style;
        }
        PropertyIdKind::FontFamily => {
            Arc::make_mut(&mut style.text).font_family = parent.text.font_family.clone();
        }
        PropertyIdKind::LineHeight => {
            Arc::make_mut(&mut style.text).line_height = parent.text.line_height.clone();
        }
        PropertyIdKind::TextAlign => {
            Arc::make_mut(&mut style.text).text_align = parent.text.text_align;
        }
        PropertyIdKind::WhiteSpace => {
            Arc::make_mut(&mut style.text).white_space = parent.text.white_space;
        }
        PropertyIdKind::Direction => {
            Arc::make_mut(&mut style.text).direction = parent.text.direction;
        }
        PropertyIdKind::WritingMode => {
            Arc::make_mut(&mut style.text).writing_mode = parent.text.writing_mode;
        }
        PropertyIdKind::TextOrientation => {
            Arc::make_mut(&mut style.text).text_orientation = parent.text.text_orientation;
        }
        PropertyIdKind::CaptionSide => {
            Arc::make_mut(&mut style.text).caption_side = parent.text.caption_side;
        }
        PropertyIdKind::BorderCollapse => {
            Arc::make_mut(&mut style.text).border_collapse = parent.text.border_collapse;
        }
        PropertyIdKind::BorderSpacing => {
            let txt = Arc::make_mut(&mut style.text);
            txt.border_spacing_x = parent.text.border_spacing_x.clone();
            txt.border_spacing_y = parent.text.border_spacing_y.clone();
        }
        PropertyIdKind::EmptyCells => {
            Arc::make_mut(&mut style.text).empty_cells = parent.text.empty_cells;
        }
        PropertyIdKind::TableLayout => {
            Arc::make_mut(&mut style.box_model).table_layout = parent.box_model.table_layout;
        }
        PropertyIdKind::VerticalAlign => {
            Arc::make_mut(&mut style.box_model).vertical_align = parent.box_model.vertical_align;
        }
        PropertyIdKind::InterpolateSize => {
            Arc::make_mut(&mut style.text).interpolate_size = parent.text.interpolate_size;
        }
        PropertyIdKind::Resize => {
            Arc::make_mut(&mut style.visual).resize = parent.visual.resize;
        }
        PropertyIdKind::OverscrollBehaviorX => {
            Arc::make_mut(&mut style.visual).overscroll_behavior_x =
                parent.visual.overscroll_behavior_x;
        }
        PropertyIdKind::OverscrollBehaviorY => {
            Arc::make_mut(&mut style.visual).overscroll_behavior_y =
                parent.visual.overscroll_behavior_y;
        }
        PropertyIdKind::Width => {
            Arc::make_mut(&mut style.box_model).width = parent.box_model.width.clone();
        }
        PropertyIdKind::Height => {
            Arc::make_mut(&mut style.box_model).height = parent.box_model.height.clone();
        }
        PropertyIdKind::MinWidth => {
            Arc::make_mut(&mut style.box_model).min_width = parent.box_model.min_width.clone();
        }
        PropertyIdKind::MinHeight => {
            Arc::make_mut(&mut style.box_model).min_height = parent.box_model.min_height.clone();
        }
        PropertyIdKind::MaxWidth => {
            Arc::make_mut(&mut style.box_model).max_width = parent.box_model.max_width.clone();
        }
        PropertyIdKind::MaxHeight => {
            Arc::make_mut(&mut style.box_model).max_height = parent.box_model.max_height.clone();
        }
        PropertyIdKind::MarginTop => {
            Arc::make_mut(&mut style.box_model).margin.top = parent.box_model.margin.top.clone();
        }
        PropertyIdKind::MarginRight => {
            Arc::make_mut(&mut style.box_model).margin.right =
                parent.box_model.margin.right.clone();
        }
        PropertyIdKind::MarginBottom => {
            Arc::make_mut(&mut style.box_model).margin.bottom =
                parent.box_model.margin.bottom.clone();
        }
        PropertyIdKind::MarginLeft => {
            Arc::make_mut(&mut style.box_model).margin.left = parent.box_model.margin.left.clone();
        }
        PropertyIdKind::PaddingTop => {
            Arc::make_mut(&mut style.box_model).padding.top = parent.box_model.padding.top.clone();
        }
        PropertyIdKind::PaddingRight => {
            Arc::make_mut(&mut style.box_model).padding.right =
                parent.box_model.padding.right.clone();
        }
        PropertyIdKind::PaddingBottom => {
            Arc::make_mut(&mut style.box_model).padding.bottom =
                parent.box_model.padding.bottom.clone();
        }
        PropertyIdKind::PaddingLeft => {
            Arc::make_mut(&mut style.box_model).padding.left =
                parent.box_model.padding.left.clone();
        }
        PropertyIdKind::Top => {
            Arc::make_mut(&mut style.box_model).inset.top = parent.box_model.inset.top.clone();
        }
        PropertyIdKind::Right => {
            Arc::make_mut(&mut style.box_model).inset.right = parent.box_model.inset.right.clone();
        }
        PropertyIdKind::Bottom => {
            Arc::make_mut(&mut style.box_model).inset.bottom =
                parent.box_model.inset.bottom.clone();
        }
        PropertyIdKind::Left => {
            Arc::make_mut(&mut style.box_model).inset.left = parent.box_model.inset.left.clone();
        }
        PropertyIdKind::BorderTopWidth => {
            Arc::make_mut(&mut style.box_model).border_width.top =
                parent.box_model.border_width.top;
        }
        PropertyIdKind::BorderRightWidth => {
            Arc::make_mut(&mut style.box_model).border_width.right =
                parent.box_model.border_width.right;
        }
        PropertyIdKind::BorderBottomWidth => {
            Arc::make_mut(&mut style.box_model).border_width.bottom =
                parent.box_model.border_width.bottom;
        }
        PropertyIdKind::BorderLeftWidth => {
            Arc::make_mut(&mut style.box_model).border_width.left =
                parent.box_model.border_width.left;
        }
        PropertyIdKind::BorderTopStyle => {
            Arc::make_mut(&mut style.visual).border_style.top = parent.visual.border_style.top;
        }
        PropertyIdKind::BorderRightStyle => {
            Arc::make_mut(&mut style.visual).border_style.right = parent.visual.border_style.right;
        }
        PropertyIdKind::BorderBottomStyle => {
            Arc::make_mut(&mut style.visual).border_style.bottom =
                parent.visual.border_style.bottom;
        }
        PropertyIdKind::BorderLeftStyle => {
            Arc::make_mut(&mut style.visual).border_style.left = parent.visual.border_style.left;
        }
        PropertyIdKind::BorderTopColor => {
            let vis = Arc::make_mut(&mut style.visual);
            vis.border_color.top = parent.visual.border_color.top;
            vis.border_color_computed[0] = parent.visual.border_color_computed[0].clone();
        }
        PropertyIdKind::BorderRightColor => {
            let vis = Arc::make_mut(&mut style.visual);
            vis.border_color.right = parent.visual.border_color.right;
            vis.border_color_computed[1] = parent.visual.border_color_computed[1].clone();
        }
        PropertyIdKind::BorderBottomColor => {
            let vis = Arc::make_mut(&mut style.visual);
            vis.border_color.bottom = parent.visual.border_color.bottom;
            vis.border_color_computed[2] = parent.visual.border_color_computed[2].clone();
        }
        PropertyIdKind::BorderLeftColor => {
            let vis = Arc::make_mut(&mut style.visual);
            vis.border_color.left = parent.visual.border_color.left;
            vis.border_color_computed[3] = parent.visual.border_color_computed[3].clone();
        }
        PropertyIdKind::BorderTopLeftRadius => {
            Arc::make_mut(&mut style.visual).border_radius.top_left =
                parent.visual.border_radius.top_left.clone();
        }
        PropertyIdKind::BorderTopRightRadius => {
            Arc::make_mut(&mut style.visual).border_radius.top_right =
                parent.visual.border_radius.top_right.clone();
        }
        PropertyIdKind::BorderBottomRightRadius => {
            Arc::make_mut(&mut style.visual).border_radius.bottom_right =
                parent.visual.border_radius.bottom_right.clone();
        }
        PropertyIdKind::BorderBottomLeftRadius => {
            Arc::make_mut(&mut style.visual).border_radius.bottom_left =
                parent.visual.border_radius.bottom_left.clone();
        }
        PropertyIdKind::Overlay => style.overlay = parent.overlay,
        PropertyIdKind::ReadingFlow => {
            style.effects_mut().reading_flow = parent
                .effects
                .as_ref()
                .map_or(ReadingFlowMode::Normal, |e| e.reading_flow);
        }
        PropertyIdKind::ReadingOrder => {
            style.effects_mut().reading_order =
                parent.effects.as_ref().map_or(0, |e| e.reading_order);
        }
        PropertyIdKind::TextJustify => {
            Arc::make_mut(&mut style.text).text_justify = parent.text.text_justify;
        }
        PropertyIdKind::TextAlignLast => {
            Arc::make_mut(&mut style.text).text_align_last = parent.text.text_align_last;
        }
        PropertyIdKind::HyphenateCharacter => {
            Arc::make_mut(&mut style.text).hyphenate_character =
                parent.text.hyphenate_character.clone();
        }
        PropertyIdKind::HyphenateLimitLines => {
            Arc::make_mut(&mut style.text).hyphenate_limit_lines =
                parent.text.hyphenate_limit_lines;
        }
        PropertyIdKind::Contain => {
            style.effects_mut().contain = parent.effects.as_ref().map_or(0, |e| e.contain);
        }
        PropertyIdKind::FlexDirection => {
            style.flex_grid_mut().flex_direction = parent
                .flex_grid
                .as_ref()
                .map_or(FlexDirectionMode::Row, |f| f.flex_direction);
        }
        PropertyIdKind::GridTemplateColumns => {
            let list = parent
                .flex_grid
                .as_ref()
                .map_or(crate::grid_css::GridTemplateTrackList::None, |f| {
                    f.grid_template_columns_list.clone()
                });
            let fg = style.flex_grid_mut();
            fg.grid_template_columns = list.flatten_simple_tracks();
            fg.grid_template_columns_list = list;
        }
        PropertyIdKind::GridTemplateRows => {
            let list = parent
                .flex_grid
                .as_ref()
                .map_or(crate::grid_css::GridTemplateTrackList::None, |f| {
                    f.grid_template_rows_list.clone()
                });
            let fg = style.flex_grid_mut();
            fg.grid_template_rows = list.flatten_simple_tracks();
            fg.grid_template_rows_list = list;
        }
        PropertyIdKind::GridTemplateAreas => {
            style.flex_grid_mut().grid_template_areas = parent
                .flex_grid
                .as_ref()
                .map_or_else(GridTemplateAreasValue::default, |f| {
                    f.grid_template_areas.clone()
                });
        }
        PropertyIdKind::GridAutoColumns => {
            style.flex_grid_mut().grid_auto_columns = parent
                .flex_grid
                .as_ref()
                .map_or_else(Vec::new, |f| f.grid_auto_columns.clone());
        }
        PropertyIdKind::GridAutoRows => {
            style.flex_grid_mut().grid_auto_rows = parent
                .flex_grid
                .as_ref()
                .map_or_else(Vec::new, |f| f.grid_auto_rows.clone());
        }
        PropertyIdKind::GridAutoFlow => {
            style.flex_grid_mut().grid_auto_flow = parent
                .flex_grid
                .as_ref()
                .map_or(GridAutoFlowMode::Row, |f| f.grid_auto_flow);
        }
        PropertyIdKind::GridRowStart => {
            style.flex_grid_mut().grid_row_start = parent
                .flex_grid
                .as_ref()
                .map_or(GridPlacementValue::Auto, |f| f.grid_row_start.clone());
        }
        PropertyIdKind::GridRowEnd => {
            style.flex_grid_mut().grid_row_end = parent
                .flex_grid
                .as_ref()
                .map_or(GridPlacementValue::Auto, |f| f.grid_row_end.clone());
        }
        PropertyIdKind::GridColumnStart => {
            style.flex_grid_mut().grid_column_start = parent
                .flex_grid
                .as_ref()
                .map_or(GridPlacementValue::Auto, |f| f.grid_column_start.clone());
        }
        PropertyIdKind::GridColumnEnd => {
            style.flex_grid_mut().grid_column_end = parent
                .flex_grid
                .as_ref()
                .map_or(GridPlacementValue::Auto, |f| f.grid_column_end.clone());
        }
        PropertyIdKind::FitTolerance => {
            style.flex_grid_mut().fit_tolerance = parent
                .flex_grid
                .as_ref()
                .map_or(crate::grid_css::FitToleranceValue::Normal, |f| {
                    f.fit_tolerance.clone()
                });
        }
        PropertyIdKind::GridLanesDirection => {
            style.flex_grid_mut().grid_lanes_direction = parent
                .flex_grid
                .as_ref()
                .map_or(crate::grid_css::GridLanesDirectionMode::Normal, |f| {
                    f.grid_lanes_direction
                });
        }
        PropertyIdKind::GridLanesPack => {
            style.flex_grid_mut().grid_lanes_pack = parent
                .flex_grid
                .as_ref()
                .map_or(crate::grid_css::GridLanesPackMode::Normal, |f| {
                    f.grid_lanes_pack
                });
        }
        PropertyIdKind::FlexWrap => {
            style.flex_grid_mut().flex_wrap = parent
                .flex_grid
                .as_ref()
                .map_or(FlexWrapMode::NoWrap, |f| f.flex_wrap);
        }
        PropertyIdKind::FlexGrow => {
            style.flex_grid_mut().flex_grow =
                parent.flex_grid.as_ref().map_or(0.0, |f| f.flex_grow);
        }
        PropertyIdKind::FlexShrink => {
            style.flex_grid_mut().flex_shrink =
                parent.flex_grid.as_ref().map_or(1.0, |f| f.flex_shrink);
        }
        PropertyIdKind::FlexBasis => {
            let fg = style.flex_grid_mut();
            if let Some(pf) = parent.flex_grid.as_ref() {
                fg.flex_basis = pf.flex_basis.clone();
                fg.flex_basis_size = pf.flex_basis_size.clone();
            } else {
                fg.flex_basis = LengthPercentageAuto::Auto;
                fg.flex_basis_size = None;
            }
        }
        PropertyIdKind::Order => {
            style.flex_grid_mut().order = parent.flex_grid.as_ref().map_or(0, |f| f.order);
        }
        PropertyIdKind::AlignItems => {
            style.flex_grid_mut().align_items = parent
                .flex_grid
                .as_ref()
                .map_or(AlignValue::Normal, |f| f.align_items);
        }
        PropertyIdKind::AlignSelf => {
            style.flex_grid_mut().align_self = parent
                .flex_grid
                .as_ref()
                .map_or(AlignValue::Auto, |f| f.align_self);
        }
        PropertyIdKind::AlignContent => {
            style.flex_grid_mut().align_content = parent
                .flex_grid
                .as_ref()
                .map_or(AlignValue::Normal, |f| f.align_content);
        }
        PropertyIdKind::JustifyContent => {
            style.flex_grid_mut().justify_content = parent
                .flex_grid
                .as_ref()
                .map_or(AlignValue::Normal, |f| f.justify_content);
        }
        PropertyIdKind::JustifyItems => {
            style.flex_grid_mut().justify_items = parent
                .flex_grid
                .as_ref()
                .map_or(AlignValue::Normal, |f| f.justify_items);
        }
        PropertyIdKind::JustifySelf => {
            style.flex_grid_mut().justify_self = parent
                .flex_grid
                .as_ref()
                .map_or(AlignValue::Auto, |f| f.justify_self);
        }
        _ => {}
    }
}

fn reset_property_to_initial(style: &mut ComputedStyle, prop: PropertyIdKind) {
    let init = ComputedStyle::default();
    match prop {
        PropertyIdKind::Display => style.display = init.display,
        PropertyIdKind::Position => style.position = init.position,
        PropertyIdKind::Float => style.float_mode = init.float_mode,
        PropertyIdKind::Clear => style.clear_mode = init.clear_mode,
        PropertyIdKind::BoxSizing => style.box_sizing = init.box_sizing,
        PropertyIdKind::OverflowX => style.overflow.x = OverflowMode::Visible,
        PropertyIdKind::OverflowY => style.overflow.y = OverflowMode::Visible,
        PropertyIdKind::Visibility => style.visibility = init.visibility,
        PropertyIdKind::PointerEvents => style.pointer_events = init.pointer_events,
        PropertyIdKind::Cursor => style.cursor = init.cursor,
        PropertyIdKind::ZIndex => style.z_index = ZIndexValue::Auto,
        PropertyIdKind::Opacity => style.opacity = 1.0,
        PropertyIdKind::Color => {
            let txt = Arc::make_mut(&mut style.text);
            txt.color = init.text.color;
            txt.color_computed = None;
        }
        PropertyIdKind::BackgroundColor => {
            let vis = Arc::make_mut(&mut style.visual);
            vis.background_color = ColorRgba::TRANSPARENT;
            vis.background_color_computed = None;
        }
        PropertyIdKind::FontSize => {
            Arc::make_mut(&mut style.text).font_size_px = init.text.font_size_px;
        }
        PropertyIdKind::FontWeight => {
            Arc::make_mut(&mut style.text).font_weight = init.text.font_weight;
        }
        PropertyIdKind::FontStyle => {
            Arc::make_mut(&mut style.text).font_style = init.text.font_style;
            if let Some(eff) = &mut style.effects {
                Arc::make_mut(eff).font_style_oblique_math = None;
            }
        }
        PropertyIdKind::BackgroundLayers => {
            let v = Arc::make_mut(&mut style.visual);
            v.background_layers.clear();
            v.background_image_raw = None;
        }
        PropertyIdKind::ViewTransitionName => {
            if let Some(eff) = &mut style.effects {
                Arc::make_mut(eff).view_transition_name = None;
            }
        }
        PropertyIdKind::FontFamily => {
            Arc::make_mut(&mut style.text).font_family = init.text.font_family.clone();
        }
        PropertyIdKind::LineHeight => {
            Arc::make_mut(&mut style.text).line_height = init.text.line_height.clone();
        }
        PropertyIdKind::LetterSpacing => {
            Arc::make_mut(&mut style.text).letter_spacing = init.text.letter_spacing.clone();
        }
        PropertyIdKind::WordSpacing => {
            Arc::make_mut(&mut style.text).word_spacing = init.text.word_spacing.clone();
        }
        PropertyIdKind::TextIndent => {
            Arc::make_mut(&mut style.text).text_indent = init.text.text_indent.clone();
        }
        PropertyIdKind::TextAlign => {
            Arc::make_mut(&mut style.text).text_align = init.text.text_align;
        }
        PropertyIdKind::TextTransform => {
            Arc::make_mut(&mut style.text).text_transform = init.text.text_transform;
        }
        PropertyIdKind::WhiteSpace => {
            Arc::make_mut(&mut style.text).white_space = init.text.white_space;
        }
        PropertyIdKind::WordBreak => {
            Arc::make_mut(&mut style.text).word_break = init.text.word_break;
        }
        PropertyIdKind::OverflowWrap => {
            Arc::make_mut(&mut style.text).overflow_wrap = init.text.overflow_wrap;
        }
        PropertyIdKind::BorderCollapse => {
            Arc::make_mut(&mut style.text).border_collapse = BorderCollapseMode::Separate;
        }
        PropertyIdKind::BorderSpacing => {
            let txt = Arc::make_mut(&mut style.text);
            txt.border_spacing_x = LengthPercentage::Zero;
            txt.border_spacing_y = LengthPercentage::Zero;
        }
        PropertyIdKind::EmptyCells => {
            Arc::make_mut(&mut style.text).empty_cells = EmptyCellsMode::Show;
        }
        PropertyIdKind::Direction => {
            Arc::make_mut(&mut style.text).direction = DirectionMode::Ltr;
        }
        PropertyIdKind::WritingMode => {
            Arc::make_mut(&mut style.text).writing_mode = WritingMode::HorizontalTb;
        }
        PropertyIdKind::TextOrientation => {
            Arc::make_mut(&mut style.text).text_orientation = TextOrientationMode::Mixed;
        }
        PropertyIdKind::CaptionSide => {
            Arc::make_mut(&mut style.text).caption_side = CaptionSideMode::Top;
        }
        PropertyIdKind::Resize => {
            Arc::make_mut(&mut style.visual).resize = ResizeMode::None;
        }
        PropertyIdKind::OverscrollBehaviorX => {
            Arc::make_mut(&mut style.visual).overscroll_behavior_x = OverscrollBehaviorMode::Auto;
        }
        PropertyIdKind::OverscrollBehaviorY => {
            Arc::make_mut(&mut style.visual).overscroll_behavior_y = OverscrollBehaviorMode::Auto;
        }
        PropertyIdKind::Width => Arc::make_mut(&mut style.box_model).width = SizeValue::Auto,
        PropertyIdKind::Height => Arc::make_mut(&mut style.box_model).height = SizeValue::Auto,
        PropertyIdKind::MinWidth => {
            Arc::make_mut(&mut style.box_model).min_width = SizeValue::Auto;
        }
        PropertyIdKind::MinHeight => {
            Arc::make_mut(&mut style.box_model).min_height = SizeValue::Auto;
        }
        PropertyIdKind::MaxWidth => {
            Arc::make_mut(&mut style.box_model).max_width = SizeValue::None;
        }
        PropertyIdKind::MaxHeight => {
            Arc::make_mut(&mut style.box_model).max_height = SizeValue::None;
        }
        PropertyIdKind::MarginTop => {
            Arc::make_mut(&mut style.box_model).margin.top =
                LengthPercentageAuto::Value(LengthPercentage::Zero);
        }
        PropertyIdKind::MarginRight => {
            Arc::make_mut(&mut style.box_model).margin.right =
                LengthPercentageAuto::Value(LengthPercentage::Zero);
        }
        PropertyIdKind::MarginBottom => {
            Arc::make_mut(&mut style.box_model).margin.bottom =
                LengthPercentageAuto::Value(LengthPercentage::Zero);
        }
        PropertyIdKind::MarginLeft => {
            Arc::make_mut(&mut style.box_model).margin.left =
                LengthPercentageAuto::Value(LengthPercentage::Zero);
        }
        PropertyIdKind::PaddingTop => {
            Arc::make_mut(&mut style.box_model).padding.top = LengthPercentage::Zero;
        }
        PropertyIdKind::PaddingRight => {
            Arc::make_mut(&mut style.box_model).padding.right = LengthPercentage::Zero;
        }
        PropertyIdKind::PaddingBottom => {
            Arc::make_mut(&mut style.box_model).padding.bottom = LengthPercentage::Zero;
        }
        PropertyIdKind::PaddingLeft => {
            Arc::make_mut(&mut style.box_model).padding.left = LengthPercentage::Zero;
        }
        PropertyIdKind::Top => {
            Arc::make_mut(&mut style.box_model).inset.top = LengthPercentageAuto::Auto;
        }
        PropertyIdKind::Right => {
            Arc::make_mut(&mut style.box_model).inset.right = LengthPercentageAuto::Auto;
        }
        PropertyIdKind::Bottom => {
            Arc::make_mut(&mut style.box_model).inset.bottom = LengthPercentageAuto::Auto;
        }
        PropertyIdKind::Left => {
            Arc::make_mut(&mut style.box_model).inset.left = LengthPercentageAuto::Auto;
        }
        PropertyIdKind::BorderTopWidth => {
            Arc::make_mut(&mut style.box_model).border_width.top = 3.0;
        }
        PropertyIdKind::BorderRightWidth => {
            Arc::make_mut(&mut style.box_model).border_width.right = 3.0;
        }
        PropertyIdKind::BorderBottomWidth => {
            Arc::make_mut(&mut style.box_model).border_width.bottom = 3.0;
        }
        PropertyIdKind::BorderLeftWidth => {
            Arc::make_mut(&mut style.box_model).border_width.left = 3.0;
        }
        PropertyIdKind::BorderTopStyle => {
            Arc::make_mut(&mut style.visual).border_style.top = LineStyle::None;
        }
        PropertyIdKind::BorderRightStyle => {
            Arc::make_mut(&mut style.visual).border_style.right = LineStyle::None;
        }
        PropertyIdKind::BorderBottomStyle => {
            Arc::make_mut(&mut style.visual).border_style.bottom = LineStyle::None;
        }
        PropertyIdKind::BorderLeftStyle => {
            Arc::make_mut(&mut style.visual).border_style.left = LineStyle::None;
        }
        PropertyIdKind::BorderTopColor => {
            Arc::make_mut(&mut style.visual).border_color.top = CssColor::CurrentColor;
        }
        PropertyIdKind::BorderRightColor => {
            Arc::make_mut(&mut style.visual).border_color.right = CssColor::CurrentColor;
        }
        PropertyIdKind::BorderBottomColor => {
            Arc::make_mut(&mut style.visual).border_color.bottom = CssColor::CurrentColor;
        }
        PropertyIdKind::BorderLeftColor => {
            Arc::make_mut(&mut style.visual).border_color.left = CssColor::CurrentColor;
        }
        PropertyIdKind::BorderTopLeftRadius => {
            Arc::make_mut(&mut style.visual).border_radius.top_left =
                init.visual.border_radius.top_left.clone();
        }
        PropertyIdKind::BorderTopRightRadius => {
            Arc::make_mut(&mut style.visual).border_radius.top_right =
                init.visual.border_radius.top_right.clone();
        }
        PropertyIdKind::BorderBottomRightRadius => {
            Arc::make_mut(&mut style.visual).border_radius.bottom_right =
                init.visual.border_radius.bottom_right.clone();
        }
        PropertyIdKind::BorderBottomLeftRadius => {
            Arc::make_mut(&mut style.visual).border_radius.bottom_left =
                init.visual.border_radius.bottom_left.clone();
        }
        PropertyIdKind::Scale => style.effects_mut().scale = crate::values_math::ScaleSpec::None,
        PropertyIdKind::Rotate => style.effects_mut().rotate = crate::values_math::RotateSpec::None,
        PropertyIdKind::Translate => {
            style.effects_mut().translate = crate::values_math::TranslateSpec::None
        }
        PropertyIdKind::ImageResolution => style.effects_mut().image_resolution = None,
        PropertyIdKind::Transform => {
            let e = style.effects_mut();
            e.transform.clear();
            e.transform_raw = None;
        }
        PropertyIdKind::BackgroundPosition => {
            Arc::make_mut(&mut style.visual).background_position = (
                LengthPercentage::Percent(0.0),
                LengthPercentage::Percent(0.0),
            );
        }
        PropertyIdKind::ObjectPosition => {
            Arc::make_mut(&mut style.visual).object_position = None;
        }
        PropertyIdKind::AspectRatio => {
            if let Some(eff) = &mut style.effects {
                Arc::make_mut(eff).aspect_ratio_math = None;
            }
        }
        prop @ (PropertyIdKind::OffsetDistance
        | PropertyIdKind::ShapeMargin
        | PropertyIdKind::StrokeWidth
        | PropertyIdKind::StrokeDashoffset
        | PropertyIdKind::StrokeDasharray
        | PropertyIdKind::BackgroundPositionX
        | PropertyIdKind::BackgroundPositionY
        | PropertyIdKind::ScrollPaddingTop
        | PropertyIdKind::ScrollPaddingRight
        | PropertyIdKind::ScrollPaddingBottom
        | PropertyIdKind::ScrollPaddingLeft
        | PropertyIdKind::SvgCx
        | PropertyIdKind::SvgCy
        | PropertyIdKind::SvgRx
        | PropertyIdKind::SvgRy
        | PropertyIdKind::SvgX
        | PropertyIdKind::SvgY
        | PropertyIdKind::BorderImageSlice) => {
            if let Some(eff) = &mut style.effects {
                Arc::make_mut(eff).extra_math_props.remove(&prop);
            }
        }
        PropertyIdKind::InterpolateSize => {
            Arc::make_mut(&mut style.text).interpolate_size = InterpolateSizeMode::NumericOnly;
        }
        PropertyIdKind::TransitionBehavior => {
            for tr in &mut style.effects_mut().transitions {
                tr.behavior = TransitionBehaviorMode::Normal;
            }
        }
        PropertyIdKind::AnimationComposition => {
            for a in &mut style.effects_mut().animations {
                a.composition = AnimationCompositionMode::Replace;
            }
        }
        PropertyIdKind::FlexBasis => {
            let fg = style.flex_grid_mut();
            fg.flex_basis = LengthPercentageAuto::Auto;
            fg.flex_basis_size = None;
        }
        PropertyIdKind::FlexDirection => {
            style.flex_grid_mut().flex_direction = FlexDirectionMode::Row;
        }
        PropertyIdKind::GridTemplateColumns => {
            let fg = style.flex_grid_mut();
            fg.grid_template_columns.clear();
            fg.grid_template_columns_list = crate::grid_css::GridTemplateTrackList::None;
        }
        PropertyIdKind::GridTemplateRows => {
            let fg = style.flex_grid_mut();
            fg.grid_template_rows.clear();
            fg.grid_template_rows_list = crate::grid_css::GridTemplateTrackList::None;
        }
        PropertyIdKind::GridTemplateAreas => {
            style.flex_grid_mut().grid_template_areas = GridTemplateAreasValue::default();
        }
        PropertyIdKind::GridAutoColumns => {
            style.flex_grid_mut().grid_auto_columns.clear();
        }
        PropertyIdKind::GridAutoRows => {
            style.flex_grid_mut().grid_auto_rows.clear();
        }
        PropertyIdKind::GridAutoFlow => {
            style.flex_grid_mut().grid_auto_flow = GridAutoFlowMode::Row;
        }
        PropertyIdKind::GridRowStart => {
            style.flex_grid_mut().grid_row_start = GridPlacementValue::Auto;
        }
        PropertyIdKind::GridRowEnd => {
            style.flex_grid_mut().grid_row_end = GridPlacementValue::Auto;
        }
        PropertyIdKind::GridColumnStart => {
            style.flex_grid_mut().grid_column_start = GridPlacementValue::Auto;
        }
        PropertyIdKind::GridColumnEnd => {
            style.flex_grid_mut().grid_column_end = GridPlacementValue::Auto;
        }
        PropertyIdKind::FitTolerance => {
            style.flex_grid_mut().fit_tolerance = crate::grid_css::FitToleranceValue::Normal;
        }
        PropertyIdKind::GridLanesDirection => {
            style.flex_grid_mut().grid_lanes_direction =
                crate::grid_css::GridLanesDirectionMode::Normal;
        }
        PropertyIdKind::GridLanesPack => {
            style.flex_grid_mut().grid_lanes_pack = crate::grid_css::GridLanesPackMode::Normal;
        }
        PropertyIdKind::FlexWrap => {
            style.flex_grid_mut().flex_wrap = FlexWrapMode::NoWrap;
        }
        PropertyIdKind::FlexGrow => {
            style.flex_grid_mut().flex_grow = 0.0;
        }
        PropertyIdKind::FlexShrink => {
            style.flex_grid_mut().flex_shrink = 1.0;
        }
        PropertyIdKind::Order => {
            style.flex_grid_mut().order = 0;
        }
        PropertyIdKind::AlignItems => {
            style.flex_grid_mut().align_items = AlignValue::Normal;
        }
        PropertyIdKind::AlignSelf => {
            style.flex_grid_mut().align_self = AlignValue::Auto;
        }
        PropertyIdKind::AlignContent => {
            style.flex_grid_mut().align_content = AlignValue::Normal;
        }
        PropertyIdKind::JustifyContent => {
            style.flex_grid_mut().justify_content = AlignValue::Normal;
        }
        PropertyIdKind::JustifyItems => {
            style.flex_grid_mut().justify_items = AlignValue::Normal;
        }
        PropertyIdKind::JustifySelf => {
            style.flex_grid_mut().justify_self = AlignValue::Auto;
        }
        PropertyIdKind::TransitionDelay => style.effects_mut().transition_delay_math = None,
        PropertyIdKind::TransitionDuration => style.effects_mut().transition_duration_math = None,
        PropertyIdKind::AnimationDelay => style.effects_mut().animation_delay_math = None,
        PropertyIdKind::AnimationDuration => style.effects_mut().animation_duration_math = None,
        PropertyIdKind::AnimationIterationCount => {
            style.effects_mut().animation_iteration_count_math = None
        }
        PropertyIdKind::Orphans => Arc::make_mut(&mut style.text).orphans = 2,
        PropertyIdKind::Widows => Arc::make_mut(&mut style.text).widows = 2,
        PropertyIdKind::TabSize => {
            let t = Arc::make_mut(&mut style.text);
            t.tab_size_num = 8.0;
            t.tab_size_len = None;
        }
        PropertyIdKind::ColumnCount => Arc::make_mut(&mut style.box_model).column_count = None,
        PropertyIdKind::Overlay => style.overlay = OverlayMode::None,
        PropertyIdKind::ReadingFlow => {
            if let Some(eff) = &mut style.effects {
                Arc::make_mut(eff).reading_flow = ReadingFlowMode::Normal;
            }
        }
        PropertyIdKind::ReadingOrder => {
            if let Some(eff) = &mut style.effects {
                Arc::make_mut(eff).reading_order = 0;
            }
        }
        PropertyIdKind::TextJustify => {
            Arc::make_mut(&mut style.text).text_justify = TextJustifyMode::Auto;
        }
        PropertyIdKind::TextAlignLast => {
            Arc::make_mut(&mut style.text).text_align_last = TextAlignLastMode::Auto;
        }
        PropertyIdKind::HyphenateCharacter => {
            Arc::make_mut(&mut style.text).hyphenate_character = None;
        }
        PropertyIdKind::HyphenateLimitLines => {
            Arc::make_mut(&mut style.text).hyphenate_limit_lines = None;
        }
        PropertyIdKind::Contain => {
            if let Some(eff) = &mut style.effects {
                Arc::make_mut(eff).contain = 0;
            }
        }
        _ => {}
    }
}

fn default_transition_spec() -> TransitionSpec {
    TransitionSpec {
        property: rix_core::intern_str("all"),
        duration_ms: 0.0,
        delay_ms: 0.0,
        timing: TimingFunction::Ease,
        behavior: TransitionBehaviorMode::Normal,
    }
}

fn default_animation_spec() -> AnimationSpec {
    AnimationSpec {
        name: rix_core::intern_str("none"),
        duration_ms: 0.0,
        delay_ms: 0.0,
        timing: TimingFunction::Ease,
        iteration_count: 1.0,
        direction: AnimationDirectionMode::Normal,
        fill_mode: AnimationFillModeKind::None,
        play_state: AnimationPlayStateMode::Running,
        composition: AnimationCompositionMode::Replace,
    }
}

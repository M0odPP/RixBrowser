//! Compact `Arc`-grouped `ComputedStyle`, CSS variable substitution, and
//! Cascade Level 5 property resolution.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use rix_core::InternedStr;
use smallvec::SmallVec;

use crate::properties::{
    is_logical_property, parse_property_from_tokens, resolve_logical_to_physical, CssToken,
    GlobalKeyword, PropertyIdKind, PropertyValue, SpecifiedDeclaration,
};
use crate::stylesheet::{CascadeOrigin, CompiledRule, LayerId, MediaContext};
use crate::values::{
    AlignValue, AnimationDirectionMode, AnimationFillModeKind, AnimationPlayStateMode,
    AnimationSpec, BackgroundLayerValue, BorderCollapseMode, BoxShadowValue, BoxSizingMode,
    CaptionSideMode, ClearMode, ClipPathValue, ColorRgba, CornerRadii, CssColor, CursorKind,
    DirectionMode, DisplayMode, EdgeValues, EvalContext, FilterOp, FlexDirectionMode, FlexWrapMode,
    FloatMode, FontStyleMode, GridAutoFlowMode, GridPlacementValue, GridTemplateAreasValue,
    GridTrackSize, LengthPercentage, LengthPercentageAuto, LineHeightValue, LineStyle,
    MixBlendMode, OverflowMode, OverflowPair, OverflowWrapMode, OverscrollBehaviorMode,
    PointerEventsMode, PositionMode, ResizeMode, SizeValue, TableLayoutMode, TextAlignMode,
    TextDecorationLine, TextOverflowMode, TextTransformMode, TimingFunction, TransformOp,
    TransformOrigin, TransitionSpec, UserSelectMode, VerticalAlignMode, VisibilityMode,
    WhiteSpaceMode, WordBreakMode, WritingMode, ZIndexValue,
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
    pub border_collapse: BorderCollapseMode,
    pub border_spacing_x: LengthPercentage,
    pub border_spacing_y: LengthPercentage,
    pub table_layout: TableLayoutMode,
    pub content: Option<InternedStr>,
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
            border_collapse: BorderCollapseMode::Separate,
            border_spacing_x: LengthPercentage::Zero,
            border_spacing_y: LengthPercentage::Zero,
            table_layout: TableLayoutMode::Auto,
            content: None,
        }
    }
}

/// Non-inherited visual/paint properties shared via copy-on-write `Arc`.
#[derive(Debug, Clone, PartialEq)]
pub struct VisualStyle {
    pub background_color: ColorRgba,
    pub background_layers: Vec<BackgroundLayerValue>,
    pub border_style: EdgeValues<LineStyle>,
    pub border_color: EdgeValues<CssColor>,
    pub border_radius: CornerRadii,
    pub outline_color: CssColor,
    pub outline_style: LineStyle,
    pub outline_width: f32,
    pub outline_offset: LengthPercentage,
    pub text_decoration_line: TextDecorationLine,
    pub text_decoration_color: Option<CssColor>,
    pub user_select: UserSelectMode,
    pub resize: ResizeMode,
    pub overscroll_behavior_x: OverscrollBehaviorMode,
    pub overscroll_behavior_y: OverscrollBehaviorMode,
}

impl Default for VisualStyle {
    fn default() -> Self {
        Self {
            background_color: ColorRgba::TRANSPARENT,
            background_layers: Vec::new(),
            border_style: EdgeValues::all(LineStyle::None),
            border_color: EdgeValues::all(CssColor::CurrentColor),
            border_radius: CornerRadii::default(),
            outline_color: CssColor::CurrentColor,
            outline_style: LineStyle::None,
            outline_width: 0.0,
            outline_offset: LengthPercentage::Zero,
            text_decoration_line: TextDecorationLine::NONE,
            text_decoration_color: None,
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
    pub font_size_px: f32,
    pub font_weight: f32,
    pub font_style: FontStyleMode,
    pub font_family: SmallVec<[InternedStr; 4]>,
    pub line_height: LineHeightValue,
    pub letter_spacing: LengthPercentage,
    pub word_spacing: LengthPercentage,
    pub text_align: TextAlignMode,
    pub text_transform: TextTransformMode,
    pub white_space: WhiteSpaceMode,
    pub word_break: WordBreakMode,
    pub overflow_wrap: OverflowWrapMode,
    pub direction: DirectionMode,
    pub writing_mode: WritingMode,
    pub caption_side: CaptionSideMode,
}

impl Default for InheritedTextStyle {
    fn default() -> Self {
        Self {
            color: ColorRgba::BLACK,
            font_size_px: 16.0,
            font_weight: 400.0,
            font_style: FontStyleMode::Normal,
            font_family: SmallVec::new(),
            line_height: LineHeightValue::Normal,
            letter_spacing: LengthPercentage::Zero,
            word_spacing: LengthPercentage::Zero,
            text_align: TextAlignMode::Start,
            text_transform: TextTransformMode::None,
            white_space: WhiteSpaceMode::Normal,
            word_break: WordBreakMode::Normal,
            overflow_wrap: OverflowWrapMode::Normal,
            direction: DirectionMode::Ltr,
            writing_mode: WritingMode::HorizontalTb,
            caption_side: CaptionSideMode::Top,
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
            && self.text_align == other.text_align
            && self.text_transform == other.text_transform
            && self.white_space == other.white_space
            && self.word_break == other.word_break
            && self.overflow_wrap == other.overflow_wrap
            && self.direction == other.direction
            && self.writing_mode == other.writing_mode
            && self.caption_side == other.caption_side
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
    pub grid_template_areas: GridTemplateAreasValue,
    pub grid_auto_columns: Vec<GridTrackSize>,
    pub grid_auto_rows: Vec<GridTrackSize>,
    pub grid_auto_flow: GridAutoFlowMode,
    pub grid_row_start: GridPlacementValue,
    pub grid_row_end: GridPlacementValue,
    pub grid_column_start: GridPlacementValue,
    pub grid_column_end: GridPlacementValue,
}

impl Default for FlexGridStyle {
    fn default() -> Self {
        Self {
            flex_direction: FlexDirectionMode::Row,
            flex_wrap: FlexWrapMode::NoWrap,
            flex_grow: 0.0,
            flex_shrink: 1.0,
            flex_basis: LengthPercentageAuto::Auto,
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
            grid_template_areas: GridTemplateAreasValue::default(),
            grid_auto_columns: Vec::new(),
            grid_auto_rows: Vec::new(),
            grid_auto_flow: GridAutoFlowMode::Row,
            grid_row_start: GridPlacementValue::Auto,
            grid_row_end: GridPlacementValue::Auto,
            grid_column_start: GridPlacementValue::Auto,
            grid_column_end: GridPlacementValue::Auto,
        }
    }
}

/// Optional paint effects (`box-shadow`, `transform`, `transform-origin`, `filter`,
/// `clip-path`, `mix-blend-mode`, `transition`, `animation`) allocated only when set.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EffectsStyle {
    pub box_shadow: Vec<BoxShadowValue>,
    pub transform: Vec<TransformOp>,
    pub transform_origin: TransformOrigin,
    pub filter: Vec<FilterOp>,
    pub clip_path: ClipPathValue,
    pub mix_blend_mode: MixBlendMode,
    pub transitions: Vec<TransitionSpec>,
    pub animations: Vec<AnimationSpec>,
}

/// Inherited map of CSS custom properties (`--*`) stored as owned token streams.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CustomPropertyMap {
    pub entries: HashMap<InternedStr, Vec<CssToken>>,
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

    fn flex_grid_mut(&mut self) -> &mut FlexGridStyle {
        let arc = self
            .flex_grid
            .get_or_insert_with(|| Arc::new(FlexGridStyle::default()));
        Arc::make_mut(arc)
    }

    fn effects_mut(&mut self) -> &mut EffectsStyle {
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
                if let Some(fb) = fallback {
                    expand_var_tokens(fb, custom_props, depth + 1, active, out, byte_budget)?;
                } else {
                    return None;
                }
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

/// Dynamic Tarjan SCC resolver for CSS custom properties (CSS Variables L1 §3 + CSSWG #11500).
///
/// Traverses only actually-evaluated `var()` edges (fallbacks are only entered when the
/// primary variable is undefined or independently guaranteed-invalid), while continuing to
/// evaluate remaining top-level tokens so overlapping secondary cycles are discovered while
/// the SCC root is still on the Tarjan stack.
struct CustomPropResolver<'a> {
    raw_map: &'a HashMap<InternedStr, Vec<CssToken>>,
    next_index: usize,
    indices: HashMap<InternedStr, usize>,
    lowlink: HashMap<InternedStr, usize>,
    on_stack: HashMap<InternedStr, bool>,
    tarjan_stack: Vec<InternedStr>,
    resolved: HashMap<InternedStr, Option<Vec<CssToken>>>,
}

enum DynamicVarOutcome {
    Valid,
    Invalid,
    Cycle(usize),
}

impl<'a> CustomPropResolver<'a> {
    fn new(raw_map: &'a HashMap<InternedStr, Vec<CssToken>>) -> Self {
        let cap = raw_map.len();
        Self {
            raw_map,
            next_index: 0,
            indices: HashMap::with_capacity(cap),
            lowlink: HashMap::with_capacity(cap),
            on_stack: HashMap::with_capacity(cap),
            tarjan_stack: Vec::with_capacity(cap),
            resolved: HashMap::with_capacity(cap),
        }
    }

    fn resolve(&mut self, name: &InternedStr, depth: u8) -> Option<Vec<CssToken>> {
        if let Some(res) = self.resolved.get(name) {
            return res.clone();
        }
        let tokens = self.raw_map.get(name)?;
        if !tokens.iter().any(token_has_var) {
            let cloned = tokens.clone();
            self.resolved.insert(name.clone(), Some(cloned.clone()));
            return Some(cloned);
        }
        self.strongconnect(name, depth);
        self.resolved.get(name).cloned().flatten()
    }

    fn strongconnect(&mut self, name: &InternedStr, depth: u8) {
        let my_idx = self.next_index;
        self.next_index += 1;
        self.indices.insert(name.clone(), my_idx);
        self.lowlink.insert(name.clone(), my_idx);
        self.on_stack.insert(name.clone(), true);
        self.tarjan_stack.push(name.clone());

        let tokens = self.raw_map.get(name).map(Vec::as_slice).unwrap_or(&[]);
        let mut out = Vec::with_capacity(tokens.len());
        let mut byte_budget = MAX_VAR_EXPANDED_BYTES;
        let mut failed = false;
        let mut hit_cycle = false;

        self.eval_token_slice(
            name,
            tokens,
            depth,
            &mut out,
            &mut byte_budget,
            &mut failed,
            &mut hit_cycle,
        );

        let my_low = self.lowlink.get(name).copied().unwrap_or(my_idx);
        if my_low == my_idx {
            let mut scc = Vec::new();
            while let Some(w) = self.tarjan_stack.pop() {
                self.on_stack.insert(w.clone(), false);
                let is_root = &w == name;
                scc.push(w);
                if is_root {
                    break;
                }
            }
            if scc.len() > 1 || hit_cycle {
                for node in scc {
                    self.resolved.insert(node, None);
                }
            } else if failed {
                self.resolved.insert(name.clone(), None);
            } else {
                self.resolved.insert(name.clone(), Some(out));
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn eval_token_slice(
        &mut self,
        owner: &InternedStr,
        tokens: &[CssToken],
        depth: u8,
        out: &mut Vec<CssToken>,
        byte_budget: &mut usize,
        failed: &mut bool,
        hit_cycle: &mut bool,
    ) -> DynamicVarOutcome {
        if depth > MAX_VAR_DEPTH || out.len() > MAX_VAR_EXPANDED_TOKENS || *byte_budget == 0 {
            *byte_budget = 0;
            *failed = true;
            return DynamicVarOutcome::Invalid;
        }
        let mut slice_failed = false;
        let mut slice_cycle: Option<usize> = None;

        for tok in tokens {
            match tok {
                CssToken::Var { name, fallback } => {
                    let dep_outcome = self.eval_var_ref(owner, name, depth + 1, out, byte_budget);
                    match dep_outcome {
                        DynamicVarOutcome::Valid => {}
                        DynamicVarOutcome::Cycle(target_low) => {
                            *hit_cycle = true;
                            *failed = true;
                            slice_failed = true;
                            slice_cycle =
                                Some(slice_cycle.map_or(target_low, |c| c.min(target_low)));
                            if let Some(cur_low) = self.lowlink.get_mut(owner) {
                                *cur_low = (*cur_low).min(target_low);
                            }
                        }
                        DynamicVarOutcome::Invalid => {
                            if let Some(fb) = fallback {
                                let saved_len = out.len();
                                let mut fb_failed = false;
                                let mut fb_cycle = false;
                                let fb_out = self.eval_token_slice(
                                    owner,
                                    fb,
                                    depth + 1,
                                    out,
                                    byte_budget,
                                    &mut fb_failed,
                                    &mut fb_cycle,
                                );
                                if fb_cycle {
                                    *hit_cycle = true;
                                }
                                match fb_out {
                                    DynamicVarOutcome::Valid => {}
                                    DynamicVarOutcome::Cycle(low) => {
                                        out.truncate(saved_len);
                                        *failed = true;
                                        slice_failed = true;
                                        slice_cycle = Some(slice_cycle.map_or(low, |c| c.min(low)));
                                    }
                                    DynamicVarOutcome::Invalid => {
                                        out.truncate(saved_len);
                                        *failed = true;
                                        slice_failed = true;
                                    }
                                }
                            } else {
                                *failed = true;
                                slice_failed = true;
                            }
                        }
                    }
                }
                CssToken::Function { name, args } => {
                    let cost = name.as_str().len() + 2;
                    if !*failed && (cost > *byte_budget || out.len() >= MAX_VAR_EXPANDED_TOKENS) {
                        *byte_budget = 0;
                        *failed = true;
                        slice_failed = true;
                    } else if !*failed {
                        *byte_budget = byte_budget.saturating_sub(cost);
                    }
                    let mut resolved_args = Vec::with_capacity(args.len());
                    let arg_out = self.eval_token_slice(
                        owner,
                        args,
                        depth + 1,
                        &mut resolved_args,
                        byte_budget,
                        failed,
                        hit_cycle,
                    );
                    match arg_out {
                        DynamicVarOutcome::Valid if !*failed => {
                            out.push(CssToken::Function {
                                name: name.clone(),
                                args: resolved_args,
                            });
                        }
                        DynamicVarOutcome::Cycle(low) => {
                            slice_failed = true;
                            slice_cycle = Some(slice_cycle.map_or(low, |c| c.min(low)));
                        }
                        _ => {
                            slice_failed = true;
                        }
                    }
                }
                other => {
                    if !*failed {
                        let cost = token_approx_bytes(other);
                        if cost > *byte_budget || out.len() >= MAX_VAR_EXPANDED_TOKENS {
                            *byte_budget = 0;
                            *failed = true;
                            slice_failed = true;
                        } else {
                            *byte_budget -= cost;
                            out.push(other.clone());
                        }
                    }
                }
            }
        }

        if let Some(low) = slice_cycle {
            DynamicVarOutcome::Cycle(low)
        } else if slice_failed {
            DynamicVarOutcome::Invalid
        } else {
            DynamicVarOutcome::Valid
        }
    }

    fn eval_var_ref(
        &mut self,
        owner: &InternedStr,
        dep: &InternedStr,
        depth: u8,
        out: &mut Vec<CssToken>,
        byte_budget: &mut usize,
    ) -> DynamicVarOutcome {
        if depth > MAX_VAR_DEPTH {
            *byte_budget = 0;
            return DynamicVarOutcome::Invalid;
        }
        if let Some(resolved_opt) = self.resolved.get(dep) {
            return match resolved_opt {
                Some(toks) => {
                    for rt in toks {
                        let cost = token_approx_bytes(rt);
                        if cost > *byte_budget || out.len() >= MAX_VAR_EXPANDED_TOKENS {
                            *byte_budget = 0;
                            return DynamicVarOutcome::Invalid;
                        }
                        *byte_budget -= cost;
                        out.push(rt.clone());
                    }
                    DynamicVarOutcome::Valid
                }
                None => DynamicVarOutcome::Invalid,
            };
        }
        if self.on_stack.get(dep).copied().unwrap_or(false) {
            let dep_idx = self.indices.get(dep).copied().unwrap_or(0);
            if let Some(cur_low) = self.lowlink.get_mut(owner) {
                *cur_low = (*cur_low).min(dep_idx);
            }
            return DynamicVarOutcome::Cycle(dep_idx);
        }
        let Some(dep_tokens) = self.raw_map.get(dep) else {
            return DynamicVarOutcome::Invalid;
        };
        if !dep_tokens.iter().any(token_has_var) {
            let cloned = dep_tokens.clone();
            self.resolved.insert(dep.clone(), Some(cloned.clone()));
            for rt in &cloned {
                let cost = token_approx_bytes(rt);
                if cost > *byte_budget || out.len() >= MAX_VAR_EXPANDED_TOKENS {
                    *byte_budget = 0;
                    return DynamicVarOutcome::Invalid;
                }
                *byte_budget -= cost;
                out.push(rt.clone());
            }
            return DynamicVarOutcome::Valid;
        }

        self.strongconnect(dep, depth);

        if self.on_stack.get(dep).copied().unwrap_or(false) {
            let dep_low = self.lowlink.get(dep).copied().unwrap_or(0);
            if let Some(cur_low) = self.lowlink.get_mut(owner) {
                *cur_low = (*cur_low).min(dep_low);
            }
            return DynamicVarOutcome::Cycle(dep_low);
        }
        match self.resolved.get(dep).and_then(Option::as_ref) {
            Some(toks) => {
                for rt in toks {
                    let cost = token_approx_bytes(rt);
                    if cost > *byte_budget || out.len() >= MAX_VAR_EXPANDED_TOKENS {
                        *byte_budget = 0;
                        return DynamicVarOutcome::Invalid;
                    }
                    *byte_budget -= cost;
                    out.push(rt.clone());
                }
                DynamicVarOutcome::Valid
            }
            None => DynamicVarOutcome::Invalid,
        }
    }
}

/// Cascade Level 5 ordering key (strictly ascending priority; highest wins).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct CascadeKey {
    band: u8,
    layer_rank: u32,
    specificity: u32,
    rule_order: u32,
    decl_order: u32,
}

impl CascadeKey {
    fn new(
        origin: CascadeOrigin,
        is_inline: bool,
        important: bool,
        layer_id: LayerId,
        specificity: u32,
        rule_order: u32,
        decl_order: u32,
    ) -> Self {
        let band = match (important, is_inline, origin) {
            (false, false, CascadeOrigin::UserAgent) => 0,
            (false, false, CascadeOrigin::User) => 1,
            (false, false, CascadeOrigin::Author) => 2,
            (false, true, _) => 3,
            (true, false, CascadeOrigin::Author) => 4,
            (true, true, _) => 5,
            (true, false, CascadeOrigin::User) => 6,
            (true, false, CascadeOrigin::UserAgent) => 7,
        };
        let layer_rank = if !important {
            layer_id.0
        } else if layer_id.is_unlayered() {
            0
        } else {
            u32::MAX - layer_id.0
        };
        Self {
            band,
            layer_rank,
            specificity,
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
    let mut style = match parent_style {
        Some(p) => ComputedStyle::inherit_from(p),
        None => ComputedStyle::default(),
    };

    let mut candidates: Vec<CandidateDecl<'_>> =
        Vec::with_capacity(matched_rules.len() * 4 + pres_hint_decls.len() + inline_decls.len());
    for decl in pres_hint_decls {
        // HTML5 §15.3 Presentational Hints: override UA stylesheet (band 0) while losing
        // to every Author rule (even 0-specificity or layered author rules).
        candidates.push(CandidateDecl {
            key: CascadeKey {
                band: 1,
                layer_rank: 0,
                specificity: 0,
                rule_order: 0,
                decl_order: decl.order,
            },
            decl,
        });
    }
    for (rule_idx, rule) in matched_rules.iter().enumerate() {
        let r_order = (rule.source_order).max(rule_idx as u32);
        for decl in rule.declarations.iter() {
            candidates.push(CandidateDecl {
                key: CascadeKey::new(
                    rule.origin,
                    false,
                    decl.important,
                    rule.layer_id,
                    rule.specificity,
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
                decl.order,
            ),
            decl,
        });
    }

    if candidates.is_empty() {
        return style;
    }

    candidates.sort_by_key(|c| c.key);

    // Pass 1: Resolve CSS custom properties (`--*`).
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
    if !raw_custom.is_empty() {
        let mut custom_map = (*style.custom_properties).clone();
        for (name, stack) in &raw_custom {
            if let Some(winner) = select_winning_candidate(stack) {
                match &winner.value {
                    PropertyValue::Custom(tokens) => {
                        custom_map.insert(name.clone(), tokens.clone());
                    }
                    PropertyValue::Global(GlobalKeyword::Initial) => {
                        custom_map.entries.remove(name);
                    }
                    PropertyValue::Global(
                        GlobalKeyword::Inherit
                        | GlobalKeyword::Unset
                        | GlobalKeyword::Revert
                        | GlobalKeyword::RevertLayer,
                    ) => {
                        // Custom properties are inherited: keep parent's computed value.
                    }
                    _ => {}
                }
            }
        }
        // Substitute `var()` references inside newly declared custom properties using
        // Dynamic Tarjan SCC over actually-evaluated edges (CSS Variables L1 §3 + CSSWG #11500).
        let raw_entries = custom_map.entries.clone();
        let mut resolver = CustomPropResolver::new(&raw_entries);
        let keys: Vec<InternedStr> = raw_custom.keys().cloned().collect();
        for name in keys {
            if !raw_entries
                .get(&name)
                .is_some_and(|t| t.iter().any(token_has_var))
            {
                continue;
            }
            match resolver.resolve(&name, 0) {
                Some(resolved) => {
                    if let Some(kw) = single_global_keyword_token(&resolved) {
                        match kw {
                            GlobalKeyword::Initial => {
                                custom_map.entries.remove(&name);
                            }
                            GlobalKeyword::Inherit | GlobalKeyword::Unset => {
                                if let Some(pv) =
                                    parent_style.and_then(|p| p.custom_properties.get(&name))
                                {
                                    custom_map.insert(name, pv.to_vec());
                                } else {
                                    custom_map.entries.remove(&name);
                                }
                            }
                            GlobalKeyword::Revert | GlobalKeyword::RevertLayer => {
                                let reverted = raw_custom
                                    .get(&name)
                                    .and_then(|st| select_candidate_after_revert(st, kw));
                                match reverted.map(|d| &d.value) {
                                    Some(PropertyValue::Custom(toks)) => {
                                        if let Some(sub) = resolve_var_tokens(toks, &custom_map) {
                                            custom_map.insert(name, sub);
                                        } else {
                                            custom_map.entries.remove(&name);
                                        }
                                    }
                                    Some(PropertyValue::Global(GlobalKeyword::Initial)) => {
                                        custom_map.entries.remove(&name);
                                    }
                                    _ => {
                                        if let Some(pv) = parent_style
                                            .and_then(|p| p.custom_properties.get(&name))
                                        {
                                            custom_map.insert(name, pv.to_vec());
                                        } else {
                                            custom_map.entries.remove(&name);
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        custom_map.insert(name, resolved);
                    }
                }
                None => {
                    // Cyclic, undefined, or over-budget custom property is guaranteed-invalid.
                    custom_map.entries.remove(&name);
                }
            }
        }
        style.custom_properties = Arc::new(custom_map);
    }

    // Pass 2: Expand any `var()`-bearing declarations into typed declarations, preserving cascade key.
    // Per CSS Variables Level 1 §3, an Unresolved declaration whose substitution fails or parses
    // to an invalid value becomes Invalid At Computed-Value Time (IACVT -> behaves as `unset`),
    // rather than falling back to lower-specificity cascaded declarations.
    let mut expanded_owned: Vec<(CascadeKey, SpecifiedDeclaration)> = Vec::new();
    let mut standard_candidates: Vec<(CascadeKey, &SpecifiedDeclaration)> = Vec::new();
    for c in &candidates {
        if c.decl.property == PropertyIdKind::Custom {
            continue;
        }
        if let PropertyValue::Unresolved(tokens) = &c.decl.value {
            let expanded = resolve_var_tokens(tokens, &style.custom_properties)
                .map(|rt| parse_property_from_tokens(c.decl.property, &rt, c.decl.important))
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

    // Pass 2a: Resolve `writing-mode`, `direction`, and `font-size` first so logical
    // property mapping and `em` units on the same element use this element's computed values.
    let parent_font_size = parent_style.map_or(16.0, |p| p.text.font_size_px);
    let parent_eval = media.eval_context(parent_font_size, root_font_size_px);
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
    let eval_ctx = media.eval_context(style.text.font_size_px, root_font_size_px);
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

    style
}

fn token_has_var(tok: &CssToken) -> bool {
    match tok {
        CssToken::Var { .. } => true,
        CssToken::Function { args, .. } => args.iter().any(token_has_var),
        _ => false,
    }
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
        0 | 7 => 0, // UserAgent (normal / !important)
        1 | 6 => 1, // User & Presentational Hints
        2..=5 => 2, // Author (stylesheet & inline, normal & !important)
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
    while idx > 0 {
        idx -= 1;
        let (key, decl) = sorted_asc[idx];
        match decl.value {
            PropertyValue::Global(GlobalKeyword::RevertLayer) => {
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
            GlobalKeyword::Unset | GlobalKeyword::Revert | GlobalKeyword::RevertLayer => {
                decl.property.is_inherited()
            }
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
        (PropertyIdKind::Opacity, PropertyValue::Number(v)) => style.opacity = v.clamp(0.0, 1.0),
        (PropertyIdKind::ColorScheme, PropertyValue::Number(v)) => {
            style.color_scheme_dark = *v > 0.5;
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
            Arc::make_mut(&mut style.visual).background_color = c.resolve(current_color, dark_mode);
        }
        (PropertyIdKind::BackgroundLayers, PropertyValue::BackgroundLayers(l)) => {
            Arc::make_mut(&mut style.visual).background_layers = l.clone();
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
        (PropertyIdKind::BorderTopColor, PropertyValue::Color(c)) => {
            Arc::make_mut(&mut style.visual).border_color.top = *c;
        }
        (PropertyIdKind::BorderRightColor, PropertyValue::Color(c)) => {
            Arc::make_mut(&mut style.visual).border_color.right = *c;
        }
        (PropertyIdKind::BorderBottomColor, PropertyValue::Color(c)) => {
            Arc::make_mut(&mut style.visual).border_color.bottom = *c;
        }
        (PropertyIdKind::BorderLeftColor, PropertyValue::Color(c)) => {
            Arc::make_mut(&mut style.visual).border_color.left = *c;
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
            Arc::make_mut(&mut style.visual).outline_color = *c;
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
            Arc::make_mut(&mut style.visual).text_decoration_color = Some(*c);
        }
        (PropertyIdKind::UserSelect, PropertyValue::UserSelect(u)) => {
            Arc::make_mut(&mut style.visual).user_select = *u;
        }

        // Typography (inherited)
        (PropertyIdKind::Color, PropertyValue::Color(c)) => {
            let inherited = parent.map_or(ColorRgba::BLACK, |p| p.text.color);
            Arc::make_mut(&mut style.text).color = c.resolve(inherited, dark_mode);
        }
        (PropertyIdKind::FontSize, PropertyValue::Length(lp)) => {
            let px = lp.resolve_px(eval.font_size_px, eval).max(1.0);
            Arc::make_mut(&mut style.text).font_size_px = px;
        }
        (PropertyIdKind::FontWeight, PropertyValue::Number(w)) => {
            Arc::make_mut(&mut style.text).font_weight = *w;
        }
        (PropertyIdKind::FontStyle, PropertyValue::FontStyle(s)) => {
            Arc::make_mut(&mut style.text).font_style = *s;
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
            style.flex_grid_mut().flex_basis = v.clone();
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
            style.flex_grid_mut().grid_template_columns = v.clone();
        }
        (PropertyIdKind::GridTemplateRows, PropertyValue::GridTracks(v)) => {
            style.flex_grid_mut().grid_template_rows = v.clone();
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

        // Effects (allocated lazily)
        (PropertyIdKind::BoxShadow, PropertyValue::BoxShadow(v)) => {
            style.effects_mut().box_shadow = v.clone();
        }
        (PropertyIdKind::Transform, PropertyValue::Transform(v)) => {
            style.effects_mut().transform = v.clone();
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
        (PropertyIdKind::Transition, PropertyValue::Transitions(v)) => {
            style.effects_mut().transitions = v.clone();
        }
        (PropertyIdKind::TransitionProperty, PropertyValue::NameList(names)) => {
            let trs = &mut style.effects_mut().transitions;
            if trs.len() < names.len() {
                trs.resize_with(names.len(), || TransitionSpec {
                    property: rix_core::intern_str("all"),
                    duration_ms: 0.0,
                    delay_ms: 0.0,
                    timing: TimingFunction::Ease,
                });
            }
            for (slot, name) in trs.iter_mut().zip(names.iter()) {
                slot.property = name.clone();
            }
        }
        (PropertyIdKind::TransitionDuration, PropertyValue::DurationList(ms)) => {
            let trs = &mut style.effects_mut().transitions;
            if trs.is_empty() && !ms.is_empty() {
                trs.resize_with(ms.len(), || TransitionSpec {
                    property: rix_core::intern_str("all"),
                    duration_ms: 0.0,
                    delay_ms: 0.0,
                    timing: TimingFunction::Ease,
                });
            }
            for (i, slot) in trs.iter_mut().enumerate() {
                if !ms.is_empty() {
                    slot.duration_ms = ms[i % ms.len()];
                }
            }
        }
        (PropertyIdKind::TransitionDelay, PropertyValue::DurationList(ms)) => {
            let trs = &mut style.effects_mut().transitions;
            for (i, slot) in trs.iter_mut().enumerate() {
                if !ms.is_empty() {
                    slot.delay_ms = ms[i % ms.len()];
                }
            }
        }
        (PropertyIdKind::TransitionTimingFunction, PropertyValue::TimingList(fns)) => {
            let trs = &mut style.effects_mut().transitions;
            for (i, slot) in trs.iter_mut().enumerate() {
                if !fns.is_empty() {
                    slot.timing = fns[i % fns.len()];
                }
            }
        }
        (PropertyIdKind::Animation, PropertyValue::Animations(v)) => {
            style.effects_mut().animations = v.clone();
        }
        (PropertyIdKind::AnimationName, PropertyValue::NameList(names)) => {
            let anims = &mut style.effects_mut().animations;
            anims.resize_with(names.len(), || AnimationSpec {
                name: rix_core::intern_str("none"),
                duration_ms: 0.0,
                delay_ms: 0.0,
                timing: TimingFunction::Ease,
                iteration_count: 1.0,
                direction: AnimationDirectionMode::Normal,
                fill_mode: AnimationFillModeKind::None,
                play_state: AnimationPlayStateMode::Running,
            });
            for (slot, name) in anims.iter_mut().zip(names.iter()) {
                slot.name = name.clone();
            }
        }
        (PropertyIdKind::AnimationDuration, PropertyValue::DurationList(ms)) => {
            let anims = &mut style.effects_mut().animations;
            for (i, slot) in anims.iter_mut().enumerate() {
                if !ms.is_empty() {
                    slot.duration_ms = ms[i % ms.len()];
                }
            }
        }
        (PropertyIdKind::AnimationDelay, PropertyValue::DurationList(ms)) => {
            let anims = &mut style.effects_mut().animations;
            for (i, slot) in anims.iter_mut().enumerate() {
                if !ms.is_empty() {
                    slot.delay_ms = ms[i % ms.len()];
                }
            }
        }
        (PropertyIdKind::AnimationTimingFunction, PropertyValue::TimingList(fns)) => {
            let anims = &mut style.effects_mut().animations;
            for (i, slot) in anims.iter_mut().enumerate() {
                if !fns.is_empty() {
                    slot.timing = fns[i % fns.len()];
                }
            }
        }
        (PropertyIdKind::AnimationIterationCount, PropertyValue::DurationList(vals)) => {
            let anims = &mut style.effects_mut().animations;
            for (i, slot) in anims.iter_mut().enumerate() {
                if !vals.is_empty() {
                    slot.iteration_count = vals[i % vals.len()];
                }
            }
        }
        (PropertyIdKind::AnimationDirection, PropertyValue::DirectionList(dirs)) => {
            let anims = &mut style.effects_mut().animations;
            for (i, slot) in anims.iter_mut().enumerate() {
                if !dirs.is_empty() {
                    slot.direction = dirs[i % dirs.len()];
                }
            }
        }
        (PropertyIdKind::AnimationFillMode, PropertyValue::FillModeList(fills)) => {
            let anims = &mut style.effects_mut().animations;
            for (i, slot) in anims.iter_mut().enumerate() {
                if !fills.is_empty() {
                    slot.fill_mode = fills[i % fills.len()];
                }
            }
        }
        (PropertyIdKind::AnimationPlayState, PropertyValue::PlayStateList(states)) => {
            let anims = &mut style.effects_mut().animations;
            for (i, slot) in anims.iter_mut().enumerate() {
                if !states.is_empty() {
                    slot.play_state = states[i % states.len()];
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
            Arc::make_mut(&mut style.box_model).border_collapse = *v;
        }
        (PropertyIdKind::BorderSpacing, PropertyValue::BorderSpacing(x, y)) => {
            let bm = Arc::make_mut(&mut style.box_model);
            bm.border_spacing_x = x.clone();
            bm.border_spacing_y = y.clone();
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
        PropertyIdKind::Color => Arc::make_mut(&mut style.text).color = parent.text.color,
        PropertyIdKind::BackgroundColor => {
            Arc::make_mut(&mut style.visual).background_color = parent.visual.background_color;
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
        PropertyIdKind::CaptionSide => {
            Arc::make_mut(&mut style.text).caption_side = parent.text.caption_side;
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
            Arc::make_mut(&mut style.visual).border_color.top = parent.visual.border_color.top;
        }
        PropertyIdKind::BorderRightColor => {
            Arc::make_mut(&mut style.visual).border_color.right = parent.visual.border_color.right;
        }
        PropertyIdKind::BorderBottomColor => {
            Arc::make_mut(&mut style.visual).border_color.bottom =
                parent.visual.border_color.bottom;
        }
        PropertyIdKind::BorderLeftColor => {
            Arc::make_mut(&mut style.visual).border_color.left = parent.visual.border_color.left;
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
        PropertyIdKind::Color => Arc::make_mut(&mut style.text).color = init.text.color,
        PropertyIdKind::BackgroundColor => {
            Arc::make_mut(&mut style.visual).background_color = ColorRgba::TRANSPARENT;
        }
        PropertyIdKind::FontSize => {
            Arc::make_mut(&mut style.text).font_size_px = init.text.font_size_px;
        }
        PropertyIdKind::FontWeight => {
            Arc::make_mut(&mut style.text).font_weight = init.text.font_weight;
        }
        PropertyIdKind::Direction => {
            Arc::make_mut(&mut style.text).direction = DirectionMode::Ltr;
        }
        PropertyIdKind::WritingMode => {
            Arc::make_mut(&mut style.text).writing_mode = WritingMode::HorizontalTb;
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
        _ => {}
    }
}

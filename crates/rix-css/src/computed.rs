//! Compact `Arc`-grouped `ComputedStyle`, CSS variable substitution, and
//! Cascade Level 5 property resolution.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use rix_core::InternedStr;
use smallvec::SmallVec;

use crate::properties::{
    parse_property_from_tokens, CssToken, GlobalKeyword, PropertyIdKind, PropertyValue,
    SpecifiedDeclaration,
};
use crate::stylesheet::{CascadeOrigin, CompiledRule, LayerId, MediaContext};
use crate::values::{
    AlignValue, AnimationDirectionMode, AnimationFillModeKind, AnimationPlayStateMode,
    AnimationSpec, BackgroundLayerValue, BorderCollapseMode, BoxShadowValue, BoxSizingMode,
    ClearMode, ClipPathValue, ColorRgba, CornerRadii, CssColor, CursorKind, DirectionMode,
    DisplayMode, EdgeValues, EvalContext, FilterOp, FlexDirectionMode, FlexWrapMode, FloatMode,
    FontStyleMode, GridAutoFlowMode, GridTrackSize, LengthPercentage, LengthPercentageAuto,
    LineHeightValue, LineStyle, MixBlendMode, OverflowMode, OverflowPair, OverflowWrapMode,
    PointerEventsMode, PositionMode, SizeValue, TableLayoutMode, TextAlignMode, TextDecorationLine,
    TextOverflowMode, TextTransformMode, TimingFunction, TransformOp, TransformOrigin,
    TransitionSpec, UserSelectMode, VerticalAlignMode, VisibilityMode, WhiteSpaceMode,
    WordBreakMode, ZIndexValue,
};

const MAX_VAR_DEPTH: u8 = 32;

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
    pub grid_auto_columns: Vec<GridTrackSize>,
    pub grid_auto_rows: Vec<GridTrackSize>,
    pub grid_auto_flow: GridAutoFlowMode,
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
            grid_auto_columns: Vec::new(),
            grid_auto_rows: Vec::new(),
            grid_auto_flow: GridAutoFlowMode::Row,
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

/// Expands `var()` references in `tokens` against `custom_props`, detecting cycles.
///
/// Returns `None` if an undefined variable has no fallback or a cycle is encountered
/// (invalid at computed-value time).
#[must_use]
pub fn resolve_var_tokens(
    tokens: &[CssToken],
    custom_props: &CustomPropertyMap,
) -> Option<Vec<CssToken>> {
    let mut active = SmallVec::<[InternedStr; 8]>::new();
    let mut out = Vec::with_capacity(tokens.len());
    expand_var_tokens(tokens, custom_props, 0, &mut active, &mut out)?;
    Some(out)
}

fn expand_var_tokens(
    tokens: &[CssToken],
    custom_props: &CustomPropertyMap,
    depth: u8,
    active: &mut SmallVec<[InternedStr; 8]>,
    out: &mut Vec<CssToken>,
) -> Option<()> {
    if depth > MAX_VAR_DEPTH {
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
                    let res = expand_var_tokens(val_tokens, custom_props, depth + 1, active, out);
                    active.pop();
                    if res.is_some() {
                        continue;
                    }
                }
                if let Some(fb) = fallback {
                    expand_var_tokens(fb, custom_props, depth + 1, active, out)?;
                } else {
                    return None;
                }
            }
            CssToken::Function { name, args } => {
                let mut resolved_args = Vec::with_capacity(args.len());
                expand_var_tokens(args, custom_props, depth + 1, active, &mut resolved_args)?;
                out.push(CssToken::Function {
                    name: name.clone(),
                    args: resolved_args,
                });
            }
            other => out.push(other.clone()),
        }
    }
    Some(())
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
    let mut style = match parent_style {
        Some(p) => ComputedStyle::inherit_from(p),
        None => ComputedStyle::default(),
    };

    let mut candidates: Vec<CandidateDecl<'_>> =
        Vec::with_capacity(matched_rules.len() * 4 + inline_decls.len());
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
                    _ => {}
                }
            }
        }
        // Substitute var() references inside newly declared custom properties.
        let snapshot = custom_map.clone();
        let keys: Vec<InternedStr> = raw_custom.keys().cloned().collect();
        for name in keys {
            if let Some(tokens) = snapshot.get(&name) {
                if tokens.iter().any(token_has_var) {
                    let mut active = SmallVec::<[InternedStr; 8]>::new();
                    active.push(name.clone());
                    let mut resolved = Vec::new();
                    if expand_var_tokens(tokens, &snapshot, 0, &mut active, &mut resolved).is_some()
                    {
                        custom_map.insert(name, resolved);
                    } else {
                        // Cyclic or invalid custom property becomes IACVT (removed/reverted).
                        custom_map.entries.remove(&name);
                    }
                }
            }
        }
        style.custom_properties = Arc::new(custom_map);
    }

    // Pass 2: Expand any `var()`-bearing declarations into typed declarations, preserving cascade key.
    let mut expanded_owned: Vec<(CascadeKey, SpecifiedDeclaration)> = Vec::new();
    let mut standard_candidates: Vec<(CascadeKey, &SpecifiedDeclaration)> = Vec::new();
    for c in &candidates {
        if c.decl.property == PropertyIdKind::Custom {
            continue;
        }
        if let PropertyValue::Unresolved(tokens) = &c.decl.value {
            if let Some(resolved_tokens) = resolve_var_tokens(tokens, &style.custom_properties) {
                for decl in
                    parse_property_from_tokens(c.decl.property, &resolved_tokens, c.decl.important)
                {
                    expanded_owned.push((c.key, decl));
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

    // Pass 2a: Resolve `font-size` first so `em` units on the same element use this element's font size.
    let parent_font_size = parent_style.map_or(16.0, |p| p.text.font_size_px);
    if let Some(stack) = per_prop.get_mut(&PropertyIdKind::FontSize) {
        stack.sort_by_key(|(k, _)| *k);
        if let Some(winner) = select_winning_candidate(stack) {
            let parent_eval = media.eval_context(parent_font_size, root_font_size_px);
            apply_declaration(
                &mut style,
                winner,
                parent_style,
                &parent_eval,
                media.dark_mode,
            );
        }
    }

    // Pass 2b: Resolve `color` next so `currentColor` on borders/backgrounds matches.
    let eval_ctx = media.eval_context(style.text.font_size_px, root_font_size_px);
    if let Some(stack) = per_prop.get_mut(&PropertyIdKind::Color) {
        stack.sort_by_key(|(k, _)| *k);
        if let Some(winner) = select_winning_candidate(stack) {
            apply_declaration(&mut style, winner, parent_style, &eval_ctx, media.dark_mode);
        }
    }

    // Pass 2c: Resolve all remaining properties.
    for (&prop, stack) in &mut per_prop {
        if matches!(prop, PropertyIdKind::FontSize | PropertyIdKind::Color) {
            continue;
        }
        stack.sort_by_key(|(k, _)| *k);
        if let Some(winner) = select_winning_candidate(stack) {
            apply_declaration(&mut style, winner, parent_style, &eval_ctx, media.dark_mode);
        }
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

fn select_winning_candidate<'a>(
    sorted_asc: &[(CascadeKey, &'a SpecifiedDeclaration)],
) -> Option<&'a SpecifiedDeclaration> {
    let mut idx = sorted_asc.len();
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
                while idx > 0 && sorted_asc[idx - 1].0.band == key.band {
                    idx -= 1;
                }
            }
            _ => return Some(decl),
        }
    }
    None
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
            Arc::make_mut(&mut style.box_model).border_width.top = *v;
        }
        (PropertyIdKind::BorderRightWidth, PropertyValue::Number(v)) => {
            Arc::make_mut(&mut style.box_model).border_width.right = *v;
        }
        (PropertyIdKind::BorderBottomWidth, PropertyValue::Number(v)) => {
            Arc::make_mut(&mut style.box_model).border_width.bottom = *v;
        }
        (PropertyIdKind::BorderLeftWidth, PropertyValue::Number(v)) => {
            Arc::make_mut(&mut style.box_model).border_width.left = *v;
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
        (PropertyIdKind::GridAutoColumns, PropertyValue::GridTracks(v)) => {
            style.flex_grid_mut().grid_auto_columns = v.clone();
        }
        (PropertyIdKind::GridAutoRows, PropertyValue::GridTracks(v)) => {
            style.flex_grid_mut().grid_auto_rows = v.clone();
        }
        (PropertyIdKind::GridAutoFlow, PropertyValue::GridAutoFlow(v)) => {
            style.flex_grid_mut().grid_auto_flow = *v;
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
        PropertyIdKind::Visibility => style.visibility = parent.visibility,
        PropertyIdKind::PointerEvents => style.pointer_events = parent.pointer_events,
        PropertyIdKind::Cursor => style.cursor = parent.cursor,
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
        PropertyIdKind::Width => Arc::make_mut(&mut style.box_model).width = SizeValue::Auto,
        PropertyIdKind::Height => Arc::make_mut(&mut style.box_model).height = SizeValue::Auto,
        _ => {}
    }
}

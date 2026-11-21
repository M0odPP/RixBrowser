//! Parent-process HTML/CSS/Vello Chrome renderer and hit-tester.
//!
//! Renders `rix://chrome/browser.html` (Chrome layer `z = 100`) and transient
//! overlays/HUD (`z = 200`) strictly inside the Parent Process via
//! `rix-html -> rix-css -> rix-layout -> DisplayList`, completely isolated from
//! untrusted web Content Processes.

use std::sync::Arc;
use std::time::Instant;

use rix_core::{Point, Size};
use rix_css::{CascadeOrigin, MediaContext, StyleContext, Stylesheet};
use rix_dom::{Document, NodeData, NodeId};
use rix_html::parse_html;
use rix_ipc::PipelineId;
use rix_layout::{hit_test, FragmentTree, LayoutEngine};
use rix_render::display_list::DisplayList;
use rix_render::ResourceRegistry;
use rix_text::FontDatabase;

use crate::assets::{
    render_browser_chrome_html, render_crashed_html, render_error_html, render_hud_html,
    render_newtab_html, render_overlay_html,
};
use crate::hud::{DevHudState, HudTab};
use crate::state::ChromeState;
use crate::tokens::{chrome_stylesheet, ThemeMode};

/// Semantic interactive Chrome UI element resolved from pointer coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChromeHitTarget {
    Tab(usize),
    CloseTab(usize),
    NewTab,
    Back,
    Forward,
    Reload,
    Omnibox,
    ToggleSidebar,
    ToggleTheme,
    ToggleHud,
    Suggestion(usize),
    HudTab(HudTab),
    ContextBack,
    ContextForward,
    ContextReload,
    ContextOpenLinkNewTab,
    ContextCopyLink,
    ContextCopy,
    ContextToggleHud,
    ReloadCrashedTab,
    OpenNewTabFromCrashed,
    RetryError,
    HistoryLink(usize),
}

/// Cached rendered Chrome / Overlay frame along with its DOM + FragmentTree for hit-testing.
pub struct RenderedChromeLayer {
    pub doc: Document,
    pub fragment_tree: FragmentTree,
    pub display_list: DisplayList,
}

impl RenderedChromeLayer {
    /// Resolves the clicked/hovered [`ChromeHitTarget`] at `point` by walking up DOM element IDs.
    #[must_use]
    pub fn hit_test_target(&self, point: Point) -> Option<ChromeHitTarget> {
        let mut current = hit_test(&self.fragment_tree, point);
        while let Some(node) = current {
            if let Some(NodeData::Element(elem)) = self.doc.get(node) {
                if let Some(id) = elem.id() {
                    if let Some(target) = parse_chrome_hit_id(id) {
                        return Some(target);
                    }
                }
            }
            current = self.doc.parent(node);
        }
        None
    }
}

/// Parent-process renderer for isolated `rix://chrome/*` surfaces.
pub struct ChromeRenderer {
    dark_sheet: Arc<Stylesheet>,
    light_sheet: Arc<Stylesheet>,
    chrome_engine: LayoutEngine,
    overlay_engine: LayoutEngine,
}

impl Default for ChromeRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl ChromeRenderer {
    #[must_use]
    pub fn new() -> Self {
        let dark_media = MediaContext {
            dark_mode: true,
            ..MediaContext::default()
        };
        let light_media = MediaContext {
            dark_mode: false,
            ..MediaContext::default()
        };
        let dark_sheet = Arc::new(Stylesheet::parse(
            &chrome_stylesheet(ThemeMode::Dark),
            CascadeOrigin::Author,
            &dark_media,
        ));
        let light_sheet = Arc::new(Stylesheet::parse(
            &chrome_stylesheet(ThemeMode::Light),
            CascadeOrigin::Author,
            &light_media,
        ));
        Self {
            dark_sheet,
            light_sheet,
            chrome_engine: LayoutEngine::new(),
            overlay_engine: LayoutEngine::new(),
        }
    }

    #[must_use]
    pub fn stylesheet_for(&self, theme: ThemeMode) -> Arc<Stylesheet> {
        match theme {
            ThemeMode::Dark => Arc::clone(&self.dark_sheet),
            ThemeMode::Light => Arc::clone(&self.light_sheet),
        }
    }

    /// Renders the main Browser Chrome (`z = 100`) into a binary [`DisplayList`].
    pub fn render_chrome(
        &mut self,
        state: &ChromeState,
        viewport: Size,
        font_db: &FontDatabase,
        pipeline: PipelineId,
        resources: &mut ResourceRegistry,
    ) -> RenderedChromeLayer {
        let html = render_browser_chrome_html(state);
        self.chrome_engine.dark_mode = state.theme.is_dark();
        let sheet = self.stylesheet_for(state.theme);
        render_html_layer(
            &mut self.chrome_engine,
            &html,
            sheet,
            state.theme.is_dark(),
            viewport,
            font_db,
            pipeline,
            resources,
        )
    }

    /// Renders transient overlays (`z = 200`): omnibox dropdown, context menu,
    /// hover URL preview pill, and `F12` Developer HUD.
    #[allow(clippy::too_many_arguments)]
    pub fn render_overlay(
        &mut self,
        state: &ChromeState,
        hud: &DevHudState,
        now: Instant,
        viewport: Size,
        font_db: &FontDatabase,
        pipeline: PipelineId,
        resources: &mut ResourceRegistry,
    ) -> RenderedChromeLayer {
        let html = render_overlay_html(state, hud, now);
        self.overlay_engine.dark_mode = state.theme.is_dark();
        let sheet = self.stylesheet_for(state.theme);
        render_html_layer(
            &mut self.overlay_engine,
            &html,
            sheet,
            state.theme.is_dark(),
            viewport,
            font_db,
            pipeline,
            resources,
        )
    }

    /// Renders a built-in internal page (`newtab.html`, `error.html`, `crashed.html`, `hud.html`).
    pub fn render_internal_document(
        &mut self,
        html: &str,
        theme: ThemeMode,
        viewport: Size,
        font_db: &FontDatabase,
        pipeline: PipelineId,
        resources: &mut ResourceRegistry,
    ) -> RenderedChromeLayer {
        let mut engine = LayoutEngine::new();
        engine.dark_mode = theme.is_dark();
        let sheet = self.stylesheet_for(theme);
        render_html_layer(
            &mut engine,
            html,
            sheet,
            theme.is_dark(),
            viewport,
            font_db,
            pipeline,
            resources,
        )
    }

    /// Convenience helper rendering `newtab.html` for the active theme.
    pub fn render_newtab(
        &mut self,
        state: &ChromeState,
        viewport: Size,
        font_db: &FontDatabase,
        pipeline: PipelineId,
        resources: &mut ResourceRegistry,
    ) -> RenderedChromeLayer {
        let html = render_newtab_html(state);
        self.render_internal_document(&html, state.theme, viewport, font_db, pipeline, resources)
    }

    /// Convenience helper rendering `crashed.html` for a crashed tab.
    #[allow(clippy::too_many_arguments)]
    pub fn render_crashed(
        &mut self,
        theme: ThemeMode,
        url: &str,
        pid: Option<u32>,
        reason: &str,
        viewport: Size,
        font_db: &FontDatabase,
        pipeline: PipelineId,
        resources: &mut ResourceRegistry,
    ) -> RenderedChromeLayer {
        let html = render_crashed_html(theme, url, pid, reason);
        self.render_internal_document(&html, theme, viewport, font_db, pipeline, resources)
    }

    /// Convenience helper rendering `error.html`.
    #[allow(clippy::too_many_arguments)]
    pub fn render_error(
        &mut self,
        theme: ThemeMode,
        url: &str,
        reason: &str,
        viewport: Size,
        font_db: &FontDatabase,
        pipeline: PipelineId,
        resources: &mut ResourceRegistry,
    ) -> RenderedChromeLayer {
        let html = render_error_html(theme, url, reason);
        self.render_internal_document(&html, theme, viewport, font_db, pipeline, resources)
    }

    /// Convenience helper rendering standalone `hud.html`.
    #[allow(clippy::too_many_arguments)]
    pub fn render_hud_standalone(
        &mut self,
        state: &ChromeState,
        hud: &DevHudState,
        now: Instant,
        viewport: Size,
        font_db: &FontDatabase,
        pipeline: PipelineId,
        resources: &mut ResourceRegistry,
    ) -> RenderedChromeLayer {
        let html = render_hud_html(state, hud, now);
        self.render_internal_document(&html, state.theme, viewport, font_db, pipeline, resources)
    }
}

#[allow(clippy::too_many_arguments)]
fn render_html_layer(
    engine: &mut LayoutEngine,
    html: &str,
    sheet: Arc<Stylesheet>,
    dark_mode: bool,
    viewport: Size,
    font_db: &FontDatabase,
    pipeline: PipelineId,
    resources: &mut ResourceRegistry,
) -> RenderedChromeLayer {
    let media = MediaContext {
        viewport_width: viewport.width,
        viewport_height: viewport.height,
        dark_mode,
        ..MediaContext::default()
    };
    let mut doc = parse_html(html);
    let mut styles = StyleContext::new(media);
    styles.add_stylesheet(sheet);
    styles.cascade_full(&mut doc);

    let frame = engine.layout_and_paint(&doc, &styles, viewport, font_db, pipeline, resources);
    RenderedChromeLayer {
        doc,
        fragment_tree: frame.fragment_tree,
        display_list: frame.display_list,
    }
}

/// Maps Chrome DOM element `id` attributes to semantic [`ChromeHitTarget`]s.
#[must_use]
pub fn parse_chrome_hit_id(id: &str) -> Option<ChromeHitTarget> {
    if let Some(rest) = id.strip_prefix("tab-close-") {
        return rest.parse::<usize>().ok().map(ChromeHitTarget::CloseTab);
    }
    if let Some(rest) = id.strip_prefix("tab-") {
        return rest.parse::<usize>().ok().map(ChromeHitTarget::Tab);
    }
    if let Some(rest) = id.strip_prefix("suggest-") {
        return rest.parse::<usize>().ok().map(ChromeHitTarget::Suggestion);
    }
    if let Some(rest) = id.strip_prefix("history-") {
        return rest.parse::<usize>().ok().map(ChromeHitTarget::HistoryLink);
    }
    match id {
        "btn-new-tab" => Some(ChromeHitTarget::NewTab),
        "btn-back" => Some(ChromeHitTarget::Back),
        "btn-forward" => Some(ChromeHitTarget::Forward),
        "btn-reload" => Some(ChromeHitTarget::Reload),
        "omnibox" => Some(ChromeHitTarget::Omnibox),
        "btn-sidebar" => Some(ChromeHitTarget::ToggleSidebar),
        "btn-theme" => Some(ChromeHitTarget::ToggleTheme),
        "btn-hud" => Some(ChromeHitTarget::ToggleHud),
        "hud-tab-perf" => Some(ChromeHitTarget::HudTab(HudTab::Performance)),
        "hud-tab-net" => Some(ChromeHitTarget::HudTab(HudTab::Network)),
        "hud-tab-missing" => Some(ChromeHitTarget::HudTab(HudTab::MissingFeatures)),
        "hud-tab-console" => Some(ChromeHitTarget::HudTab(HudTab::Console)),
        "ctx-back" => Some(ChromeHitTarget::ContextBack),
        "ctx-forward" => Some(ChromeHitTarget::ContextForward),
        "ctx-reload" => Some(ChromeHitTarget::ContextReload),
        "ctx-open-link-tab" => Some(ChromeHitTarget::ContextOpenLinkNewTab),
        "ctx-copy-link" => Some(ChromeHitTarget::ContextCopyLink),
        "ctx-copy" => Some(ChromeHitTarget::ContextCopy),
        "ctx-hud" => Some(ChromeHitTarget::ContextToggleHud),
        "btn-reload-crashed" => Some(ChromeHitTarget::ReloadCrashedTab),
        "btn-newtab-crashed" => Some(ChromeHitTarget::OpenNewTabFromCrashed),
        "btn-retry" => Some(ChromeHitTarget::RetryError),
        _ => None,
    }
}

/// Finds the first ancestor `<a href="...">` URL starting from a hit-tested DOM node.
#[must_use]
pub fn find_link_href(doc: &Document, start: NodeId) -> Option<String> {
    let mut cur = Some(start);
    while let Some(node) = cur {
        if let Some(NodeData::Element(elem)) = doc.get(node) {
            if elem.name.local.eq_str_ignore_ascii_case("a") {
                if let Some(href) = elem.get_attribute("href") {
                    if !href.trim().is_empty() {
                        return Some(href.trim().to_string());
                    }
                }
            }
        }
        cur = doc.parent(node);
    }
    None
}

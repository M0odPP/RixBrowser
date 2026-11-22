# Spec 07: Crafted Browser Chrome UI & Developer HUD (`rix-chrome`, `rix-app`)

**Status:** Draft (Phase 1 Target)  
**Governing Architecture:** [docs/ARCHITECTURE.md](../ARCHITECTURE.md)  
**Design System:** [`unslop`](../../.agents/skills/unslop/SKILL.md) by Damir Sokolovsky ([@heydamir](https://x.com/heydamir), [heydamir/unslop](https://github.com/heydamir/unslop), CC BY 4.0)  
**Unsafe Policy:** `#![deny(unsafe_code)]`.

---

## 1. Chrome Architecture (Firefox-Style Isolated UI)

RixBrowser renders its browser interface (`rix://chrome/*`) using its own fast HTML/CSS/Vello stack **inside the Parent Browser Process**, strictly isolated from untrusted web pages:

- **Zero Content Interference:** Page DOM, page layout, and page JS run in separate Content Processes. A frozen or heavy tab can never stall hovering a tab, typing in the omnibox, switching tabs, or opening settings.
- **Dedicated Retained Chrome Layer:** Chrome UI composites into its own cached `Compositor` layer (`z = 100`), separated from the active web surface (`z = 10`) and transient overlays/menus/HUD (`z = 200`).
- **Asset Injection (No Circular Deps):** `rix-chrome` registers its embedded `rix://chrome/*` assets into `rix-net::ResourceManager` via `ChromeAssetProvider` at startup.

---

## 2. Design Direction (`unslop` Craft Rules)

Every screen under `rix://chrome/*` (`browser.html`, `newtab.html`, `settings.html`, `error.html`, `crashed.html`, and the `F12` HUD) MUST be designed and audited using `.agents/skills/unslop/SKILL.md` and its references (`signs.md`, `tokens.md`, `visuals.md`).

### 2.1 Visual Character: "Precision Optical Instrument"
- **Quiet Chrome First:** The browser frame disappears around web content. Surfaces use tightly stepped **tinted neutrals** (cool slate-charcoal in dark mode, warm paper-stone in light mode) separated by crisp `1px` hairlines—never glowing shadows or translucent rainbow glass.
- **One Restrained Accent:** A single crisp accent color (cobalt-cyan or amber-vermilion, tokenized via CSS custom properties) reserved strictly for active focus rings, active loading hairline progress, and primary confirmation buttons.
- **Nested Border Radii:** Follow `inner_radius = outer_radius - padding` (minimum `4px`):
  - Toolbar controls & tab pills: `6px–8px`
  - Omnibox & dropdown menus: `8px–10px`
  - Modal panels & cards (settings / crash card): `12px–14px`
- **Typography & Density:**
  - Clean sans-serif UI family (`Inter` / system sans fallback) with tabular numerals (`font-variant-numeric: tabular-nums`) for frame counters, URL ports, and download speeds.
  - Monospace family (`JetBrains Mono` / system mono) strictly for URLs in technical views, shortcut keycaps, and the `F12` Developer HUD.
- **Strict Anti-Slop Ban List (Enforced in CodeRabbit & PR Reviews):**
  - NO purple/indigo/pink AI gradients, NO glowing buttons/cards, NO gradient wordmarks.
  - NO emoji icons in tabs, buttons, menus, or badges (crisp `16px` stroke SVG icons only).
  - NO floating decorative blobs, pulsing status dots, or shouty uppercase micro-labels.
  - NO uneven/random padding—strict 4px spacing scale (`4, 8, 12, 16, 24`).
  - Visible keyboard focus rings on every interactive control and proper keycap badges (`Ctrl+L`, `Ctrl+T`, `Ctrl+W`, `F12`).

---

## 3. Core Browser Chrome Features (Phase 1 Usable Milestone)

To allow comfortable **manual day-to-day testing from Phase 1**, Chrome UI ships with:

1. **Tab Strip (Horizontal & Optional Vertical Sidebar):**
   - Create (`Ctrl+T`), close (`Ctrl+W`), switch (`Ctrl+Tab` / `Ctrl+1..9`), middle-click close, reopen closed tab (`Ctrl+Shift+T`).
   - Quiet loading indicator (subtle `2px` accent progress bar along the omnibox bottom edge—no spinning neon rings).
   - Clear crashed-tab badge and active-tab surface step with hairline top highlight.
2. **Navigation Bar & Omnibox (`Ctrl+L`):**
   - Back (`Alt+Left`), Forward (`Alt+Right`), Reload/Stop (`Ctrl+R` / `Esc`), Home.
   - Smart address bar: detects valid URLs (`https://`, `http://`, `rix://`, `localhost:PORT`, `example.com`) vs search queries, displays clean origin security state (`HTTPS` / `HTTP` / `Internal`), full text selection, copy/paste (`Ctrl+C`/`V`/`X`/`A`), and history autocomplete dropdown.
3. **In-Page Manual Ergonomics:**
   - Smooth APZ wheel/trackpad scrolling + scrollbar indicator,
   - Link hover URL preview pill in bottom-left corner,
   - Text selection & OS clipboard copy,
   - Native HTML `<input>`, `<textarea>`, `<button>`, `<select>` interaction and GET/POST `<form>` submission,
   - Context menu (Back, Forward, Reload, Copy Link, Open Link in New Tab, Inspect / Toggle HUD).
4. **Built-In Utility & Preferences Pages (`rix://chrome/*`):**
   - `rix://chrome/newtab.html` (`rix://newtab`) — Calm, centered start page with active search-engine selector, 4-column pinned shortcuts grid, recent pages list, and single-row keyboard shortcut hints.
   - `rix://chrome/settings.html` (`rix://settings`, `Ctrl+,`) — Complete preferences workspace (`248px` left navigation rail + fluid `720px` content pane) covering **Appearance** (Dark/Light polarity, Horizontal/Vertical tabs, bookmarks bar toggle, compact density, zoom/font steppers), **Search & address bar** (DuckDuckGo/Google/Startpage/Kagi, autocomplete, full URL toggle), **Privacy & security** (HTTPS-Only, cookie policy, Do Not Track, Site Isolation status, local history wipe, reset defaults), **Performance & media** (Cranelift JIT tiers, Vello GPU toggle, pure-Rust codec stack telemetry, startup HUD toggle), and **About & shortcuts**, persisted atomically as JSON via `BrowserSettings`.
   - `rix://chrome/bookmarks.html` (`rix://bookmarks`) & `rix://chrome/history.html` (`rix://history`, `Ctrl+H`) — Unified library managers with live item removal and navigation.
   - `rix://chrome/error.html` — Clear network/TLS/DNS diagnostics with Retry & Go back actions.
   - `rix://chrome/crashed.html` — Isolated tab crash recovery page showing Content PID, exit reason, and **Reload tab** action.
5. **Bookmarks Bar, App Menu & Find-in-Page (`Ctrl+F`):**
   - Optional `32px` Bookmarks Bar (`Ctrl+B`), address-bar bookmark star (`Ctrl+D`), zoom indicator & steppers (`Ctrl+=`/`-`/`0`), floating App Menu popover, and live `Ctrl+F` Find-in-page bar with match counts.

---

## 4. Built-In Developer HUD & Missing-Feature Telemetry (`F12`)

Designed specifically so developers can manually browse real websites and immediately diagnose performance or compatibility gaps without external tooling:

Toggle with **`F12`** or **`Ctrl+Shift+I`** (docked compact right/bottom drawer or floating top-right performance strip):

1. **Live Frame Telemetry Strip:**
   - Current **FPS** and rolling 120-frame **frame-time bar** (ms),
   - Per-stage breakdown: `Style ms` | `Layout ms` | `Paint ms` | `IPC ms` | `GPU ms`,
   - Active Content Process PID & resident memory (RSS MB),
   - Idle wakeup check (shows `IDLE (0 Hz)` when no work is occurring).
2. **Missing-Feature Inspector Tab:**
   - Live deduplicated list of every **unsupported CSS property / selector / at-rule** and **missing Web API** requested by the current page, sorted by call count—making it effortless to file or prioritize the next GitHub Issue.
3. **Console & Network Waterfall Tabs:**
   - `console.log/warn/error` + uncaught JS exceptions with source URL and line,
   - Network request table: URL, HTTP protocol (`H1`/`H2`/`H3`), status, MIME, transfer size, cache hit/miss, TTFB & total ms.

---

## 5. Verification & Acceptance Criteria

- Run `.agents/skills/unslop/SKILL.md` audit pass against screenshots of `browser.html`, `newtab.html`, `crashed.html`, and the `F12` HUD in both dark and light themes: **0 critical slop signs**.
- Manual keyboard navigation test: full tab/omnibox control works smoothly without a mouse; focus rings visible everywhere.
- Switching tabs and typing in the omnibox maintains **< 2 ms input latency** even while a Content Process is under synthetic CPU load.

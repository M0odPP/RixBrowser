# Engineering & Agent Guidelines (`AGENTS.md`)

This document defines non-negotiable engineering rules for both human contributors and AI agents working on **RixBrowser**.

---

## 1. Pure Rust & Memory Safety Policy

- **No External Browser/JS/Media Runtimes:** Never embed or invoke V8, SpiderMonkey, JavaScriptCore, Node.js, Python, Blink, Gecko, WebKit, or FFmpeg.
- **Safe Rust by Default:** Every crate MUST start with `#![deny(unsafe_code)]` unless explicitly allowlisted below.
- **Allowlisted Isolated `unsafe` Crates:**
  - `rix-jit`: executable code page allocation (`mmap`/`mprotect`) and calling JIT-compiled native functions.
  - `rix-ipc`: shared memory mapping (`memfd`/mmap) and Linux OS sandbox setup (`seccomp`/namespaces).
  - `rix-js` (optional GC arena internals only if proven necessary by benchmark and approved via RFC).
- Every allowlisted crate MUST enforce `#![deny(unsafe_op_in_unsafe_fn)]`, keep `unsafe` isolated in dedicated internal modules, and include an explicit `// SAFETY:` justification on every `unsafe` block.

---

## 2. Zero Hacks, Zero Mocks, Standards-First

- **Fix Root Causes Only:** Solve defects at the architectural/spec level (parsing, cascade, layout, compositor, VM, network). Never patch around broken engine behavior with ad-hoc overrides.
- **No Site-Specific Hardcodes:** Never branch on specific hostnames, domains, element IDs, CSS classes, or site-specific protocols (for example `youtube.com`, `google.com`, `SOCS=`, `PREF=`, or hardcoded signature ciphers).
- **No Fake Consent or Synthetic State:** Cookies, consent dialogs, CORS, storage, and authentication must work through genuine W3C / WHATWG / IETF protocols.
- **No Placeholder Stubs:** Do not merge `todo!()`, `unimplemented!()`, or fake no-op implementations pretending to support a Web API. Unimplemented APIs should remain absent or return standard errors until properly built.

---

## 3. Spec-Driven Development

- Every subsystem is governed by its specification in [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) and [`docs/specs/`](docs/specs/).
- **Read the Spec First:** Before implementing an Issue, read its referenced specification document and verify all invariants, message schemas, and complexity budgets.
- **Keep Specs Honest:** If implementation reveals a required design change, update the relevant `docs/specs/*.md` file in the **same PR** and explain why in the PR description.

---

## 4. Architecture & Performance Non-Negotiables

- **DOM Never Crosses IPC:** The DOM tree lives exclusively in the Content Process. Never serialize DOM nodes or raw RGBA canvas/video pixel buffers over JSON pipes.
- **No Busy-Polling Loops:** Never add fixed-interval wakeups (such as `WaitUntil(1ms)` or polling timers) to the window event loop or IPC transport. All scheduling must be event-driven (OS wakeups, IPC readiness, VSync).
- **Never Block the UI Thread:** Never run `pollster::block_on`, synchronous network I/O, synchronous JS execution, or heavy page layout on the Browser Process UI thread.
- **Zero String Round-Tripping in CSS:** Never serialize parsed `lightningcss` AST properties back into CSS strings only to re-parse them during cascade.
- **GPU-First Scrolling & Media:** Scroll offsets, transforms, opacity animations, and video frames must update compositor layers/textures on the GPU without cloning full DisplayLists or mutating the DOM.

---

## 5. Browser Chrome UI Craft (`unslop`)

All browser interface work (`rix://chrome/*`: tab strip, omnibox, navigation toolbar, new tab page, settings, history/bookmarks panels, error/crash pages, and the `F12` Developer HUD) MUST follow:

1. The **`unslop` design skill** located at [`.agents/skills/unslop/SKILL.md`](.agents/skills/unslop/SKILL.md) (and its reference files under `.agents/skills/unslop/references/`), created by Damir Sokolovsky ([@heydamir](https://x.com/heydamir), [heydamir/unslop](https://github.com/heydamir/unslop), CC BY 4.0).
2. [`docs/specs/07-chrome-ui-and-devtools.md`](docs/specs/07-chrome-ui-and-devtools.md).

Specifically for browser chrome:
- Keep chrome quiet: tinted neutrals, one restrained accent, hairline borders, crisp nested radii, keyboard keycaps, and visible focus rings.
- Zero AI slop: no purple/neon gradients, no glowing cards/pills, no emoji labels, no decorative blobs, no arbitrary inconsistent spacing.
- Every UI change must be visually inspected by running the browser (or taking rendered screenshots) before opening a PR.

---

## 6. PR Size & Issue Hygiene

- **One Issue, One Focused PR:** Target **400–2,000 net lines** of Rust code per PR.
- **Hard CI Limit:** PRs exceeding **3,500 changed source lines** (excluding `Cargo.lock`, `docs/`, and test fixtures) fail CI unless explicitly split into smaller stacked PRs.
- Every PR must reference a GitHub Issue (`Closes #<id>` or `Part of #<id>`).

---

## 7. Mandatory Verification Before Every Commit/PR

Run and verify all checks locally before pushing:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Once Phase 1 lands a runnable window:
- Manually launch the browser or run the automated headless screenshot/reftest harness on affected pages.
- Verify no new panics, frame-time regressions, or unhandled errors appear in logs or the `F12` HUD.

---

## 8. Language, Cleanliness & Git Identity

- **English Only in Repository Files:** All code, identifiers, comments, specs, docs, Issues, PRs, and commit messages must be strictly in English (zero Cyrillic characters inside repository files).
- **No Junk Artifacts:** Never commit `.local/`, `.redb`, `.log`, `.pcap`, `.har`, ad-hoc debug screenshots, or commented-out dead code.
- **Conventional Commits:** Use `feat(scope): ...`, `fix(scope): ...`, `perf(scope): ...`, `docs(scope): ...`, `refactor(scope): ...`, `chore(scope): ...`.

# Contributing to RixBrowser

Thank you for contributing to RixBrowser. This project follows a **Spec-Driven, Incremental Architecture** model so that every PR leaves the engine cleaner, faster, and measurably closer to standards compliance.

---

## Development Workflow

1. **Pick an Issue** from GitHub Issues using `phase:*` and `area:*` labels.
   - Start from open tasks in the current active phase (check the Phase Tracking Issue).
2. **Read the Governing Spec** listed at the top of the Issue (under `docs/ARCHITECTURE.md` and `docs/specs/`).
3. **Create a Feature Branch** from `main`:
   ```bash
   git checkout -b feat/ipc-framed-transport
   ```
4. **Keep PRs Scoped:**
   - Ideal PR size: **400–2,000 changed source lines**.
   - CI enforces a hard cap at **3,500 source lines** (excluding `Cargo.lock`, `docs/`, and test fixtures). If a feature is larger, split it into clean stacked PRs linked to the same tracking issue.
5. **Run All Local Checks** before pushing:
   ```bash
   ./scripts/check.sh
   ```
   (Which runs `#![deny(unsafe_code)]` verification, anti-hack/English-only checks, `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace`.)
6. **Open a Pull Request** filling out `.github/pull_request_template.md`.
   - Automated checks (CI + CodeRabbit) will verify formatting, Clippy, tests, safety/hygiene rules, PR size, and spec compliance.

---

## Fast Local Builds (Linux)

The workspace `Cargo.toml` sets `[profile.dev.package."*"] opt-level = 2` so heavy dependencies (graphics, font shaping, crypto, storage) stay fast even in debug runs for manual browser testing.

For faster link times on Linux, you can configure `lld` or `mold` in your **personal** `~/.cargo/config.toml` (do not commit machine-specific flags into the repository):

```toml
[target.x86_64-unknown-linux-gnu]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=lld"]
```

---

## Standards & Testing

- **Unit & Integration Tests:** Every new engine primitive must include targeted unit tests covering both happy paths and spec edge cases.
- **Conformance Suites (Phases 1–5):**
  - **WPT (Web Platform Tests)** subsets for URL, HTML, CSS, DOM, Fetch, CORS, and Layout.
  - **Test262** for the custom JS VM and JIT (`rix-js` / `rix-jit`).
  - **Visual Reftests** comparing rendered PNG output against reference HTML snapshots.
  - **Criterion Benchmarks** for CSS cascade, layout, IPC throughput, and VM hot loops.
- **UI Changes:** Follow `.agents/skills/unslop/SKILL.md` and `docs/specs/07-chrome-ui-and-devtools.md` for all `rix://chrome/*` screens.

---

## Code Review & CodeRabbit

RixBrowser uses [CodeRabbit](https://coderabbit.ai) configured via `.coderabbit.yaml` alongside human review. Please address every flagged architectural, safety, or performance issue (or explain explicitly why a suggestion does not apply) before merging.

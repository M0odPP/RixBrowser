## Summary

Closes #

<!-- Concisely explain what this PR implements and which spec section it satisfies. -->

## Specification Reference

- Governing spec: `docs/specs/...`
- Spec updated in this PR? (Yes / No — N/A if behavior matches existing spec)

## Verification

- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] PR size stays well below the 3,500-line CI ceiling
- [ ] Zero site-specific hardcodes, fake cookies, or placeholder `todo!()` stubs
- [ ] All core crates retain `#![deny(unsafe_code)]` (any allowlisted `unsafe` has `// SAFETY:` proof)
- [ ] **UI changes only:** Audited with `.agents/skills/unslop/SKILL.md` and visually verified in light + dark themes

## Manual Testing Notes

<!-- If applicable, describe what pages/interactions you manually tested and what the F12 HUD reported. -->

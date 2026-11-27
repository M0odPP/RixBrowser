#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

echo "==> 1/5 Checking #![deny(unsafe_code)] policy..."
ALLOWLIST="rix-jit rix-ipc"
for lib in crates/*/src/lib.rs; do
  crate=$(basename "$(dirname "$(dirname "$lib")")")
  if echo " $ALLOWLIST " | grep -q " $crate "; then
    if ! grep -q '#!\[deny(unsafe_op_in_unsafe_fn)\]' "$lib"; then
      echo "ERROR: Allowlisted crate $crate must declare #![deny(unsafe_op_in_unsafe_fn)] in $lib"
      exit 1
    fi
  else
    if ! grep -q '#!\[deny(unsafe_code)\]' "$lib"; then
      echo "ERROR: Core crate $crate is missing #![deny(unsafe_code)] in $lib"
      exit 1
    fi
  fi
done

echo "==> 2/5 Checking zero site-specific hardcodes, UA spoofing, and placeholder stubs..."
if grep -rnE 'youtube\.com|youtu\.be|SOCS=|PENDING\+999|signature_cipher|youtube_hydrator' crates/; then
  echo "ERROR: Site-specific hardcode detected in crates/."
  exit 1
fi
python3 - <<'PY'
import pathlib, re, sys

# Universal anti-hardcode & anti-UA-spoofing scanner across ALL crates/ non-test source files.
# Forbids domain-specific checks, URL literals to real external websites, or browser UA spoofing
# outside unit tests (`#[cfg(test)]`), doc comments, W3C/WHATWG spec namespace URIs, RFC examples,
# and `rix-chrome` built-in default bookmarks / user-configurable search engines.
DOMAIN_RE = re.compile(
    r'"[^"\n]*\b([a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?\.(?:com|org|net|io|dev|tv|co|sh|ai|gg|ru|uk|de|fr|jp|edu|gov|news|app|run|build))\b[^"\n]*"',
    re.IGNORECASE,
)
UA_SPOOF_RE = re.compile(r'"[^"\n]*(Firefox/|Chrome/|Chromium/|Edg/|Safari/[0-9])[^"\n]*"')
SPEC_NAMESPACE_URIS = {
    "http://www.mozilla.org/newlayout/xml/parsererror.xml",
    # Normative WHATWG HTML §13.2.6.4.1 DOCTYPE Quirks Mode system identifier.
    "http://www.ibm.com/data/dtd/v11/ibmxhtml1-transitional.dtd",
}
CANONICAL_UA_FILES = {
    "crates/rix-core/src/browser_profile.rs",
    "crates/rix-content/src/web_apis.rs",
    "crates/rix-content/src/workers.rs",
}
CHROME_DEFAULTS_FILES = {
    "crates/rix-chrome/src/settings.rs",
    "crates/rix-chrome/src/state.rs",
}

violations = []
for path in sorted(pathlib.Path("crates").rglob("*.rs")):
    rel = path.as_posix()
    if "/tests/" in rel or "/benches/" in rel or rel.endswith("_tests.rs"):
        continue
    text = path.read_text(encoding="utf-8", errors="ignore")
    # Truncate at the start of inline `#[cfg(test)]` module at file tail.
    cfg_test_idx = text.find("#[cfg(test)]")
    code = text[:cfg_test_idx] if cfg_test_idx != -1 else text

    for lineno, raw_line in enumerate(code.splitlines(), start=1):
        stripped = raw_line.strip()
        if stripped.startswith("//") or stripped.startswith("/*") or stripped.startswith("*"):
            continue
        line = raw_line
        if rel not in CHROME_DEFAULTS_FILES:
            for m in DOMAIN_RE.finditer(line):
                dom = m.group(1).lower()
                full_lit = m.group(0)
                if full_lit.strip('"') in SPEC_NAMESPACE_URIS:
                    continue
                # Ignore W3C/WHATWG XML namespace URLs and RFC example domains.
                if dom in ("w3.org", "example.com", "example.org", "example.net", "whatwg.org", "iana.org", "ietf.org"):
                    continue
                if dom.endswith((".sh", ".co", ".io", ".ai", ".tv", ".app", ".run", ".build", ".news")) and (
                    "/" not in full_lit and "http" not in full_lit and not full_lit.startswith(('".', '"www.'))
                ):
                    continue
                violations.append(f"{rel}:{lineno}: disallowed site/domain literal `{full_lit}`")
        # Forbid per-site or ad-hoc User-Agent spoofing outside canonical UA definition files.
        if UA_SPOOF_RE.search(line) and rel not in CANONICAL_UA_FILES:
            violations.append(f"{rel}:{lineno}: disallowed User-Agent spoof string `{stripped}`")

if violations:
    print("ERROR: Anti-hardcode CI gate failed:")
    for v in violations[:40]:
        print("  ", v)
    sys.exit(1)
PY
if grep -rnE '\b(todo|unimplemented)!\(' crates/; then
  echo "ERROR: Placeholder macro (todo!/unimplemented!) detected in crates/."
  exit 1
fi

echo "==> 3/5 Checking English-only repository files..."
python3 - <<'PY'
import pathlib, re, sys
cyrillic = re.compile(r"[\u0400-\u04FF]")
bad = []
for root in ("crates", "docs", ".github", "scripts"):
    p = pathlib.Path(root)
    if not p.exists():
        continue
    for f in p.rglob("*"):
        if not f.is_file():
            continue
        try:
            text = f.read_text(encoding="utf-8")
        except UnicodeDecodeError:
            continue
        if cyrillic.search(text):
            bad.append(str(f))
for top in ("README.md", "AGENTS.md", "CONTRIBUTING.md", ".coderabbit.yaml"):
    f = pathlib.Path(top)
    if f.exists() and cyrillic.search(f.read_text(encoding="utf-8")):
        bad.append(str(f))
if bad:
    print("ERROR: Cyrillic characters found in repository files:", bad)
    sys.exit(1)
PY

echo "==> 4/5 Checking rustfmt & clippy (-D warnings)..."
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings

echo "==> 5/5 Running workspace tests..."
cargo test --workspace

echo "ALL LOCAL CI CHECKS PASSED."

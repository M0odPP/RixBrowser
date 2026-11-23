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

echo "==> 2/5 Checking zero site-specific hardcodes and placeholder stubs..."
if grep -rnE 'youtube\.com|youtu\.be|SOCS=|PENDING\+999|signature_cipher|youtube_hydrator' crates/; then
  echo "ERROR: Site-specific hardcode detected in crates/."
  exit 1
fi
ENGINE_SRC_DIRS="crates/rix-html/src crates/rix-dom/src crates/rix-css/src crates/rix-text/src crates/rix-layout/src crates/rix-render/src crates/rix-js/src crates/rix-jit/src crates/rix-content/src crates/rix-media/src crates/rix-net/src crates/rix-storage/src"
if grep -rnE '\b(google|github|wikipedia|reddit|twitter|facebook|instagram|amazon|netflix|tiktok|discord|telegram|ycombinator|stackoverflow|nytimes|bbc|twitch|spotify|linkedin)\.(com|org|tv)\b' $ENGINE_SRC_DIRS; then
  echo "ERROR: Site-specific domain hardcode detected in core engine crates."
  exit 1
fi
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

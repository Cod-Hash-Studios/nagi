#!/bin/sh
set -eu

printf '%s\n' \
  'Nagi does not publish signed binaries yet.' \
  'Build the current source with Rust 1.96.1 and Zig 0.15.2:' \
  '' \
  '  git clone https://github.com/Cod-Hash-Studios/nagi.git' \
  '  cd nagi' \
  '  cargo build --release --locked' \
  '' \
  'Refusing to download an inherited or unsigned binary.' >&2

exit 1

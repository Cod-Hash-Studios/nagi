#!/bin/sh

set -eu

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PYTHON_BIN=${PYTHON_BIN:-python3}

export PYTHONDONTWRITEBYTECODE=1
exec "$PYTHON_BIN" "$SCRIPT_DIR/bench_render.py" "$@"

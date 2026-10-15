#!/bin/bash
set -euo pipefail
find "$EVAL_TARGET" -mindepth 1 -maxdepth 1 -type f -exec basename '{}' \; | LC_ALL=C sort | tee reports/inventory.txt

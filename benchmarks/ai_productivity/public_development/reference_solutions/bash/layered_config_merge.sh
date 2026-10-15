#!/bin/bash
set -euo pipefail
jq -cS 'reduce .layers[] as $layer ({}; . * $layer)' "$EVAL_SETTINGS" | tee reports/effective.json

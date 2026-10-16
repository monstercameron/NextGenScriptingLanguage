^jq -cS 'reduce .layers[] as $layer ({}; . * $layer)' $env.EVAL_SETTINGS | ^tee reports/effective.json

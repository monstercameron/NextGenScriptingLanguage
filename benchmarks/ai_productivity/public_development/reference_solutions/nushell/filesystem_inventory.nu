^find $env.EVAL_TARGET -mindepth 1 -maxdepth 1 -type f -exec basename '{}' ';' | ^sort | ^tee reports/inventory.txt

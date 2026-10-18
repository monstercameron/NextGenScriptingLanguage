# bash completion for execute
_execute() {
    local cur prev commands options command
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"
    commands="check explain inspect run resume test run-parts show-run replay repl repair compile checkpoint help attach dap daemon applets modules service live model lsp"
    command="${COMP_WORDS[1]}"
    options="--help -h"
    case "$command" in
        check) options="--json --input --manifest-dir --manifest-catalog --capability-policy --data-flow-policy --format --help -h" ;;
        explain) options="--json --input --manifest-dir --manifest-catalog --capability-policy --data-flow-policy --allow --policy-header --format --help -h" ;;
        inspect) options="--input --manifest-dir --manifest-catalog --capability-policy --data-flow-policy --help -h" ;;
        resume) options="--input --manifest-dir --manifest-catalog --capability-policy --data-flow-policy --cache-dir --checkpoint --retry-uncertain --trace --max-output-bytes --command-timeout-ms --total-timeout-ms --max-record-bytes --help -h" ;;
        test) options="--answers --json --help -h" ;;
        run-parts) options="--once --continue --json --manifest-dir --help -h" ;;
        compile) options="--to --out --manifest-dir --manifest-catalog --help -h" ;;
        *) options="--input --manifest-dir --manifest-catalog --capability-policy --data-flow-policy --cache-dir --checkpoint --answers --record --what-if --confirm --single-instance --control --break-at --daemon --json-errors --headless --once --init --trace --max-output-bytes --max-runs --missed-ticks --max-catch-up --calendar-dedup --command-timeout-ms --drain-timeout-ms --total-timeout-ms --max-record-bytes --help -h" ;;
    esac
    case "$prev" in
        --manifest-dir|--cache-dir|--checkpoint|--single-instance)
            compopt -o filenames
            mapfile -t COMPREPLY < <(compgen -d -- "$cur")
            return 0 ;;
        --manifest-catalog|--capability-policy|--data-flow-policy|--answers|--record|--confirm)
            compopt -o filenames
            mapfile -t COMPREPLY < <(compgen -f -- "$cur")
            return 0 ;;
    esac
    if [[ $COMP_CWORD -eq 1 ]]; then
        compopt -o filenames
        mapfile -t COMPREPLY < <(compgen -W "$commands --help -h --version -V" -- "$cur")
        while IFS= read -r candidate; do COMPREPLY+=("$candidate"); done < <(compgen -f -- "$cur")
    elif [[ "$cur" == -* ]]; then
        mapfile -t COMPREPLY < <(compgen -W "$options" -- "$cur")
    else
        compopt -o filenames
        mapfile -t COMPREPLY < <(compgen -f -- "$cur")
    fi
    return 0
}
complete -F _execute execute

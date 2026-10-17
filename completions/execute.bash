# bash completion for execute
_execute() {
    local cur prev commands options
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"
    commands="check explain inspect run resume applets modules service live model lsp"
    options="--input --json --manifest-dir --manifest-catalog --capability-policy --data-flow-policy --cache-dir --checkpoint --retry-uncertain --headless --once --init --trace --max-output-bytes --max-runs --missed-ticks --max-catch-up --calendar-dedup --command-timeout-ms --drain-timeout-ms --max-record-bytes --total-timeout-ms --help"
    case "$prev" in
        --manifest-dir|--cache-dir|--checkpoint)
            COMPREPLY=( $(compgen -d -- "$cur") )
            return 0 ;;
        --manifest-catalog|--capability-policy|--data-flow-policy)
            COMPREPLY=( $(compgen -f -- "$cur") )
            return 0 ;;
    esac
    if [[ $COMP_CWORD -eq 1 ]]; then
        COMPREPLY=( $(compgen -W "$commands --help" -- "$cur") )
    elif [[ "$cur" == -* ]]; then
        COMPREPLY=( $(compgen -W "$options" -- "$cur") )
    else
        COMPREPLY=( $(compgen -f -- "$cur") )
    fi
    return 0
}
complete -F _execute execute

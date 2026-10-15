# Fixture compatibility for macOS Bash 3: upstream requires Bash 5 mapfile -t.
# This implements only the exact invocation in the pinned source, without edits.
if ! type mapfile >/dev/null 2>&1; then
  mapfile() {
    local line
    SORTED_WEB_APPS=()
    while IFS= read -r line; do
      SORTED_WEB_APPS+=("$line")
    done
  }
fi

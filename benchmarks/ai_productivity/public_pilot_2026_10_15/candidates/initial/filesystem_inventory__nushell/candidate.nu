let names = (ls --all $env.EVAL_TARGET | where type == "file" | get name | each {|entry| $entry | path basename})
let input = if ($names | is-empty) { "" } else { ($names | str join "\n") + "\n" }
let sorted = ($input | ^env LC_ALL=C sort | complete).stdout
mkdir reports
$sorted | save --raw --force reports/inventory.txt
print --raw $sorted

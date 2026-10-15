# Web app removal differential fixture

The unchanged `upstream.sh` and its `provenance.json` identify the Omarchy
source revision and pinned SHA-256 hashes. The acceptance test rechecks every
hash and never edits the copied original.

`08-webapp-remove.program` owns validation, selection, deletion ordering, and
menu refresh. Generic path applets remove the literal `.desktop` suffix before
extracting both each menu label and the indexed launcher name. This preserves
the Bash target identity for hidden basenames such as `nested/.desktop`: its
key is `nested`, while the unrelated native `nested.desktop` remains untouched.
Regression tests check both direct selection and the displayed menu label with
both launchers in one isolated HOME.

The fixture runs only owned command doubles and temporary HOME data; it never
invokes desktop services or modifies the user's applications. Run it from the
repository root with:

```sh
EXECUTE_TEST_BINARY=/path/to/current/execute python3 -m unittest discover -s tests/omarchy_rewrites -p test_08_webapp_remove.py -v
```

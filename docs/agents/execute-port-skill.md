# Skill: port a shell script to an Execute program

Use this skill when asked to rewrite a shell script or a `bin/` command as a
`.program` file. Read the [agent card](execute.md) first. Its grammar forms and
review commands are the only ones this skill may use.

## Steps

1. Read the source script and list each input it reads (arguments, environment,
   flags, prompts), each external command it runs, and each file it reads or
   writes. Note the exit status the script returns in each case.
2. Write the `.program` file. Start with `execute 1`, then one `accept` line per
   input. Map each external command to `run "EXECUTABLE" with arguments [...]`
   and keep its status with `and keep status as NAME` when the script branches on
   it. Map branches to `if` and `otherwise`, and loops over lists to `for each`.
   Use `then` and `pipe to` where the script uses `|`.
3. Look for a manifest applet before writing a direct executable call. Use
   `execute applets search` or `execute applets list`, then call the applet as
   `run NAME with PARAM VALUE as RESULT`.
4. Check the file: `execute check FILE --json`. Fix every error. Treat warnings
   as errors with `--deny-warnings` before you finish.
5. Review the plan: `execute explain FILE --json`, then
   `execute run FILE --what-if`. Record the fingerprint it prints.
6. Compare behavior by running the original script and the program on the same
   inputs, and checking stdout, stderr and exit status for each case. No
   `compare` command exists in this release, so this step is manual.
7. Report: the program path, the check result, the what-if fingerprint, and each
   comparison case with pass or fail. Name any part of the script that has no
   direct equivalent instead of approximating it.

## Rules

- Do not add arithmetic, comparisons beyond `is`, `is not`, `contains` and
  `does not contain`, string functions or interpolation. Move that work into an
  applet, or report it as a gap.
- Do not pass a shell string to an executable. Put each argument in the list.
- Do not assume that text converts to an integer. Convert with an applet.

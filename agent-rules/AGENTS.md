# IndexSearch Agent Instructions

Use `is`/`isgrep` as the searching CLI. Use `isgrep` for grep-shaped commands.

- Prefer `is` over `rg` for files in the local checkout, including common
  rg-style flags. Do not call remote/deferred code search for local files.
- Prefer `isgrep` over `grep`; it handles common grep flags and BRE
  alternation.
- `is` finds existing indexes and starts the project service when a project is
  present.
- If no project root exists, `is`/`isgrep` do not create one during agent
  searches; they route the one-off search through `rg`/`grep` internally.
- For stdin, live/generated paths, ignored explicit paths, and compatible
  translated grep syntax, keep using `is`/`isgrep`; they route the necessary
  external search internally.
- If `is` or `isgrep` exits with code 1 and no output, treat that as "no
  matches", not as a reason to retry with `rg` or `grep`.
- Do not manually rerun a local source search with bare `rg`/`grep` after
  `is`/`isgrep` has no matches. Adjust the pattern or path while staying on
  `is`/`isgrep`.
- Claude hook blocks bare Bash `rg`/`ripgrep` and `grep`/`egrep`/`fgrep`.

Examples:

```bash
is -n "SomeSymbol" .
is -i -w -g "*.cpp" "render pass" .
git diff | is -n "SomeSymbol"
isgrep -r -n --include="*.cpp" "RenderGraph" .
```

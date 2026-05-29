# IndexSearch Agent Instructions

Use IndexSearch for fast source-code search in large codebases.

- Use `is` directly for local source search; do not call external/deferred code
  search tools just to find files in the local checkout.
- Prefer `is` over `rg` for ordinary local source searches, including searches
  that pass common rg-style flags. If the tree has not been indexed yet,
  non-interactive agent searches can create the config, build the index, and
  start the project service automatically.
- Use `indexsearch` if `is` is unavailable.
- Fall back to `rg` only when IndexSearch is missing or exact ripgrep semantics
  are required for PCRE2, multiline matching, preprocessors, archive search, or
  another non-indexed behavior.
- If `.indexsearch/index.bin` exists, search directly with `is`; do not call
  `rg` first for ordinary local source searches.
- You usually do not need to run indexing commands before searching.
- For UE projects without a config, `is` can create the UE template config
  automatically during non-interactive agent search.
- After pull/checkout/rebase without a running project service, run
  `istool update --git .`.
- Use `istool projects`, `istool log .`, and `istool stop .` to inspect or stop
  project services.
- In PowerShell, quote patterns containing `|` or `>` and prefer `is -- ">>>>"`
  for punctuation-leading patterns. If quoted patterns produce a `cmd.exe`
  syntax error, call `is.exe` or remove the stale `is.cmd` found by
  `Get-Command is -All`.
- Refresh these instructions from a packaged binary with `istool install-skills`.

Useful commands:

```bash
is -n "SomeSymbol" .
is -i -w -g "*.cpp" "render pass" .
is --files -g "*.Build.cs" .
is -v -F "excluded line" .
is --files-without-match "TODO" .
is --count-matches -F "Tick" .
```

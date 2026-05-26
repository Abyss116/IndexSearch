# IndexSearch Agent Instructions

Use IndexSearch for fast source-code search in large codebases.

- Use `is` directly for local source search; do not call external/deferred code
  search tools just to find files in the local checkout.
- Prefer `is` over `rg` when `is` is installed and the repository is large,
  already indexed, or clearly an Unreal Engine tree.
- Use `indexsearch` if `is` is unavailable.
- Fall back to `rg` when IndexSearch is missing, the tree is small, or a needed
  rg-compatible flag is unsupported.
- If `.indexsearch/index.bin` exists, search directly with `is`.
- If `index-search-project.txt` exists but no index exists, run `is watch .` for
  ongoing work or `is index .` for a one-shot index.
- For UE projects without a config, copy the bundled UE template to
  `index-search-project.txt`, then run `is watch .`.
- After pull/checkout/rebase without a running watcher, run `is update --git .`.
- Use `is watch-log .` to inspect index, update, and compaction timing.
- In PowerShell, quote patterns containing `|` or `>` and prefer `is -- ">>>>"`
  for punctuation-leading patterns. If quoted patterns produce a `cmd.exe`
  syntax error, call `is.exe` or remove the stale `is.cmd` found by
  `Get-Command is -All`.
- Refresh these instructions from a packaged binary with `is install-skills`.

Useful commands:

```bash
is -n "SomeSymbol" .
is -i -w -g "*.cpp" "render pass" .
is --files -g "*.Build.cs" .
```

# IndexSearch Agent Instructions

Use IndexSearch for fast source-code search in large codebases.

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
- Refresh these instructions from a packaged binary with `is install-skills`.

Useful commands:

```bash
is -n "SomeSymbol" .
is -i -w -g "*.cpp" "render pass" .
is --files -g "*.Build.cs" .
```

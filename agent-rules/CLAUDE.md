# IndexSearch

For large source trees, prefer `is`/`indexsearch` over `rg` when an
IndexSearch index exists or when the project is an Unreal Engine tree.

- Use `is` first, `indexsearch` second, `rg` as fallback.
- Run `is watch .` when `index-search-project.txt` exists but the index has not
  been created yet.
- Run `is update --git .` after Git changes if no watcher was running.
- Use the UE template from `templates/unreal-engine/index-search-project.txt`
  when a UE project has no config.

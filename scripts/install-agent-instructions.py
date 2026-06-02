#!/usr/bin/env python3
"""Install IndexSearch agent instructions for common coding assistants."""

from __future__ import annotations

import argparse
import json
import shutil
import sys
from pathlib import Path


START = "<!-- indexsearch-agent:start -->"
END = "<!-- indexsearch-agent:end -->"


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def copy_tree(src: Path, dst: Path) -> None:
    dst.parent.mkdir(parents=True, exist_ok=True)
    shutil.copytree(src, dst, dirs_exist_ok=True)
    print(f"installed {dst}")


def read_text(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def write_marked_block(path: Path, block: str) -> None:
    wrapped = f"{START}\n{block.rstrip()}\n{END}\n"
    if path.exists():
        existing = read_text(path)
        start = existing.find(START)
        end = existing.find(END)
        if start != -1 and end != -1 and end > start:
            end += len(END)
            updated = existing[:start] + wrapped.rstrip() + existing[end:]
            if not updated.endswith("\n"):
                updated += "\n"
        else:
            sep = "" if existing.endswith("\n") or not existing else "\n"
            updated = existing + sep + "\n" + wrapped
    else:
        path.parent.mkdir(parents=True, exist_ok=True)
        updated = wrapped
    path.write_text(updated, encoding="utf-8")
    print(f"updated {path}")


def install_ue_template(root: Path, force: bool) -> None:
    src = repo_root() / "templates" / "unreal-engine" / "is-project-config.txt"
    dst = root / ".indexsearch" / "is-project-config.txt"
    legacy = root / "index-search-project.txt"
    existing = dst if dst.exists() else legacy if legacy.exists() else None
    if existing is not None and not force:
        print(f"kept existing {existing}; pass --force to replace it")
        return
    dst.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(src, dst)
    print(f"installed {dst}")


def install_codex(scope: str, project: Path | None) -> None:
    if scope == "project":
        if project is None:
            raise SystemExit("--project is required for project scope")
        install_agents(project)
        return
    copy_tree(repo_root() / "skills" / "indexsearch", Path.home() / ".codex" / "skills" / "indexsearch")


def install_claude(scope: str, project: Path | None) -> None:
    if scope == "project":
        if project is None:
            raise SystemExit("--project is required for project scope")
        skill_dir = project / ".claude" / "skills" / "indexsearch"
        copy_tree(repo_root() / "skills" / "indexsearch", skill_dir)
        install_claude_hook(
            project / ".claude" / "settings.json",
            "${CLAUDE_PROJECT_DIR}/.claude/skills/indexsearch/scripts/prefer-isgrep-hook.py",
        )
        write_marked_block(project / "CLAUDE.md", read_text(repo_root() / "agent-rules" / "CLAUDE.md"))
        return
    skill_dir = Path.home() / ".claude" / "skills" / "indexsearch"
    copy_tree(repo_root() / "skills" / "indexsearch", skill_dir)
    install_claude_hook(
        Path.home() / ".claude" / "settings.json",
        str(skill_dir / "scripts" / "prefer-isgrep-hook.py"),
    )


def install_claude_hook(settings_path: Path, script_path: str) -> None:
    hook = {
        "type": "command",
        "command": "python3",
        "args": [script_path],
        "timeout": 5,
    }
    if settings_path.exists():
        settings = json.loads(settings_path.read_text(encoding="utf-8"))
    else:
        settings = {}
    for group in settings.get("hooks", {}).get("PreToolUse", []):
        for existing in group.get("hooks", []):
            if existing.get("command") == "python3" and script_path in existing.get("args", []):
                print(f"kept existing Claude hook in {settings_path}")
                return
    settings.setdefault("hooks", {}).setdefault("PreToolUse", []).append(
        {"matcher": "Bash", "hooks": [hook]}
    )
    settings_path.parent.mkdir(parents=True, exist_ok=True)
    settings_path.write_text(json.dumps(settings, indent=2) + "\n", encoding="utf-8")
    print(f"updated {settings_path}")


def install_opencode(scope: str, project: Path | None) -> None:
    if scope == "project":
        if project is None:
            raise SystemExit("--project is required for project scope")
        install_agents(project)
        return
    write_marked_block(
        Path.home() / ".config" / "opencode" / "AGENTS.md",
        read_text(repo_root() / "agent-rules" / "AGENTS.md"),
    )


def install_cursor(scope: str, project: Path | None) -> None:
    if project is None:
        raise SystemExit("--project is required for Cursor rule installs")
    dst = project / ".cursor" / "rules" / "indexsearch.mdc"
    dst.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(repo_root() / "agent-rules" / "cursor" / "indexsearch.mdc", dst)
    print(f"installed {dst}")
    if scope == "project":
        install_agents(project)


def install_agents(project: Path) -> None:
    write_marked_block(project / "AGENTS.md", read_text(repo_root() / "agent-rules" / "AGENTS.md"))


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--target",
        choices=["all", "codex", "claude", "opencode", "cursor", "agents"],
        default="all",
        help="assistant integration to install",
    )
    parser.add_argument(
        "--scope",
        choices=["user", "project"],
        default="user",
        help="install globally for the user or into a project",
    )
    parser.add_argument("--project", type=Path, help="project root for project-scoped installs")
    parser.add_argument("--ue-template", action="store_true", help="copy the UE template into the project")
    parser.add_argument("--force", action="store_true", help="replace an existing UE template")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    project = args.project.resolve() if args.project else None

    if args.target == "all":
        targets = ["codex", "claude", "opencode"]
        if args.scope == "project" or project is not None:
            targets.append("cursor")
        else:
            print("skipping Cursor rule install; pass --project for Cursor")
    else:
        targets = [args.target]
    for target in targets:
        if target == "codex":
            install_codex(args.scope, project)
        elif target == "claude":
            install_claude(args.scope, project)
        elif target == "opencode":
            install_opencode(args.scope, project)
        elif target == "cursor":
            install_cursor(args.scope, project)
        elif target == "agents":
            if project is None:
                raise SystemExit("--project is required for AGENTS.md installs")
            install_agents(project)

    if args.ue_template:
        if project is None:
            raise SystemExit("--project is required with --ue-template")
        install_ue_template(project, args.force)

    return 0


if __name__ == "__main__":
    sys.exit(main())

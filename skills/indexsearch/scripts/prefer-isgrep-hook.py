#!/usr/bin/env python3
"""Claude Code PreToolUse hook that nudges Bash rg/grep calls to is/isgrep."""

from __future__ import annotations

import json
import os
import re
import shlex
import sys


WRAPPER_COMMANDS = {"command", "builtin", "env"}
SEPARATORS = {";", "&&", "||", "|", "(", ")"}
GREP_COMMANDS = {"grep", "egrep", "fgrep"}
RG_COMMANDS = {"rg", "ripgrep"}


def shell_tokens(command: str) -> list[str]:
    try:
        lexer = shlex.shlex(command, posix=True, punctuation_chars=";&|()")
        lexer.whitespace_split = True
        lexer.commenters = ""
        return list(lexer)
    except ValueError:
        return []


def is_assignment(token: str) -> bool:
    return re.match(r"^[A-Za-z_][A-Za-z0-9_]*=", token) is not None


def truthy_assignment(token: str, name: str) -> bool:
    if not token.startswith(f"{name}="):
        return False
    value = token.split("=", 1)[1].strip().lower()
    return value not in {"", "0", "false", "no"}


def truthy_env(name: str) -> bool:
    value = os.environ.get(name, "").strip().lower()
    return value not in {"", "0", "false", "no"}


def first_blocked_search(
    command: str,
    allow_grep: bool = False,
    allow_rg: bool = False,
) -> tuple[str, str, str] | None:
    tokens = shell_tokens(command)
    expect_command = True
    passthrough_wrapper = False
    for token in tokens:
        if token in SEPARATORS:
            expect_command = True
            passthrough_wrapper = False
            allow_grep = False
            allow_rg = False
            continue
        if not expect_command:
            continue
        if is_assignment(token):
            allow_grep = allow_grep or truthy_assignment(token, "INDEXSEARCH_ALLOW_GREP")
            allow_rg = allow_rg or truthy_assignment(token, "INDEXSEARCH_ALLOW_RG")
            continue
        name = os.path.basename(token)
        if name in WRAPPER_COMMANDS:
            passthrough_wrapper = True
            continue
        if passthrough_wrapper and token.startswith("-"):
            continue
        if name in GREP_COMMANDS:
            if allow_grep:
                expect_command = False
                passthrough_wrapper = False
                continue
            return name, "isgrep", "INDEXSEARCH_ALLOW_GREP"
        if name in RG_COMMANDS:
            if allow_rg:
                expect_command = False
                passthrough_wrapper = False
                continue
            return name, "is", "INDEXSEARCH_ALLOW_RG"
        expect_command = False
        passthrough_wrapper = False
    return None


def main() -> int:
    try:
        payload = json.load(sys.stdin)
    except json.JSONDecodeError:
        return 0
    if payload.get("tool_name") != "Bash":
        return 0
    command = payload.get("tool_input", {}).get("command") or ""
    blocked = first_blocked_search(
        command,
        allow_grep=truthy_env("INDEXSEARCH_ALLOW_GREP"),
        allow_rg=truthy_env("INDEXSEARCH_ALLOW_RG"),
    )
    if not blocked:
        return 0
    blocked_cmd, replacement, allow_var = blocked
    print(
        f"Blocked bare `{blocked_cmd}`. Use `{replacement}` for indexed local "
        f"source search; set {allow_var}=1 only when exact `{blocked_cmd}` "
        "semantics are intentionally required.",
        file=sys.stderr,
    )
    return 2


if __name__ == "__main__":
    raise SystemExit(main())

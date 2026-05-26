#!/usr/bin/env python3
"""Benchmark IndexSearch search performance across git revisions."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import statistics
import subprocess
import tempfile
import time
from pathlib import Path


DEFAULT_CASES = {
    "qualified-call": r"[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\(",
    "nanite": "Nanite",
    "skeletal": "SkeletalMeshComponent",
    "alternation": "(Nanite|Lumen|SkeletalMeshComponent)",
}


def run(cmd: list[str], cwd: Path | None = None, capture: bool = False) -> subprocess.CompletedProcess[bytes]:
    return subprocess.run(
        cmd,
        cwd=cwd,
        stdout=subprocess.PIPE if capture else subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        check=False,
    )


def checked(cmd: list[str], cwd: Path | None = None) -> None:
    subprocess.run(cmd, cwd=cwd, check=True)


def measure(cmd: list[str], repeats: int) -> dict[str, float]:
    run(cmd)
    values = []
    for _ in range(repeats):
        start = time.perf_counter()
        run(cmd)
        values.append((time.perf_counter() - start) * 1000)
    return {
        "min_ms": min(values),
        "median_ms": statistics.median(values),
        "max_ms": max(values),
    }


def line_count(cmd: list[str]) -> tuple[int, int]:
    proc = run(cmd, capture=True)
    stdout = proc.stdout or b""
    lines = stdout.count(b"\n") + (0 if not stdout or stdout.endswith(b"\n") else 1)
    return lines, len(stdout)


def safe_name(ref: str) -> str:
    return "".join(ch if ch.isalnum() or ch in "._-" else "_" for ch in ref)


def build_ref(repo: Path, ref: str, work_root: Path) -> Path:
    worktree = work_root / safe_name(ref)
    if not worktree.exists():
        checked(["git", "worktree", "add", "--detach", str(worktree), ref], cwd=repo)
    checked(["cargo", "build", "--release", "--locked"], cwd=worktree)
    return worktree / "target" / "release" / executable_name("indexsearch")


def executable_name(stem: str) -> str:
    return stem + ".exe" if os.name == "nt" else stem


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("root", help="large indexed repository to search")
    parser.add_argument(
        "--refs",
        nargs="+",
        default=["HEAD~1", "HEAD"],
        help="git refs to build and benchmark",
    )
    parser.add_argument(
        "--case",
        choices=sorted(DEFAULT_CASES),
        default="qualified-call",
    )
    parser.add_argument("--pattern", help="override the benchmark pattern")
    parser.add_argument("--repeats", type=int, default=5)
    parser.add_argument("--keep-worktrees", action="store_true")
    args = parser.parse_args()

    repo = Path(__file__).resolve().parents[1]
    root = str(Path(args.root).resolve())
    pattern = args.pattern or DEFAULT_CASES[args.case]

    work_root = Path(tempfile.mkdtemp(prefix="indexsearch-history-"))
    rows = []
    try:
        for ref in args.refs:
            exe = build_ref(repo, ref, work_root)
            cmd = [str(exe), pattern, root]
            lines, bytes_out = line_count(cmd)
            timings = measure(cmd, args.repeats)
            row = {
                "ref": ref,
                "case": args.case,
                "pattern": pattern,
                "lines": lines,
                "bytes": bytes_out,
                **timings,
            }
            rows.append(row)
            print(json.dumps(row), flush=True)

        print()
        print("| Ref | Lines | Output | Min | Median | Max |")
        print("| --- | ---: | ---: | ---: | ---: | ---: |")
        for row in rows:
            mib = row["bytes"] / (1024 * 1024)
            print(
                f"| `{row['ref']}` | {row['lines']} | {mib:.1f} MiB | "
                f"{row['min_ms']:.2f}ms | {row['median_ms']:.2f}ms | {row['max_ms']:.2f}ms |"
            )
    finally:
        if args.keep_worktrees:
            print(f"kept worktrees under {work_root}")
        else:
            for worktree in work_root.iterdir():
                if worktree.is_dir():
                    subprocess.run(
                        ["git", "worktree", "remove", "--force", str(worktree)],
                        cwd=repo,
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                    )
            shutil.rmtree(work_root, ignore_errors=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

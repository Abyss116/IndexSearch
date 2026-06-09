#!/usr/bin/env python3
"""Benchmark IndexSearch, qgrep, ripgrep, and grep on an Unreal Engine checkout."""

from __future__ import annotations

import argparse
import fnmatch
import json
import os
import shutil
import statistics
import subprocess
import time
from pathlib import Path


INCLUDES = [
    "*.bat",
    "*.c",
    "*.cc",
    "*.cpp",
    "*.cs",
    "*.cxx",
    "*.h",
    "*.hh",
    "*.hpp",
    "*.ini",
    "*.inl",
    "*.ispc",
    "*.isph",
    "*.txt",
    "*.uplugin",
    "*.uproject",
    "*.usf",
    "*.ush",
    "*.verse",
    "*.json",
    "*.xml",
]

EXCLUDES = [
    ".indexsearch/**",
    ".git/**",
    ".vs/**",
    ".vscode/**",
    "**/Cooked/**",
    "**/DerivedDataCache/**",
    "**/Intermediate/**",
    "**/Saved/**",
    "**/Staged/**",
    "**/Content/**",
    "**/Binaries/**",
    "**/Logs/**",
    "**/Private_Projects/**",
    "**/obj/**",
    "**/bin/**",
    "**/enc_temp_folder/**",
    "**/LocalBuilds/**",
    "Engine/Programs/UnrealBuildTool/Log*",
    "*.bak",
    "*.cpp.txt",
    "*.gen.cpp",
    "*.gen.h",
    "*.udd",
    "*.uasset",
    "**/ThirdParty/**/*.json",
]

GREP_BIN = os.environ.get("GREP", shutil.which("grep") or "/usr/bin/grep")
GREP_BATCH_SIZE = int(os.environ.get("GREP_BATCH_SIZE", "512"))

GREP_EXCLUDE_DIRS = [
    ".indexsearch",
    ".git",
    ".vs",
    ".vscode",
    "Cooked",
    "DerivedDataCache",
    "Intermediate",
    "Saved",
    "Staged",
    "Content",
    "Binaries",
    "Logs",
    "Private_Projects",
    "obj",
    "bin",
    "enc_temp_folder",
    "LocalBuilds",
]

GREP_EXCLUDE_FILES = [
    "*.bak",
    "*.cpp.txt",
    "*.gen.cpp",
    "*.gen.h",
    "*.udd",
    "*.uasset",
]


def qgrep_config(root: Path, path: Path) -> None:
    text = f"""path {root}
include \\.(bat|c|cc|cpp|cs|cxx|h|hh|hpp|ini|inl|ispc|isph|txt|uplugin|uproject|usf|ush|verse|json|xml)$
exclude (^|.*/)\\.indexsearch/
exclude (^|.*/)\\.git/
exclude (^|.*/)\\.vs/
exclude (^|.*/)\\.vscode/
exclude (^|.*/)Cooked/
exclude (^|.*/)DerivedDataCache/
exclude (^|.*/)Intermediate/
exclude (^|.*/)Saved/
exclude (^|.*/)Staged/
exclude (^|.*/)Content/
exclude (^|.*/)Binaries/
exclude (^|.*/)Logs/
exclude (^|.*/)Private_Projects/
exclude (^|.*/)obj/
exclude (^|.*/)bin/
exclude (^|.*/)enc_temp_folder/
exclude (^|.*/)LocalBuilds/
exclude Engine/Programs/UnrealBuildTool/Log.*
exclude \\.bak$
exclude \\.cpp\\.txt$
exclude \\.gen\\.cpp$
exclude \\.gen\\.h$
exclude \\.udd$
exclude \\.uasset$
exclude (^|.*/)ThirdParty/.*\\.json$
"""
    path.write_text(text)


def rg_flags(include_globs: bool = True) -> list[str]:
    flags = ["--hidden", "--no-ignore-vcs"]
    if include_globs:
        for glob in INCLUDES:
            flags += ["-g", glob]
    for glob in EXCLUDES:
        flags += ["-g", "!" + glob]
    return flags


def grep_file_allowed(root: Path, path: Path, include_globs: list[str]) -> bool:
    name = path.name
    if not any(fnmatch.fnmatchcase(name, glob) for glob in include_globs):
        return False
    if any(fnmatch.fnmatchcase(name, glob) for glob in GREP_EXCLUDE_FILES):
        return False

    rel = path.relative_to(root).as_posix()
    if rel.startswith("Engine/Programs/UnrealBuildTool/Log"):
        return False
    if "/ThirdParty/" in f"/{rel}" and name.endswith(".json"):
        return False
    return True


def collect_grep_files(root: Path, include_globs: list[str]) -> list[str]:
    files: list[str] = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [name for name in dirnames if name not in GREP_EXCLUDE_DIRS]
        base = Path(dirpath)
        for filename in filenames:
            path = base / filename
            if grep_file_allowed(root, path, include_globs):
                files.append(str(path))
    return files


def run(cmd: list[str]) -> int:
    return subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode


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


def run_grep(files: list[str], grep_args: list[str], pattern: str) -> None:
    for start in range(0, len(files), GREP_BATCH_SIZE):
        batch = files[start : start + GREP_BATCH_SIZE]
        subprocess.run(
            [GREP_BIN, "-I", "-n", *grep_args, "--", pattern, *batch],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )


def measure_grep(files: list[str], grep_args: list[str], pattern: str, repeats: int) -> dict[str, float]:
    values = []
    for _ in range(repeats):
        start = time.perf_counter()
        run_grep(files, grep_args, pattern)
        values.append((time.perf_counter() - start) * 1000)
    return {
        "min_ms": min(values),
        "median_ms": statistics.median(values),
        "max_ms": max(values),
    }


def count_lines(cmd: list[str]) -> int:
    proc = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    if not proc.stdout:
        return 0
    return proc.stdout.count(b"\n") + (0 if proc.stdout.endswith(b"\n") else 1)


def count_grep_lines(files: list[str], grep_args: list[str], pattern: str) -> int:
    count = 0
    for start in range(0, len(files), GREP_BATCH_SIZE):
        batch = files[start : start + GREP_BATCH_SIZE]
        proc = subprocess.run(
            [GREP_BIN, "-I", "-n", *grep_args, "--", pattern, *batch],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        if proc.stdout:
            count += proc.stdout.count(b"\n") + (0 if proc.stdout.endswith(b"\n") else 1)
    return count


def speedup(tool: dict[str, float], baseline: dict[str, float]) -> float | None:
    target = tool["median_ms"]
    if target <= 0:
        return None
    return baseline["median_ms"] / target


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("root", nargs="?", default="/Users/abyss/Projects/UnrealEngine")
    parser.add_argument("--qgrep-config", default="/tmp/ue-qgrep.cfg")
    parser.add_argument("--search-repeats", type=int, default=5)
    parser.add_argument("--rg-repeats", type=int, default=3)
    parser.add_argument("--grep-repeats", type=int, default=1)
    parser.add_argument("--case", action="append", help="Run only the named benchmark case; may be repeated.")
    parser.add_argument("--prepare-qgrep", action="store_true")
    args = parser.parse_args()

    root = Path(args.root).resolve()
    qcfg = Path(args.qgrep_config)
    qgrep_config(root, qcfg)
    if args.prepare_qgrep:
        subprocess.run(
            ["qgrep", "update", str(qcfg)],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )

    base_rg = rg_flags(True)
    rg_cpp = rg_flags(False)
    grep_all_files = collect_grep_files(root, INCLUDES)
    grep_cpp_files = collect_grep_files(root, ["*.cpp"])
    cases = [
        (
            "Literal: common token",
            "Nanite",
            ["is", "-F", "Nanite", str(root)],
            ["qgrep", "search", str(qcfg), "l", "Nanite"],
            ["rg", *base_rg, "-F", "Nanite", str(root)],
            ["-F"],
            "Nanite",
            grep_all_files,
            len(grep_all_files),
        ),
        (
            "Literal: long symbol",
            "SkeletalMeshComponent",
            ["is", "-F", "SkeletalMeshComponent", str(root)],
            ["qgrep", "search", str(qcfg), "l", "SkeletalMeshComponent"],
            ["rg", *base_rg, "-F", "SkeletalMeshComponent", str(root)],
            ["-F"],
            "SkeletalMeshComponent",
            grep_all_files,
            len(grep_all_files),
        ),
        (
            "Literal: missing",
            "DefinitelyMissingIndexSearchNeedle",
            ["is", "-F", "DefinitelyMissingIndexSearchNeedle", str(root)],
            ["qgrep", "search", str(qcfg), "l", "DefinitelyMissingIndexSearchNeedle"],
            ["rg", *base_rg, "-F", "DefinitelyMissingIndexSearchNeedle", str(root)],
            ["-F"],
            "DefinitelyMissingIndexSearchNeedle",
            grep_all_files,
            len(grep_all_files),
        ),
        (
            "Case-insensitive literal",
            "skeletalmeshcomponent",
            ["is", "-i", "-F", "skeletalmeshcomponent", str(root)],
            ["qgrep", "search", str(qcfg), "il", "skeletalmeshcomponent"],
            ["rg", *base_rg, "-i", "-F", "skeletalmeshcomponent", str(root)],
            ["-i", "-F"],
            "skeletalmeshcomponent",
            grep_all_files,
            len(grep_all_files),
        ),
        (
            "Word regex",
            r"\bActor\b",
            ["is", "-w", "Actor", str(root)],
            ["qgrep", "search", str(qcfg), "", r"\bActor\b"],
            ["rg", *base_rg, "-w", "Actor", str(root)],
            ["-w"],
            "Actor",
            grep_all_files,
            len(grep_all_files),
        ),
        (
            "Regex: alternation",
            "(Nanite|Lumen|SkeletalMeshComponent)",
            ["is", "(Nanite|Lumen|SkeletalMeshComponent)", str(root)],
            ["qgrep", "search", str(qcfg), "", "(Nanite|Lumen|SkeletalMeshComponent)"],
            ["rg", *base_rg, "(Nanite|Lumen|SkeletalMeshComponent)", str(root)],
            ["-E"],
            "(Nanite|Lumen|SkeletalMeshComponent)",
            grep_all_files,
            len(grep_all_files),
        ),
        (
            "Regex: prefix/suffix",
            "Skeletal[A-Za-z0-9_]*Component",
            ["is", "Skeletal[A-Za-z0-9_]*Component", str(root)],
            ["qgrep", "search", str(qcfg), "", "Skeletal[A-Za-z0-9_]*Component"],
            ["rg", *base_rg, "Skeletal[A-Za-z0-9_]*Component", str(root)],
            ["-E"],
            "Skeletal[A-Za-z0-9_]*Component",
            grep_all_files,
            len(grep_all_files),
        ),
        (
            "Regex: qualified call",
            r"[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\(",
            ["is", r"[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\(", str(root)],
            ["qgrep", "search", str(qcfg), "", r"[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\("],
            ["rg", *base_rg, r"[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\(", str(root)],
            ["-E"],
            r"[A-Za-z_][A-Za-z0-9_]*::[A-Za-z0-9_]+\(",
            grep_all_files,
            len(grep_all_files),
        ),
        (
            "Glob: *.cpp literal",
            "Nanite in *.cpp",
            ["is", "-F", "-g", "*.cpp", "Nanite", str(root)],
            ["qgrep", "search", str(qcfg), r"lfi\.cpp$", "Nanite"],
            ["rg", *rg_cpp, "-g", "*.cpp", "-F", "Nanite", str(root)],
            ["-F"],
            "Nanite",
            grep_cpp_files,
            len(grep_cpp_files),
        ),
    ]
    if args.case:
        selected = set(args.case)
        known = {case[0] for case in cases}
        unknown = selected - known
        if unknown:
            parser.error(f"unknown benchmark case(s): {', '.join(sorted(unknown))}")
        cases = [case for case in cases if case[0] in selected]

    subprocess.run(["is", "-F", "Nanite", str(root)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    rows = []
    for name, pattern, is_cmd, qgrep_cmd, rg_cmd, grep_args, grep_pattern, grep_files, grep_file_count in cases:
        matches = {
            "indexsearch": count_lines(is_cmd),
            "qgrep": count_lines(qgrep_cmd),
            "rg": count_lines(rg_cmd),
            "grep": count_grep_lines(grep_files, grep_args, grep_pattern),
        }
        timings = {
            "indexsearch": measure(is_cmd, args.search_repeats),
            "qgrep": measure(qgrep_cmd, args.search_repeats),
            "rg": measure(rg_cmd, args.rg_repeats),
            "grep": measure_grep(grep_files, grep_args, grep_pattern, args.grep_repeats),
        }
        row = {
            "case": name,
            "pattern": pattern,
            "grep_files": grep_file_count,
            "matches": matches,
            "timings": timings,
            "speedups": {
                "vs_qgrep": speedup(timings["indexsearch"], timings["qgrep"]),
                "vs_rg": speedup(timings["indexsearch"], timings["rg"]),
                "vs_grep": speedup(timings["indexsearch"], timings["grep"]),
            },
        }
        rows.append(row)
        print(json.dumps(row), flush=True)

    print(json.dumps({"rows": rows}, indent=2), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

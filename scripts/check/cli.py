#!/usr/bin/env python3
"""格式与静态检查总闸。

代码格式化与静态检查聚合入口，任一检查失败即非零退出。

| 检查 | 命令 |
|---|---|
| Rust 格式 | `cargo fmt --all --check` |
| Rust 静态检查 | `cargo clippy --workspace --all-targets`（`-D warnings`） |
| Python 格式 | `ruff format --check` |
| Python 静态检查 | `ruff check` + `pyright` |
| 治理与文档门禁 | `.agents/scripts/gates/verify_agent_gates.py` |

## clippy 的分级策略

`-D warnings` 是阻塞项（基线为 0 告警）。

`-W clippy::pedantic` 为非阻塞参考建议项，可通过 `--pedantic` 显式开启评估。
"""

from __future__ import annotations

import argparse
import sys

from scripts.lib.common import REPO_ROOT, BuildError, cargo_env, run

#: 治理门禁脚本。
GATES = REPO_ROOT / ".agents" / "scripts" / "gates" / "verify_agent_gates.py"

#: Python 源码的检查范围（与 pyproject.toml 的 pyright include 保持一致）。
PYTHON_PATHS = ("scripts", ".agents/scripts")


def rust_checks(*, fix: bool, pedantic: bool, quiet: bool) -> list[list[str]]:
    """Rust 侧的检查命令列表（每个元素是一条完整命令行）。"""
    if fix:
        return [["cargo", "fmt", "--all"]]

    commands = [["cargo", "fmt", "--all", "--check"]]

    clippy = ["cargo", "clippy", "--workspace", "--all-targets"]
    if quiet:
        # `--message-format=short` 让每条告警一行，便于在 CI 日志里读。
        clippy += ["--message-format=short"]
    clippy += ["--"]
    if not pedantic:
        clippy += ["-D", "warnings"]
    else:
        clippy += ["-W", "clippy::pedantic"]
    commands.append(clippy)
    return commands


def python_checks(*, fix: bool) -> list[list[str]]:
    """Python 侧的检查命令列表。

    统一通过 `uv run` 执行：工具版本由 `uv.lock` 严格锁定，
    避免因宿主环境安装了其他版本而导致静态检查结论不一致。
    """
    if fix:
        return [
            ["uv", "run", "ruff", "format", *PYTHON_PATHS],
            ["uv", "run", "ruff", "check", "--fix", *PYTHON_PATHS],
        ]

    return [
        ["uv", "run", "ruff", "format", "--check", *PYTHON_PATHS],
        ["uv", "run", "ruff", "check", *PYTHON_PATHS],
        ["uv", "run", "pyright", *PYTHON_PATHS],
    ]


def main(argv: list[str] | None = None) -> int:
    """命令行入口。"""
    parser = argparse.ArgumentParser(
        description="Unified code formatting and static-analysis gate for GadgetDisk"
    )
    parser.add_argument(
        "--fix",
        action="store_true",
        help="Automatically fix formatting and auto-fixable lint issues in place",
    )
    parser.add_argument(
        "--rust-only",
        action="store_true",
        help="Run Rust checks only (cargo fmt, clippy)",
    )
    parser.add_argument(
        "--python-only",
        action="store_true",
        help="Run Python checks only (ruff, pyright)",
    )
    parser.add_argument(
        "--pedantic",
        action="store_true",
        help="Enable clippy::pedantic lints (non-blocking baseline for code review)",
    )
    parser.add_argument(
        "--quiet",
        action="store_true",
        help="Use clippy's short message format",
    )
    parser.add_argument(
        "--no-gates",
        action="store_true",
        help="Skip governance and documentation gate checks",
    )
    args = parser.parse_args(argv)

    if args.rust_only and args.python_only:
        print(
            "error: options --rust-only and --python-only are mutually exclusive.",
            file=sys.stderr,
        )
        return 2

    do_rust = not args.python_only
    do_python = not args.rust_only

    planned: list[tuple[str, list[str]]] = []
    if do_rust:
        for command in rust_checks(fix=args.fix, pedantic=args.pedantic, quiet=args.quiet):
            planned.append(("Rust", command))
    if do_python:
        for command in python_checks(fix=args.fix):
            planned.append(("Python", command))
    if not args.fix and not args.no_gates:
        planned.append(("governance", [sys.executable, str(GATES)]))

    if not planned:
        print("No checks were selected to run.")
        return 0

    env = cargo_env()
    failures: list[tuple[str, list[str], str]] = []

    for kind, command in planned:
        print(f"\n===== [{kind}] {' '.join(command)} =====")
        try:
            run(command, env=env, capture=False)
        except BuildError as exc:
            failures.append((kind, command, str(exc)))
            if not args.fix:
                continue

    if failures:
        print(f"\nCheck failed: {len(failures)} check item(s) failed", file=sys.stderr)
        for kind, command, message in failures:
            print(f"\n  [{kind}] {' '.join(command)}", file=sys.stderr)
            print(f"      {message.splitlines()[0]}", file=sys.stderr)
        if args.fix:
            print(
                "\nApplied auto-fixable changes; re-run `uv run gd-check` to verify.",
                file=sys.stderr,
            )
        return 1

    if args.fix:
        print("\nApplied formatting and auto-fixes; re-run `uv run gd-check` to verify.")
    else:
        print(f"\nAll {len(planned)} checks passed.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

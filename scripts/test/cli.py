#!/usr/bin/env python3
"""差分 / 全量测试脚本。

契约见 docs/testing.md：

- 默认模式：按 `git diff` 自动选择受影响的 crate（差分测试）；
- `--all`：全量运行所有测试（仅在提交前或 CI 中使用）；
- `--base <ref>`：指定比较基线；
- `--dry-run`：只打印将执行的命令。

nextest 缺失时给出官方安装器的指引（预编译二进制，避免源码编译拉入 cmake 依赖），
并回退到 `cargo test`（明确提示失去 `-E` 过滤能力），而不是静默失败。
"""

from __future__ import annotations

import argparse
import re
import shutil
import subprocess
import sys

from scripts.lib.common import (
    REPO_ROOT,
    WEBUI_DIR,
    BuildError,
    cargo_env,
    run,
)

#: WebUI 相对仓库根的路径前缀（`webui/`）。
_WEBUI_PREFIX = f"{WEBUI_DIR.name}/"

#: crate 目录前缀 → crate 名（docs/testing.md 要求新增 crate 时同步维护）。
CRATE_PREFIXES: dict[str, str] = {
    "crates/gadgetdisk-core/": "gadgetdisk-core",
    "crates/gadgetdisk-proto/": "gadgetdisk-proto",
    "crates/gadgetdisk-usb/": "gadgetdisk-usb",
    "crates/gadgetdisk-loop/": "gadgetdisk-loop",
    "crates/gadgetdisk-gdd/": "gadgetdisk-gdd",
    "crates/gadgetdisk-cli/": "gadgetdisk-cli",
    "crates/gadgetdisk-mkfsvfat/": "gadgetdisk-mkfsvfat",
}

#: 这些前缀的改动不影响 Rust 产物，可跳过 Rust 测试。
NON_RUST_PREFIXES: tuple[str, ...] = (
    "docs/",
    ".agents/",
    _WEBUI_PREFIX,
)

#: 触发 WebUI（Node）测试的路径前缀。
#:
#: WebUI 的测试就在其自身目录下的 `tests/`，因此一个前缀即可覆盖「改实现」
#: 与「改测试」两种情况。
WEBUI_PREFIXES: tuple[str, ...] = (_WEBUI_PREFIX,)

#: 这些文件一旦改动，必须保守回退全量测试。
FULL_RUN_FILES: frozenset[str] = frozenset(
    {
        "Cargo.toml",
        "Cargo.lock",
        "rustfmt.toml",
        "pyproject.toml",
        "uv.lock",
    }
)

#: 非 Rust 但会影响测试选择的目录/文件前缀（改动即回退全量）。
FULL_RUN_PREFIXES: tuple[str, ...] = ("scripts/",)

NEXTEST_INSTALL_HINT = """\
`cargo-nextest` not found. Install options (any one; all install into `~/.cargo/bin`):

  1. Official installer:
       curl --proto '=https' --tlsv1.2 -LsSf https://get.nexte.st/latest/linux | sh
  2. If you already have cargo-binstall:
       cargo binstall cargo-nextest
  3. Download a prebuilt binary manually (GitHub Releases):
       https://github.com/nextest-rs/nextest/releases

  Do NOT use `cargo install cargo-nextest`: a source build pulls in `aws-lc-sys`,
  which needs cmake and is unusable on a machine without it.

Falling back to `cargo test`: tests still run, but you lose the ability to filter a
single test by name with a -E expression.
"""


def git_changed_files(base: str | None) -> list[str] | None:
    """收集改动文件集合。

    返回 `None` 表示无法判定（例如不在 git 仓库中），调用方应保守回退全量。
    """
    files: set[str] = set()

    def git(*args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["git", *args],
            cwd=str(REPO_ROOT),
            text=True,
            capture_output=True,
            check=False,
        )

    inside = git("rev-parse", "--is-inside-work-tree")
    if inside.returncode != 0 or inside.stdout.strip() != "true":
        return None

    if base:
        diff = git("diff", "--name-only", f"{base}...HEAD")
        if diff.returncode != 0:
            return None
        files.update(diff.stdout.split())
    else:
        # 无基线：比较 HEAD 与工作区（含未跟踪文件）。
        for args in (
            ("diff", "--name-only", "HEAD"),
            ("diff", "--name-only", "--cached"),
            ("ls-files", "--others", "--exclude-standard"),
        ):
            completed = git(*args)
            if completed.returncode != 0:
                # 空仓库（无 HEAD）时 diff 会失败，视为「无法判定」。
                return None
            files.update(completed.stdout.split())

    return sorted(files)


def select_crates(files: list[str]) -> tuple[list[str], str]:
    """把改动文件映射为待测 crate 列表。

    返回 `(crates, reason)`；`crates` 为空表示无需跑 Rust 测试。
    """
    if not files:
        return [], "no changes detected"

    for name in files:
        if name in FULL_RUN_FILES or name.startswith(FULL_RUN_PREFIXES):
            return sorted(
                set(CRATE_PREFIXES.values())
            ), f"{name} was modified; conservatively falling back to full test run"
        # 发现未被映射表覆盖的 crate 目录 → 保守回退全量。
        match = re.match(r"crates/([^/]+)/", name)
        if match and f"crates/{match.group(1)}/" not in CRATE_PREFIXES:
            return sorted(set(CRATE_PREFIXES.values())), (
                f"found a crate directory not covered by the mapping table: "
                f"crates/{match.group(1)}/; "
                "falling back to the full run "
                "(please update the mapping table in scripts/test/cli.py)"
            )

    if all(_is_non_rust(name) for name in files):
        return (
            [],
            "Only docs, governance, or WebUI static assets were modified; skipping Rust tests",
        )

    hits = {
        crate
        for name in files
        for prefix, crate in CRATE_PREFIXES.items()
        if name.startswith(prefix)
    }

    unmapped = [
        name
        for name in files
        if not _is_non_rust(name)
        and not _matches_any(name, CRATE_PREFIXES)
        and not name.startswith("crates/")
    ]
    if unmapped:
        return sorted(set(CRATE_PREFIXES.values())), (
            "these changes cannot be mapped to a crate; falling back to the full run: "
            f"{', '.join(unmapped[:5])}"
        )

    if not hits:
        return sorted(
            set(CRATE_PREFIXES.values())
        ), "changes could not be mapped to a crate; conservatively falling back to the full run"

    return sorted(hits), f"Impacted crates: {', '.join(sorted(hits))}"


def affects_webui(files: list[str]) -> bool:
    """改动是否影响 WebUI（进而需要跑 Node 测试）。"""
    return any(name.startswith(WEBUI_PREFIXES) for name in files)


def run_webui_tests(dry_run: bool) -> int:
    """运行 WebUI 的 Node 原生测试。"""
    if shutil.which("node") is None:
        print(
            "warning: `node` not found; skipping WebUI tests.\n"
            "         The WebUI logic tests need Node's test runner.",
            file=sys.stderr,
        )
        return 0

    command = ["node", "--test", "tests/"]
    if dry_run:
        print("\nwill run:")
        print(f"  (in {WEBUI_DIR.name}/) " + " ".join(command))
        return 0

    return subprocess.call(command, cwd=str(WEBUI_DIR))


def _matches_any(name: str, mapping: dict[str, str]) -> bool:
    return any(name.startswith(prefix) for prefix in mapping)


def _is_non_rust(name: str) -> bool:
    return any(name.startswith(prefix) for prefix in NON_RUST_PREFIXES)


# ---------------------------------------------------------------- nextest


def find_nextest() -> str | None:
    """定位 `cargo-nextest`：从 `PATH` 查找。

    安装位置由官方安装器决定（缺省 `~/.cargo/bin`，本就在 `PATH` 上）；
    本脚本**不再**自建仓库内工具目录，避免每份检出各存一份二进制。
    """
    return shutil.which("cargo-nextest")


# ---------------------------------------------------------------- 主流程


def build_command(nextest: str | None, crates: list[str]) -> list[str]:
    """构造测试命令。

    预编译的 `cargo-nextest` 二进制本身模拟 `cargo` multicall，
    因此必须以 `cargo-nextest nextest run` 调用（直接 `cargo-nextest run`
    会被当成未知的 cargo 子命令）。
    """
    if nextest:
        command = [nextest, "nextest", "run"]
        for crate in crates:
            command += ["-p", crate]
        command.append("--no-fail-fast")
        return command

    command = ["cargo", "test"]
    for crate in crates:
        command += ["-p", crate]
    return command


def main(argv: list[str] | None = None) -> int:
    """命令行入口。"""
    parser = argparse.ArgumentParser(
        description="GadgetDisk differential / full test run",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=(
            "Examples:\n"
            "  uv run gd-test                 # select tests from git diff\n"
            "  uv run gd-test --all           # full run (before committing)\n"
            "  uv run gd-test --base main     # use a specific baseline\n"
            "  uv run gd-test --dry-run       # print commands only\n"
        ),
    )
    parser.add_argument(
        "--all",
        action="store_true",
        help="Run the complete test suite across all crates and WebUI Node tests",
    )
    parser.add_argument(
        "--base",
        default=None,
        help="Git baseline ref to compare against (e.g., origin/main)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the planned test commands without executing them",
    )
    args = parser.parse_args(argv)

    env = cargo_env()
    nextest = find_nextest()
    files: list[str] = []

    if args.all:
        crates = sorted(set(CRATE_PREFIXES.values()))
        reason = "--all requests the full run"
    else:
        changed = git_changed_files(args.base)
        if changed is None:
            crates = sorted(set(CRATE_PREFIXES.values()))
            reason = (
                "cannot obtain the set of git changes (not a repo, or HEAD missing); "
                "falling back to the full run"
            )
        else:
            files = changed
            crates, reason = select_crates(files)
            if files:
                print(f"changed files ({len(files)}):")
                for name in files[:20]:
                    print(f"  {name}")
                if len(files) > 20:
                    print(f"  ... and {len(files) - 20} more")
                print()

    print(f"Selection basis  : {reason}")

    # WebUI 静态资源与测试本身的改动需要跑 Node 测试。
    need_webui = args.all or affects_webui(files)

    if not crates:
        print("Skipping Rust tests.")
        if need_webui:
            return run_webui_tests(args.dry_run)
        return 0

    print(f"Crates under test: {', '.join(crates)}")

    if nextest:
        print(f"Test runner      : cargo-nextest ({nextest})")
    else:
        print(NEXTEST_INSTALL_HINT, file=sys.stderr)

    command = build_command(nextest, crates)

    if args.dry_run:
        print("\nwill run:")
        print("  " + " ".join(command))
        return 0

    try:
        run(command, env=env)
    except BuildError as exc:
        print(f"\n{exc}", file=sys.stderr)
        return 1

    if need_webui:
        print()
        return run_webui_tests(args.dry_run)

    return 0


if __name__ == "__main__":
    raise SystemExit(main())

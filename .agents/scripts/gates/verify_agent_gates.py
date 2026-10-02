#!/usr/bin/env python3
"""GadgetDisk 治理门禁。

零第三方依赖，仅用 Python 3 标准库。校验四类规则：

1. Agent Note 头部格式与 status/目录一致性
2. Agent Note 必需章节
3. 根 AGENTS.md 词数预算
4. Markdown 相对链接与锚点完整性

用法:
    python3 .agents/scripts/gates/verify_agent_gates.py [--root DIR] [-v]

退出码: 0 全部通过; 1 存在违规。
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

# ---------------------------------------------------------------- 配置

NOTE_STATUSES = ("proposed", "implemented", "rejected", "archived")
NOTE_CATEGORIES = (
    "feature",
    "bug-fix",
    "simplification",
    "architecture",
    "process",
    "testing",
)

REQUIRED_SECTIONS = (
    "## Problem",
    "## Proposal",
    "## Alternatives considered",
    "## Acceptance criteria",
    "## Risks",
)

AGENTS_WORD_BUDGET = 1500

NOTES_REL = Path(".agents/notes")

# 治理文件本身不是 Agent Note，不参与 Note 格式校验
NOTE_DIR_EXEMPT_NAMES = {"README.md", "AGENTS.md"}

# 不参与链接校验的 URL scheme
EXTERNAL_SCHEMES = ("http://", "https://", "mailto:", "tel:", "data:")

# 遍历时跳过的目录名。
#
# 这些目录不承载本仓库的规格事实：前者是 git 内部数据，后者是依赖与构建产物。
# 尤其 `target` / `.venv` / `.uv-cache` 会引入第三方
# Markdown（其相对链接指向上游仓库的文件），把它们纳入校验只会产生与本源无关的误报。
SKIP_DIR_NAMES = frozenset(
    {
        ".git",
        "node_modules",
        ".venv",
        ".uv-cache",
        ".ruff_cache",
        ".pyright",
        "target",
        "local",
        ".probe",
        ".tmp",
    }
)

LINK_RE = re.compile(r"!?\[(?P<text>[^\]]*)\]\((?P<target>[^)]+)\)")
H1_RE = re.compile(r"^#\s+(?P<title>.+?)\s*$")
STATUS_RE = re.compile(r"^Status:\s*(?P<status>\S+)\s*$", re.MULTILINE)
FENCE_RE = re.compile(r"^\s*(```|~~~)")

# 中文按字计，ASCII 按词计
CJK_RE = re.compile(r"[\u3400-\u4dbf\u4e00-\u9fff\uf900-\ufaff]")
ASCII_WORD_RE = re.compile(r"[A-Za-z0-9_][A-Za-z0-9_./+-]*")


class Report:
    """收集违规项并按文件分组输出。"""

    def __init__(self, root: Path, verbose: bool = False) -> None:
        self.root = root
        self.verbose = verbose
        self.errors: list[tuple[Path, str]] = []
        self.checked = 0

    def error(self, path: Path, message: str) -> None:
        self.errors.append((path, message))

    def ok(self, message: str) -> None:
        if self.verbose:
            print(f"  ok  {message}")

    def rel(self, path: Path) -> str:
        try:
            return str(path.relative_to(self.root))
        except ValueError:
            return str(path)

    def finish(self) -> int:
        if not self.errors:
            print(f"PASS  {self.checked} 个文件通过治理门禁")
            return 0

        print(f"FAIL  {len(self.errors)} 处违规（已检查 {self.checked} 个文件）\n")
        by_file: dict[Path, list[str]] = {}
        for path, message in self.errors:
            by_file.setdefault(path, []).append(message)
        for path in sorted(by_file, key=str):
            print(f"  {self.rel(path)}")
            for message in by_file[path]:
                print(f"      - {message}")
            print()
        return 1


def strip_code(text: str) -> str:
    """把围栏代码块内容替换为空行，避免在代码中误判标题/链接。"""
    out: list[str] = []
    in_fence = False
    for line in text.splitlines():
        if FENCE_RE.match(line):
            in_fence = not in_fence
            out.append("")
            continue
        out.append("" if in_fence else line)
    return "\n".join(out)


def count_words(text: str) -> int:
    """中文按字符计，ASCII 按词计；忽略代码块。"""
    body = strip_code(text)
    return len(CJK_RE.findall(body)) + len(ASCII_WORD_RE.findall(body))


def is_note_file(path: Path, root: Path) -> bool:
    """判断是否位于 .agents/notes/<status>/<category>/ 下且非治理文件。"""
    try:
        rel = path.relative_to(root / NOTES_REL)
    except ValueError:
        return False
    parts = rel.parts
    if len(parts) < 3:
        return False
    return parts[0] in NOTE_STATUSES and parts[1] in NOTE_CATEGORIES


# ---------------------------------------------------------------- 校验 1 & 2


def check_note(path: Path, root: Path, report: Report) -> None:
    text = path.read_text(encoding="utf-8")
    body = strip_code(text)
    rel = path.relative_to(root / NOTES_REL)
    expected_status = rel.parts[0]

    # 头部 H1
    first_heading = None
    for line in body.splitlines():
        match = H1_RE.match(line)
        if match:
            first_heading = match.group("title")
            break
    if first_heading is None:
        report.error(path, "缺少 H1 标题")
    elif not first_heading.startswith("Agent Note:"):
        report.error(
            path,
            f"H1 必须以 'Agent Note: ' 开头，当前为 '# {first_heading}'",
        )

    # Status 行
    status_match = STATUS_RE.search(body)
    if status_match is None:
        report.error(path, "缺少 'Status: <status>' 行")
    else:
        status = status_match.group("status")
        if status != expected_status:
            report.error(
                path,
                f"Status '{status}' 与所在目录 '{expected_status}/' 不一致",
            )

    # 必需章节
    headings = {line.strip() for line in body.splitlines() if line.startswith("## ")}
    for section in REQUIRED_SECTIONS:
        if section not in headings:
            report.error(path, f"缺少必需章节 '{section}'")

    # 文件名规范
    if not re.match(r"^\d{4}-\d{2}-\d{2}-[a-z0-9]+(?:-[a-z0-9]+)*\.md$", path.name):
        report.error(
            path,
            "文件名应为 'yyyy-mm-dd-kebab-title.md'",
        )


# ---------------------------------------------------------------- 校验 3


def check_budget(path: Path, report: Report) -> None:
    words = count_words(path.read_text(encoding="utf-8"))
    if words > AGENTS_WORD_BUDGET:
        report.error(
            path,
            f"词数 {words} 超出预算 {AGENTS_WORD_BUDGET}（超出 {words - AGENTS_WORD_BUDGET}）",
        )
    else:
        report.ok(f"{report.rel(path)} 词数 {words}/{AGENTS_WORD_BUDGET}")


# ---------------------------------------------------------------- 校验 4


def slugify(heading: str) -> str:
    """近似 GitHub 的锚点生成规则。"""
    text = heading.strip().lower()
    text = re.sub(r"[^\w\s\u3400-\u4dbf\u4e00-\u9fff-]", "", text, flags=re.UNICODE)
    text = re.sub(r"\s+", "-", text)
    return text.strip("-")


def collect_anchors(path: Path) -> set[str]:
    anchors: set[str] = set()
    for line in strip_code(path.read_text(encoding="utf-8")).splitlines():
        if line.startswith("#"):
            heading = line.lstrip("#").strip()
            if heading:
                anchors.add(slugify(heading))
    return anchors


def check_links(
    path: Path, root: Path, anchors_cache: dict[Path, set[str]], report: Report
) -> None:
    body = strip_code(path.read_text(encoding="utf-8"))
    for match in LINK_RE.finditer(body):
        target = match.group("target").strip()
        if not target:
            continue
        # 去掉可选的 title 部分: (path "title")
        target = target.split()[0].strip("<>")
        if target.startswith(EXTERNAL_SCHEMES) or target.startswith("//"):
            continue
        if target.startswith("#"):
            anchor = target[1:]
            if anchor and anchor not in collect_anchors(path):
                report.error(path, f"锚点不存在: #{anchor}")
            continue

        raw_path, _, anchor = target.partition("#")
        if not raw_path:
            continue

        resolved = (path.parent / raw_path).resolve()
        if not resolved.exists():
            report.error(path, f"相对链接失效: {target}")
            continue

        if anchor:
            if resolved.is_dir():
                report.error(path, f"目录链接不应带锚点: {target}")
                continue
            if resolved.suffix.lower() == ".md":
                if resolved not in anchors_cache:
                    anchors_cache[resolved] = collect_anchors(resolved)
                if anchor not in anchors_cache[resolved]:
                    report.error(path, f"链接目标缺少锚点: {target}")


# ---------------------------------------------------------------- 主流程


def main() -> int:
    parser = argparse.ArgumentParser(description="GadgetDisk 治理门禁")
    parser.add_argument(
        "--root",
        default=None,
        help="仓库根目录（默认从本脚本位置向上推断）",
    )
    parser.add_argument("-v", "--verbose", action="store_true", help="输出通过项")
    args = parser.parse_args()

    root = Path(args.root).resolve() if args.root else Path(__file__).resolve().parents[3]
    if not (root / "AGENTS.md").is_file():
        print(f"FAIL  在 {root} 未找到 AGENTS.md，根目录推断错误", file=sys.stderr)
        return 1

    report = Report(root, args.verbose)
    md_files = sorted(p for p in root.rglob("*.md") if not SKIP_DIR_NAMES.intersection(p.parts))
    anchors_cache: dict[Path, set[str]] = {}

    for path in md_files:
        report.checked += 1
        if is_note_file(path, root):
            check_note(path, root, report)
        check_links(path, root, anchors_cache, report)

    # 词数预算：根 AGENTS.md 与 .agents/notes 下的 AGENTS.md
    check_budget(root / "AGENTS.md", report)
    for path in md_files:
        if path.name == "AGENTS.md" and path != root / "AGENTS.md":
            check_budget(path, report)

    return report.finish()


if __name__ == "__main__":
    sys.exit(main())

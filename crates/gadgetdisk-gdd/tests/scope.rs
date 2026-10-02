//! `gdd` 的**职责边界**测试：它只做 mass_storage。
//!
//! ## 为什么用源码扫描而不是普通单测
//!
//! 边界本身是「不该存在某类代码」，而不是「某个函数返回什么」。例如
//! 「`gdd` 不写 `idVendor`」无法用调用断言表达——只要没有任何调用就无所谓
//! 断言，而一旦有人加了调用，普通单测也不会失败。
//!
//! 源码扫描是唯一能直接表达这条约束的形式。它把架构决策（见
//! [gdd 拆分 Note](../../../.agents/notes/implemented/architecture/2026-10-04-on-demand-process-model.md)）
//! 变成会失败的测试。
//!
//! ## 扫描的规则
//!
//! 1. **身份属性**：`idVendor`/`idProduct`/`strings/`/`os_desc` 不得出现。
//!    它们归 CLI 的 `gadgetdisk_usb::identity`。
//! 2. **状态文件**：`state.json`/`gadget-backup.json`/`offsets.json` 不得出现。
//!    `gdd` 无状态；「上次导出到哪」由 CLI 拥有。
//! 3. **配置目录**：`gadget.json`/`config/` 不得出现（持久配置归 CLI）。
//!
//! 注释与文档里提到这些词是**允许的**（本模块的说明正需要提到它们），因此
//! 扫描先剥掉注释行再匹配。

use std::path::{Path, PathBuf};

/// 递归收集 `dir` 下的全部 `.rs` 文件。
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(rust_files(&path));
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// 剥掉注释与文档注释，只保留可执行代码。
///
/// 这不是完整的 Rust 词法分析（也不需要）：目的只是让「文档里提到某个词」
/// 不被误判为「代码里用了它」。字符串字面量里的出现**保留**——那才是真正
/// 需要禁止的用法（例如 `fs.write("idVendor", …)`）。
fn strip_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    let mut in_block = 0usize;
    let mut in_line = false;

    while let Some(c) = chars.next() {
        if in_line {
            if c == '\n' {
                in_line = false;
                out.push('\n');
            }
            continue;
        }
        if in_block > 0 {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                in_block -= 1;
            } else if c == '/' && chars.peek() == Some(&'*') {
                chars.next();
                in_block += 1;
            }
            continue;
        }
        if c == '/' {
            match chars.peek() {
                Some('/') => {
                    chars.next();
                    in_line = true;
                    continue;
                }
                Some('*') => {
                    chars.next();
                    in_block += 1;
                    continue;
                }
                _ => {}
            }
        }
        out.push(c);
    }
    out
}

/// 断言 `src/` 下的代码里不出现任何 `forbidden` 片段。
fn assert_absent(forbidden: &[&str], why: &str) {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();

    for file in rust_files(&src) {
        let Ok(raw) = std::fs::read_to_string(&file) else {
            continue;
        };
        let code = strip_comments(&raw);
        for needle in forbidden {
            if code.contains(needle) {
                violations.push(format!(
                    "{needle:?} appears in {}",
                    file.file_name().unwrap_or_default().to_string_lossy()
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "{why}\nviolations:\n  {}",
        violations.join("\n  ")
    );
}

/// `gdd` 不得写任何 gadget 身份属性。
///
/// 身份（`idVendor`/`idProduct`/字符串描述符/`os_desc`）归 CLI：它只在下次 bind
/// 时生效，需要 CLI 在写完属性后请 `gdd` 重绑。若 `gdd` 也写它们，就会出现
/// 「两个进程都改同一组属性」的竞态，且 `gdd` 的权限面无谓地变大。
#[test]
fn gdd_never_touches_gadget_identity() {
    assert_absent(
        &[
            "idVendor",
            "idProduct",
            "id_product",
            "id_vendor",
            "bcdDevice",
            "bDeviceClass",
            "strings/",
            "os_desc",
            "manufacturer",
            "serialnumber",
        ],
        "gdd does mass_storage only; identity attributes belong to the CLI (see gadgetdisk_usb::identity)",
    );
}

/// `gdd` 不得读写任何状态/配置文件。
///
/// 它是**无状态**进程：不记「上次导出到哪」（由 CLI 的 `run/state.json` 承担），
/// 不存身份备份，不缓存分区偏移。这样它可以被随时拉起或杀死，重启后行为一致。
#[test]
fn gdd_is_stateless_and_owns_no_files() {
    assert_absent(
        &[
            "state.json",
            "gadget-backup.json",
            "offsets.json",
            "gadget.json",
            "service.log",
        ],
        "gdd is stateless: the export intent and the identity backup are both owned by the CLI",
    );
}

/// 边界测试自身必须真的在扫描（防「路径写错导致零文件、永远通过」）。
#[test]
fn the_scan_actually_finds_source_files() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = rust_files(&src);
    assert!(
        files.len() >= 5,
        "expected to scan gdd source files, found only {} (is the path wrong?)",
        files.len()
    );
    // 至少应包含 mass_storage 编排所依赖的接线点。
    let names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    for expected in ["service.rs", "usb_adapter.rs", "kernel.rs", "paths.rs"] {
        assert!(names.contains(&expected.to_string()), "missing {expected}");
    }
}

/// 注释剥离器不得把代码也剥掉（否则上面几条断言会变成永远通过）。
#[test]
fn comment_stripping_keeps_code_and_drops_comments() {
    let source = r#"
// idVendor in a comment, must be stripped
/// doc comments too
let x = "idVendor"; // kept inside a string
/* 块注释
   idProduct here */
"#;
    let code = strip_comments(source);
    assert!(!code.contains("//"), "line comments must be stripped");
    assert!(!code.contains("/*"), "block comments must be stripped");
    assert!(
        code.contains(r#""idVendor""#),
        "string literals must be kept, otherwise the scan is meaningless"
    );
    assert!(code.contains("let x"), "the code itself must be kept");
}

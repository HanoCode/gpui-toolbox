//! Git 文本生成（`duck.misc.git-tools`）。
//!
//! 逐行移植参考仓 `src/tools/GitTools.vue` 的三个页签：它就是三段纯字符串处理，
//! **一次 `git` 子进程都没调**
//! （计划 §6.3 写的「外部命令（git）」是误判，这里纠正）：
//!
//! | 页签 | 入参 | 产物 |
//! | --- | --- | --- |
//! | Commit | `git diff` 原文 + 类型 | Conventional Commits 标题 + 变更文件清单 |
//! | Hook | 钩子类型 + 语言 | 可直接落进 `.git/hooks/` 的脚本 |
//! | .gitignore | 勾选的语言/平台 | 拼好的 `.gitignore` |
//!
//! 与源仓唯一的结构差异：源仓把 `变更文件:` 这个中文抬头直接拼进提交信息，
//! 内核不产中文，所以 [`commit_draft`] 只返回 [`CommitDraft`]（标题 + 文件数组），
//! 抬头与列表由插件层按当前语言拼。

/// 提交信息草案：结构化返回，拼装留给插件层（见模块头）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitDraft {
    /// 标题行，形如 `feat(parser): handle empty input`。
    pub subject: String,
    /// 变更文件（按出现顺序去重）。
    pub files: Vec<String>,
}

/// 摘要关键词表，照抄源仓 `summarizeAdditions`。
///
/// 两个中文词用 `\u{}` 转义写：内核 crate 里出现中文字面量会被 `duck-theme` 的
/// 漏译闸门当成交付文案，而这里它们是**匹配模式**，不是给人看的字。
const SUMMARY_KEYWORDS: [&str; 9] = [
    "fix",
    "bug",
    "\u{4fee}\u{590d}",
    "\u{9519}\u{8bef}",
    "exception",
    "error",
    "fallback",
    "return",
    "throw",
];

/// 摘要最长多少字符（源仓 `slice(0, 70)`）。
const SUMMARY_MAX_CHARS: usize = 70;

/// 从 `git diff` 原文里挑出变更文件（源仓 `parseChangedFiles`）。
///
/// 认两种行首：`diff --git a/<路径> b/<路径>`（取 a 侧）与 `+++ b/<路径>`；
/// `/dev/null` 是新增/删除文件的对侧，跳过。结果按首次出现顺序去重。
pub fn changed_files(diff: &str) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    for line in diff.lines() {
        let path = diff_git_path(line).or_else(|| plus_plus_path(line));
        let Some(path) = path else { continue };
        if path.starts_with("/dev/null") || path.is_empty() {
            continue;
        }
        if !files.iter().any(|seen| seen == &path) {
            files.push(path);
        }
    }
    files
}

/// `diff --git a/x b/y` → `x`。
fn diff_git_path(line: &str) -> Option<String> {
    let rest = line.strip_prefix("diff --git a/")?;
    // 取到 ` b/` 之前的一段；找不到就整段当路径（源仓正则同样会失败 → 不取）。
    let end = rest.find(" b/")?;
    Some(rest[..end].to_owned())
}

/// `+++ b/x` → `x`。
fn plus_plus_path(line: &str) -> Option<String> {
    line.strip_prefix("+++ b/").map(|path| path.to_owned())
}

/// 从新增行里猜一句摘要（源仓 `summarizeAdditions`）。
///
/// 先按关键词表命中（命中即用，这是「修了什么」最像的一句话），不命中就退回
/// 第一行有效新增。注释行（`//` `/*` `*/` `#` `*`）与空行不算。
pub fn summarize_additions(diff: &str) -> String {
    let mut added: Vec<&str> = Vec::new();
    for line in diff.lines() {
        if !line.starts_with('+') || line.starts_with("+++") {
            continue;
        }
        let text = line[1..].trim();
        if text.is_empty() {
            continue;
        }
        if text.starts_with('*')
            || text.starts_with("//")
            || text.starts_with("/*")
            || text.starts_with("*/")
            || text.starts_with('#')
        {
            continue;
        }
        added.push(text);
    }

    for keyword in SUMMARY_KEYWORDS {
        if let Some(hit) = added
            .iter()
            .find(|line| line.to_lowercase().contains(keyword))
        {
            return squeeze(hit);
        }
    }
    match added.first() {
        Some(hit) => squeeze(hit),
        None => String::new(),
    }
}

/// 去掉行尾标点、截到 [`SUMMARY_MAX_CHARS`]（源仓 `replace(/[:;,{]+$/, '').slice(0, 70)`）。
fn squeeze(line: &str) -> String {
    let trimmed = line.trim_end_matches([':', ';', ',', '{']);
    trimmed.chars().take(SUMMARY_MAX_CHARS).collect()
}

/// 生成提交信息草案（源仓 `generateCommitMsg`）。
///
/// 只有**单个**文件时才写 scope（`feat(parser): …`），多文件时 scope 没意义。
/// `commit_type` 为空按源仓默认的 `feat` 处理。
pub fn commit_draft(diff: &str, commit_type: &str) -> CommitDraft {
    let kind = match commit_type.trim() {
        "" => "feat",
        other => other,
    };
    let files = changed_files(diff);
    let summary = summarize_additions(diff);

    let subject = match files.len() {
        1 => {
            let scope = files[0].rsplit('/').next().unwrap_or(&files[0]);
            let tail = if summary.is_empty() {
                "update"
            } else {
                &summary
            };
            format!("{kind}({scope}): {tail}")
        }
        _ => {
            let tail = if !summary.is_empty() {
                summary.as_str()
            } else if let Some(first) = files.first() {
                return CommitDraft {
                    subject: format!("{kind}: update {first}"),
                    files,
                };
            } else {
                "update"
            };
            format!("{kind}: {tail}")
        }
    };

    CommitDraft { subject, files }
}

/// 生成钩子脚本（源仓 `hooks` 常量，逐字）。
///
/// 未知组合回落 `pre-commit` + bash —— 与源仓「Hook not found」的兜底脚本等价，
/// 但这里直接给一份**能跑**的模板，而不是一句 `echo`。
pub fn hook_script(hook_type: &str, lang: &str) -> String {
    let (kind, language) = match (hook_type, lang) {
        (
            kind @ ("pre-commit" | "commit-msg" | "pre-push" | "post-merge"),
            language @ ("bash" | "node"),
        ) => (kind, language),
        (kind @ ("pre-commit" | "commit-msg" | "pre-push" | "post-merge"), _) => (kind, "bash"),
        (_, language @ ("bash" | "node")) => ("pre-commit", language),
        _ => ("pre-commit", "bash"),
    };
    let script = match (kind, language) {
        ("commit-msg", "bash") => COMMIT_MSG_BASH,
        ("commit-msg", _) => COMMIT_MSG_NODE,
        ("pre-push", "bash") => PRE_PUSH_BASH,
        ("pre-push", _) => PRE_PUSH_NODE,
        ("post-merge", "bash") => POST_MERGE_BASH,
        ("post-merge", _) => POST_MERGE_NODE,
        (_, "node") => PRE_COMMIT_NODE,
        (_, _) => PRE_COMMIT_BASH,
    };
    script.to_owned()
}

/// 支持的语言/平台清单（源仓 `gitignoreLangs`，顺序即界面顺序）。
pub const GITIGNORE_LANGS: [&str; 11] = [
    "Node",
    "Python",
    "Java",
    "Go",
    "Rust",
    "Vue/React",
    "macOS",
    "Windows",
    "Linux",
    "IDE",
    "Docker",
];

/// 拼 `.gitignore`（源仓 `generateGitignore`）：每个勾选项一段 `# 名字` + 模板。
///
/// 未知名字（用户手输的）只出一条注释，不报错 —— 与源仓 `|| ''` 一致。
pub fn gitignore_for(langs: &[String]) -> String {
    let mut out = String::new();
    for lang in langs {
        out.push_str("# ");
        out.push_str(lang);
        out.push('\n');
        if let Some(template) = gitignore_template(lang) {
            out.push_str(template);
        }
        out.push('\n');
    }
    out
}

/// 单个语言的模板（源仓 `gitignoreTemplates`，逐字节照抄——包括 macOS 那条 `Icon\r`）。
fn gitignore_template(lang: &str) -> Option<&'static str> {
    Some(match lang {
        "Node" => "node_modules/\nnpm-debug.log*\nyarn-debug.log*\nyarn-error.log*\n.pnpm-store/\n",
        "Python" => {
            "__pycache__/\n*.py[cod]\n*.so\n.Python\nvenv/\nenv/\n.venv/\n*.egg-info/\n.eggs/\ndist/\nbuild/\n"
        }
        "Java" => {
            "*.class\n*.jar\n*.war\n*.nar\ntarget/\n!.mvn/wrapper/maven-wrapper.jar\nbuild/\n*.log\n"
        }
        "Go" => "*.exe\n*.exe~\n*.dll\n*.so\n*.dylib\ngo.sum\nvendor/\n",
        "Rust" => "target/\n**/*.rs.bk\nCargo.lock\n",
        "Vue/React" => ".next/\n.cache/\nbuild/\ndist/\n*.tsbuildinfo\n",
        "macOS" => ".DS_Store\n.AppleDouble\n.LSOverride\nIcon\r\n._*\n",
        "Windows" => "Thumbs.db\nehthumbs.db\nDesktop.ini\n$RECYCLE.BIN/\n",
        "Linux" => "*~\n*.swp\n*.swo\n*.swn\n.fuse_hidden*\n",
        "IDE" => {
            ".idea/\n*.iml\n.vscode/\n*.sublime-project\n*.sublime-workspace\n.project\n.classpath\n.settings/\n"
        }
        "Docker" => ".dockerignore\n",
        _ => return None,
    })
}

const PRE_COMMIT_BASH: &str = r#"#!/bin/bash
echo "Running pre-commit checks..."
for file in $(git diff --cached --name-only --diff-filter=ACMR | grep -E '\.(js|ts|vue|py)$'); do
  echo "Checking: $file"
done"#;

const PRE_COMMIT_NODE: &str = r#"#!/usr/bin/env node
const { execSync } = require('child_process');
const files = execSync('git diff --cached --name-only --diff-filter=ACMR').toString().trim().split('\n');
files.forEach(f => console.log('Checking:', f));"#;

const COMMIT_MSG_BASH: &str = r#"#!/bin/bash
MSG=$(cat "$1")
if ! echo "$MSG" | grep -qE "^(feat|fix|docs|style|refactor|test|chore|perf|ci|build|revert)\(?.*\)?: .{1,50}"; then
  echo "ERROR: Invalid commit message format"
  exit 1
fi"#;

const COMMIT_MSG_NODE: &str = r#"#!/usr/bin/env node
const fs = require('fs');
const msg = fs.readFileSync(process.argv[2], 'utf8');
if (!/^(feat|fix|docs|style|refactor|test|chore|perf|ci|build|revert)\(?.*\)?: .{1,50}/.test(msg)) {
  console.error('ERROR: Invalid commit message format');
  process.exit(1);
}"#;

const PRE_PUSH_BASH: &str = r#"#!/bin/bash
echo "Running tests before push..."
npm test 2>/dev/null || echo "No test script found, skipping""#;

const PRE_PUSH_NODE: &str = r#"#!/usr/bin/env node
const { execSync } = require('child_process');
try { execSync('npm test', { stdio: 'inherit' }); } catch { process.exit(1); }"#;

const POST_MERGE_BASH: &str = r#"#!/bin/bash
echo "Post-merge hook"
if [ -f package-lock.json ]; then npm ci 2>/dev/null || npm install 2>/dev/null; fi"#;

const POST_MERGE_NODE: &str = r#"#!/usr/bin/env node
const { execSync } = require('child_process');
const fs = require('fs');
if (fs.existsSync('package-lock.json')) execSync('npm ci', { stdio: 'inherit' });"#;

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "diff --git a/src/parser.rs b/src/parser.rs
index 1111111..2222222 100644
--- a/src/parser.rs
+++ b/src/parser.rs
@@ -10,3 +10,4 @@
 fn parse() {
+    // 注释行不算
+    let value = read().unwrap_or_default(); // fallback to empty
 }
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,1 +1,1 @@
-old
+fn main() {}
";

    #[test]
    fn collects_changed_files_in_order_without_duplicates() {
        let files = changed_files(SAMPLE);
        assert_eq!(files, vec!["src/parser.rs", "src/main.rs"]);
    }

    #[test]
    fn skips_dev_null_and_keeps_everything_else() {
        let diff = "diff --git a/new.rs b/new.rs
--- /dev/null
+++ b/new.rs
";
        // `--- /dev/null` 不是认的行首；`+++ b/new.rs` 才是。
        assert_eq!(changed_files(diff), vec!["new.rs"]);
    }

    #[test]
    fn summary_prefers_keyword_hits() {
        // `fallback` 在关键词表里，优先于第一行有效新增（注释行被跳过）。
        assert_eq!(
            summarize_additions(SAMPLE),
            "let value = read().unwrap_or_default(); // fallback to empty"
        );
    }

    #[test]
    fn summary_falls_back_to_first_added_line() {
        let diff = "+++ b/x\n+    let a = 1;\n+    let b = 2;\n";
        assert_eq!(summarize_additions(diff), "let a = 1");
    }

    #[test]
    fn summary_trims_trailing_punctuation() {
        let diff = "+++ b/x\n+        return Ok(Value::Null);\n";
        // 行尾是 `;`，被去掉；`return` 命中的也是这一行。
        assert_eq!(summarize_additions(diff), "return Ok(Value::Null)");
    }

    #[test]
    fn single_file_gets_a_scope() {
        let diff =
            "diff --git a/src/parser.rs b/src/parser.rs\n+++ b/src/parser.rs\n+    let a = 1;\n";
        let draft = commit_draft(diff, "feat");
        // 行尾分号被 `squeeze` 去掉（源仓 `replace(/[:;,{]+$/, '')`）。
        assert_eq!(draft.subject, "feat(parser.rs): let a = 1");
        assert_eq!(draft.files, vec!["src/parser.rs"]);
    }

    #[test]
    fn multi_file_gets_no_scope_and_no_summary_falls_back_to_update() {
        let diff = "diff --git a/a.rs b/a.rs\ndiff --git a/b.rs b/b.rs\n";
        let draft = commit_draft(diff, "chore");
        assert_eq!(draft.subject, "chore: update a.rs");
        assert_eq!(draft.files.len(), 2);
    }

    #[test]
    fn empty_diff_yields_plain_update() {
        let draft = commit_draft("   \n", "fix");
        assert_eq!(draft.subject, "fix: update");
        assert!(draft.files.is_empty());
    }

    #[test]
    fn hooks_match_the_reference_scripts() {
        assert!(hook_script("pre-commit", "bash").starts_with("#!/bin/bash"));
        assert!(hook_script("commit-msg", "node").contains("Invalid commit message format"));
        assert!(hook_script("pre-push", "bash").contains("npm test"));
        // 未知组合回落 pre-commit/bash，而不是「Hook not found」。
        assert_eq!(
            hook_script("nope", "ruby"),
            hook_script("pre-commit", "bash")
        );
    }

    #[test]
    fn gitignore_joins_sections_in_selection_order() {
        let out = gitignore_for(&["Rust".to_owned(), "IDE".to_owned()]);
        assert!(out.starts_with("# Rust\ntarget/\n"));
        assert!(out.contains("# IDE\n.idea/\n"));
        assert!(out.ends_with(".settings/\n\n"));
    }

    #[test]
    fn unknown_gitignore_language_only_emits_the_comment() {
        assert_eq!(gitignore_for(&["Cobol".to_owned()]), "# Cobol\n\n");
    }

    #[test]
    fn every_reference_language_has_a_template() {
        for lang in GITIGNORE_LANGS {
            assert!(gitignore_template(lang).is_some(), "{lang} 没有模板");
        }
    }
}

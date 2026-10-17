//! 命令建议匹配引擎：从输入行前缀 + 历史与候选表里挑出建议。
//!
//! 这一层**零 GPUI、零终端状态**：输入是「用户已打的一行文本」加几张候选表，
//! 输出是带插入方式的建议列表。纯函数意味着所有边界（空输入、宽字符、
//! 多行拼接的历史）都能被单测钉住，宿主层只需要管渲染与回填。
//!
//! 建议分四档，按优先级排序：
//!
//! 1. **历史整行**（fish 式灰字补全）：历史命令以当前输入为**逐字节**前缀 →
//!    接受 = 把差集**追加**到光标后；
//! 2. **历史词补全**：当前词（最后一个空白分隔 token）能对上历史里某个词，
//!    且历史与输入在词之前完全一致 → 接受 = 删掉已打的半个词、重打；
//! 3. **内置命令**：仅当前词在**第一个 token** 位置时匹配 ——
//!    命令名只出现在行首，`ls` 出现在 `ls` 后面就不是命令了；
//! 4. **路径候选**：调用方现查后传入的候选串（本地目录项 / 远端列表）。
//!
//! 「接受怎么落进终端」由宿主执行：退格删掉 `input[insert_from..]` 的**字符数**，
//! 再把 `text[insert_from..]` 写进 PTY。整条建议的语义是「用 `text` 整体替换
//! `input[insert_from..]`」，因此 pass 1/2 都要求历史串与输入**逐字节一致到
//! 替换点** —— 插错一个字符的代价是用户再也不敢按 Tab。

/// 建议来源。只影响展示（图标 / 弱化样式），不影响插入语义。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuggestSource {
    History,
    Builtin,
    Path,
}

/// 一条建议。
///
/// 语义：**用 `text` 整体替换 `input[insert_from..]`**（字节偏移）。
/// `insert_from == input.len()` 时是纯追加（fish 式差集）；否则从
/// `insert_from` 起删词重打。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suggestion {
    /// 建议的完整命令行（含用户已打的部分）。
    pub text: String,
    /// `input` 的字节偏移：从这里到输入末尾是要被替换的部分。
    pub insert_from: usize,
    pub source: SuggestSource,
}

impl Suggestion {
    /// 接受时需要**退格**的字符数（删掉用户已打的半个词）。
    ///
    /// 按字符而不是字节：退格字节一次删一个字符，中文一个字占三个字节
    /// 也只算一次。0 = 纯追加，不用删。
    #[must_use]
    pub fn backspaces(&self, input: &str) -> usize {
        input
            .get(self.insert_from..)
            .map_or(0, |tail| tail.chars().count())
    }

    /// 接受时要**写入** PTY 的文本（`text` 里用户已打部分的差集）。
    #[must_use]
    pub fn insert_text(&self, input: &str) -> String {
        input
            .get(self.insert_from..)
            .and_then(|_| self.text.get(self.insert_from..))
            .unwrap_or_default()
            .to_owned()
    }
}

/// 默认候选上限。
const MAX_LIMIT: usize = 8;

/// 计算建议。`input` 是输入区文本（**不含提示符**）；`paths` 由调用方现查
/// （本地目录项等）后传入，引擎不碰文件系统；`limit` 钳到 `[1, 8]`。
#[must_use]
pub fn suggest(
    input: &str,
    history: &[String],
    paths: &[String],
    limit: usize,
) -> Vec<Suggestion> {
    let mut out: Vec<Suggestion> = Vec::new();
    if input.trim_end().is_empty() {
        return out;
    }
    let limit = limit.clamp(1, MAX_LIMIT);

    let word_start = last_token_start(input);
    let word = &input[word_start..];

    let mut push = |suggestion: Suggestion| {
        // 去重按 `text`：同一串文本从不同插入点出现（整行档与词档同时命中
        // 同一条历史）是噪音，整行档优先级更高、先到先得。
        if !out.iter().any(|seen| seen.text == suggestion.text) {
            out.push(suggestion);
        }
    };

    // 1. 历史整行前缀。要求**逐字节**前缀（`starts_with(input)`）：
    //    输入结尾的空格天然参与匹配 —— "git " 不会误配 "github"，
    //    因为 "github" 并不以 "git " 开头。
    for entry in history {
        if entry.starts_with(input) && entry.len() > input.len() {
            push(Suggestion {
                text: entry.clone(),
                insert_from: input.len(),
                source: SuggestSource::History,
            });
        }
    }

    // 2. 内置命令：只在第一个 token 的位置。词级替换的语义要求建议串与输入
    //    在替换点之前逐字节一致，所以命令名只在行首才认。
    //（历史整行档天然覆盖「词在输入尾部」的所有情形 —— 词条是输入的尾巴，
    //  `token.starts_with(word)` 蕴含 `entry.starts_with(input)`，单独的历史
    //  词档是死代码，刻意不做。）
    if word_start == 0 && !word.is_empty() {
        for builtin in BUILTIN_COMMANDS {
            if builtin.starts_with(word) && *builtin != word {
                push(Suggestion {
                    text: (*builtin).to_owned(),
                    insert_from: 0,
                    source: SuggestSource::Builtin,
                });
            }
        }
    }

    // 3. 路径候选：调用方给的串直接当「替换当前词」的完整文本。
    if !word.is_empty() {
        for path in paths {
            if path.starts_with(word) && path != word {
                push(Suggestion {
                    text: path.clone(),
                    insert_from: word_start,
                    source: SuggestSource::Path,
                });
            }
        }
    }

    out.truncate(limit);
    out
}

/// 最后一个空白分隔 token 的字节起点。输入以空白结尾时返回 `input.len()`
///（空词，词级补全各档自然跳过）。
fn last_token_start(input: &str) -> usize {
    input
        .char_indices()
        .rev()
        .find(|(_, ch)| ch.is_whitespace())
        .map_or(0, |(at, ch)| at + ch.len_utf8())
}

/// 内置常用命令表（第一 token 位置的候选）。
///
/// 覆盖日常 shell + 本仓用户常见工具链；不求全，求「高频的都在」。
pub const BUILTIN_COMMANDS: &[&str] = &[
    "cd", "ls", "pwd", "cat", "less", "head", "tail", "touch", "mkdir", "rmdir", "rm", "cp",
    "mv", "ln", "find", "grep", "rg", "sed", "awk", "sort", "uniq", "wc", "diff", "chmod",
    "chown", "curl", "wget", "tar", "zip", "unzip", "gzip", "ssh", "scp", "rsync", "sftp",
    "ping", "netstat", "lsof", "dig", "nslookup", "ifconfig", "ip", "ps", "top", "htop",
    "kill", "killall", "df", "du", "free", "uname", "whoami", "which", "whereis", "man",
    "history", "echo", "printf", "export", "env", "date", "sleep", "watch", "clear", "exit",
    "git", "gh", "cargo", "rustc", "rustup", "python", "python3", "pip", "node", "npm",
    "pnpm", "yarn", "make", "cmake", "docker", "docker-compose", "kubectl", "helm", "brew",
    "sqlite3", "mysql",
    "psql", "redis-cli", "open", "pbcopy", "pbpaste", "codesign", "systemctl", "journalctl",
    "apt", "dnf", "yum", "vim", "vi", "nano", "code", "sudo",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(entries: &[&str]) -> Vec<String> {
        entries.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn empty_input_has_no_suggestions() {
        assert!(suggest("", &entries(&["git status"]), &[], 8).is_empty());
        assert!(suggest("   ", &entries(&["git status"]), &[], 8).is_empty());
    }

    #[test]
    fn history_suffix_suggestion_appends_the_diff() {
        let history = entries(&["git status --short", "git stash"]);
        let out = suggest("git st", &history, &[], 8);
        // 两条都以 "git st" 逐字节开头，且按历史顺序（近期优先）。
        assert_eq!(
            out,
            vec![
                Suggestion {
                    text: "git status --short".to_owned(),
                    insert_from: 6,
                    source: SuggestSource::History,
                },
                Suggestion {
                    text: "git stash".to_owned(),
                    insert_from: 6,
                    source: SuggestSource::History,
                },
            ]
        );
        let first = &out[0];
        // "git st" → "git status --short"：无退格，追加 "atus --short"。
        assert_eq!(first.backspaces("git st"), 0);
        assert_eq!(first.insert_text("git st"), "atus --short");
    }

    #[test]
    fn trailing_space_never_mismatches_a_longer_word() {
        // "git " 不能配出 "github x"：那会让追加语义写出 "git hub x"。
        let history = entries(&["github clone"]);
        let out = suggest("git ", &history, &[], 8);
        assert!(out.is_empty());

        let history = entries(&["git status"]);
        let out = suggest("git ", &history, &[], 8);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].insert_text("git "), "status");
    }

    #[test]
    fn word_replacement_deletes_typed_chars_first() {
        // 词级替换（路径档）：接受 = 退格删掉已打的 "Doc"（3 字符），再打 "uments/"。
        let paths = vec!["Documents/".to_owned()];
        let out = suggest("cd Doc", &[], &paths, 8);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].insert_from, 3);
        assert_eq!(out[0].backspaces("cd Doc"), 3);
        assert_eq!(out[0].insert_text("cd Doc"), "uments/");
    }

    #[test]
    fn builtins_only_match_the_first_token() {
        let out = suggest("dock", &[], &[], 8);
        assert_eq!(
            out.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
            vec!["docker", "docker-compose"]
        );
        assert!(out.iter().all(|s| s.source == SuggestSource::Builtin));
        // 第二个 token 位置不再当命令名。
        assert!(suggest("sudo dock", &[], &[], 8).is_empty());
        // 完整打完的命令不再建议它自己。
        assert!(suggest("docker", &[], &[], 8)
            .iter()
            .all(|s| s.text != "docker"));
    }

    #[test]
    fn paths_replace_the_current_word() {
        let paths = vec!["Documents/".to_owned(), "Downloads/".to_owned()];
        let out = suggest("cd D", &[], &paths, 8);
        assert_eq!(
            out.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(),
            vec!["Documents/", "Downloads/"]
        );
        assert!(out.iter().all(|s| s.source == SuggestSource::Path));
    }

    #[test]
    fn cjk_input_matches_by_bytes_and_backspaces_by_chars() {
        let history = entries(&["echo 你好"]);
        let out = suggest("echo 你", &history, &[], 8);
        assert_eq!(out.len(), 1);
        // "你" 占 3 字节：追加差集 = "好"（1 个字符）。
        assert_eq!(out[0].insert_text("echo 你"), "好");
        assert_eq!(out[0].backspaces("echo 你"), 0);
    }

    #[test]
    fn multiline_history_is_a_single_line_candidate() {
        // 多行输入在采集时已折成空格拼接 —— 这里验证匹配语义不受影响。
        let history = entries(&["for f in *; do echo $f; done"]);
        let out = suggest("for f", &history, &[], 8);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].insert_text("for f"), " in *; do echo $f; done");
    }

    #[test]
    fn limit_is_clamped_and_dedup_keeps_the_best_source() {
        let history = entries(&["git status", "git status"]);
        let paths = vec!["git status".to_owned()];
        let out = suggest("git st", &history, &paths, 8);
        // 重复历史 + 路径候选同名：去重后只剩一条（pass 1 的历史档）。
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].source, SuggestSource::History);

        let many = entries(&["a1", "a2", "a3", "a4", "a5", "a6", "a7", "a8", "a9", "a10"]);
        assert_eq!(suggest("a", &many, &[], 8).len(), 8);
        assert_eq!(suggest("a", &many, &[], 0).len(), 1);
    }
}

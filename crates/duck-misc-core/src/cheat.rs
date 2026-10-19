//! 速查手册（`duck.text.cheat-sheets`）：navi 风格 `.cheat` 解析 + 模糊搜索。
//!
//! 逐行移植参考仓 `src/tools/cheat-parser.ts`（188 行）与 `CheatSheets.vue` 的搜索
//! 打分（`fuzzyScore`），数据来自同仓 `cheat-sheets-data.ts` 的 20 张表（由
//! `scripts/gen_cheat_sheets.py` 转成 [`CHEAT_SHEETS`] 目录下的 `.cheat` 文件）。
//!
//! ## 语法（`parse_commands`，完全照搬 navi）
//!
//! ```text
//! % tag1, tag2       标签行（不参与命令块）
//! # 描述             开始一个新命令块，这一行是描述
//! $ var: <命令>      变量的动态候选：跑本机 shell，输出每行一个候选
//! $ var: =a|b|c      变量的静态候选（参考仓扩展语法，等号前缀 + | 分隔）
//! shell 命令 <var>   命令模板；行尾 `\` 是续行符，多行直接拼
//! ```
//!
//! ## 分节指令（本仓扩展，`%%` 开头）
//!
//! 参考仓的数据是「markdown 正文 + 命令模板」两个字段，`.cheat` 本身只有命令区，
//! 没处放那几百行 markdown。所以加了三条指令：
//!
//! ```text
//! %% name Git
//! %% category 版本控制
//! %% content      之后的每一行原样作为 markdown 正文
//! %% commands     之后的每一行按 navi 语法解析
//! ```
//!
//! **没有 `%%` 行的文件整份按命令区解析** —— 这正是参考页「导入 .cheat 目录」的
//! 形态，导入的现成 navi 表不需要改一个字。
//!
//! ## 与参考实现的三处差别
//!
//! 1. **不执行 `$ var: <命令>`**。参考仓在插件进程里 `sh -c` 跑任意用户命令取候选；
//!    进程轨插件拿的是「表单 → 文本」的授权面，跑任意 shell 超出这个面。这里只把
//!    命令**列出来**（变量表里标「需手动执行」），由用户自己跑。
//! 2. **不从命令反推 markdown 正文**（参考仓 `buildContentFromCommands`）。那段会拼
//!    `变量 / 说明 / 候选` 三列表头，中文得进内核；命令区本来就会逐条列出变量，重复
//!    一遍没有信息增量。
//! 3. **slug 取文件名而不是名称音译**：中文表名 slugify 会得到 `正则表达式`，不能当
//!    下拉值。内置表用文件名（`regex`），导入表也用文件名。
//!
//! 内核不出中文：所有文案（分类缺省值、变量提示、报告抬头）都由插件层拼。

use std::collections::BTreeMap;

/// 一个变量：要么有静态候选，要么有一条「自己跑」的命令，可能两者都没有。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variable {
    pub name: String,
    /// `$ var: =a|b|c` 里的候选。
    pub options: Vec<String>,
    /// `$ var: <命令>` 里的命令原文（只展示，不执行 —— 见模块头第 1 条）。
    pub command: Option<String>,
}

/// 一个命令块。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Command {
    pub description: String,
    /// 命令模板，`<name>` 是占位符。
    pub template: String,
    /// 按定义顺序（参考仓是对象插入顺序）。
    pub variables: Vec<Variable>,
}

impl Command {
    /// 模板里出现、但没定义的占位符也要有输入位（参考页只列定义过的变量，
    /// 但那样 `<file>` 这种忘了写 `$` 的模板就永远填不上）。
    pub fn variable_names(&self) -> Vec<String> {
        let mut names = extract_variables(&self.template);
        for variable in &self.variables {
            if !names.contains(&variable.name) {
                names.push(variable.name.clone());
            }
        }
        names
    }

    pub fn variable(&self, name: &str) -> Option<&Variable> {
        self.variables.iter().find(|item| item.name == name)
    }
}

/// 一张速查表。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Sheet {
    pub slug: String,
    pub name: String,
    pub category: String,
    pub tags: Vec<String>,
    /// markdown 正文（`%% content` 之后的部分，原样）。
    pub content: String,
    pub commands: Vec<Command>,
}

impl Sheet {
    /// 左侧列表那一行：`版本控制 / Git`。
    pub fn label(&self, separator: &str) -> String {
        if self.category.is_empty() {
            self.name.clone()
        } else {
            format!("{}{separator}{}", self.category, self.name)
        }
    }
}

/// 内置速查表：`(slug, 源文本)`。
///
/// 顺序就是左侧列表的顺序（参考仓 `cheatSheetCategories` 的分类顺序，同分类内按
/// slug 排）。数据是 `scripts/gen_cheat_sheets.py` 从参考仓生成的，**不要手改**。
pub const CHEAT_SHEETS: &[(&str, &str)] = &[
    ("python", include_str!("cheat_sheets/python.cheat")),
    ("javascript", include_str!("cheat_sheets/javascript.cheat")),
    ("typescript", include_str!("cheat_sheets/typescript.cheat")),
    ("java", include_str!("cheat_sheets/java.cheat")),
    ("go", include_str!("cheat_sheets/go.cheat")),
    ("vue", include_str!("cheat_sheets/vue.cheat")),
    ("react", include_str!("cheat_sheets/react.cheat")),
    ("git", include_str!("cheat_sheets/git.cheat")),
    ("mysql", include_str!("cheat_sheets/mysql.cheat")),
    ("redis", include_str!("cheat_sheets/redis.cheat")),
    ("bash", include_str!("cheat_sheets/bash.cheat")),
    ("ssh", include_str!("cheat_sheets/ssh.cheat")),
    ("regex", include_str!("cheat_sheets/regex.cheat")),
    ("nginx", include_str!("cheat_sheets/nginx.cheat")),
    (
        "http-status",
        include_str!("cheat_sheets/http-status.cheat"),
    ),
    ("json", include_str!("cheat_sheets/json.cheat")),
    ("docker", include_str!("cheat_sheets/docker.cheat")),
    (
        "docker-compose",
        include_str!("cheat_sheets/docker-compose.cheat"),
    ),
    ("kubernetes", include_str!("cheat_sheets/kubernetes.cheat")),
    ("vscode", include_str!("cheat_sheets/vscode.cheat")),
];

/// 全部内置表（每次调用都重新解析：20 张表解析一遍是毫秒级，缓存反而要处理失效）。
pub fn builtin() -> Vec<Sheet> {
    CHEAT_SHEETS
        .iter()
        .map(|(slug, text)| parse(text, slug))
        .collect()
}

/// 分节指令。
enum Section {
    Content,
    Commands,
}

/// 解析一份 `.cheat`。
///
/// `fallback_slug` 非空时直接当 slug（内置表传文件名）；空时由名称 slugify。
pub fn parse(text: &str, fallback_slug: &str) -> Sheet {
    let mut sheet = Sheet {
        slug: fallback_slug.to_owned(),
        ..Sheet::default()
    };
    let mut commands_text = String::new();
    let mut section: Option<Section> = None;
    let has_sections = text.lines().any(|line| line.trim().starts_with("%% "));

    if !has_sections {
        // 纯 navi 文件：整份都是命令区。
        commands_text.push_str(text);
    } else {
        for line in text.lines() {
            if let Some(directive) = line.trim_end().strip_prefix("%% ") {
                let directive = directive.trim();
                if let Some(value) = directive.strip_prefix("name ") {
                    sheet.name = value.trim().to_owned();
                    section = None;
                    continue;
                }
                if let Some(value) = directive.strip_prefix("category ") {
                    sheet.category = value.trim().to_owned();
                    section = None;
                    continue;
                }
                match directive {
                    "content" => section = Some(Section::Content),
                    "commands" => section = Some(Section::Commands),
                    _ => {}
                }
                continue;
            }
            match section {
                Some(Section::Content) => {
                    sheet.content.push_str(line);
                    sheet.content.push('\n');
                }
                Some(Section::Commands) => {
                    commands_text.push_str(line);
                    commands_text.push('\n');
                }
                None => {}
            }
        }
        sheet.content = sheet.content.trim_end().to_owned();
    }

    sheet.commands = parse_commands(&commands_text);
    sheet.tags = tags_of(&commands_text);
    // 参考仓 `parseCheat`：`const category = tags[0] || '导入'`。纯 navi 文件没有
    // `%% category`，分类只能来自第一行 `% 标签`；缺省值（「导入」）由插件层给，
    // 内核不出中文。
    if sheet.category.is_empty() {
        sheet.category = sheet.tags.first().cloned().unwrap_or_default();
    }
    if sheet.name.is_empty() {
        sheet.name = fallback_slug.to_owned();
    }
    if sheet.slug.is_empty() {
        sheet.slug = slugify(&sheet.name);
    }
    sheet
}

/// `% 标签1, 标签2` 行（navi 的标签，参考仓拿第一个当分类）。
fn tags_of(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix('%'))
        .map(|rest| {
            rest.split([',', '，'])
                .map(str::trim)
                .filter(|tag| !tag.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// 把 navi 语法解析成命令块，逐行对齐参考仓 `parseCheatCommands`。
pub fn parse_commands(text: &str) -> Vec<Command> {
    let mut commands: Vec<Command> = Vec::new();
    let mut current: Option<Command> = None;
    let mut description = String::new();
    let mut pending: Vec<Variable> = Vec::new();

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('%') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('#') {
            flush(&mut commands, &mut current, &mut pending);
            description = rest.trim().to_owned();
            continue;
        }
        if let Some((name, value)) = variable_line(line) {
            current.get_or_insert_with(|| Command {
                description: description.clone(),
                ..Command::default()
            });
            pending.push(make_variable(&name, &value));
            continue;
        }
        // 续行符只是「这行还没完」的标记，值本身不换行（照抄参考实现）。
        let template_line = line.strip_suffix('\\').unwrap_or(line);
        let command = current.get_or_insert_with(|| Command {
            description: description.clone(),
            ..Command::default()
        });
        if command.template.is_empty() {
            command.template = template_line.to_owned();
        } else {
            command.template.push('\n');
            command.template.push_str(template_line);
        }
    }
    flush(&mut commands, &mut current, &mut pending);
    commands.retain(|command| !command.template.trim().is_empty());
    commands
}

fn flush(commands: &mut Vec<Command>, current: &mut Option<Command>, pending: &mut Vec<Variable>) {
    if let Some(mut command) = current.take() {
        for variable in pending.drain(..) {
            // 同名变量后者覆盖（参考仓写进对象就是覆盖），位置留在第一次出现处。
            match command
                .variables
                .iter_mut()
                .find(|item| item.name == variable.name)
            {
                Some(slot) => *slot = variable,
                None => command.variables.push(variable),
            }
        }
        commands.push(command);
    }
    pending.clear();
}

/// `$ name: value` → `(name, value)`。
///
/// 参考仓正则 `^\$\s+([^\s:]+)\s*:\s*(.+)$`：`$` 后**必须**有空白，名字里不能有冒号
/// 或空白，值非空。逐条对应。
fn variable_line(line: &str) -> Option<(String, String)> {
    let rest = line.strip_prefix('$')?;
    if rest.len() == line.len() - 1 && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start();
    let colon = rest.find(':')?;
    let name = rest[..colon].trim();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return None;
    }
    let value = rest[colon + 1..].trim();
    if value.is_empty() {
        return None;
    }
    Some((name.to_owned(), value.to_owned()))
}

fn make_variable(name: &str, value: &str) -> Variable {
    let mut variable = Variable {
        name: name.to_owned(),
        options: Vec::new(),
        command: None,
    };
    if let Some(list) = value.strip_prefix('=') {
        variable.options = list
            .split('|')
            .map(str::trim)
            .filter(|option| !option.is_empty())
            .map(str::to_owned)
            .collect();
    } else if let Some(command) = value.strip_prefix('$') {
        variable.command = Some(command.trim().to_owned());
    } else {
        variable.command = Some(value.to_owned());
    }
    variable
}

/// 模板里的 `<name>` 占位符，按出现顺序去重。
pub fn extract_variables(template: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let chars: Vec<char> = template.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != '<' {
            index += 1;
            continue;
        }
        let start = index + 1;
        let mut end = start;
        while end < chars.len() && is_placeholder_char(chars[end]) {
            end += 1;
        }
        if end > start && end < chars.len() && chars[end] == '>' {
            let name: String = chars[start..end].iter().collect();
            if !names.contains(&name) {
                names.push(name);
            }
            index = end + 1;
        } else {
            index += 1;
        }
    }
    names
}

/// 参考仓的字符类是 `[a-zA-Z0-9_-]`：`<stash@{n}>` 这种不算占位符，原样留着。
fn is_placeholder_char(char: char) -> bool {
    char.is_ascii_alphanumeric() || char == '_' || char == '-'
}

/// 还有没填的占位符（参考仓 `hasUnresolvedPlaceholder`）。
pub fn has_placeholder(text: &str) -> bool {
    !extract_variables(text).is_empty()
}

/// 把模板里的占位符换成取值；没给值的留着 `<name>`（参考仓 `renderCommandTemplate`）。
pub fn render_template(template: &str, values: &BTreeMap<String, String>) -> String {
    let mut out = template.to_owned();
    for name in extract_variables(template) {
        let value = values
            .get(&name)
            .cloned()
            .unwrap_or_else(|| format!("<{name}>"));
        out = out.replace(&format!("<{name}>"), &value);
    }
    out
}

/// `branch=main message=修复 登录` / 换行 / 分号分隔 → 取值表。
///
/// 值里可以有空格（`message=修复 登录页`）：切分点在「空白 + `标识符=`」这种位置，
/// 不是任意空白。分号与换行是硬分隔，段落里再做这一层细分。
pub fn parse_assignments(raw: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for piece in split_assignments(raw) {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        let Some((name, value)) = piece.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        values.insert(name.to_owned(), value.trim().to_owned());
    }
    values
}

/// 按 `;` / `；` / 换行粗切，再按「空白 + `ident=`」细分。
fn split_assignments(raw: &str) -> Vec<String> {
    let chars: Vec<char> = raw.chars().collect();
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut index = 0;
    while index < chars.len() {
        let char = chars[index];
        if matches!(char, ';' | '；' | '\n') {
            pieces.push(std::mem::take(&mut current));
            index += 1;
            continue;
        }
        if char.is_whitespace() {
            let mut probe = index;
            while probe < chars.len() && chars[probe].is_whitespace() {
                probe += 1;
            }
            let mut end = probe;
            while end < chars.len()
                && (chars[end].is_alphanumeric() || chars[end] == '_' || chars[end] == '-')
            {
                end += 1;
            }
            // 只有「一段标识符后面紧跟 `=`」才当新变量的开头；否则这个空格属于值。
            if end > probe && end < chars.len() && chars[end] == '=' {
                pieces.push(std::mem::take(&mut current));
                index = probe;
                continue;
            }
        }
        current.push(char);
        index += 1;
    }
    pieces.push(current);
    pieces
}

/// 参考仓 `slugify`：非字母数字（含中文）压成 `-`，去首尾。
pub fn slugify(text: &str) -> String {
    let mut out = String::new();
    for char in text.to_lowercase().chars() {
        if char.is_alphanumeric() || char == '_' {
            out.push(char);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_owned()
}

/// 参考仓 `fuzzyScore`：命中 -1 表示对不上；连续命中 +12，词首 +8，其余 +3。
///
/// 返回 `i32` 而不是 `usize`：`-1` 是「没匹配」的哨兵值，参差的排序逻辑靠它。
pub fn fuzzy_score(query: &str, text: &str) -> i32 {
    if query.is_empty() {
        return 1;
    }
    let query: Vec<char> = query.to_lowercase().chars().collect();
    let haystack: Vec<char> = text.to_lowercase().chars().collect();
    let mut score = 0;
    let mut next = 0;
    let mut previous: i32 = -2;
    for (index, char) in haystack.iter().enumerate() {
        if next >= query.len() {
            break;
        }
        if *char != query[next] {
            continue;
        }
        let index = index as i32;
        if index == previous + 1 {
            score += 12;
        } else if index == 0 || is_separator(haystack[index as usize - 1]) {
            score += 8;
        } else {
            score += 3;
        }
        previous = index;
        next += 1;
    }
    if next < query.len() { -1 } else { score }
}

fn is_separator(char: char) -> bool {
    char.is_whitespace() || matches!(char, '-' | '_' | '/' | '.')
}

/// 一条搜索结果。
#[derive(Debug, Clone, Copy)]
pub struct Hit<'a> {
    pub sheet: &'a Sheet,
    pub score: i32,
}

/// 参考页 `filteredCategories` 的打分：名称权重 1.0、slug 0.9、分类 0.6。
pub fn search<'a>(sheets: &'a [Sheet], query: &str) -> Vec<Hit<'a>> {
    let query = query.trim();
    if query.is_empty() {
        return sheets.iter().map(|sheet| Hit { sheet, score: 1 }).collect();
    }
    let mut hits: Vec<Hit<'a>> = sheets
        .iter()
        .filter_map(|sheet| {
            let score = fuzzy_score(query, &sheet.name)
                .max((fuzzy_score(query, &sheet.slug) as f64 * 0.9) as i32)
                .max((fuzzy_score(query, &sheet.category) as f64 * 0.6) as i32);
            (score > 0).then_some(Hit { sheet, score })
        })
        .collect();
    // 稳定排序：同分的保持原顺序（评分相同的情况下，内置顺序就是想要的顺序）。
    hits.sort_by_key(|hit| std::cmp::Reverse(hit.score));
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
%% name 演示
%% category 测试分类
%% content
# 演示速查

正文第一段。
%% commands
% 演示, tag2
# 创建分支
git checkout -b <branch>
$ branch: =main|dev|release

# 提交
git commit -m \"<message>\"
$ message: echo hello
";

    #[test]
    fn sections_split_content_and_commands() {
        let sheet = parse(SAMPLE, "demo");
        assert_eq!(sheet.slug, "demo");
        assert_eq!(sheet.name, "演示");
        assert_eq!(sheet.category, "测试分类");
        assert_eq!(sheet.content, "# 演示速查\n\n正文第一段。");
        assert_eq!(sheet.commands.len(), 2);
        assert_eq!(sheet.tags, vec!["演示", "tag2"]);
    }

    #[test]
    fn static_options_and_shell_command_are_distinguished() {
        let sheet = parse(SAMPLE, "demo");
        let branch = sheet.commands[0].variable("branch").expect("branch");
        assert_eq!(branch.options, vec!["main", "dev", "release"]);
        assert!(branch.command.is_none());

        // `$ message: echo hello` 是 navi 原生的「跑命令取候选」，我们只存不跑。
        let message = sheet.commands[1].variable("message").expect("message");
        assert!(message.options.is_empty());
        assert_eq!(message.command.as_deref(), Some("echo hello"));
    }

    #[test]
    fn plain_navi_file_is_all_commands() {
        // 导入的现成 .cheat 没有 `%%` 行：整份按命令区解析，slug 用文件名。
        let sheet = parse(
            "% net\n# 看端口\nss -lntp | grep <port>\n$ port: =80|443\n",
            "imported",
        );
        assert_eq!(sheet.slug, "imported");
        assert_eq!(sheet.name, "imported");
        assert!(sheet.content.is_empty());
        assert_eq!(sheet.commands.len(), 1);
        assert_eq!(sheet.tags, vec!["net"]);
        // 分类回落到第一行 `%` 标签（参考仓 `tags[0]`）。
        assert_eq!(sheet.category, "net");
    }

    #[test]
    fn description_moves_to_the_next_block() {
        // `#` 行先 flush 再改描述，所以第二块拿的是第二条描述，不是第一条。
        let commands = parse_commands("# 一\ncmd1\n# 二\ncmd2\n");
        assert_eq!(commands[0].description, "一");
        assert_eq!(commands[1].description, "二");
    }

    #[test]
    fn empty_template_blocks_are_dropped() {
        // 只有描述没有命令行的块不产出（参考仓最后 filter template 非空）。
        let commands = parse_commands("# 空\n# 实\nls\n");
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].description, "实");
    }

    #[test]
    fn variable_names_include_undeclared_placeholders() {
        let commands = parse_commands("# x\nscp <src> <dst>\n$ src: =a|b\n");
        assert_eq!(commands[0].variable_names(), vec!["src", "dst"]);
        assert!(commands[0].variable("dst").is_none());
    }

    #[test]
    fn render_leaves_unknown_placeholders() {
        let mut values = BTreeMap::new();
        values.insert("branch".to_owned(), "dev".to_owned());
        assert_eq!(
            render_template("git switch <branch> && <rest>", &values),
            "git switch dev && <rest>"
        );
        assert!(has_placeholder("git switch <branch>"));
        assert!(!has_placeholder("git switch dev"));
    }

    #[test]
    fn placeholder_syntax_stays_ascii() {
        // `stash@{n}` 不是占位符：`@` `{` `}` 不在参考仓的字符类里。
        assert!(extract_variables("git stash apply <stash@{n}>").is_empty());
        assert_eq!(
            extract_variables("a <one> b <two> <one>"),
            vec!["one", "two"]
        );
    }

    #[test]
    fn assignments_split_on_whitespace_only_before_an_identifier() {
        let values = parse_assignments("branch=main;message=修复 登录页\nfile=a.txt");
        assert_eq!(values["branch"], "main");
        assert_eq!(values["message"], "修复 登录页");
        assert_eq!(values["file"], "a.txt");

        // 空格分隔多组（placeholder 里就是这么写的）……
        let values = parse_assignments("branch=main message=修复 登录页");
        assert_eq!(values["branch"], "main");
        assert_eq!(
            values["message"], "修复 登录页",
            "空格后不是 ident= 就不该切"
        );

        // ……但值里出现 `x=` 形式时仍会切（命令行里 `--opt=value` 极少见于占位符值）。
        let values = parse_assignments("a=1 --pretty=oneline");
        assert_eq!(values["a"], "1");
        assert_eq!(values["--pretty"], "oneline");

        assert!(parse_assignments("   ").is_empty());
        // 没有等号的片段忽略，不 panic。
        assert!(parse_assignments("garbage").is_empty());
    }

    #[test]
    fn fuzzy_score_rewards_word_starts_and_repeats() {
        assert_eq!(fuzzy_score("", "anything"), 1);
        assert_eq!(fuzzy_score("zzz", "git"), -1);
        // 词首（开头或分隔符之后）比词中高 5 分；连续命中比跳字高。
        assert_eq!(fuzzy_score("s", "ssh"), 8);
        assert_eq!(fuzzy_score("s", "-ssh"), 8);
        assert_eq!(fuzzy_score("s", "bash"), 3);
        assert!(fuzzy_score("ssh", "ssh") > fuzzy_score("ssh", "s-s-h"));
        // `dcom` 这种跳字缩写也该命中。
        assert!(fuzzy_score("dcom", "docker-compose") > 0);
    }

    #[test]
    fn search_prefers_name_over_category() {
        let sheets = vec![
            Sheet {
                slug: "git".into(),
                name: "Git".into(),
                category: "版本控制".into(),
                ..Sheet::default()
            },
            Sheet {
                slug: "bash".into(),
                name: "Bash".into(),
                category: "版本控制".into(),
                ..Sheet::default()
            },
        ];
        let hits = search(&sheets, "git");
        assert_eq!(hits.len(), 1, "只有 Git 该命中");
        assert_eq!(hits[0].sheet.slug, "git");

        // 按分类搜也能命中，只是分低。
        let hits = search(&sheets, "版本");
        assert_eq!(hits.len(), 2);
        // 空词返回全部。
        assert_eq!(search(&sheets, "  ").len(), 2);
    }

    #[test]
    fn builtin_sheets_parse_with_expected_shape() {
        let sheets = builtin();
        assert_eq!(sheets.len(), CHEAT_SHEETS.len());
        // 20 张表都有名字与分类，5 张带命令模板 —— 数字来自参考仓数据本身。
        assert!(sheets.iter().all(|sheet| !sheet.name.is_empty()));
        assert!(sheets.iter().all(|sheet| !sheet.category.is_empty()));
        let with_commands = sheets
            .iter()
            .filter(|sheet| !sheet.commands.is_empty())
            .count();
        assert_eq!(with_commands, 5, "参考仓只有 5 张表带 commands");
        // slug 与文件名一致（下拉值要稳定）。
        for (slug, _) in CHEAT_SHEETS {
            assert!(sheets.iter().any(|sheet| sheet.slug == *slug), "{slug}");
        }
    }

    #[test]
    fn git_sheet_keeps_its_command_templates() {
        let sheet = parse(CHEAT_SHEETS[7].1, CHEAT_SHEETS[7].0);
        assert_eq!(sheet.slug, "git");
        assert_eq!(sheet.commands.len(), 4);
        // 静态候选照抄参考仓的三条提交信息。
        let message = sheet.commands[1].variable("message").expect("message");
        assert_eq!(message.options.len(), 3);
        assert!(sheet.content.contains("## 创建存储库"));
    }
}

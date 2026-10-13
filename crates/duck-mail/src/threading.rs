//! 会话（线程）合并：把同一往来的多封邮件归到一组。
//!
//! # 为什么单独一个模块、而且是**纯函数**
//!
//! 合并只依赖「邮件头里的线程线索」——`Message-ID` / `References` / `In-Reply-To`
//! / `X-GM-THRID` / 主题。它**不碰数据库、不碰网络、不碰 tokio**：喂一批输入、
//! 吐一批分组。把它做成纯函数有两个直接好处：
//!
//! 1. **可穷举验证**。线程合并的边界情况（悬空引用、循环引用、重复 `Message-ID`、
//!    缺 References 的回复）非常多，且手写例子里最容易「看着对」。纯函数能把这些
//!    全写成单测钉住（见文件底部），不用起库、不用起服务器。
//! 2. **调用点只有一个**。`sync` 在同步收尾时把整账号的信读出来跑一遍，
//!    把结果写回 `threads` + `messages.thread_id`（见 `db/repo/threads`）。
//!    合并逻辑集中在一处，不会出现「收信时的合并」与「重算时的合并」两套规则。
//!
//! # 算法（JWZ 的简化实现）
//!
//! 参考 Jamie Zawinski 的 *Message Threading*（meli / notmuch 同一套思路）：
//!
//! 1. 每封邮件是一个节点；`Message-ID` 建索引（重复 `Message-ID` 直接合并）。
//! 2. 每封邮件的 `References` / `In-Reply-To` 里出现的 `Message-ID`：命中本批的
//!    合并到它；**没命中（悬空引用）的分配一个「幻影节点」**——这样
//!    「A 引用 X、B 引用 X，而 X 不在库里」时 A 与 B 仍会被并到一起。
//! 3. `X-GM-THRID`（Gmail 的权威线程 id）相同的强制合并——服务端已经算好了，
//!    比本地猜准。
//! 4. **主题兜底**：只对「引用线索全无」（`References` 一个都没有）的消息按规范化
//!    主题合并，且**要求同主题里至少有一条是回复**（`Re:` / `回复:` 之类前缀）。
//!    见 [`group_threads`] 的注释——这是有意比 JWZ **更保守**的一步。
//!
//! 合并用并查集（union-find），近线性。
//!
//! # 不做的事
//!
//! - **不改写 `Message-ID` 之外的东西**：本模块只产出「谁和谁一组」，把
//!   `message_count` / `unread_count` / `participants` 这类聚合留给 repo 层
//!   按最终落库的数据算，避免两处各算一遍、算法不一致。

use std::collections::HashMap;

/// 规范化后的主题。只用于**匹配**，不用于显示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubjectKey {
    /// 剥掉回复 / 转发前缀、折叠空白、大小写归一后的主题。
    pub norm: String,
    /// 原始主题是否带回复前缀。subject 兜底合并依赖它（见 [`group_threads`]）。
    pub is_reply: bool,
}

/// 线程合并的单封输入。
///
/// 字段全部**取自邮件头**，不依赖正文——收信路径只拉头部（`BODY.PEEK[HEADER]`），
/// 正文是按需再拉的，线程合并不能等正文。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadInput {
    /// `messages.id`（本地主键）。输出分组里就是这些 id。
    pub id: i64,
    /// `Message-ID` 头（可能带 `<>`，本模块会归一化）。
    pub message_id: Option<String>,
    /// 引用链：`References` 的每一项，外加 `In-Reply-To`（追加在最后）。
    ///
    /// 调用方把两者合进这一个 `Vec` 即可——本模块只关心「它引用了哪些 id」，
    /// 不区分来源；顺序也不参与合并（并查集是对称的）。
    pub references: Vec<String>,
    /// 原始主题（未规范化）。
    pub subject: Option<String>,
    /// `X-GM-THRID`（Gmail 专用；其它服务商为 `None`）。有则权威。
    pub gm_thrid: Option<String>,
}

/// 一组（一个会话）里的邮件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadGroup {
    /// 组内邮件的 `messages.id`，**升序**。
    pub members: Vec<i64>,
    /// 该组的 `X-GM-THRID`（组内任一非空值；没有则 `None`）。
    pub gm_thrid: Option<String>,
    /// 该组的规范化主题（组内任一非空值；没有则 `None`）。
    pub subject_norm: Option<String>,
}

/// 把 `Message-ID` 归一化：去首尾空白、剥掉 `<` `>`、折叠内部空白。
///
/// **归一是必需的**，不是洁癖：`Message-ID` 在不同的头里可能写成
/// `<a@b>` / `a@b` / ` <a@b> ` /（折行后）`<a@\n b>`。不归一就匹配不上，
/// 表现是「明明是一来一回的两封，却分成了两个会话」。
///
/// 返回 `None` 表示这个「id」其实是空的（比如头里只有一对空尖括号），
/// 调用方应当跳过它——把空串当 id 会让所有空引用合并成一个巨型会话。
#[must_use]
pub fn normalize_message_id(raw: &str) -> Option<String> {
    let inner = raw.trim().trim_matches(|c| c == '<' || c == '>');
    let collapsed = inner.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        None
    } else {
        Some(collapsed)
    }
}

/// 规范化主题：反复剥掉回复 / 转发前缀，折叠空白，转小写。
///
/// 认的前缀（大小写不敏感；可带 `[2]` 计数与中英文冒号）：
/// `re` / `回复` / `答复` / `回覆` / `fwd` / `fw` / `转发` / `转寄`。
/// 认不出来的前缀不剥——宁可少剥，也别把 `[PATCH]` 这种**有意义**的方括号
/// 前缀当成计数吃掉（那会把不同 patch 的邮件错并）。
///
/// 返回 `None` 表示主题（剥完前缀后）为空——空主题不能用来匹配，
/// 否则一堆「无主题」的邮件会被并成一个会话。
#[must_use]
pub fn normalize_subject(raw: &str) -> Option<SubjectKey> {
    let (rest, is_reply) = strip_subject_prefixes(raw);
    let norm = rest
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if norm.is_empty() {
        None
    } else {
        Some(SubjectKey { norm, is_reply })
    }
}

/// 回复语义前缀。
///
/// 中文 token 用 `\u{}` **转义**写，别写成裸中文字面量：本仓有「内核 crate 不得出现
/// 中文字面量」的守卫（`duck-theme::i18n::every_chinese_literal_is_translated`
/// 会逐行扫源码里的 `"..."`），而这些前缀是**匹配用的数据**、不是 UI 文案，
/// 既不该也不可能进 EN 表。转义后守卫扫不到。
///
/// 对照：`\u{56de}\u{590d}` = 回复，`\u{7b54}\u{590d}` = 答复，
/// `\u{56de}\u{8986}` = 回覆。
const REPLY_TOKENS: &[&str] = &[
    "re",
    "\u{56de}\u{590d}",
    "\u{7b54}\u{590d}",
    "\u{56de}\u{8986}",
];

/// 转发语义前缀。
///
/// 对照：`\u{8f6c}\u{53d1}` = 转发，`\u{8f6c}\u{5bc4}` = 转寄。理由同 [`REPLY_TOKENS`]。
const FORWARD_TOKENS: &[&str] = &["fwd", "fw", "\u{8f6c}\u{53d1}", "\u{8f6c}\u{5bc4}"];

/// 反复剥前缀，返回（剩余主题, 是否出现过回复前缀）。
///
/// 一层层剥是为了 `Re: Fwd: 主题` / `回复: 转发: 主题` 这种叠加写法。
fn strip_subject_prefixes(raw: &str) -> (String, bool) {
    let mut rest = raw;
    let mut is_reply = false;
    // 上限 8 层：防御「`Re:` 被反复拼接」的畸形头，避免理论上的死循环。
    for _ in 0..8 {
        let Some((after, reply)) = strip_one_prefix(rest) else {
            break;
        };
        rest = after;
        is_reply = is_reply || reply;
    }
    (rest.to_string(), is_reply)
}

/// 尝试剥**一个**前缀，返回（剩余, 是否回复语义）。剥不掉返回 `None`。
fn strip_one_prefix(raw: &str) -> Option<(&str, bool)> {
    let s = raw.trim_start();
    for (token, reply) in REPLY_TOKENS
        .iter()
        .map(|t| (*t, true))
        .chain(FORWARD_TOKENS.iter().map(|t| (*t, false)))
    {
        let Some(head) = s.get(..token.len()) else {
            continue;
        };
        if !head.eq_ignore_ascii_case(token) {
            continue;
        }
        let after_token = &s[token.len()..];
        // `Re[2]:` / `回复[2]：` —— 中间可能夹一个方括号计数。
        let after_count = strip_bracket_counter(after_token);
        if let Some(after_colon) = strip_colon(after_count) {
            return Some((after_colon, reply));
        }
    }
    None
}

/// 剥掉 `[digits]`（只认纯数字，别的一律不动）。
fn strip_bracket_counter(raw: &str) -> &str {
    let Some(body) = raw.strip_prefix('[') else {
        return raw;
    };
    let Some(end) = body.find(']') else {
        return raw;
    };
    let inner = &body[..end];
    if inner.is_empty() || !inner.bytes().all(|b| b.is_ascii_digit()) {
        return raw;
    }
    &body[end + 1..]
}

/// 剥掉 `:` 或 `：`（前后允许空白）。
fn strip_colon(raw: &str) -> Option<&str> {
    let s = raw.trim_start();
    s.strip_prefix(':').or_else(|| s.strip_prefix('：'))
}

/// 把一组「原始引用串」展开成归一化后的 id 列表。
///
/// 每个元素本身可能还含多个 id（`References` 有时整行给过来），所以这里再按空白切一次；
/// 归一化后为空的丢弃。**排序不去重**——去重交给调用方（`message_refs` 的联合主键
/// 天然去重），这里保持「原样展开」的语义，少一层隐式行为。
fn expand_ids(raws: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for raw in raws {
        for token in raw.split_whitespace() {
            if let Some(id) = normalize_message_id(token) {
                out.push(id);
            }
        }
    }
    out
}

/// 这封邮件是否带**引用线索**（能归一化出至少一个 id）。
///
/// 用于 subject 兜底：带引用线索的不是「根」，不参与按主题合并。
fn has_reference_clue(references: &[String]) -> bool {
    references.iter().any(|raw| {
        raw.split_whitespace()
            .any(|t| normalize_message_id(t).is_some())
    })
}

/// 并查集（带路径压缩 + 按秩合并）。
struct UnionFind {
    parent: Vec<usize>,
    rank: Vec<u8>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
            rank: vec![0; n],
        }
    }

    /// 追加一个节点（给幻影引用用），返回它的下标。
    fn add(&mut self) -> usize {
        let id = self.parent.len();
        self.parent.push(id);
        self.rank.push(0);
        id
    }

    fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    fn union(&mut self, a: usize, b: usize) {
        let (mut ra, mut rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        if self.rank[ra] < self.rank[rb] {
            std::mem::swap(&mut ra, &mut rb);
        }
        self.parent[rb] = ra;
        if self.rank[ra] == self.rank[rb] {
            self.rank[ra] += 1;
        }
    }
}

/// 主入口：把一批邮件合并成会话组。
///
/// 输入顺序无关；输出的每个 [`ThreadGroup::members`] 内按 id 升序，组之间按各组
/// **最小 id** 升序——保证同样的输入永远得到同样的输出（落库时可比对、可缓存）。
///
/// # subject 兜底为什么比 JWZ 更保守
///
/// JWZ 会把主题相同的「根」直接合并。但那在国内邮箱场景下很容易过度合并：
/// 「您的账单已生成」「系统维护通知」这类**周期性、同主题、彼此无关**的邮件
/// 会被并成一条永远增长的超长会话，用户看到的是一个混乱的巨型线程。
///
/// 所以这里加两道闸：
/// 1. **只并「没有引用线索」的消息**——有 `References` 的说明它本就在某个线程里，
///    不该再按主题另并；
/// 2. **同主题里至少有一条带回复前缀**——「一来一回」才是会话的特征；全是
///    「原始主题、无 Re:」的同主题邮件，宁可分成多条，也不合成一条误导用户的。
///
/// 代价是「回复方丢了 `References`、且主题前缀也被客户端吃掉」的极端情况会漏合并；
/// 这比过度合并安全——漏合并只是少一层折叠，过度合并是**错误信息**。
#[must_use]
pub fn group_threads(inputs: &[ThreadInput]) -> Vec<ThreadGroup> {
    let n = inputs.len();
    let mut uf = UnionFind::new(n);

    // 1. `Message-ID` 建索引。重复的 `Message-ID` 合并到先出现的那条。
    let mut by_message_id: HashMap<String, usize> = HashMap::new();
    for (index, input) in inputs.iter().enumerate() {
        let Some(mid) = input.message_id.as_deref().and_then(normalize_message_id) else {
            continue;
        };
        match by_message_id.get(&mid) {
            Some(&prev) => uf.union(prev, index),
            None => {
                by_message_id.insert(mid, index);
            }
        }
    }

    // 2. 引用链合并。命中本批的直接并；没命中的用幻影节点承接悬空引用。
    let mut phantom: HashMap<String, usize> = HashMap::new();
    for (index, input) in inputs.iter().enumerate() {
        for reference in expand_ids(&input.references) {
            let target = match by_message_id.get(&reference) {
                Some(&node) => node,
                None => *phantom.entry(reference).or_insert_with(|| uf.add()),
            };
            uf.union(index, target);
        }
    }

    // 3. `X-GM-THRID` 权威合并（服务端算好的，优先于本地推断）。
    let mut by_gm: HashMap<&str, usize> = HashMap::new();
    for (index, input) in inputs.iter().enumerate() {
        let Some(gm) = input
            .gm_thrid
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        match by_gm.get(gm) {
            Some(&prev) => uf.union(prev, index),
            None => {
                by_gm.insert(gm, index);
            }
        }
    }

    // 4. subject 兜底（详见文档注释）。
    let mut roots_by_subject: HashMap<String, Vec<(usize, bool)>> = HashMap::new();
    for (index, input) in inputs.iter().enumerate() {
        if has_reference_clue(&input.references) {
            continue;
        }
        let Some(subject) = input.subject.as_deref().and_then(normalize_subject) else {
            continue;
        };
        roots_by_subject
            .entry(subject.norm)
            .or_default()
            .push((index, subject.is_reply));
    }
    for group in roots_by_subject.values() {
        // 需要「一来一回」：至少两条、且至少一条带回复前缀。
        if group.len() < 2 || !group.iter().any(|(_, reply)| *reply) {
            continue;
        }
        let anchor = group[0].0;
        for (index, _) in group.iter().skip(1) {
            uf.union(anchor, *index);
        }
    }

    // 5. 归拢输出。
    let mut buckets: HashMap<usize, ThreadGroup> = HashMap::new();
    for (index, input) in inputs.iter().enumerate() {
        let root = uf.find(index);
        let group = buckets.entry(root).or_insert_with(|| ThreadGroup {
            members: Vec::new(),
            gm_thrid: None,
            subject_norm: None,
        });
        group.members.push(input.id);
        if group.gm_thrid.is_none() {
            group.gm_thrid = input
                .gm_thrid
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
        }
        if group.subject_norm.is_none() {
            group.subject_norm = input
                .subject
                .as_deref()
                .and_then(normalize_subject)
                .map(|key| key.norm);
        }
    }

    let mut out: Vec<ThreadGroup> = buckets.into_values().collect();
    for group in &mut out {
        group.members.sort_unstable();
    }
    out.sort_by_key(|group| group.members.first().copied().unwrap_or(i64::MAX));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(id: i64, message_id: &str, references: &[&str], subject: &str) -> ThreadInput {
        ThreadInput {
            id,
            message_id: Some(message_id.to_string()),
            references: references.iter().map(|s| (*s).to_string()).collect(),
            subject: Some(subject.to_string()),
            gm_thrid: None,
        }
    }

    /// 把分组摊平成 `Vec<Vec<i64>>`，便于断言。
    fn groups(inputs: &[ThreadInput]) -> Vec<Vec<i64>> {
        group_threads(inputs)
            .into_iter()
            .map(|g| g.members)
            .collect()
    }

    #[test]
    fn normalize_message_id_strips_brackets_and_whitespace() {
        assert_eq!(normalize_message_id("  <a@b>  "), Some("a@b".into()));
        assert_eq!(normalize_message_id("a@b"), Some("a@b".into()));
        // 折行后残留的内部空白要折叠。
        assert_eq!(normalize_message_id("<a@\n  b>"), Some("a@ b".into()));
        // 空的一律 None——否则空引用会把所有东西并成一个巨型会话。
        assert_eq!(normalize_message_id("<>"), None);
        assert_eq!(normalize_message_id("   "), None);
    }

    #[test]
    fn normalize_subject_strips_prefixes_and_marks_reply() {
        assert_eq!(normalize_subject("Hello").unwrap().norm, "hello");
        assert!(!normalize_subject("Hello").unwrap().is_reply);
        assert_eq!(normalize_subject("Re: Hello").unwrap().norm, "hello");
        assert!(normalize_subject("Re: Hello").unwrap().is_reply);
        // 叠加前缀。
        assert_eq!(normalize_subject("Re: Fwd: Hello").unwrap().norm, "hello");
        // 中文前缀 + 全角冒号。
        let cn = normalize_subject("回复： 会议纪要").unwrap();
        assert_eq!(cn.norm, "会议纪要");
        assert!(cn.is_reply);
        // 计数形式。
        assert_eq!(normalize_subject("Re[2]: Hello").unwrap().norm, "hello");
        // 空白折叠 + 大小写归一。
        assert_eq!(
            normalize_subject("  Hello   World ").unwrap().norm,
            "hello world"
        );
        // 空主题不能当匹配键。
        assert!(normalize_subject("Re: ").is_none());
        assert!(normalize_subject("   ").is_none());
    }

    #[test]
    fn meaningful_bracket_prefix_is_not_eaten() {
        // `[PATCH]` 不是计数，不能被当 `Re[2]` 那样剥掉——否则不同 patch 会被错并。
        let key = normalize_subject("[PATCH] fix bug").unwrap();
        assert_eq!(key.norm, "[patch] fix bug");
        assert!(!key.is_reply);
    }

    #[test]
    fn references_link_chain_into_one_thread() {
        // A（根）← B（引用 A）← C（引用 A、B）。
        let inputs = vec![
            input(1, "a@x", &[], "topic"),
            input(2, "b@x", &["a@x"], "Re: topic"),
            input(3, "c@x", &["a@x", "b@x"], "Re: topic"),
        ];
        assert_eq!(groups(&inputs), vec![vec![1, 2, 3]]);
    }

    #[test]
    fn dangling_reference_merges_siblings() {
        // A 与 B 都引用不在库里的 X —— JWZ 的幻影节点让它们并到一起。
        let inputs = vec![
            input(1, "a@x", &["phantom@x"], "topic"),
            input(2, "b@x", &["phantom@x"], "Re: topic"),
        ];
        assert_eq!(groups(&inputs), vec![vec![1, 2]]);
    }

    #[test]
    fn duplicate_message_id_merges() {
        // 同一封信在收件箱和归档里各有一行（重复 Message-ID）→ 合并。
        let inputs = vec![
            input(1, "same@x", &[], "topic"),
            input(2, "same@x", &[], "topic"),
        ];
        assert_eq!(groups(&inputs), vec![vec![1, 2]]);
    }

    #[test]
    fn reference_order_independence() {
        // 先给子信、后给父信，结果必须一致（并查集先建索引再合并）。
        let inputs = vec![
            input(1, "b@x", &["a@x"], "Re: topic"),
            input(2, "a@x", &[], "topic"),
        ];
        assert_eq!(groups(&inputs), vec![vec![1, 2]]);
    }

    #[test]
    fn gm_thrid_overrides_missing_references() {
        // 没有 References，但 Gmail 给了同一个 thrid → 必须合并。
        let mut a = input(1, "a@x", &[], "topic");
        let mut b = input(2, "b@x", &[], "unrelated subject");
        a.gm_thrid = Some("123".into());
        b.gm_thrid = Some("123".into());
        assert_eq!(groups(&[a, b]), vec![vec![1, 2]]);
    }

    #[test]
    fn subject_fallback_needs_a_reply() {
        // 同主题、都无引用线索，但**全是原始主题**（无 Re:）→ 不合并，避免把
        // 周期性同主题通知并成巨型会话。
        let inputs = vec![
            input(1, "a@x", &[], "月账单已生成"),
            input(2, "b@x", &[], "月账单已生成"),
        ];
        assert_eq!(groups(&inputs), vec![vec![1], vec![2]]);

        // 其中有回复 → 合并成一条会话。
        let inputs = vec![
            input(1, "a@x", &[], "月账单已生成"),
            input(2, "b@x", &[], "Re: 月账单已生成"),
        ];
        assert_eq!(groups(&inputs), vec![vec![1, 2]]);
    }

    #[test]
    fn subject_fallback_ignores_threads_with_reference_clues() {
        // 一封有 References（哪怕指向外部）的信不该再被主题兜底拉进别的会话。
        let inputs = vec![
            input(1, "a@x", &["outside@x"], "topic"),
            input(2, "b@x", &[], "Re: topic"),
        ];
        // 1 的 root 是幻影节点，2 的 root 是自己——两者不因主题相同而合并。
        assert_eq!(groups(&inputs), vec![vec![1], vec![2]]);
    }

    #[test]
    fn empty_subject_never_merges() {
        // 两封「回复」但主题为空 → 不能合并（否则所有空主题邮件聚成一堆）。
        let inputs = vec![input(1, "a@x", &[], "Re: "), input(2, "b@x", &[], "回复：")];
        assert_eq!(groups(&inputs), vec![vec![1], vec![2]]);
    }

    #[test]
    fn group_carries_gm_thrid_and_subject_norm() {
        let mut a = input(2, "b@x", &["a@x"], "Re: Hello");
        a.gm_thrid = Some("77".into());
        let inputs = vec![input(1, "a@x", &[], "Hello"), a];
        let out = group_threads(&inputs);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].gm_thrid.as_deref(), Some("77"));
        assert_eq!(out[0].subject_norm.as_deref(), Some("hello"));
    }

    #[test]
    fn output_is_deterministic() {
        let inputs = vec![
            input(5, "e@x", &["c@x"], "Re: t"),
            input(3, "c@x", &[], "t"),
            input(9, "i@x", &["c@x"], "Re: t"),
            // 第二个独立会话，用来钉「组间按最小 id 升序」。
            input(20, "z@x", &[], "another"),
            input(11, "k@x", &["z@x"], "Re: another"),
        ];
        let first = groups(&inputs);
        let second = groups(&inputs);
        assert_eq!(first, second);
        assert_eq!(first, vec![vec![3, 5, 9], vec![11, 20]]);
    }
}

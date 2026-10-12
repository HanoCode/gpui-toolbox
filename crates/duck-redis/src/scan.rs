//! SCAN 键空间遍历：游标分页、命名空间分组与探测模式。
//!
//! **绝不用 `KEYS`** 作为键树数据源（设计文档 §4.5）：`SCAN` 是增量式的，
//! 百万键也只在每次调用里返回一小批，不会阻塞服务端主线程。
//!
//! 本模块把「协议解析」与「树的构造」都做成**纯函数**，网络调用只剩薄薄一层
//! 命令拼装，因此树的形状可以用普通 `#[test]` 钉死。

use redis::Value;

use crate::error::RedisError;
use crate::value::as_text;

/// `SCAN` 的缺省 `COUNT`。
///
/// `COUNT` 是**提示**不是上限（服务端可能一次返回更多）。1000 在本机与远端
/// 都是「一次往返够用、UI 不至于因为等一批而卡顿」的平衡点。
pub const SCAN_COUNT_DEFAULT: usize = 1000;

/// `SCAN` 的 `COUNT` 上限（防止 UI 传入离谱值把服务端拖住）。
pub const SCAN_COUNT_MAX: usize = 10_000;

/// 命名空间树的层数上限。
///
/// 键名里可能出现很长的 `:` 链（如 `a:b:c:d:e:f:g`），全展开会退化成一条竖线、
/// 比平铺还难读。超过这个层数就不再往下切，余下的整段作为一个节点名。
pub const MAX_TREE_DEPTH: usize = 8;

/// 一次 `SCAN` 往返的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanPage {
    /// 下一次调用要带上的游标；`0` 表示遍历结束。
    pub cursor: u64,
    /// 本批键名（服务端可能未去重——同一键在两批里都出现是允许的）。
    pub keys: Vec<String>,
}

impl ScanPage {
    /// 遍历是否结束（游标归零）。
    #[must_use]
    pub const fn is_finished(&self) -> bool {
        self.cursor == 0
    }
}

/// 解析 `SCAN` / `HSCAN` / `SSCAN` / `ZSCAN` 的应答。
///
/// 应答恒为 `[cursor, [items...]]` 两元组。元素本身是二进制安全的键名，
/// 这里按**有损 UTF-8** 转成 `String`——键名是文本的场景占绝对多数，
/// 含坏字节的键名会在树里显示替换字符，但仍能按键名发命令（服务端按字节比对）。
pub fn parse_scan_reply(reply: &Value) -> Result<ScanPage, RedisError> {
    let parts = match reply {
        Value::Array(parts) | Value::Set(parts) if parts.len() >= 2 => parts,
        _ => {
            return Err(RedisError::Command(
                "SCAN reply is not a [cursor, items] pair".into(),
            ));
        }
    };
    let cursor = as_text(&parts[0])
        .ok_or_else(|| RedisError::Command("SCAN cursor is not a string".into()))?
        .trim()
        .parse::<u64>()
        .map_err(|_| RedisError::Command("SCAN cursor is not a number".into()))?;

    let keys = crate::value::as_array(&parts[1])
        .iter()
        .filter_map(as_text)
        .collect();

    Ok(ScanPage { cursor, keys })
}

/// 把一个键名按分隔符切成命名空间层级。
///
/// 空段被丢弃（`a::b` 与 `a:b` 同层），返回的每一段都非空；
/// 整键没有分隔符时返回单元素切片。
#[must_use]
pub fn split_namespace<'k>(key: &'k str, separator: &str) -> Vec<&'k str> {
    if separator.is_empty() {
        return vec![key];
    }
    let parts: Vec<&str> = key
        .split(separator)
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() { vec![key] } else { parts }
}

/// 命名空间树的一个节点。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeNode {
    /// 本层显示名（不含父前缀）。
    pub label: String,
    /// 本前缀下的**完整键名**；非空表示这是个可打开的叶子前缀。
    pub keys: Vec<String>,
    /// 子节点（按 `label` 升序）。
    pub children: Vec<TreeNode>,
}

impl TreeNode {
    /// 本节点下的键总数（含所有子节点）。
    #[must_use]
    pub fn total_keys(&self) -> usize {
        self.keys.len() + self.children.iter().map(Self::total_keys).sum::<usize>()
    }

    /// 是否为纯叶子（有键、无子节点）。
    #[must_use]
    pub fn is_leaf(&self) -> bool {
        !self.keys.is_empty() && self.children.is_empty()
    }

    /// 是否为纯中间节点（无键、有子节点）。
    #[must_use]
    pub fn is_branch(&self) -> bool {
        self.keys.is_empty() && !self.children.is_empty()
    }
}

/// 把一批键名构造成命名空间树（层数用内核缺省 [`MAX_TREE_DEPTH`]）。
///
/// 规则见 [`build_tree_with_depth`]。
#[must_use]
pub fn build_tree<'k, I>(keys: I, separator: &str) -> Vec<TreeNode>
where
    I: IntoIterator<Item = &'k str>,
{
    build_tree_with_depth(keys, separator, MAX_TREE_DEPTH)
}

/// 把一批键名构造成命名空间树，层数上限可指定（连接档案的「键树最大深度」）。
///
/// 规则：
/// - 每层按 `label` 升序（同名合并）；
/// - 层级超过 `max_depth` 时不再切分，余下整段作为一个节点名；
/// - 同一前缀既可能是节点又有实体键（`user` 与 `user:1` 同时存在），
///   此时该节点既有 `keys` 又有 `children`——两种呈现都保留；
/// - `max_depth` 为 0 时收敛到 1（至少切出一层，不能因为用户填 0 就把树退化成空）。
#[must_use]
pub fn build_tree_with_depth<'k, I>(keys: I, separator: &str, max_depth: usize) -> Vec<TreeNode>
where
    I: IntoIterator<Item = &'k str>,
{
    let max_depth = max_depth.max(1);
    let mut root = TreeNode::default();
    for key in keys {
        let parts = split_namespace(key, separator);
        if parts.is_empty() {
            continue;
        }
        insert_parts(&mut root, key, &parts, 0, max_depth);
    }
    sort_tree(&mut root);
    root.children
}

/// 递归插入已切好的层级。
fn insert_parts(node: &mut TreeNode, key: &str, parts: &[&str], depth: usize, max_depth: usize) {
    // 只剩一段，或已到层数上限：整段（可能含分隔符）挂成节点名，键落在该节点。
    let stop = parts.len() <= 1 || depth + 1 >= max_depth;
    let label = if stop {
        parts.join(":")
    } else {
        parts[0].to_owned()
    };

    let index = match node.children.iter().position(|child| child.label == label) {
        Some(index) => index,
        None => {
            node.children.push(TreeNode {
                label,
                keys: Vec::new(),
                children: Vec::new(),
            });
            node.children.len() - 1
        }
    };

    if stop {
        node.children[index].keys.push(key.to_owned());
    } else {
        insert_parts(
            &mut node.children[index],
            key,
            &parts[1..],
            depth + 1,
            max_depth,
        );
    }
}

/// 递归排序（先子后己不必要，但排序要在插入全部完成后做一次）。
fn sort_tree(node: &mut TreeNode) {
    for child in &mut node.children {
        sort_tree(child);
    }
    node.children.sort_by(|a, b| a.label.cmp(&b.label));
    node.children.iter_mut().for_each(|child| child.keys.sort());
}

/// 下钻某个前缀时的 `MATCH` 模式（`prefix:*`）。
///
/// 前缀为空时返回 `*`（等价于不过滤）。
#[must_use]
pub fn prefix_pattern(prefix: &str, separator: &str) -> String {
    let trimmed = prefix.trim();
    if trimmed.is_empty() {
        return "*".to_owned();
    }
    if separator.is_empty() {
        return format!("{trimmed}*");
    }
    format!("{trimmed}{separator}*")
}

/// 一个节点是否该**默认展开**（连接档案的「自动展开阈值」）。
///
/// 阈值 `0` 表示「一律不自动展开」（用户只想看顶层，自己点开感兴趣的）；
/// 否则子树键数不超过阈值时展开——小命名空间铺开无妨，大命名空间铺开会把
/// 侧栏/列表撑成几千行，滚动条失去意义。
#[must_use]
pub fn should_auto_expand(node: &TreeNode, threshold: usize) -> bool {
    threshold > 0 && node.total_keys() <= threshold
}

/// 收窄 `SCAN` 的 `COUNT`：0 或过大都收敛到安全区间。
#[must_use]
pub fn clamp_count(count: usize) -> usize {
    if count == 0 {
        SCAN_COUNT_DEFAULT
    } else {
        count.min(SCAN_COUNT_MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SCAN 应答解析：正常两元组、游标归零、异常形态。
    #[test]
    fn scan_reply_parsing() {
        let reply = Value::Array(vec![
            Value::BulkString(b"17".to_vec()),
            Value::Array(vec![
                Value::BulkString(b"user:1".to_vec()),
                Value::BulkString(b"user:2".to_vec()),
            ]),
        ]);
        let page = parse_scan_reply(&reply).expect("正常应答");
        assert_eq!(page.cursor, 17);
        assert_eq!(page.keys, vec!["user:1".to_owned(), "user:2".to_owned()]);
        assert!(!page.is_finished());

        // 遍历结束：游标 "0"
        let done = Value::Array(vec![Value::BulkString(b"0".to_vec()), Value::Array(vec![])]);
        let page = parse_scan_reply(&done).expect("结束应答");
        assert!(page.is_finished());
        assert!(page.keys.is_empty());

        // 异常：不是二元组 / 游标不是数字
        assert!(parse_scan_reply(&Value::BulkString(b"nope".to_vec())).is_err());
        assert!(
            parse_scan_reply(&Value::Array(vec![Value::BulkString(b"x".to_vec())])).is_err(),
            "缺第二元应报错"
        );
        assert!(
            parse_scan_reply(&Value::Array(vec![
                Value::BulkString(b"abc".to_vec()),
                Value::Array(vec![]),
            ]))
            .is_err(),
            "游标非数字应报错"
        );
    }

    /// 命名空间切分：正常、连续分隔符、无分隔符、空分隔符。
    #[test]
    fn namespace_splitting() {
        assert_eq!(
            split_namespace("user:1:name", ":"),
            vec!["user", "1", "name"]
        );
        assert_eq!(split_namespace("a::b", ":"), vec!["a", "b"], "空段被丢弃");
        assert_eq!(split_namespace("plain", ":"), vec!["plain"]);
        assert_eq!(split_namespace("a:b", ""), vec!["a:b"], "空分隔符不切");
        assert_eq!(split_namespace(":lead", ":"), vec!["lead"]);
        assert_eq!(split_namespace("trail:", ":"), vec!["trail"]);
    }

    /// 构树：同名合并、按名排序、键挂到对应层。
    #[test]
    fn tree_building_merges_and_sorts() {
        let keys = ["user:2", "user:1", "session:a", "plain"];
        let tree = build_tree(keys, ":");

        // 顶层按 label 升序：plain / session / user
        let labels: Vec<&str> = tree.iter().map(|n| n.label.as_str()).collect();
        assert_eq!(labels, vec!["plain", "session", "user"]);

        let plain = &tree[0];
        assert!(plain.is_leaf(), "无分隔符的键是纯叶子");
        assert_eq!(plain.keys, vec!["plain".to_owned()]);
        assert_eq!(plain.total_keys(), 1);

        let user = &tree[2];
        assert!(user.is_branch(), "user 只做中间节点");
        assert!(user.keys.is_empty());
        assert_eq!(user.children.len(), 2);
        assert_eq!(user.children[0].label, "1");
        assert_eq!(user.children[0].keys, vec!["user:1".to_owned()]);
        assert_eq!(user.total_keys(), 2);
    }

    /// 同一前缀既是实体键又有子键时，两种呈现都保留。
    #[test]
    fn node_can_hold_both_key_and_children() {
        let tree = build_tree(["user", "user:1"], ":");
        assert_eq!(tree.len(), 1);
        let node = &tree[0];
        assert_eq!(node.label, "user");
        assert_eq!(node.keys, vec!["user".to_owned()], "实体键保留在节点上");
        assert_eq!(node.children.len(), 1, "子层照常挂上");
        assert!(!node.is_leaf(), "有子节点就不是纯叶子");
        assert!(!node.is_branch(), "有键就不是纯中间节点");
        assert_eq!(node.total_keys(), 2);
    }

    /// 深层级不无限展开：超过 MAX_TREE_DEPTH 后余下整段作为一个节点名。
    #[test]
    fn deep_paths_stop_at_depth_limit() {
        let deep = "a:b:c:d:e:f:g:h:i:j";
        let tree = build_tree([deep], ":");
        // 从根往下走，层数不超过上限。
        let mut depth = 0;
        let mut node = &tree[0];
        loop {
            depth += 1;
            if node.children.is_empty() {
                break;
            }
            node = &node.children[0];
        }
        assert!(depth <= MAX_TREE_DEPTH, "实际深度 {depth} 应不超过上限");
        // 该键最终必须出现在某个节点上（不能因为限深而丢键）。
        assert_eq!(tree[0].total_keys(), 1, "限深不得丢键");
    }

    /// 下钻模式与 COUNT 收敛。
    #[test]
    fn patterns_and_count_clamping() {
        assert_eq!(prefix_pattern("user", ":"), "user:*");
        assert_eq!(prefix_pattern("", ":"), "*");
        assert_eq!(prefix_pattern("  ", ":"), "*");
        assert_eq!(prefix_pattern("user", ""), "user*");

        assert_eq!(clamp_count(0), SCAN_COUNT_DEFAULT, "0 收敛到缺省");
        assert_eq!(clamp_count(500), 500);
        assert_eq!(clamp_count(usize::MAX), SCAN_COUNT_MAX, "过大收敛到上限");
    }

    /// 空输入产出空树，不 panic。
    #[test]
    fn empty_input_yields_empty_tree() {
        let tree = build_tree(Vec::<&str>::new(), ":");
        assert!(tree.is_empty());
    }

    /// 自定义深度：深度 1 只切一层（键名整段挂在顶层节点上），深度 0 收敛到 1。
    #[test]
    fn custom_depth_limits_branching() {
        let keys = ["user:1:name", "user:2:name"];
        // 深度 1：不再往下切，`user:1:name` 整体成节点名。
        let shallow = build_tree_with_depth(keys, ":", 1);
        assert_eq!(shallow.len(), 2, "深度 1 时每条键各占一个顶层节点");
        let labels: Vec<&str> = shallow.iter().map(|node| node.label.as_str()).collect();
        assert_eq!(labels, vec!["user:1:name", "user:2:name"]);
        assert_eq!(shallow[0].total_keys(), 1);

        // 深度 0 → 收敛到 1（不是空树，也不 panic）。
        let zero = build_tree_with_depth(keys, ":", 0);
        assert_eq!(zero.len(), 2, "深度 0 收敛为 1");

        // 深度 2：切到 `user` 一层，子层拿到剩余整段。
        let deeper = build_tree_with_depth(keys, ":", 2);
        assert_eq!(deeper.len(), 1);
        assert_eq!(deeper[0].label, "user");
        assert_eq!(deeper[0].children.len(), 2);
        assert_eq!(deeper[0].children[0].label, "1:name");
    }

    /// 自动展开判定：阈值为 0 一律不展开；小节点展开、大节点收起。
    #[test]
    fn auto_expand_threshold() {
        let tree = build_tree(["user:1", "user:2", "order:9", "plain"], ":");
        let user = tree
            .iter()
            .find(|node| node.label == "user")
            .expect("有 user 节点");
        assert_eq!(user.total_keys(), 2);
        assert!(!should_auto_expand(user, 0), "阈值 0 表示一律不自动展开");
        assert!(should_auto_expand(user, 2), "键数等于阈值应展开");
        assert!(should_auto_expand(user, 5));
        assert!(!should_auto_expand(user, 1), "超过阈值不展开");
    }
}

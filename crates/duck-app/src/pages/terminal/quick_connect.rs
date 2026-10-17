//! 快速连接选择器：标签条上那个 `+` 与 `⌘N` 的落点。
//!
//! 「连一台机器」原先有两条路，都很贵：
//! - 左栏分组里找（要先把连接存成档案）；
//! - `⌘N` 打开 14 个字段的新建表单（只为临时看一台机器，得先给它起名、选分组、挑认证）。
//!
//! 这一层把「找一个已经有的」与「临时连一台新的」合成**一次输入**：
//!
//! ```text
//! [ 搜索连接，或输入 user@host:port 直接连 ]
//!   直接连接    alice@web-01:2222          ← 输入像地址时的第一行
//!   最近        prod-web-01                prod-web-01 · 生产环境
//!   生产环境    staging-db
//!   新建        本地终端 / 新建连接…
//! ```
//!
//! 回车永远等于「列表第一项」：输入像地址就是直连，否则就是最匹配的那条连接。
//! 这样「打字 → 回车」两个动作覆盖 90% 的使用，剩下的靠鼠标点。
//!
//! # 为什么正文是一个独立视图
//!
//! 弹窗正文的闭包是 `Fn` 且只跑一次（见 `dialog.rs` 与 `ConnectionForm` 的说明），
//! 「边打边过滤」必须有自己的 `notify` 落点。所以过滤、选中、回车全在这个视图里，
//! 弹窗只负责镶个框。

use super::*;

use duck_components::ControlExt as _;
use gpui_component::dialog::DialogButtonProps;
use gpui_component::scroll::ScrollableElement as _;

/// 用户在列表里选中的东西。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Choice {
    /// 直连一行地址（原文，未解析）。
    Direct(String),
    /// 打开一条已存档案 / 临时连接。
    Profile(String),
    /// 新建本地终端标签。
    LocalShell,
    /// 打开完整的新建连接表单。
    NewConnection,
}

/// 列表里的一行。
struct Row {
    choice: Choice,
    title: String,
    /// 右侧的次要信息（地址 / 分类名）。
    hint: String,
    /// 分组标题；与上一行相同则不重复渲染。
    section: SharedString,
}

/// 候选连接（打开弹窗时拍下的快照）。
///
/// 快照而非实时读页面：弹窗活得很短（选一条就关），而「弹窗开着的时候档案变了」
/// 只会让行数在手指底下跳动。真删了的话点下去也只会得到一句「找不到连接」。
pub(super) struct Entry {
    id: String,
    label: String,
    address: String,
    /// 分组显示名（内置三档已按当次语言翻译，自定义分组原样）。
    group: String,
}

pub(super) struct QuickConnect {
    input: Entity<InputState>,
    entries: Vec<Entry>,
    /// 最近用过的 id（已按档案存在性过滤、已截到 [`data::RECENT_SHOWN`]）。
    recents: Vec<String>,
    submit: Rc<dyn Fn(Choice, &mut Window, &mut App)>,
    _input_sub: Subscription,
}

impl QuickConnect {
    pub(super) fn new(
        entries: Vec<Entry>,
        recents: Vec<String>,
        submit: Rc<dyn Fn(Choice, &mut Window, &mut App)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(tr(cx, "搜索连接，或输入 user@host:port 直接连"))
        });
        let input_sub = cx.subscribe(&input, |_this: &mut Self, _, event: &InputEvent, cx| {
            match event {
                // 每次改动都要重画列表（这是「边打边过滤」的唯一驱动）。
                InputEvent::Change => cx.notify(),
                InputEvent::PressEnter { .. } => {
                    let entity = cx.entity();
                    crate::update::on_notify_window(cx, move |window, cx| {
                        entity.update(cx, |this, cx| this.commit_first(window, cx));
                    });
                }
                _ => {}
            }
        });
        Self {
            input,
            entries,
            recents,
            submit,
            _input_sub: input_sub,
        }
    }

    /// 弹窗打开时把焦点送进输入框。
    pub(super) fn focus(&self, window: &mut Window, cx: &mut App) {
        self.input.update(cx, |state, cx| state.focus(window, cx));
    }

    /// 把列表算出来。
    ///
    /// 渲染与回车**共用这一份**：两处各算一遍的话，「看见的第一行」与「回车打开的那条」
    /// 迟早会不一致 —— 而那种不一致用户只会理解成「回车打开了别的东西」。
    fn plan(&self, cx: &App) -> Vec<Row> {
        let query = self.input.read(cx).value().trim().to_owned();
        let needle = query.to_lowercase();
        let mut rows: Vec<Row> = Vec::new();

        // 1. 输入像地址 → 第一行就是「直连」。这一行永远排在最前：
        //    用户敲下 `root@10.0.0.9` 之后想要的就是连上去，不是「搜索这个字符串」。
        if let Some(target) = data::parse_direct_target(&query) {
            let address = if target.port == 22 {
                format!("{}@{}", target.user, target.host)
            } else {
                format!("{}@{}:{}", target.user, target.host, target.port)
            };
            rows.push(Row {
                choice: Choice::Direct(query.clone()),
                title: trf(cx, "连接到 {}", &[&address]),
                hint: tr(cx, "直接连接（不保存）").to_owned(),
                section: tr(cx, "直接连接").into(),
            });
        }

        // 2. 最近用过的（只在没输入时出现：一旦开始打字，用户要的是搜索不是历史）。
        if needle.is_empty() {
            for id in &self.recents {
                let Some(entry) = self.entries.iter().find(|entry| &entry.id == id) else {
                    continue;
                };
                rows.push(Row {
                    choice: Choice::Profile(entry.id.clone()),
                    title: entry.label.clone(),
                    hint: entry.address.clone(),
                    section: tr(cx, "最近").into(),
                });
            }
        }

        // 3. 全部档案（按分组）。搜索时空分组自然消失 —— `rows` 里没有它的行，
        //    渲染时的「换组才画标题」也就不会留下一个空壳标题。
        for entry in &self.entries {
            // 已经在「最近」里出现过的，不再在分组里重复一遍：同一个 id 两行，
            // 点哪一行结果都一样，只会让人怀疑「这是两台机器吗」。
            if needle.is_empty() && self.recents.contains(&entry.id) {
                continue;
            }
            if !entry_matches(entry, &needle) {
                continue;
            }
            rows.push(Row {
                choice: Choice::Profile(entry.id.clone()),
                title: entry.label.clone(),
                hint: entry.address.clone(),
                section: entry.group.clone().into(),
            });
        }

        // 4. 兜底动作。搜索时也留着：正是「搜不到」的时候最需要「那就新建一个」。
        rows.push(Row {
            choice: Choice::LocalShell,
            title: tr(cx, "本地终端").to_owned(),
            hint: tr(cx, "不连远端").to_owned(),
            section: tr(cx, "新建").into(),
        });
        rows.push(Row {
            choice: Choice::NewConnection,
            title: tr(cx, "新建连接…").to_owned(),
            hint: tr(cx, "完整表单（私钥 / 口令 / 跳板机）").to_owned(),
            section: tr(cx, "新建").into(),
        });
        rows
    }

    /// 回车：走列表第一行。
    fn commit_first(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(first) = self.plan(cx).into_iter().next() else {
            return;
        };
        (self.submit)(first.choice, window, cx);
        window.close_dialog(cx);
    }
}

impl Render for QuickConnect {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();
        let rows = self.plan(cx);

        let mut list: Vec<AnyElement> = Vec::with_capacity(rows.len());
        let mut last_section: Option<SharedString> = None;
        for row in rows {
            if last_section.as_ref() != Some(&row.section) {
                last_section = Some(row.section.clone());
                list.push(
                    div()
                        .w_full()
                        .px(tokens.spacing.xs)
                        .pt(tokens.spacing.sm)
                        .pb(tokens.spacing.xxs)
                        .child(meta_text(row.section.clone(), cx))
                        .into_any_element(),
                );
            }
            let id = match &row.choice {
                Choice::Direct(raw) => format!("quick-direct-{raw}"),
                Choice::Profile(id) => format!("quick-profile-{id}"),
                Choice::LocalShell => "quick-local".to_owned(),
                Choice::NewConnection => "quick-new".to_owned(),
            };
            let submit = Rc::clone(&self.submit);
            let choice = row.choice.clone();
            let aria = format!("{} {}", row.title, row.hint);
            list.push(
                div()
                    .id(SharedString::from(id))
                    .flex()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .w_full()
                    .px(tokens.spacing.xs)
                    .py(px(5.))
                    .rounded(tokens.radius.sm)
                    .cursor_pointer()
                    .role(Role::Button)
                    .aria_label(SharedString::from(aria))
                    .hover(move |style| style.bg(extended.colors.hover_tint))
                    .active(move |style| style.bg(extended.colors.hover_tint_press))
                    .on_click(move |_, window, cx| {
                        submit(choice.clone(), window, cx);
                        window.close_dialog(cx);
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.sm.size)
                            .line_height(tokens.typography.sm.line_height)
                            .text_color(tokens.colors.foreground)
                            .child(row.title),
                    )
                    .child(meta_text(row.hint, cx))
                    .into_any_element(),
            );
        }

        v_flex()
            .w_full()
            .gap(tokens.spacing.sm)
            .child(Input::new(&self.input).control(cx).w_full())
            .child(
                v_flex()
                    .w_full()
                    .max_h(px(QUICK_LIST_MAX_H))
                    .overflow_y_scrollbar()
                    .children(list),
            )
    }
}

/// 列表最高高度：再高就把弹窗顶到屏幕外，而候选一多起来滚动比拉高窗口合适。
const QUICK_LIST_MAX_H: f32 = 360.;

/// 列出候选连接（档案在前，按左栏同样的分组顺序）。
///
/// 分组顺序沿用 [`data::GROUP_DEFS`] 的骨架 + 自定义分组，与左栏一致：
/// 同一个概念在两个地方排序不同，会让人以为选择器里少了东西。
pub(super) fn collect_entries(page: &TerminalPage, cx: &App) -> Vec<Entry> {
    let lang = active_theme(cx).lang();
    let group_label = |group: &str| -> String {
        data::GROUP_DEFS
            .iter()
            .find(|(id, _, _)| *id == group)
            .map_or_else(
                || group.to_owned(),
                |(_, label, _)| t(lang, label).to_owned(),
            )
    };
    let mut ordered: Vec<&StoredProfile> = Vec::new();
    for (id, _, _) in data::GROUP_DEFS {
        ordered.extend(page.profiles.iter().filter(|profile| profile.group == id));
    }
    for name in data::custom_group_names(&page.profiles) {
        ordered.extend(page.profiles.iter().filter(|profile| profile.group == name));
    }
    ordered
        .into_iter()
        .map(|profile| Entry {
            id: profile.id.clone(),
            label: profile.label.clone(),
            address: address_of(profile),
            group: group_label(&profile.group),
        })
        .collect()
}

/// 一条候选是否命中搜索词。
///
/// 匹配面三个：档案名、地址、分组名。分组名也匹配是刻意的 ——
/// 搜「生产」时用户想要的通常不是一台叫「生产」的机器，而是那一整组。
fn entry_matches(entry: &Entry, needle: &str) -> bool {
    needle.is_empty()
        || entry.label.to_lowercase().contains(needle)
        || entry.address.to_lowercase().contains(needle)
        || entry.group.to_lowercase().contains(needle)
}

/// `user@host:port`（22 省略）。
fn address_of(profile: &StoredProfile) -> String {
    let port = profile.port.trim();
    if port.is_empty() || port == "22" {
        format!("{}@{}", profile.user, profile.host)
    } else {
        format!("{}@{}:{}", profile.user, profile.host, port)
    }
}

impl TerminalPage {
    /// 打开快速连接选择器（`+` 按钮 / `⌘N` / 标签条双击空白）。
    pub(crate) fn open_quick_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entries = collect_entries(self, cx);
        // 最近：先按「档案还在不在」过滤（删掉的 id 留在 kv 里是很常见的），
        // 再截前几条 —— 顺序不能反：先截再过滤会让「最近 3 条里有 2 条已删」
        // 变成「只剩 1 条」。
        let recents: Vec<String> = data::recent_ids(cx)
            .into_iter()
            .filter(|id| entries.iter().any(|entry| &entry.id == id))
            .take(data::RECENT_SHOWN)
            .collect();
        let page = self.view.clone();
        let submit: Rc<dyn Fn(Choice, &mut Window, &mut App)> =
            Rc::new(move |choice, window, cx| {
                let _ = page.update(cx, |this, cx| match choice {
                    Choice::Direct(raw) => this.open_direct(&raw, window, cx),
                    Choice::Profile(id) => this.open_connection(&id, window, cx),
                    Choice::LocalShell => this.new_local_tab(cx),
                    Choice::NewConnection => this.open_new_connection(window, cx),
                });
            });
        let quick = cx.new(|cx| QuickConnect::new(entries, recents, submit, window, cx));
        window.open_dialog(cx, {
            let quick = quick.clone();
            move |dialog, _, cx| {
                let for_ok = quick.clone();
                let for_content = quick.clone();
                dialog
                    .w(px(QUICK_CONNECT_W))
                    .title(SharedString::from(tr(cx, "连接")))
                    .close_button(true)
                    .button_props(
                        DialogButtonProps::default()
                            .ok_text(tr(cx, "连接"))
                            .cancel_text(tr(cx, "取消"))
                            .show_cancel(true),
                    )
                    .content(move |content, _, _cx| content.child(for_content.clone()))
                    // 主按钮与回车走同一条路（「列表第一项」）：两者不一致时，
                    // 用户按回车得到的和点按钮得到的不是同一台机器，最难查。
                    .on_ok(move |_, window, cx| {
                        let _ = for_ok.update(cx, |this, cx| this.commit_first(window, cx));
                        true
                    })
            }
        });
        quick.update(cx, |this, cx| this.focus(window, cx));
    }
}

/// 选择器宽度：够放下一行 `user@host:port` 与右侧地址，又不至于宽得像设置窗。
const QUICK_CONNECT_W: f32 = 560.;

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(label: &str, address: &str, group: &str) -> Entry {
        Entry {
            id: label.to_owned(),
            label: label.to_owned(),
            address: address.to_owned(),
            group: group.to_owned(),
        }
    }

    /// 三个匹配面都要生效：名字、地址、分组名。
    /// 分组名能搜是刻意的 —— 搜「生产」的人要的是那一整组，不是一台叫「生产」的机器。
    #[test]
    fn search_covers_name_address_and_group() {
        let web = entry("prod-web-01", "alice@10.0.1.13", "生产环境");
        assert!(entry_matches(&web, ""), "空搜索词匹配一切");
        assert!(entry_matches(&web, "web"), "名字");
        assert!(entry_matches(&web, "10.0.1"), "地址");
        assert!(entry_matches(&web, "生产"), "分组");
        assert!(!entry_matches(&web, "staging"));
        // 大小写不敏感（needle 已在 `plan` 里小写化，entry 侧要跟着小写）。
        assert!(entry_matches(&web, "prod-web"));
        assert!(
            !entry_matches(&web, "PROD"),
            "entry 侧只小写化，不做别的归一"
        );
    }

    /// 地址的写法：22 端口省略，其余带上。
    #[test]
    fn address_omits_the_default_port() {
        let mut profile = StoredProfile {
            id: "x".into(),
            label: "x".into(),
            host: "h".into(),
            port: "22".into(),
            user: "u".into(),
            auth: "agent".into(),
            key_file: String::new(),
            group: "prod".into(),
            remember: false,
            password: None,
            forwards: Vec::new(),
            agent_forwarding: false,
            proxy_jump: String::new(),
        };
        assert_eq!(address_of(&profile), "u@h");
        profile.port = "2222".into();
        assert_eq!(address_of(&profile), "u@h:2222");
    }
}

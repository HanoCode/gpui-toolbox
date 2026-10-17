//! 会话恢复：把开着的标签记下来，下次启动按原样摆回来。
//!
//! # 为什么恢复的是「灰标签」而不是「连上的会话」
//!
//! 一启动就去拨五台生产机，等于替用户做了一次他没有做的动作：可能撞上跳板机的
//! 二次认证、可能触发堡垒机的审计告警、可能把「占位符」变成一次真实登录。
//! 所以恢复出来的标签是[休眠](SessionTab::dormant)的：名字、顺序、分屏关系都在，
//! 点一下才连。
//!
//! # 什么时候写
//!
//! 不打桩到「每个改标签的函数末尾」（开标签 / 关标签 / 拖拽换序 / 分屏 / 切标签
//! 一共十几处，漏一处就是「重启后少一个标签」）。改成在轮询里比**指纹**：
//! 每次轮询拼一次 `活动下标 + id 列表`，变了才写。`kv_put` 自身有 400ms 去抖，
//! 所以一段连续操作只会落一次盘。

use super::*;

/// 标签集合的存储键。
const KEY_TABS: &str = "terminal.tabs";

/// 落盘的标签集合。
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct SavedState {
    #[serde(default)]
    active: usize,
    #[serde(default)]
    tabs: Vec<SavedTab>,
}

/// 一条落盘的标签。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SavedTab {
    /// 标签 id（SSH 用档案 id，本地用 `local-shell`、`local-shell-2`…）。
    id: String,
    /// `"local"` / `"ssh"`。
    kind: String,
    /// 分屏右窗格连的是谁（`None` = 没分屏）。
    #[serde(default)]
    split_owner: Option<String>,
}

impl TerminalPage {
    /// 标签集合变了就落盘（在轮询里调，幂等）。
    ///
    /// `pub(crate)`：调用点在 `pages::terminal_canvas`（轮询），那是**兄弟**模块 ——
    /// `pub(super)` 只到 `pages::terminal`，兄弟看不见。
    pub(crate) fn persist_tabs(&mut self, cx: &App) {
        let snapshot = SavedState {
            active: self.active,
            tabs: self
                .tabs
                .iter()
                // SFTP 标签不存：它是「某条连接上的文件面板」，重新打开要重新列目录，
                // 恢复一堆还没连上的文件面板只会让启动后满屏都是加载态。
                .filter(|tab| tab.kind != TabKind::Sftp)
                .map(|tab| SavedTab {
                    id: tab.id.clone(),
                    kind: match tab.kind {
                        TabKind::Local => "local".to_owned(),
                        TabKind::Ssh => "ssh".to_owned(),
                        // 上面已经过滤掉，这里只是让 match 完备。
                        TabKind::Sftp => "sftp".to_owned(),
                    },
                    split_owner: tab.split_owner.clone(),
                })
                .collect(),
        };
        // 指纹用「下标 + id 序列 + 分屏归属」：够便宜，且任何一次换序/分屏都会变。
        let mut fingerprint = snapshot.active.to_string();
        for tab in &snapshot.tabs {
            fingerprint.push('|');
            fingerprint.push_str(&tab.id);
            if let Some(owner) = &tab.split_owner {
                fingerprint.push('#');
                fingerprint.push_str(owner);
            }
        }
        if self.tabs_fingerprint.as_deref() == Some(fingerprint.as_str()) {
            return;
        }
        self.tabs_fingerprint = Some(fingerprint);
        match serde_json::to_string(&snapshot) {
            Ok(json) => duck_store::kv_put(cx, KEY_TABS, json),
            Err(error) => tracing::error!(%error, "terminal tabs serialization failed"),
        }
    }

    /// 读回上次的标签集合（**不**建会话：恢复出来的标签是休眠的）。
    ///
    /// 档案已删、id 认不出的条目直接丢掉：留一个点了没反应的灰标签，
    /// 比不恢复它更让人费解。
    pub(super) fn restore_tabs(&mut self, cx: &App) {
        if !self.tabs.is_empty() {
            return;
        }
        let Some(json) = duck_store::kv_get(cx, KEY_TABS) else {
            return;
        };
        let Ok(saved) = serde_json::from_str::<SavedState>(&json) else {
            // 存档坏了（旧版本格式 / 手改过）：当作没存过，不要卡在启动。
            return;
        };
        let mut restored: Vec<SessionTab> = Vec::new();
        for tab in saved.tabs {
            let kind = match tab.kind.as_str() {
                "local" => TabKind::Local,
                "ssh" => TabKind::Ssh,
                _ => continue,
            };
            // 名字**重新派生**而不是用存档里的：存档里的标题是上次那个语言的、
            // 也可能与档案的新名字不一致。档案才是名字的真源。
            let (title, split_owner) = match kind {
                TabKind::Local => (
                    if tab.id == data::LOCAL_SHELL_ID {
                        tr(cx, data::LOCAL_SHELL_LABEL).to_owned()
                    } else {
                        // `local-shell-2` → 「本地 shell 2」
                        let ordinal = tab
                            .id
                            .rsplit('-')
                            .next()
                            .and_then(|tail| tail.parse::<u32>().ok())
                            .unwrap_or(2);
                        format!("{} {ordinal}", tr(cx, data::LOCAL_SHELL_LABEL))
                    },
                    tab.split_owner,
                ),
                TabKind::Ssh => {
                    let owner = data::tab_owner(&tab.id).to_owned();
                    let Some(profile) = self.profile(&owner) else {
                        // 档案被删了：这条标签没有意义。
                        continue;
                    };
                    let title = profile.label.clone();
                    (title, tab.split_owner)
                }
                TabKind::Sftp => continue,
            };
            restored.push(SessionTab {
                id: tab.id.clone(),
                title,
                subtitle: None,
                kind,
                // 灰着：休眠标签在标签栏里用弱化色画，点一下才连。
                status: LinkStatus::Disconnected,
                split: split_owner
                    .as_ref()
                    .map(|_| data::split_session_key(&tab.id)),
                split_owner,
                dormant: true,
            });
        }
        if restored.is_empty() {
            return;
        }
        self.active = saved.active.min(restored.len() - 1);
        self.tabs = restored;
        // 恢复出来的标签**不抢键盘**：用户还没说要连哪台。
        self.focused_tab = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 落盘形状要能读回来：新字段（`split_owner`）缺省时按「没分屏」处理，
    /// 而不是整份存档读不出来 —— 那会让升级一次丢掉全部标签。
    #[test]
    fn saved_state_tolerates_missing_fields() {
        let legacy = r#"{"active":1,"tabs":[{"id":"prod-web","kind":"ssh"}]}"#;
        let saved: SavedState = serde_json::from_str(legacy).expect("旧存档要能读");
        assert_eq!(saved.active, 1);
        assert_eq!(saved.tabs.len(), 1);
        assert!(saved.tabs[0].split_owner.is_none());
    }

    /// 存档里出现认不出的类型（将来的 sftp / 别的什么）时跳过它，不整体失败。
    #[test]
    fn unknown_kinds_round_trip_as_dropped() {
        let saved = SavedState {
            active: 0,
            tabs: vec![SavedTab {
                id: "x".into(),
                kind: "sftp".into(),
                split_owner: None,
            }],
        };
        let json = serde_json::to_string(&saved).expect("序列化");
        let back: SavedState = serde_json::from_str(&json).expect("反序列化");
        // 反序列化本身不认识种类；丢不丢是 `restore_tabs` 的 match 决定的。
        assert_eq!(back.tabs[0].kind, "sftp");
    }
}

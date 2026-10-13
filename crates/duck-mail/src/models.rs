//! 邮箱模块的数据模型。
//!
//! 字段与 `db/migrations/001_init.sql` 的表列**逐字对应**——`db/repo/*` 直接按这些
//! 名字读写，所以改模型时必须同步改 schema（或加迁移）。
//!
//! **口令 / 令牌不在这些结构里**：`Account` 是脱敏档案，凭据走
//! `duck-platform::secret`（见 [`crate::accounts`]）。

use serde::{Deserialize, Serialize};

/// 邮箱服务商。决定默认端口、认证方式与（M2 起的）专用协议路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// 通用 IMAP/SMTP（含企业自建、QQ / 163 / Gmail 的 IMAP 通道）。
    Imap,
    /// Gmail 专用 REST API（M2，`feature = "gmail"`）。
    Gmail,
    /// Microsoft Graph（M4）。
    Microsoft,
}

impl Provider {
    /// 落盘值（稳定，别改）。
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Imap => "imap",
            Self::Gmail => "gmail",
            Self::Microsoft => "microsoft",
        }
    }

    /// 从落盘值恢复；未知值回落到 [`Provider::Imap`]（保守：通用通道总能连）。
    pub fn from_wire(value: &str) -> Self {
        match value {
            "gmail" => Self::Gmail,
            "microsoft" => Self::Microsoft,
            _ => Self::Imap,
        }
    }
}

/// 认证方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
    /// IMAP/SMTP 口令（含应用专用密码）。
    Password,
    /// OAuth2（Gmail / Outlook，M2）。
    OAuth2,
}

impl AuthKind {
    /// 落盘值。
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::OAuth2 => "oauth2",
        }
    }

    /// 从落盘值恢复。
    pub fn from_wire(value: &str) -> Self {
        match value {
            "oauth2" => Self::OAuth2,
            _ => Self::Password,
        }
    }
}

/// 连接加密方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionSecurity {
    /// 直连 TLS（993 / 465）。**默认**。
    #[default]
    Tls,
    /// 明文连接后 `STARTTLS` 升级（143 / 587）。
    StartTls,
    /// 明文（仅本地测试服务器；界面上必须给红色警示）。
    Plain,
}

impl ConnectionSecurity {
    /// 落盘值。
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Tls => "tls",
            Self::StartTls => "starttls",
            Self::Plain => "plain",
        }
    }

    /// 从落盘值恢复。
    pub fn from_wire(value: &str) -> Self {
        match value {
            "starttls" => Self::StartTls,
            "plain" => Self::Plain,
            _ => Self::Tls,
        }
    }
}

/// 账户同步状态（对应 `accounts.sync_state`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// 空闲（已同步完或尚未开始）。
    #[default]
    Idle,
    /// 正在同步（首次全量或增量）。
    Syncing,
    /// 上次同步失败，`sync_error` 里有原因。
    Error,
    /// 需要重新认证。
    NeedsReauth,
    /// 连接断开、正在退避重连。
    Offline,
}

impl SyncState {
    /// 落盘值。
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Syncing => "syncing",
            Self::Error => "error",
            Self::NeedsReauth => "needs_reauth",
            Self::Offline => "offline",
        }
    }

    /// 从落盘值恢复。
    pub fn from_wire(value: &str) -> Self {
        match value {
            "syncing" => Self::Syncing,
            "error" => Self::Error,
            "needs_reauth" => Self::NeedsReauth,
            "offline" => Self::Offline,
            _ => Self::Idle,
        }
    }
}

/// 一个邮箱账户的**脱敏档案**（口令与令牌不在这里）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// 主键。
    pub id: i64,
    /// 邮箱地址（唯一）。
    pub email: String,
    /// 显示名（界面上的账户名）。
    pub display_name: Option<String>,
    /// 服务商。
    pub provider: Provider,
    /// 认证方式。
    pub auth_kind: AuthKind,
    /// IMAP 登录名（可能与邮箱地址不同）。
    pub username: String,
    /// IMAP 主机。
    pub imap_host: String,
    /// IMAP 端口。
    pub imap_port: u16,
    /// SMTP 主机。
    pub smtp_host: String,
    /// SMTP 端口。
    pub smtp_port: u16,
    /// 连接加密方式。
    pub security: ConnectionSecurity,
    /// 同步状态。
    pub sync_state: SyncState,
    /// 上次同步失败原因（[`SyncState::Error`] 时非空）。
    pub sync_error: Option<String>,
    /// 创建时间（Unix 秒）。
    pub created_at: i64,
}

impl Account {
    /// 界面上显示的名字：有显示名用显示名，否则用邮箱地址。
    pub fn label(&self) -> &str {
        self.display_name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&self.email)
    }

    /// 头像占位字符（邮箱首字母，大写）。
    pub fn initial(&self) -> char {
        self.label()
            .chars()
            .next()
            .map(|c| c.to_ascii_uppercase())
            .unwrap_or('?')
    }
}

/// 新增账户的入参。口令随结构体一起进内核，由内核写入凭据存储后即丢。
#[derive(Debug, Clone)]
pub struct NewAccount {
    /// 邮箱地址。
    pub email: String,
    /// 显示名。
    pub display_name: Option<String>,
    /// IMAP 登录名。
    pub username: String,
    /// IMAP 主机。
    pub imap_host: String,
    /// IMAP 端口。
    pub imap_port: u16,
    /// SMTP 主机。
    pub smtp_host: String,
    /// SMTP 端口。
    pub smtp_port: u16,
    /// 连接加密方式。
    pub security: ConnectionSecurity,
    /// 口令 / 应用专用密码。**内核只把它写进凭据存储，绝不落 SQLite。**
    pub password: String,
}

/// 文件夹角色。IMAP 的 `\Inbox` / `\Sent` / `\Drafts` / `\Trash` / `\Junk` 旗标，
/// 加日历惯例的 `Archive`；认不出来的归 [`FolderRole::Other`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FolderRole {
    /// 收件箱。
    Inbox,
    /// 已发送。
    Sent,
    /// 草稿。
    Drafts,
    /// 垃圾箱。
    Trash,
    /// 归档。
    Archive,
    /// 垃圾邮件。
    Junk,
    /// 普通文件夹。
    Other,
}

impl FolderRole {
    /// 落盘值。
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Sent => "sent",
            Self::Drafts => "drafts",
            Self::Trash => "trash",
            Self::Archive => "archive",
            Self::Junk => "junk",
            Self::Other => "other",
        }
    }

    /// 从落盘值恢复。
    pub fn from_wire(value: &str) -> Self {
        match value {
            "inbox" => Self::Inbox,
            "sent" => Self::Sent,
            "drafts" => Self::Drafts,
            "trash" => Self::Trash,
            "archive" => Self::Archive,
            "junk" => Self::Junk,
            _ => Self::Other,
        }
    }

    /// 从 IMAP 的 `LIST` 属性里认角色。
    ///
    /// 除了 `\Inbox` / `\Sent` / `\Drafts` / `\Trash` / `\Junk` 这几个标准旗标，
    /// 还要认**名字**——国内邮箱（QQ / 163 / Foxmail）经常不给旗标，只给中文名或
    /// 英文名（「已发送」「Sent Messages」「Drafts」）。
    pub fn from_imap(name: &str, attrs: &[String]) -> Self {
        let has = |needle: &str| attrs.iter().any(|a| a.eq_ignore_ascii_case(needle));
        if has("\\Inbox") {
            return Self::Inbox;
        }
        if has("\\Sent") {
            return Self::Sent;
        }
        if has("\\Drafts") {
            return Self::Drafts;
        }
        if has("\\Trash") {
            return Self::Trash;
        }
        if has("\\Junk") || has("\\Spam") {
            return Self::Junk;
        }
        if has("\\Archive") || has("\\All") {
            return Self::Archive;
        }
        // 退到名字匹配。⚠️ 名字是 **Modified UTF-7**（`&XfJT0ZAB-`）——不解码的话
        // 中文名一个都匹配不上，全部落 `Other`，而同步会跳过 `Other`，表现为
        // 「同步完成：0 个文件夹，新增 0 封」。先解回 UTF-8 再小写化 + 去掉分隔符
        // 噪音做子串判断。
        let decoded = crate::proto::utf7::decode_mailbox_name(name);
        let lower = decoded.to_ascii_lowercase();
        let tail = lower.rsplit(['/', '.', '\\']).next().unwrap_or(&lower);
        match tail {
            "inbox" => Self::Inbox,
            "sent" | "sent messages" | "sent items" | "sent mail" | "已发送" | "已发送邮件"
            | "sentbox" | "outbox" => Self::Sent,
            "drafts" | "draft" | "草稿" | "草稿箱" => Self::Drafts,
            "trash" | "deleted" | "deleted items" | "deleted messages" | "bin" | "已删除"
            | "已删除邮件" | "垃圾箱" => Self::Trash,
            "junk" | "spam" | "junk e-mail" | "垃圾邮件" | "广告邮件" => Self::Junk,
            "archive" | "archives" | "all mail" | "已归档" | "归档" => Self::Archive,
            _ => Self::Other,
        }
    }

    /// 排序权重：界面左栏按这个把常用文件夹排前面。
    pub const fn order(self) -> u8 {
        match self {
            Self::Inbox => 0,
            Self::Sent => 1,
            Self::Drafts => 2,
            Self::Archive => 3,
            Self::Junk => 4,
            Self::Trash => 5,
            Self::Other => 6,
        }
    }
}

/// 一个 IMAP 文件夹（邮箱）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    /// 主键。
    pub id: i64,
    /// 所属账户。
    pub account_id: i64,
    /// IMAP 上的原始名字（如 `INBOX`、`已发送`、`Work/Projects`）。
    pub name: String,
    /// 层级分隔符（`/` 或 `.`）。
    pub delimiter: Option<String>,
    /// 角色。
    pub role: FolderRole,
    /// 服务端给的 UIDVALIDITY（变了说明该文件夹整体作废重建）。
    pub uidvalidity: Option<i64>,
    /// 服务端给的 UIDNEXT（下一个可用 UID，即水位上界）。
    pub uidnext: Option<i64>,
    /// 本地已同步到的最大 UID。
    pub last_seen_uid: i64,
    /// 未读数。
    pub unread: i64,
    /// 总数。
    pub total: i64,
}

/// 列表里的一行。
///
/// M1 是**单封邮件**（列表直接基于 `messages`）。会话合并（`threading`）在 M2 落地，
/// 那时这里会换成按 `threads` 聚合的形态：`message_count` / `unread_count` /
/// `starred_count` / `participants` 都变成线程级聚合值。**现在不预先塞进这些字段**——
/// 没有 `threads` 挂钩却摆一堆恒为 1/0 的计数，只会让调用方误信它们是线程语义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSummary {
    /// 主键。
    pub id: i64,
    /// 所属文件夹。
    pub folder_id: i64,
    /// IMAP UID。
    pub uid: Option<i64>,
    /// 主题。
    pub subject: String,
    /// 发件人显示名。
    pub from_name: Option<String>,
    /// 发件人地址。
    pub from_addr: Option<String>,
    /// 摘要正文（列表第三行；M1 取正文首段，未拉正文时为空）。
    pub snippet: String,
    /// 发送时间（Unix 秒）。
    pub date: i64,
    /// 是否已读。
    pub is_read: bool,
    /// 是否旗标。
    pub is_starred: bool,
    /// 是否含附件。
    pub has_attachments: bool,
}

impl MessageSummary {
    /// 列表上显示的发件人：优先显示名，退化到地址，再退化到「(未知发件人)」的占位。
    pub fn sender_display(&self) -> &str {
        self.from_name
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .or(self.from_addr.as_deref())
            .unwrap_or("")
    }
}

/// 会话列表的一行（`threads` 表的聚合值 + 会话内最新一封的主题）。
///
/// 与 [`MessageSummary`] **并存**：M1 的列表是一封一行（直接查 `messages`），
/// 会话视图用这个（查 `threads`）。线程聚合是后台重算的结果，不改变既有读路径，
/// 所以两者不是替代关系。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummary {
    /// 主键（`threads.id`）。
    pub id: i64,
    /// 所属账户。
    pub account_id: i64,
    /// 会话主题：取会话内**最新一封**的原始主题（保留 `Re:` 前缀，由界面决定怎么显示）。
    pub subject: String,
    /// 摘要：取会话内最新一封的 `snippet`。
    pub snippet: String,
    /// 最新一封的时间（Unix 秒）——列表按它倒序。
    pub last_message_at: i64,
    /// 会话内消息数。
    pub message_count: i64,
    /// 会话内未读数。
    pub unread_count: i64,
    /// 会话内旗标数。
    pub starred_count: i64,
    /// 会话内带附件的消息数。
    pub attachment_count: i64,
    /// 参与者地址（去重，按首次出现顺序）。
    pub participants: Vec<String>,
}

impl ThreadSummary {
    /// 界面上显示的参与者串：前两个，多了补省略号（与草稿的收件人展示同口径）。
    pub fn participants_display(&self) -> String {
        match self.participants.split_first() {
            None => String::new(),
            Some((first, [])) => first.clone(),
            Some((first, [second, rest @ ..])) => {
                if rest.is_empty() {
                    format!("{first}, {second}")
                } else {
                    format!("{first}, {second} …")
                }
            }
        }
    }
}

/// 草稿的由来（`drafts_meta.mode`）。
///
/// 表里存的是**英文短标识**（`new` / `reply` / `forward`）——与 [`crate::MailError::code`]
/// 同一口径：语言无关，界面要显示什么由界面层决定。存中文会让这一列变成「界面的
/// 一部分」，换语言时数据就脏了。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DraftMode {
    /// 新写（也包括「转发的空壳」——没有可回填的原信时它就是新写）。
    #[default]
    New,
    /// 回复 / 全部回复。
    Reply,
    /// 转发。
    Forward,
}

impl DraftMode {
    /// 落库用的短标识。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Reply => "reply",
            Self::Forward => "forward",
        }
    }

    /// 读库。**认不出来一律当 [`DraftMode::New`]**：草稿列表里最保守的一档，
    /// 而且老数据（本列有 `DEFAULT 'new'`）本来就都是 `new`。
    pub fn from_db(text: &str) -> Self {
        match text {
            "reply" => Self::Reply,
            "forward" => Self::Forward,
            _ => Self::New,
        }
    }
}

/// 草稿箱列表的一行。
///
/// **不能复用 [`MessageSummary`]**：那个类型的 `folder_id` 是**非空 `i64`**，而草稿的
/// `folder_id` 恒为 NULL（见 [`crate::db::repo::drafts`]）——拿它去接 NULL 会直接
/// `InvalidColumnType`。多一个类型比在那边挖一个 `Option` 更安全：收信路径没有任何
/// 理由去面对「这封邮件不属于任何文件夹」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftSummary {
    /// 本地行号（`messages.id`）。
    pub id: i64,
    /// 所属账户。
    pub account_id: i64,
    /// 主题（草稿允许为空）。
    pub subject: String,
    /// 收件人的**展示串**（`名字 <地址>` 或裸地址，顺序与用户录入一致）。
    pub recipients: Vec<String>,
    /// 最后一次保存时间（Unix 秒）——草稿列表按它倒序。
    pub date: i64,
    /// 草稿的由来。
    pub mode: DraftMode,
    /// 附件条数（`draft_attachments` 的行数，**不是**字节数）。
    pub attachment_count: i64,
}

impl DraftSummary {
    /// 列表上显示的收件人：前两个，多了补省略号。空收件人给空串（由界面决定
    /// 「(无收件人)」怎么写——文案不住在内核里）。
    pub fn recipients_display(&self) -> String {
        match self.recipients.split_first() {
            None => String::new(),
            Some((first, [])) => first.clone(),
            Some((first, [second, rest @ ..])) => {
                if rest.is_empty() {
                    format!("{first}, {second}")
                } else {
                    format!("{first}, {second} …")
                }
            }
        }
    }
}

/// 附件元数据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentMeta {
    /// 主键。
    pub id: i64,
    /// 所属邮件。
    pub message_id: i64,
    /// 文件名。
    pub filename: String,
    /// MIME 类型。
    pub mime_type: String,
    /// 字节数。
    pub size: i64,
    /// `Content-ID`（内联图用，去掉尖括号）。
    pub content_id: Option<String>,
    /// 是否内联（`Content-Disposition: inline`）。
    pub is_inline: bool,
    /// 本地实体文件路径（未下载时为 `None`）。
    pub file_path: Option<String>,
    /// IMAP 段号（如 `2.1`）——按需下载时用它 `UID FETCH BODY[2.1]`。
    pub imap_section: Option<String>,
}

/// 一封邮件的详情。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageDetail {
    /// 主键。
    pub id: i64,
    /// 所属账户。
    pub account_id: i64,
    /// 所属线程。
    pub thread_id: Option<i64>,
    /// 所属文件夹。
    pub folder_id: Option<i64>,
    /// IMAP UID。
    pub uid: Option<i64>,
    /// `Message-ID` 头。
    pub message_id: Option<String>,
    /// 主题。
    pub subject: String,
    /// 发件人显示名。
    pub from_name: Option<String>,
    /// 发件人地址。
    pub from_addr: Option<String>,
    /// 收件人（已渲染成 `"名字 <addr>"` 列表）。
    pub to: Vec<String>,
    /// 抄送。
    pub cc: Vec<String>,
    /// 发送时间（Unix 秒）。
    pub date: i64,
    /// 是否已读。
    pub is_read: bool,
    /// 是否旗标。
    pub is_starred: bool,
    /// 是否含附件。
    pub has_attachments: bool,
    /// 大小（字节）。
    pub size: Option<i64>,
    /// 正文状态：`none`（未拉）/ `fetching` / `cached`。
    pub body_state: String,
    /// 纯文本正文（已解码）。
    pub text_body: Option<String>,
    /// HTML 正文（已解码，未净化——渲染前由 UI 层做协议白名单）。
    pub html_body: Option<String>,
    /// 附件清单。
    pub attachments: Vec<AttachmentMeta>,
}

/// 标签。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Label {
    /// 主键。
    pub id: i64,
    /// 标签名。
    pub name: String,
    /// 颜色（`#RRGGBB`）。
    pub color: String,
    /// IMAP 关键字（服务端存储用）。
    pub keyword: String,
}

/// 连通性测试结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionTestResult {
    /// 是否成功。
    pub ok: bool,
    /// 成功时报告的文件夹数。
    pub folder_count: usize,
    /// 失败原因（成功时为 `None`）。
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_role_prefers_flags_then_names() {
        // 标准旗标优先。
        assert_eq!(
            FolderRole::from_imap("Whatever", &["\\Sent".to_string()]),
            FolderRole::Sent
        );
        // 国内邮箱常见的是「没旗标、只有名字」，包括中文名。
        assert_eq!(FolderRole::from_imap("已发送", &[]), FolderRole::Sent);
        assert_eq!(FolderRole::from_imap("已删除邮件", &[]), FolderRole::Trash);
        assert_eq!(FolderRole::from_imap("Drafts", &[]), FolderRole::Drafts);
        assert_eq!(FolderRole::from_imap("垃圾邮件", &[]), FolderRole::Junk);
        // 带层级前缀时只看最后一段。
        assert_eq!(
            FolderRole::from_imap("Work/Projects", &[]),
            FolderRole::Other
        );
        assert_eq!(FolderRole::from_imap("Work/INBOX", &[]), FolderRole::Inbox);
    }

    #[test]
    fn folder_role_matches_decoded_utf7_names() {
        // 国内邮箱不给 SPECIAL-USE 旗标、名字还是 Modified UTF-7 的场景：
        // 不先解码就匹配不上，全部落 `Other` → 同步一个文件夹都不跑。
        assert_eq!(FolderRole::from_imap("&XfJT0ZAB-", &[]), FolderRole::Sent);
        assert_eq!(FolderRole::from_imap("&g0l6P3ux-", &[]), FolderRole::Drafts);
        assert_eq!(
            FolderRole::from_imap("&V4NXPpCuTvY-", &[]),
            FolderRole::Junk
        );
        assert_eq!(FolderRole::from_imap("&XfJSIJZk-", &[]), FolderRole::Trash);
        // 层级前缀 + 编码段混合（如 `[QQ Mail]/&XfJT0ZAB-`）也认得出尾巴。
        assert_eq!(
            FolderRole::from_imap("[QQ Mail]/&XfJT0ZAB-", &[]),
            FolderRole::Sent
        );
    }

    #[test]
    fn account_label_falls_back_to_email() {
        let mut account = Account {
            id: 1,
            email: "a@b.com".into(),
            display_name: Some("   ".into()),
            provider: Provider::Imap,
            auth_kind: AuthKind::Password,
            username: "a@b.com".into(),
            imap_host: "imap.b.com".into(),
            imap_port: 993,
            smtp_host: "smtp.b.com".into(),
            smtp_port: 465,
            security: ConnectionSecurity::Tls,
            sync_state: SyncState::Idle,
            sync_error: None,
            created_at: 0,
        };
        // 空白显示名不算数。
        assert_eq!(account.label(), "a@b.com");
        assert_eq!(account.initial(), 'A');
        account.display_name = Some("工作邮箱".into());
        assert_eq!(account.label(), "工作邮箱");
    }

    #[test]
    fn wire_roundtrip_is_total() {
        for p in [Provider::Imap, Provider::Gmail, Provider::Microsoft] {
            assert_eq!(Provider::from_wire(p.wire_name()), p);
        }
        for s in [
            SyncState::Idle,
            SyncState::Syncing,
            SyncState::Error,
            SyncState::NeedsReauth,
            SyncState::Offline,
        ] {
            assert_eq!(SyncState::from_wire(s.wire_name()), s);
        }
        for r in [
            FolderRole::Inbox,
            FolderRole::Sent,
            FolderRole::Drafts,
            FolderRole::Trash,
            FolderRole::Archive,
            FolderRole::Junk,
            FolderRole::Other,
        ] {
            assert_eq!(FolderRole::from_wire(r.wire_name()), r);
        }
        for c in [
            ConnectionSecurity::Tls,
            ConnectionSecurity::StartTls,
            ConnectionSecurity::Plain,
        ] {
            assert_eq!(ConnectionSecurity::from_wire(c.wire_name()), c);
        }
        // 未知值必须回落到安全的默认。
        assert_eq!(Provider::from_wire("nope"), Provider::Imap);
        assert_eq!(
            ConnectionSecurity::from_wire("nope"),
            ConnectionSecurity::Tls
        );
    }
}

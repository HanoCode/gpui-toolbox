//! 传输通道：SSH 隧道 / 代理（SOCKS5、HTTP CONNECT）/ AWS SSM 三档的展开与建连，
//! 以及失败分层（断在哪一层 · 走的是哪条通道）与失败提示的统一形状。
//!
//! 建连链路是 `open_transport`（通道）→ `open_session`（会话 + 只读档 + 目录内省）；
//! 探活 `probe_connection` 只走第一步。类型定义仍在 `mod.rs`。

use super::*;

impl TunnelProfile {
    /// 这份档案配的是哪一档（用于界面标签；三档都没配得 `None`）。
    ///
    /// 优先级与 [`ConnectionConfig::transport`] 一致——界面上的标签与真正生效的
    /// 必须是同一档，否则用户按标签排错会排到错的那条路上。
    pub(super) fn kind(&self) -> Option<TransportKind> {
        if self.ssm.as_ref().is_some_and(SsmConfig::is_usable) {
            Some(TransportKind::Ssm)
        } else if self
            .ssh
            .as_ref()
            .is_some_and(|tunnel| !tunnel.host.trim().is_empty() && !tunnel.user.trim().is_empty())
        {
            Some(TransportKind::Ssh)
        } else if self
            .proxy
            .as_ref()
            .is_some_and(|proxy| !proxy.host.trim().is_empty())
        {
            Some(TransportKind::Proxy)
        } else {
            None
        }
    }
}

impl Transport {
    /// 本机监听端口（数据库连接实际指向 `127.0.0.1:{port}`）。
    pub(super) fn local_port(&self) -> u16 {
        match self {
            Self::Ssh(tunnel) => tunnel.local_port(),
            Self::Proxy(tunnel) => tunnel.local_port(),
            Self::Ssm(tunnel) => tunnel.local_port(),
        }
    }

    /// 通道还活着吗（子进程未退出 / 监听线程未停）。
    pub(super) fn is_alive(&self) -> bool {
        match self {
            Self::Ssh(tunnel) => tunnel.is_alive(),
            Self::Proxy(tunnel) => tunnel.is_alive(),
            Self::Ssm(tunnel) => tunnel.is_alive(),
        }
    }

    /// 界面用的名字（连接信息卡「接入方式」那一行）。
    pub(super) fn label(&self, cx: &App) -> &'static str {
        match self {
            Self::Ssh(_) => transport_kind_label(TransportKind::Ssh, cx),
            Self::Proxy(_) => transport_kind_label(TransportKind::Proxy, cx),
            Self::Ssm(_) => transport_kind_label(TransportKind::Ssm, cx),
        }
    }
}

impl Credentials {
    /// 全部从凭据文件读（真实建连路径用）。
    ///
    /// 「测试连接 / 保存」走表单值，见 `EditFormView::prepared`——那里表单非空优先、
    /// 留空再回落到这里同一套凭据文件，两处语义一致。
    pub(super) fn from_secret_store(id: &str) -> Self {
        Self {
            db: load_password(id).unwrap_or(None),
            ssh: load_ssh_password(id).unwrap_or(None),
            ssh_key: load_ssh_key_passphrase(id).unwrap_or(None),
            proxy: load_proxy_password(id).unwrap_or(None),
        }
    }
}

impl StageError {
    /// 出错的原始错误。
    pub(super) fn error(&self) -> &DbError {
        match self {
            Self::Transport(error) | Self::Session(error) => error,
        }
    }

    /// 落到审计的哪一类（提示语与流水共用这一个判据，不会两边打架）。
    pub(super) fn kind(&self) -> crate::audit::AuditKind {
        match self {
            Self::Transport(_) => crate::audit::AuditKind::Transport,
            Self::Session(_) => crate::audit::AuditKind::Connection,
        }
    }

    /// 这一层的中文名（提示语里点明「哪一段断了」）。
    pub(super) fn label(&self, cx: &App) -> &'static str {
        match self {
            Self::Transport(_) => tr(cx, "传输通道"),
            Self::Session(_) => tr(cx, "连接"),
        }
    }
}

/// 显示成**原始错误**：提示语自己会在外面补上「哪一层」与关联 id
///（`Display` 拿不到 `cx`，译不出层名 —— 所以层名不在这里拼）。
impl std::fmt::Display for StageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error().fmt(f)
    }
}

/// 展开「隧道档案」的三档，返回**命中的那份档案**（供调用方取它的凭据）。
///
/// 语义上取的是**整体替换**而不是逐档合并：档案只配了代理、连接自己还留着一条 SSH 时，
/// 「合并」会得到两份都成立的三档，而 [`ConnectionConfig::transport`] 只挑一个 ——
/// 结果是「用户以为在走代理，实际在走 SSH」。整体替换没有这个歧义。
///
/// 引用的档案不在了（被删 / 从别的机器导入的档案）：**回落到连接自己的三档**，
/// 而不是静默直连 —— 「配了隧道却直连」是最危险的那种失败。
pub(super) fn expand_tunnel_profile<'a>(
    cfg: &ConnectionConfig,
    profiles: &'a [TunnelProfile],
) -> (ConnectionConfig, Option<&'a TunnelProfile>) {
    let Some(profile_id) = cfg.tunnel_profile.as_deref() else {
        return (cfg.clone(), None);
    };
    let Some(profile) = profiles.iter().find(|profile| profile.id == profile_id) else {
        return (cfg.clone(), None);
    };
    let mut out = cfg.clone();
    out.ssh_tunnel = profile.ssh.clone();
    out.proxy = profile.proxy.clone();
    out.ssm = profile.ssm.clone();
    (out, Some(profile))
}

/// 展开档案并**把凭据也换过去**（建连路径用）。
///
/// 档案是独立凭据主体（`dbprofile:{id}:…`）：一份公司 SOCKS5 出口被五条连接引用时，
/// 口令只该存一份。界面判据（只想知道「走哪一档」）用 [`expand_tunnel_profile`]，
/// 不必白读一遍凭据文件。
pub(super) fn resolve_transport(
    cfg: &ConnectionConfig,
    profiles: &[TunnelProfile],
    mut creds: Credentials,
) -> (ConnectionConfig, Credentials) {
    let (out, profile) = expand_tunnel_profile(cfg, profiles);
    if let Some(profile) = profile {
        creds.ssh = load_profile_ssh_password(&profile.id).unwrap_or(None);
        creds.proxy = load_profile_proxy_password(&profile.id).unwrap_or(None);
    }
    (out, creds)
}

/// 按档案建传输通道，并给出**实际该连的主机与端口**。
///
/// 生效档位由 [`ConnectionConfig::transport`] 决定（优先级只写在那里一处），
/// 本函数只负责把它落地成具体的进程 / 监听端口。返回的第二个值是「指向通道入口的
/// 副本」：调用方拿它去建连，原档案不改（否则「关掉代理」要重连才发现主机被改写过）。
pub(super) fn open_transport(
    cfg: &ConnectionConfig,
    creds: &Credentials,
) -> Result<(Option<Transport>, ConnectionConfig), DbError> {
    let mut effective = cfg.clone();
    // SQLite / 本地文件不走任何通道：起隧道只会白开一个没人用的端口。
    if !cfg.kind.needs_host() {
        return Ok((None, effective));
    }
    let remote_port = cfg
        .port
        .unwrap_or_else(|| cfg.kind.default_port().expect("network kind has a port"));
    let transport = match cfg.transport() {
        TransportKind::Direct => None,
        TransportKind::Ssh => {
            // `transport()` 只在 ssh_tunnel 存在且填全时才说 Ssh，这里不可能落空。
            let tunnel = cfg
                .ssh_tunnel
                .as_ref()
                .expect("ssh transport implies config");
            // askpass 答的答案随认证方式走：口令档是登录口令，「私钥 + 口令」档是
            // 私钥口令（留空回落登录口令 —— 有些「加密」私钥其实没设口令）。
            let password = match tunnel.auth_method {
                SshAuthMethod::KeyAndPassword => {
                    creds.ssh_key.clone().or_else(|| creds.ssh.clone())
                }
                _ => creds.ssh.clone(),
            };
            Some(Transport::Ssh(Arc::new(SshTunnel::open(SshTunnelSpec {
                jump_host: tunnel.host.clone(),
                jump_port: tunnel.port,
                user: tunnel.user.clone(),
                remote_host: tunnel.remote_host.clone(),
                remote_port,
                auth_method: tunnel.auth_method,
                key_path: tunnel.key_path.clone(),
                password,
            })?)))
        }
        TransportKind::Proxy => {
            let proxy = cfg.proxy.as_ref().expect("proxy transport implies config");
            Some(Transport::Proxy(Arc::new(ProxyTunnel::open(
                ProxyTunnelSpec {
                    proxy: ProxyConfig {
                        // 口令永远不在档案里（`#[serde(skip)]`），只能从凭据文件补。
                        password: creds.proxy.clone(),
                        ..proxy.clone()
                    },
                    target_host: cfg.host.clone(),
                    target_port: remote_port,
                },
            )?)))
        }
        TransportKind::Ssm => {
            let ssm = cfg.ssm.as_ref().expect("ssm transport implies config");
            Some(Transport::Ssm(Arc::new(SsmTunnel::open(SsmTunnelSpec {
                config: ssm.clone(),
                target_host: cfg.host.clone(),
                target_port: remote_port,
            })?)))
        }
    };
    if let Some(transport) = transport.as_ref() {
        effective.host = "127.0.0.1".into();
        effective.port = Some(transport.local_port());
    }
    Ok((transport, effective))
}

pub(super) fn probe_connection(
    cfg: &ConnectionConfig,
    creds: &Credentials,
) -> Result<String, StageError> {
    let (_transport, effective) = open_transport(cfg, creds).map_err(StageError::Transport)?;
    let mut conn =
        Connection::connect(&effective, creds.db.as_deref()).map_err(StageError::Session)?;
    Ok(conn.server_version().unwrap_or_default())
}

/// 「选择显示数据库」用：按给定配置**临时连一次**，拉取实例上的库清单。
///
/// 与 [`probe_connection`] 同一条链路（传输通道 → 连接），只是收尾换成 `list_databases`；
/// 清单为空（引擎没有库层，比如 PG 只配了 schema 视角）时回落 `list_schemas`。
/// 连接与传输通道都随函数返回一起释放，不留任何会话状态。
pub(super) fn list_database_names(
    cfg: &ConnectionConfig,
    creds: &Credentials,
) -> Result<Vec<String>, StageError> {
    let (_transport, effective) = open_transport(cfg, creds).map_err(StageError::Transport)?;
    let mut conn =
        Connection::connect(&effective, creds.db.as_deref()).map_err(StageError::Session)?;
    let names = conn.list_databases().unwrap_or_default();
    if !names.is_empty() {
        return Ok(names);
    }
    Ok(conn.list_schemas().unwrap_or_default())
}

/// 建连的实体动作：传输通道 → 连接 → 只读档 → 目录内省。
///
/// 抽成自由函数是为了让钩子能插在这一串动作的两头：写成
/// 「先跑钩子 → 建连 → 再跑钩子」的话，闭包里三层嵌套的缩进会把流程读到看不清。
///
/// 凭据由调用方传进来而不是这里现读：走「传输档案」时口令在档案自己的键上
/// （`dbprofile:{id}:…`），展开动作在 [`resolve_transport`] 里，这里不该再猜一遍。
pub(super) fn open_session(
    cfg: &ConnectionConfig,
    creds: &Credentials,
) -> Result<Session, StageError> {
    // 传输通道（SQLite 走本地文件，不需要）：先建通道再指回 127.0.0.1。
    let (transport, effective) = open_transport(cfg, creds).map_err(StageError::Transport)?;
    let mut conn =
        Connection::connect(&effective, creds.db.as_deref()).map_err(StageError::Session)?;
    // 连接级只读档：由**服务器**兜底拒绝写（不是界面软拦），建连后立刻生效。
    if effective.read_only {
        conn.set_read_only(true).map_err(StageError::Session)?;
    }
    let version = conn.server_version().unwrap_or_default();
    // 命名空间清单 + 目录 + 库级对象（触发器 / 事件 / 索引 / 用户）一次取齐；
    // 只有目录失败算真失败，其余取不到就退化成空列表。
    let introspection =
        Introspection::load(&mut conn, effective.schema.as_deref()).unwrap_or_default();
    let database =
        (!effective.database.trim().is_empty()).then(|| effective.database.trim().to_owned());
    // 标识符引号按**这条连接**取（agent 档问对端、内建七档看引擎），建连时取一次存进
    // 会话：界面渲染路径不能去 `lock()` 这个连接（后台长查询一直握着锁）。
    let quote = conn.identifier_quote();
    Ok(Session {
        conn: Arc::new(Mutex::new(conn)),
        version,
        catalog: introspection.catalog,
        triggers: introspection.triggers,
        events: introspection.events,
        indexes: introspection.indexes,
        users: introspection.users,
        schemas: introspection.schemas,
        databases: introspection.databases,
        database,
        schema: effective.schema.clone(),
        kind: effective.kind,
        quote,
        transport,
        in_tx: false,
    })
}

/// 传输档位的中文名（提示与标签用；判据与持久化一律走 [`TransportKind::key`]）。
pub(super) fn transport_kind_label(kind: TransportKind, cx: &App) -> &'static str {
    match kind {
        TransportKind::Direct => tr(cx, "直连"),
        TransportKind::Ssh => tr(cx, "SSH 隧道"),
        TransportKind::Proxy => tr(cx, "代理"),
        TransportKind::Ssm => tr(cx, "AWS SSM"),
    }
}

/// 「断在哪一层 · 走的是哪条通道」。
///
/// 只在不走直连时补通道名：直连是默认形态，写出来是噪音；而配了通道仍报失败时，
/// 「通道这一层过了没有」正是第一个要分清的事 —— `StageError` 说层，这里补通道。
pub(super) fn failure_layer(error: &StageError, cfg: &ConnectionConfig, cx: &App) -> String {
    match cfg.transport() {
        TransportKind::Direct => error.label(cx).to_owned(),
        kind => format!("{} · {}", error.label(cx), transport_kind_label(kind, cx)),
    }
}

/// 失败落在哪一层 → 审计动作键（探活与建连共用）。
pub(super) fn stage_action(kind: crate::audit::AuditKind) -> &'static str {
    match kind {
        crate::audit::AuditKind::Transport => crate::audit::action::OPEN_TRANSPORT,
        _ => crate::audit::action::PROBE,
    }
}

/// 失败提示的统一形状：`<什么失败>（<关联 id>）：<原因>`。
///
/// 关联 id 与审计里那一行**是同一个** —— 用户把提示截图发过来，就能直接定位那条流水；
/// 没有 id（理论上失败一定有）时退化成不带括号的旧形状，不显示空括号。
pub(super) fn failure_message(
    cx: &App,
    what: &str,
    trace: Option<&str>,
    reason: &str,
) -> SharedString {
    match trace {
        Some(trace) => trf(cx, "{}失败（{}）：{}", &[what, trace, reason]).into(),
        None => trf(cx, "{}失败：{}", &[what, reason]).into(),
    }
}

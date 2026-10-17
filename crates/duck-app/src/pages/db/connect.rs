//! 连接管理：驱动选择器（`DriverPickerView`）与新建 / 编辑连接表单（`EditFormView`）
//! —— 两个**独立视图**（弹窗的正文就是它们），外加档案导入的失败文案。
//!
//! 两个类型仍定义在 `mod.rs`（与 `pages::terminal` 同一口径：类型留模块根，子模块只放
//! `impl` 块），所以这里的 `impl` 能直接读写它们的私有字段，无需给字段加可见性。

use super::*;

impl EditFormView {
    /// 新建态：按引擎把可预判的字段直接填好（对齐 dbx 的 `defaultForm()`），少让用户手打；
    /// SQLite 走本地文件，三样都不填。
    ///
    /// `driver` 只有 agent 档有值 —— 缺省端口 / 惯用账号 / 展示名都随它走，所以这里
    /// 一律从 [`EngineRef`] 问（而不是各自 `match kind`）。
    ///
    /// `brand` 是**用户在墙上选中的那张品牌卡**（[`PickerTarget::Brand`]）。有值时它
    /// 比 `kind` + `driver` 更准：`mysql` 协议下站着 12 个牌子，档位与驱动键两边完全
    /// 一样，**只有牌子能把端口与账号区分开**（3306 / 4000 / 2883 / 9030…）。为 `None`
    /// 时退回 [`EngineRef::new`] 的推断（旧入口：内建七档与驱动清单卡）。
    pub(super) fn new(
        page: WeakEntity<DatabasePage>,
        kind: DatabaseKind,
        driver: Option<&'static str>,
        brand: Option<&'static Brand>,
        profiles: Vec<TunnelProfile>,
        window: &mut Window,
        cx: &mut Context<DatabasePage>,
    ) -> Self {
        // 预填对齐 dbx 的 `defaultForm()`（`ConnectionDialog.vue`）：host=`127.0.0.1`、
        // 端口=引擎缺省数字、用户=引擎惯用账号；SQLite 走本地文件，三样都不填。
        // host 输入框在网络引擎下恒被预填，占位符不出现，留空即可。
        //
        // ⚠️ 这三个值都**随牌子走**，不随档位走：同样是 `mysql` 协议，MySQL 是
        // 3306/root、TiDB 是 4000/root、OceanBase 是 2883/root、Doris 是 9030/root。
        // 见 `EngineRef::{default_port, default_user}`（内部先查品牌表）。
        let engine = match brand {
            Some(brand) => EngineRef::with_profile(brand),
            None => EngineRef::new(kind, driver),
        };
        // 档位以 `engine` 为准：牌子自己带档位（OceanBase 的 Oracle 模式是 agent 轨），
        // 用入参 `kind` 会让「MySQL 模式」那条路的档位串到 Oracle 模式上。
        let kind = engine.kind;
        let host = text_input(window, cx, "");
        let port = text_input(window, cx, tr(cx, "端口"));
        let user = text_input(window, cx, tr(cx, "用户名"));
        // 文件型引擎（SQLite / DuckDB）不预填主机——预填一个 `127.0.0.1` 只会让
        // 用户以为「这里要填地址」，而它要的是文件路径（在「数据库文件」那一格）。
        if kind.needs_host() && !engine.is_file_backed() {
            host.update(cx, |state, cx| state.set_value("127.0.0.1", window, cx));
            // 推不出牌子时惯用账号是空串（几十个引擎的惯例不同，填错比留空更害人 ——
            // 详见 `EngineRef::default_user`），空串就把占位符露出来。
            let default_user = engine.default_user();
            if !default_user.is_empty() {
                user.update(cx, |state, cx| state.set_value(default_user, window, cx));
            }
            if let Some(default_port) = engine.default_port() {
                port.update(cx, |state, cx| {
                    state.set_value(default_port.to_string(), window, cx)
                });
            }
        }
        // URL 附加参数：**少数引擎的必填项**（星环 Argo / Impala 要 `auth=noSasl`、
        // Kyuubi 要 `auth=NONE`），品牌表里有值就预填 —— 不预填的话用户只会看到
        // 一个「连接被拒绝」，而不知道该补什么。
        let url_params = text_input(window, cx, tr(cx, "如 auth=noSasl；留空即不附加"));
        let default_params = engine.default_url_params();
        if !default_params.is_empty() {
            url_params.update(cx, |state, cx| state.set_value(default_params, window, cx));
        }
        // 服务名：GBase 8s / Informix 才有（对端有缺省回落，留空合法 —— 新建不预填）。
        let server_name = text_input(
            window,
            cx,
            match engine.server_name() {
                Some(spec) => tr(cx, spec.placeholder()),
                None => "",
            },
        );
        // 新建时的**预置值**（只读档 / 默认超时 / 环境标签）来自「设置 → 数据库」。
        // 出厂默认仍是「无标签 / 可写 / 跟随引擎」，所以没设置过的话行为与从前一致。
        let prefs = db_prefs(cx);
        let env_tag = prefs
            .new_env_tag
            .as_deref()
            .map_or(EnvTag::None, EnvTag::from_wire);
        let timeout = text_input(window, cx, tr(cx, "留空跟随默认"));
        if let Some(secs) = prefs.new_timeout_secs {
            timeout.update(cx, |state, cx| {
                state.set_value(secs.to_string(), window, cx)
            });
        }
        Self {
            page,
            id: None,
            kind,
            driver: engine.driver,
            profile: engine.profile,
            tls: TlsMode::Disable,
            // 认证方式缺省口令；引擎不认得口令以外的档（SQLite / agent 轨等），下拉
            // 也就只画口令一档，状态永远不会指到不可用的档位上。
            auth_mode: AuthMode::Password,
            connection_string: text_input(window, cx, tr(cx, "postgresql://user@host:5432/db")),
            url_params,
            server_name,
            ca_cert_path: text_input(window, cx, tr(cx, "留空用内置公网根集")),
            client_cert_path: text_input(window, cx, tr(cx, "客户端证书（PEM）路径")),
            client_key_path: text_input(window, cx, tr(cx, "客户端私钥（PEM）路径")),
            visible_databases: None,
            env_tag,
            name: text_input(window, cx, tr(cx, "例如：本地开发库")),
            host,
            port,
            user,
            database: text_input(window, cx, tr(cx, "库名（网络引擎可留空）")),
            password: secret_input(window, cx, tr(cx, "留空表示保持既有口令不变")),
            group: text_input(window, cx, tr(cx, "默认分组")),
            timeout,
            schema: text_input(window, cx, tr(cx, "留空用引擎当前命名空间")),
            read_only: prefs.new_read_only,
            // 新建态恒落在「连接信息」；编辑态同理（from_config 复用 new()）。
            tab: 0,
            ssh_enabled: false,
            ssh_host: text_input(window, cx, tr(cx, "跳板机主机")),
            ssh_port: text_input(window, cx, tr(cx, "留空用 22")),
            ssh_user: text_input(window, cx, tr(cx, "跳板机用户")),
            ssh_remote_host: text_input(window, cx, tr(cx, "留空表示跳板机本机")),
            ssh_auth_method: SshAuthMethod::Password,
            ssh_key_path: text_input(window, cx, tr(cx, "私钥路径（如 ~/.ssh/id_ed25519）")),
            ssh_key_passphrase: secret_input(window, cx, tr(cx, "无口令私钥留空即可")),
            ssh_password: secret_input(window, cx, tr(cx, "留空用密钥认证 / 保持既有")),
            // 两档都是**按需**功能：默认关闭，不预置占位值（与钩子同一条口径）。
            proxy_enabled: false,
            proxy_kind: ProxyKind::default(),
            proxy_host: text_input(window, cx, tr(cx, "代理主机")),
            proxy_port: text_input(window, cx, tr(cx, "留空用默认端口")),
            proxy_user: text_input(window, cx, tr(cx, "留空表示不需要认证")),
            proxy_password: secret_input(window, cx, tr(cx, "留空表示保持既有 / 不需要认证")),
            ssm_enabled: false,
            ssm_instance: text_input(window, cx, tr(cx, "实例 id 或实例名（i-… / tag:Name=…）")),
            ssm_remote_host: text_input(window, cx, tr(cx, "留空表示实例本机")),
            ssm_region: text_input(window, cx, tr(cx, "留空用 aws 配置里的区域")),
            ssm_profile: text_input(window, cx, tr(cx, "留空用 aws 默认 profile")),
            // 传输档案：新建态恒为「自定义」（不会替用户挑一份档案去改）。
            tunnel_profiles: profiles,
            tunnel_profile: None,
            profile_name: text_input(window, cx, tr(cx, "例如：公司代理")),
            // 新建态没有钩子：这是个**按需**功能，默认给空列表而不是预置一条占位的。
            hook_rows: Vec::new(),
            hook_results: std::collections::HashMap::new(),
        }
    }

    /// 编辑态：用既有档案填表（口令不入表，留空即保持既有）。
    pub(super) fn from_config(
        page: WeakEntity<DatabasePage>,
        cfg: &ConnectionConfig,
        profiles: Vec<TunnelProfile>,
        window: &mut Window,
        cx: &mut Context<DatabasePage>,
    ) -> Self {
        // 驱动键只认**清单里查得到**的那些（`ConnectionConfig::driver_entry` 同一条口径）：
        // 档案里写错了键时按 `None` 处理，展示层回落通用文案，而不是把错的键显示出来。
        let driver = cfg.driver_entry().map(|entry| entry.key);
        // 牌子同一条口径：档案里的键在品牌表里查不到就当没有，交给 `new()` 去推
        // （旧档案没有 `profile` 字段，一对一推得出来的照样推得出来：达梦 / Oracle / DB2…）。
        // ⚠️ 必须在 `new()` 这一层传进去而不是回来补：牌子决定**预填的端口与账号**
        //（OceanBase 2883 / TiDB 4000 / Doris 9030 都是 `mysql` 协议），补晚了字段里
        // 已经躺着 MySQL 的 3306 了。
        let brand = cfg.profile.as_deref().and_then(brands::brand);
        let mut form = Self::new(page, cfg.kind, driver, brand, profiles, window, cx);
        form.id = Some(cfg.id.clone());
        form.tls = cfg.tls;
        // URL 附加参数**无条件写一遍**（`None` 就写空串）：品牌表可能刚给这条连接
        // 预填过（Argo 的 `auth=noSasl`），而这条既有连接当年就是没填 —— 留着它等于
        // 替用户改了连接参数。
        form.url_params.update(cx, |state, cx| {
            state.set_value(cfg.url_params.clone().unwrap_or_default(), window, cx)
        });
        // 服务名同一条口径：引擎认得才写（写进格子的只有自己那家的值）。
        let engine = EngineRef::from_config(cfg);
        form.server_name.update(cx, |state, cx| {
            let value = match engine.server_name() {
                Some(ServerName::Gbase) => cfg.gbase_server.clone().unwrap_or_default(),
                Some(ServerName::Informix) => cfg.informix_server.clone().unwrap_or_default(),
                None => String::new(),
            };
            state.set_value(value, window, cx)
        });
        // 引擎不认得的档位（比如旧档案手工改成 ES 的连接串模式）落回口令，
        // 别让表单停在一个保存后驱动会忽略的档位上。
        let auth_mode = if cfg.auth_mode.available_for(cfg.kind) {
            cfg.auth_mode
        } else {
            AuthMode::Password
        };
        form.auth_mode = auth_mode;
        form.visible_databases = cfg.visible_databases.clone();
        if let Some(raw) = cfg.connection_string.clone() {
            form.connection_string
                .update(cx, |state, cx| state.set_value(raw, window, cx));
        }
        if let Some(raw) = cfg.ca_cert_path.clone() {
            form.ca_cert_path
                .update(cx, |state, cx| state.set_value(raw, window, cx));
        }
        if let Some(raw) = cfg.client_cert_path.clone() {
            form.client_cert_path
                .update(cx, |state, cx| state.set_value(raw, window, cx));
        }
        if let Some(raw) = cfg.client_key_path.clone() {
            form.client_key_path
                .update(cx, |state, cx| state.set_value(raw, window, cx));
        }
        form.env_tag = cfg.env_tag;
        form.name.update(cx, |state, cx| {
            state.set_value(cfg.name.clone(), window, cx)
        });
        form.host.update(cx, |state, cx| {
            state.set_value(cfg.host.clone(), window, cx)
        });
        if let Some(port) = cfg.port {
            form.port.update(cx, |state, cx| {
                state.set_value(port.to_string(), window, cx)
            });
        }
        form.user.update(cx, |state, cx| {
            state.set_value(cfg.user.clone(), window, cx)
        });
        form.database.update(cx, |state, cx| {
            state.set_value(cfg.database.clone(), window, cx)
        });
        if let Some(group) = cfg.group.clone() {
            form.group
                .update(cx, |state, cx| state.set_value(group, window, cx));
        }
        // 超时**无条件写一遍**（`None` 就写空串）：`new()` 可能按设置页的
        // 「默认超时」预填过数字，留着它会把一条本来「跟随引擎」的既有连接
        // 变成「显式 30 秒」——保存时就把语义改掉了。
        form.timeout.update(cx, |state, cx| {
            state.set_value(
                cfg.default_timeout()
                    .map_or_else(String::new, |secs| secs.to_string()),
                window,
                cx,
            )
        });
        if let Some(schema) = cfg.schema.clone() {
            form.schema
                .update(cx, |state, cx| state.set_value(schema, window, cx));
        }
        form.read_only = cfg.read_only;
        // 引用了传输档案：下面三档显示的是**那份档案的内容**（选中态下三档就是档案本体，
        // 保存时原样写回档案）。展开一次，后面三段填表读的都是它。
        let (transport_cfg, profile) = expand_tunnel_profile(cfg, &form.tunnel_profiles);
        let profile_name = profile.map(|profile| profile.name.clone());
        form.tunnel_profile = cfg.tunnel_profile.clone();
        if let Some(name) = profile_name {
            form.profile_name
                .update(cx, |state, cx| state.set_value(name, window, cx));
        }
        if let Some(tunnel) = &transport_cfg.ssh_tunnel {
            form.ssh_enabled = true;
            form.ssh_host.update(cx, |state, cx| {
                state.set_value(tunnel.host.clone(), window, cx)
            });
            if let Some(port) = tunnel.port {
                form.ssh_port.update(cx, |state, cx| {
                    state.set_value(port.to_string(), window, cx)
                });
            }
            form.ssh_user.update(cx, |state, cx| {
                state.set_value(tunnel.user.clone(), window, cx)
            });
            form.ssh_remote_host.update(cx, |state, cx| {
                state.set_value(tunnel.remote_host.clone(), window, cx)
            });
            form.ssh_auth_method = tunnel.auth_method;
            if let Some(key) = tunnel.key_path.clone() {
                form.ssh_key_path
                    .update(cx, |state, cx| state.set_value(key, window, cx));
            }
        }
        if let Some(proxy) = &transport_cfg.proxy {
            form.proxy_enabled = true;
            form.proxy_kind = proxy.kind;
            form.proxy_host.update(cx, |state, cx| {
                state.set_value(proxy.host.clone(), window, cx)
            });
            if let Some(port) = proxy.port {
                form.proxy_port.update(cx, |state, cx| {
                    state.set_value(port.to_string(), window, cx)
                });
            }
            form.proxy_user.update(cx, |state, cx| {
                state.set_value(proxy.username.clone(), window, cx)
            });
        }
        if let Some(ssm) = &transport_cfg.ssm {
            form.ssm_enabled = true;
            form.ssm_instance.update(cx, |state, cx| {
                state.set_value(ssm.instance.clone(), window, cx)
            });
            if let Some(host) = ssm.remote_host.clone() {
                form.ssm_remote_host
                    .update(cx, |state, cx| state.set_value(host, window, cx));
            }
            if let Some(region) = ssm.region.clone() {
                form.ssm_region
                    .update(cx, |state, cx| state.set_value(region, window, cx));
            }
            if let Some(profile) = ssm.profile.clone() {
                form.ssm_profile
                    .update(cx, |state, cx| state.set_value(profile, window, cx));
            }
        }
        form.hook_rows = cfg
            .hooks
            .hooks
            .iter()
            .map(|hook| HookRow::new(hook.id.clone(), Some(hook), window, cx))
            .collect();
        form.hook_results.clear();
        form
    }

    /// 这条连接选的**子进程驱动**在本机装了没有。
    ///
    /// 内建七档恒为 `true`（不需要装）。判据查的是本机状态而不是清单 —— 清单只说
    /// 「有这么一条驱动」，说不说「它现在能跑」是两件事。
    pub(super) fn driver_ready(&self) -> bool {
        match self.driver {
            None => true,
            Some(key) => driver_store().installed(key).is_some(),
        }
    }

    /// **切模式**：OceanBase 的 MySQL / Oracle 模式、GBase 的 8a / 8s …
    ///
    /// 这不是「改一个字段」而是一次**换引擎**，所以四样一起换（`EngineRef::with_profile`）：
    ///
    /// - 档位（OceanBase 两种模式分别是内建 mysql 与 agent 轨）；
    /// - 驱动键（agent 模式下才有值）；
    /// - 牌子（档案里记住选了哪一种）；
    /// - **端口与惯用账号**（同一个产品两种模式就是两套惯例：2883/`root` 与 2883/`SYS`）。
    ///
    /// 端口 / 账号**无条件覆盖**，与 dbx 的 `switchOceanbaseMode`（内部走 `applyProfile`）
    /// 同一条口径：切换模式之后还留着上一种模式的值，多半是错的，而错的值会让连接
    /// 以「认证失败」告终 —— 那时用户根本想不到是模式切换留下的残值。
    /// 主机 / 库名 / 密码这些**与模式无关**的一律不动。
    ///
    /// 名称（连接名）也不动：它是用户起的，不是引擎给的。
    pub(super) fn apply_variant(
        &mut self,
        brand: &'static Brand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let engine = EngineRef::with_profile(brand);
        self.kind = engine.kind;
        self.driver = engine.driver;
        self.profile = engine.profile;
        // 认证方式：换档之后原档位可能不适用（内建 mysql 有连接串模式，agent 轨没有）。
        // 沿用 `from_config` 的同一条收敛（不认得就落回口令）。
        if !self.auth_mode.available_for(self.kind) {
            self.auth_mode = AuthMode::Password;
        }
        let port = engine.default_port();
        self.port.update(cx, |state, cx| {
            state.set_value(
                port.map_or_else(String::new, |port| port.to_string()),
                window,
                cx,
            )
        });
        self.user.update(cx, |state, cx| {
            state.set_value(engine.default_user(), window, cx)
        });
        self.url_params.update(cx, |state, cx| {
            state.set_value(engine.default_url_params(), window, cx)
        });
        // 服务名跟着引擎走：切到不认得它的引擎就把格子清掉（残值会让保存时
        // 把上一家的服务名写进这一家的档案）。
        self.server_name
            .update(cx, |state, cx| state.set_value(String::new(), window, cx));
        cx.notify();
    }

    /// 把表单读回结构体；端口解析失败返回可显示的提示。
    pub(super) fn to_config(&self, cx: &App) -> Result<ConnectionConfig, SharedString> {
        let port_text = self.port.read(cx).value().trim().to_owned();
        let port = if port_text.is_empty() {
            None
        } else {
            match port_text.parse::<u16>() {
                Ok(port) if port > 0 => Some(port),
                _ => return Err(tr(cx, "端口必须是 1-65535 的数字").into()),
            }
        };
        let group_text = self.group.read(cx).value().trim().to_owned();
        let group = (!group_text.is_empty()).then_some(group_text);
        // 查询超时：留空 / `0` 都表示「用全局默认」（写回 `None`，别把 0 存进去）。
        let timeout_text = self.timeout.read(cx).value().trim().to_owned();
        let default_timeout_secs = if timeout_text.is_empty() {
            None
        } else {
            match timeout_text.parse::<u64>() {
                Ok(0) => None,
                Ok(secs) if secs <= MAX_TIMEOUT_SECS => Some(secs),
                _ => {
                    return Err(trf(
                        cx,
                        "查询超时必须是 1-{} 的整数秒",
                        &[&MAX_TIMEOUT_SECS.to_string()],
                    )
                    .into());
                }
            }
        };
        let ssh_tunnel = if self.ssh_enabled {
            let tunnel_port_text = self.ssh_port.read(cx).value().trim().to_owned();
            let tunnel_port = if tunnel_port_text.is_empty() {
                None
            } else {
                match tunnel_port_text.parse::<u16>() {
                    Ok(port) if port > 0 => Some(port),
                    _ => return Err(tr(cx, "SSH 端口必须是 1-65535 的数字").into()),
                }
            };
            Some(SshTunnelConfig {
                host: self.ssh_host.read(cx).value().trim().to_owned(),
                port: tunnel_port,
                user: self.ssh_user.read(cx).value().trim().to_owned(),
                remote_host: self.ssh_remote_host.read(cx).value().trim().to_owned(),
                auth_method: self.ssh_auth_method,
                key_path: {
                    let text = self.ssh_key_path.read(cx).value().trim().to_owned();
                    (self.ssh_auth_method.needs_key() && !text.is_empty()).then_some(text)
                },
            })
        } else {
            None
        };
        let proxy = if self.proxy_enabled {
            let proxy_port_text = self.proxy_port.read(cx).value().trim().to_owned();
            let proxy_port = if proxy_port_text.is_empty() {
                None
            } else {
                match proxy_port_text.parse::<u16>() {
                    Ok(port) if port > 0 => Some(port),
                    _ => return Err(tr(cx, "代理端口必须是 1-65535 的数字").into()),
                }
            };
            Some(ProxyConfig {
                kind: self.proxy_kind,
                host: self.proxy_host.read(cx).value().trim().to_owned(),
                port: proxy_port,
                username: self.proxy_user.read(cx).value().trim().to_owned(),
                // 口令不入档案（`#[serde(skip)]`）：只在保存那一步进凭据文件。
                password: None,
            })
        } else {
            None
        };
        let ssm = if self.ssm_enabled {
            let text = |input: &Entity<InputState>| input.read(cx).value().trim().to_owned();
            let optional = |value: String| (!value.is_empty()).then_some(value);
            Some(SsmConfig {
                instance: text(&self.ssm_instance),
                remote_host: optional(text(&self.ssm_remote_host)),
                region: optional(text(&self.ssm_region)),
                profile: optional(text(&self.ssm_profile)),
            })
        } else {
            None
        };
        // 启用的钩子必须先填完：存下一条永远跑不起来的命令，错误会推迟到建连那一刻才暴露。
        if self.hook_rows.iter().any(|row| row.is_incomplete(cx)) {
            return Err(tr(cx, "有一条启用的钩子还没填完（命令或脚本不能为空）。").into());
        }
        // 服务名：认得它的引擎才落档案（GBase 8s 与 Informix 各写各的字段，其余引擎
        // 两边都 `None` —— 对端有缺省回落，留空合法，空串别存成空壳）。
        let server_name_text = self.server_name.read(cx).value().trim().to_owned();
        let engine = EngineRef {
            kind: self.kind,
            driver: self.driver,
            profile: self.profile,
        };
        let (gbase_server, informix_server) =
            match (engine.server_name(), server_name_text.is_empty()) {
                (Some(ServerName::Gbase), false) => (Some(server_name_text), None),
                (Some(ServerName::Informix), false) => (None, Some(server_name_text)),
                _ => (None, None),
            };
        Ok(ConnectionConfig {
            id: self.id.clone().unwrap_or_else(|| {
                format!("conn-{}", id_suffix(self.name.read(cx).value().as_ref()))
            }),
            name: self.name.read(cx).value().trim().to_owned(),
            kind: self.kind,
            // 驱动键只在 agent 档有意义；内建档位恒写 `None`（`EngineRef::new` 同一条口径）
            // —— 留着它会串到别的引擎上（`ConnectionConfig::driver_entry` 查得到就认）。
            driver: if self.kind == DatabaseKind::Agent {
                self.driver.map(str::to_owned)
            } else {
                None
            },
            // 牌子与档位/驱动无关，恒写（MySQL 协议下的十几个牌子就靠它区分）。
            profile: self.profile.map(str::to_owned),
            // URL 附加参数：空串即 `None`（「不附加」是缺省语义，别在档案里留一个空壳）。
            url_params: {
                let text = self.url_params.read(cx).value().trim().to_owned();
                (!text.is_empty()).then_some(text)
            },
            // 服务名各归各家：引擎认得才写（空串即 `None`，走对端的缺省回落）。
            gbase_server,
            informix_server,
            host: self.host.read(cx).value().trim().to_owned(),
            port,
            user: self.user.read(cx).value().trim().to_owned(),
            database: self.database.read(cx).value().trim().to_owned(),
            tls: self.tls,
            auth_mode: self.auth_mode,
            connection_string: {
                let text = self.connection_string.read(cx).value().trim().to_owned();
                (self.auth_mode == AuthMode::ConnectionString && !text.is_empty()).then_some(text)
            },
            ca_cert_path: {
                let text = self.ca_cert_path.read(cx).value().trim().to_owned();
                (!text.is_empty()).then_some(text)
            },
            client_cert_path: {
                let text = self.client_cert_path.read(cx).value().trim().to_owned();
                (!text.is_empty()).then_some(text)
            },
            client_key_path: {
                let text = self.client_key_path.read(cx).value().trim().to_owned();
                (!text.is_empty()).then_some(text)
            },
            visible_databases: self.visible_databases.clone(),
            env_tag: self.env_tag,
            group,
            ssh_tunnel,
            default_timeout_secs,
            read_only: self.read_only,
            schema: {
                let text = self.schema.read(cx).value().trim().to_owned();
                (!text.is_empty()).then_some(text)
            },
            hooks: ConnectionHooks {
                hooks: self.hook_rows.iter().map(|row| row.to_hook(cx)).collect(),
            },
            proxy,
            ssm,
            // 选中档案时三档照旧读出来：它是**这份档案的内容**，保存那一步会写回档案，
            // 而连接自己存的是 `None` + 引用（见 `EditFormView::save`）。
            tunnel_profile: self.tunnel_profile.clone(),
        })
    }

    /// 配置 + 解析后的口令：表单里非空用表单值，否则读既有凭据（与保存语义一致）。
    pub(super) fn prepared(
        &self,
        cx: &App,
    ) -> Result<(ConnectionConfig, Credentials), SharedString> {
        let cfg = self.to_config(cx)?;
        // 口令格里装的东西随认证方式变：口令档是口令，API Key 档是 key 原文；
        // 无认证与连接串模式整格作废（连接串自带凭据，无认证不该发口令）。
        let secret_text = self.password.read(cx).value().trim().to_owned();
        let db = match self.auth_mode {
            AuthMode::Password | AuthMode::ApiKey => {
                if secret_text.is_empty() {
                    load_password(&cfg.id).unwrap_or(None)
                } else {
                    Some(secret_text)
                }
            }
            AuthMode::None_ | AuthMode::ConnectionString => None,
        };
        // 三份额外口令同一条语义：表单非空优先，留空回落凭据文件。**档位缺失时得 `None`**
        // （跳过读文件）——`map` 会得到 `Some(None)`，那两个值在提示语里分不出来。
        //
        // ⚠️ 回落的是**谁的**凭据要看模式：选中传输档案时，三档是那份**档案**的内容，
        // 口令自然也在档案自己的键上（`dbprofile:{id}:…`）——从连接的键上读会得到一份
        // 别的口令（或者干脆是 `None`），表现成「档案里的代理明明能通，测试却认证失败」。
        let profile_id = self.tunnel_profile.clone();
        let ssh = if cfg.ssh_tunnel.is_some() {
            let text = self.ssh_password.read(cx).value().trim().to_owned();
            if !text.is_empty() {
                Some(text)
            } else if let Some(profile_id) = profile_id.as_deref() {
                load_profile_ssh_password(profile_id).unwrap_or(None)
            } else {
                load_ssh_password(&cfg.id).unwrap_or(None)
            }
        } else {
            None
        };
        // 私钥口令：只有「私钥 + 口令」档才往凭据链路里送；回落顺序与 SSH 口令同一条。
        let ssh_key = if cfg
            .ssh_tunnel
            .as_ref()
            .is_some_and(|tunnel| tunnel.auth_method == SshAuthMethod::KeyAndPassword)
        {
            let text = self.ssh_key_passphrase.read(cx).value().trim().to_owned();
            if !text.is_empty() {
                Some(text)
            } else if let Some(profile_id) = profile_id.as_deref() {
                load_profile_ssh_key_passphrase(profile_id).unwrap_or(None)
            } else {
                load_ssh_key_passphrase(&cfg.id).unwrap_or(None)
            }
        } else {
            None
        };
        let proxy = if cfg.proxy.is_some() {
            let text = self.proxy_password.read(cx).value().trim().to_owned();
            if !text.is_empty() {
                Some(text)
            } else if let Some(profile_id) = profile_id.as_deref() {
                load_profile_proxy_password(profile_id).unwrap_or(None)
            } else {
                load_proxy_password(&cfg.id).unwrap_or(None)
            }
        } else {
            None
        };
        Ok((
            cfg,
            Credentials {
                db,
                ssh,
                ssh_key,
                proxy,
            },
        ))
    }

    /// 保存：校验 → 口令进凭据文件 → 档案落 kv（经页面）。返回是否成功。
    pub(super) fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let (mut cfg, creds) = match self.prepared(cx) {
            Ok(prepared) => prepared,
            Err(message) => {
                notify::warning(message, window, cx);
                return false;
            }
        };
        if let Err(error) = cfg.validate() {
            notify::warning(SharedString::from(error.to_string()), window, cx);
            return false;
        }
        // 选中档案时，三档是**档案的内容**：先把它们写回档案（含档案自己的口令），
        // 连接本体只留一个引用。这样「改一处，引用它的五条连接下次建连全都生效」。
        if let Some(profile_id) = self.tunnel_profile.clone() {
            let name = self.profile_name.read(cx).value().trim().to_owned();
            let profile = TunnelProfile {
                id: profile_id.clone(),
                name: if name.is_empty() {
                    tr(cx, "未命名档案").to_owned()
                } else {
                    name
                },
                ssh: cfg.ssh_tunnel.clone(),
                proxy: cfg.proxy.clone(),
                ssm: cfg.ssm.clone(),
            };
            let _ = self
                .page
                .update(cx, |page, cx| page.upsert_tunnel_profile(profile, cx));
            self.refresh_tunnel_profiles(cx);
            // ⚠️ 档案的口令存在档案自己的键上，**不能**顺手写到连接的键：
            // 写过去等于把「这份档案的口令」在每条引用它的连接上各留一份副本。
            if let Some(password) = creds.proxy.clone().filter(|text| !text.is_empty())
                && let Err(error) = store_profile_proxy_password(&profile_id, &password)
            {
                notify::error(
                    trf(cx, "代理口令保存失败：{}", &[&error.to_string()]),
                    window,
                    cx,
                );
                return false;
            }
            if let Some(password) = creds.ssh.clone().filter(|text| !text.is_empty())
                && let Err(error) = store_profile_ssh_password(&profile_id, &password)
            {
                notify::error(
                    trf(cx, "SSH 口令保存失败：{}", &[&error.to_string()]),
                    window,
                    cx,
                );
                return false;
            }
            if let Some(passphrase) = creds.ssh_key.clone().filter(|text| !text.is_empty())
                && let Err(error) = store_profile_ssh_key_passphrase(&profile_id, &passphrase)
            {
                notify::error(
                    trf(cx, "SSH 私钥口令保存失败：{}", &[&error.to_string()]),
                    window,
                    cx,
                );
                return false;
            }
            cfg.tunnel_profile = Some(profile_id);
            cfg.ssh_tunnel = None;
            cfg.proxy = None;
            cfg.ssm = None;
        }
        // 口令：非空才写凭据文件（编辑时留空 = 保持既有口令不变）。
        // 上面那条分支已经把档案口令写完了，这里的 `cfg.*` 都是 `None`，
        // 所以不会重复写——两处的判据都是「这个档位还在不在连接上」。
        if let Some(password) = creds.db.filter(|text| !text.is_empty())
            && let Err(error) = store_password(&cfg.id, &password)
        {
            notify::error(
                trf(cx, "口令保存失败：{}", &[&error.to_string()]),
                window,
                cx,
            );
            return false;
        }
        if cfg.ssh_tunnel.is_some()
            && let Some(ssh_password) = creds.ssh.filter(|text| !text.is_empty())
            && let Err(error) = store_ssh_password(&cfg.id, &ssh_password)
        {
            notify::error(
                trf(cx, "SSH 口令保存失败：{}", &[&error.to_string()]),
                window,
                cx,
            );
            return false;
        }
        if cfg.ssh_tunnel.is_some()
            && let Some(passphrase) = creds.ssh_key.filter(|text| !text.is_empty())
            && let Err(error) = store_ssh_key_passphrase(&cfg.id, &passphrase)
        {
            notify::error(
                trf(cx, "SSH 私钥口令保存失败：{}", &[&error.to_string()]),
                window,
                cx,
            );
            return false;
        }
        if cfg.proxy.is_some()
            && let Some(proxy_password) = creds.proxy.filter(|text| !text.is_empty())
            && let Err(error) = store_proxy_password(&cfg.id, &proxy_password)
        {
            notify::error(
                trf(cx, "代理口令保存失败：{}", &[&error.to_string()]),
                window,
                cx,
            );
            return false;
        }
        let _ = self
            .page
            .update(cx, |page, cx| page.upsert_connection(cfg, cx));
        notify::success(tr(cx, "连接已保存。"), window, cx);
        true
    }

    /// 把页面的档案快照拉回来（下拉选项要立刻反映刚才的新增 / 删除）。
    pub(super) fn refresh_tunnel_profiles(&mut self, cx: &Context<Self>) {
        let Some(page) = self.page.upgrade() else {
            return;
        };
        self.tunnel_profiles = page.read(cx).tunnel_profiles.clone();
    }

    /// 选中档案：把它的三档读进表单（选中态下三档就是这份档案的内容）。
    pub(super) fn select_tunnel_profile(
        &mut self,
        profile_id: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.tunnel_profile = profile_id.clone();
        let Some(profile) = profile_id
            .as_deref()
            .and_then(|id| self.tunnel_profiles.iter().find(|profile| profile.id == id))
            .cloned()
        else {
            // 切回「自定义」：三档**保持当前值**不动，用户改完再存成新档案。
            cx.notify();
            return;
        };
        self.profile_name.update(cx, |state, cx| {
            state.set_value(profile.name.clone(), window, cx)
        });
        self.load_transport_into_form(&profile, window, cx);
        cx.notify();
    }

    /// 把一份档案的三档写进表单控件（切档案 / 新建时共用）。
    pub(super) fn load_transport_into_form(
        &mut self,
        profile: &TunnelProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = |value: &str| value.to_owned();
        self.ssh_enabled = profile.ssh.is_some();
        self.ssh_host.update(cx, |state, cx| {
            state.set_value(
                profile
                    .ssh
                    .as_ref()
                    .map_or_else(String::new, |t| text(&t.host)),
                window,
                cx,
            )
        });
        self.ssh_port.update(cx, |state, cx| {
            state.set_value(
                profile
                    .ssh
                    .as_ref()
                    .and_then(|t| t.port)
                    .map_or_else(String::new, |port| port.to_string()),
                window,
                cx,
            )
        });
        self.ssh_user.update(cx, |state, cx| {
            state.set_value(
                profile
                    .ssh
                    .as_ref()
                    .map_or_else(String::new, |t| text(&t.user)),
                window,
                cx,
            )
        });
        self.ssh_remote_host.update(cx, |state, cx| {
            state.set_value(
                profile
                    .ssh
                    .as_ref()
                    .map_or_else(String::new, |t| text(&t.remote_host)),
                window,
                cx,
            )
        });
        self.ssh_auth_method = profile
            .ssh
            .as_ref()
            .map_or(SshAuthMethod::Password, |t| t.auth_method);
        self.ssh_key_path.update(cx, |state, cx| {
            state.set_value(
                profile
                    .ssh
                    .as_ref()
                    .and_then(|t| t.key_path.clone())
                    .unwrap_or_default(),
                window,
                cx,
            )
        });
        self.proxy_enabled = profile.proxy.is_some();
        if let Some(proxy) = profile.proxy.as_ref() {
            self.proxy_kind = proxy.kind;
        }
        self.proxy_host.update(cx, |state, cx| {
            state.set_value(
                profile
                    .proxy
                    .as_ref()
                    .map_or_else(String::new, |p| text(&p.host)),
                window,
                cx,
            )
        });
        self.proxy_port.update(cx, |state, cx| {
            state.set_value(
                profile
                    .proxy
                    .as_ref()
                    .and_then(|p| p.port)
                    .map_or_else(String::new, |port| port.to_string()),
                window,
                cx,
            )
        });
        self.proxy_user.update(cx, |state, cx| {
            state.set_value(
                profile
                    .proxy
                    .as_ref()
                    .map_or_else(String::new, |p| text(&p.username)),
                window,
                cx,
            )
        });
        self.ssm_enabled = profile.ssm.is_some();
        self.ssm_instance.update(cx, |state, cx| {
            state.set_value(
                profile
                    .ssm
                    .as_ref()
                    .map_or_else(String::new, |s| text(&s.instance)),
                window,
                cx,
            )
        });
        self.ssm_remote_host.update(cx, |state, cx| {
            state.set_value(
                profile
                    .ssm
                    .as_ref()
                    .and_then(|s| s.remote_host.clone())
                    .unwrap_or_default(),
                window,
                cx,
            )
        });
        self.ssm_region.update(cx, |state, cx| {
            state.set_value(
                profile
                    .ssm
                    .as_ref()
                    .and_then(|s| s.region.clone())
                    .unwrap_or_default(),
                window,
                cx,
            )
        });
        self.ssm_profile.update(cx, |state, cx| {
            state.set_value(
                profile
                    .ssm
                    .as_ref()
                    .and_then(|s| s.profile.clone())
                    .unwrap_or_default(),
                window,
                cx,
            )
        });
    }

    /// 「存为新档案」：用当前三档 + 名称输入框的值建一份新档案，并把连接切到引用它。
    pub(super) fn add_tunnel_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.profile_name.read(cx).value().trim().to_owned();
        if name.is_empty() {
            notify::warning(tr(cx, "先给档案起个名字。"), window, cx);
            return;
        }
        let cfg = match self.to_config(cx) {
            Ok(cfg) => cfg,
            Err(message) => {
                notify::warning(message, window, cx);
                return;
            }
        };
        if cfg.ssh_tunnel.is_none() && cfg.proxy.is_none() && cfg.ssm.is_none() {
            notify::warning(tr(cx, "三档都空着，存下来会是一份空档案。"), window, cx);
            return;
        }
        // id 从名字派生 + 序号去重：与连接档案同一套做法，改名不会丢定位。
        let existing: Vec<String> = self
            .tunnel_profiles
            .iter()
            .map(|profile| profile.id.clone())
            .collect();
        let base = format!("tunnel-{}", id_suffix(&name));
        let mut id = base.clone();
        let mut seq = 2;
        while existing.contains(&id) {
            id = format!("{base}-{seq}");
            seq += 1;
        }
        let profile = TunnelProfile {
            id: id.clone(),
            name,
            ssh: cfg.ssh_tunnel.clone(),
            proxy: cfg.proxy.clone(),
            ssm: cfg.ssm.clone(),
        };
        let _ = self
            .page
            .update(cx, |page, cx| page.upsert_tunnel_profile(profile, cx));
        self.refresh_tunnel_profiles(cx);
        // 档案自己的口令：把表单里刚打的那份也存到档案的键上（不然「新建档案」之后
        // 第一次建连会因为档案没口令而认证失败，用户还得再进一次表单）。
        let creds = self.prepared(cx).map(|(_, creds)| creds).ok();
        if let Some(creds) = creds {
            if let Some(password) = creds.proxy.filter(|text| !text.is_empty()) {
                let _ = store_profile_proxy_password(&id, &password);
            }
            if let Some(password) = creds.ssh.filter(|text| !text.is_empty()) {
                let _ = store_profile_ssh_password(&id, &password);
            }
            if let Some(passphrase) = creds.ssh_key.filter(|text| !text.is_empty()) {
                let _ = store_profile_ssh_key_passphrase(&id, &passphrase);
            }
        }
        self.tunnel_profile = Some(id);
        notify::success(tr(cx, "传输档案已保存。"), window, cx);
        cx.notify();
    }

    /// 「删除这份档案」：引用它的连接一并清掉引用（页面的 `delete_tunnel_profile` 负责）。
    pub(super) fn remove_tunnel_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile_id) = self.tunnel_profile.clone() else {
            return;
        };
        let refs = self
            .page
            .upgrade()
            .map_or(0, |page| page.read(cx).tunnel_profile_refs(&profile_id));
        let message = if refs > 0 {
            trf(
                cx,
                "删除后引用它的 {} 条连接会回到「自定义」，确定继续吗？",
                &[&refs.to_string()],
            )
        } else {
            tr(cx, "确定删除这份传输档案吗？").to_owned()
        };
        // 询问走主题确认框（异步），不用 `duck_platform::confirm` 的同步系统模态 ——
        // 见 `crate::confirm` 模块头。
        crate::confirm::open_confirm_in(
            tr(cx, "删除传输档案"),
            message,
            tr(cx, "删除"),
            move |this, window, cx| this.delete_tunnel_profile_now(&profile_id, window, cx),
            window,
            cx,
        );
    }

    /// 用户点了「删除」之后真正动手的那一半。
    fn delete_tunnel_profile_now(
        &mut self,
        profile_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let _ = self
            .page
            .update(cx, |page, cx| page.delete_tunnel_profile(profile_id, cx));
        self.refresh_tunnel_profiles(cx);
        self.tunnel_profile = None;
        self.profile_name
            .update(cx, |state, cx| state.set_value(String::new(), window, cx));
        notify::success(tr(cx, "传输档案已删除。"), window, cx);
        cx.notify();
    }

    /// 「测试连接」：用表单当前值探活（不保存、不开会话），报告版本或错误。
    pub(super) fn test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (cfg, creds) = match self.prepared(cx) {
            Ok(prepared) => prepared,
            Err(message) => {
                notify::warning(message, window, cx);
                return;
            }
        };
        if let Err(error) = cfg.validate() {
            notify::warning(SharedString::from(error.to_string()), window, cx);
            return;
        }
        let _ = self
            .page
            .update(cx, |page, cx| page.spawn_probe(cfg, creds, cx));
    }

    /// 「选择显示数据库」：用表单当前配置临时连一次，拉库清单进勾选弹窗。
    ///
    /// 与「测试连接」同一套链路（[`list_database_names`]，含传输通道展开），连接随
    /// 拉取完成即释放。结果不直接落盘 —— 写回表单的 `visible_databases`，随「保存连接」
    /// 一起进档案。
    pub(super) fn open_visible_databases(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.kind.needs_host() {
            notify::warning(
                tr(cx, "SQLite 是本地文件，没有「库」可以筛选。"),
                window,
                cx,
            );
            return;
        }
        let (cfg, creds) = match self.prepared(cx) {
            Ok(prepared) => prepared,
            Err(message) => {
                notify::warning(message, window, cx);
                return;
            }
        };
        if let Err(error) = cfg.validate() {
            notify::warning(SharedString::from(error.to_string()), window, cx);
            return;
        }
        let form = cx.entity().downgrade();
        let _ = self.page.update(cx, |page, cx| {
            page.spawn_db(
                cx,
                move || list_database_names(&cfg, &creds),
                move |_, result, window, cx| match result {
                    Ok(names) if names.is_empty() => {
                        notify::warning(tr(cx, "这个实例上没有列出任何库。"), window, cx);
                    }
                    Ok(names) => {
                        let _ = form.update(cx, |form, cx| {
                            form.show_visible_database_picker(names, window, cx);
                        });
                    }
                    Err(error) => {
                        let reason = error.to_string();
                        notify::error(
                            failure_message(cx, error.label(cx), None, &reason),
                            window,
                            cx,
                        );
                    }
                },
            );
        });
    }

    /// 打开「选择显示数据库」弹窗（勾选初值 = 表单里已存的白名单）。
    fn show_visible_database_picker(
        &mut self,
        names: Vec<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let form = cx.entity().downgrade();
        let search = text_input(window, cx, tr(cx, "搜索库"));
        let initial: std::collections::HashSet<String> = self
            .visible_databases
            .clone()
            .unwrap_or_default()
            .into_iter()
            .collect();
        let picker = cx.new(|cx| {
            let subscription = cx.subscribe(
                &search,
                |_this: &mut VisibleDatabasesView,
                 _source: Entity<InputState>,
                 event: &InputEvent,
                 cx: &mut Context<VisibleDatabasesView>| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                },
            );
            VisibleDatabasesView {
                form,
                selected: initial,
                names,
                search,
                _subscription: subscription,
            }
        });
        let drag = modal_drag(cx);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let drag_for_content = drag.clone();
            let picker_for_content = picker.clone();
            let picker_for_footer = picker.clone();

            let footer = DialogFooter::new()
                .child(
                    button(
                        "db-visible-all",
                        tr(cx, "全选"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(move |_, _, cx| {
                        picker_for_footer.update(cx, |picker, cx| {
                            picker.selected = picker.names.iter().cloned().collect();
                            cx.notify();
                        });
                    }),
                )
                .child(
                    button(
                        "db-visible-clear",
                        tr(cx, "清空"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click({
                        let picker_for_clear = picker.clone();
                        move |_, _, cx| {
                            picker_for_clear.update(cx, |picker, cx| {
                                picker.selected.clear();
                                cx.notify();
                            });
                        }
                    }),
                )
                .child(div().flex_1())
                .child(
                    button(
                        "db-visible-cancel",
                        tr(cx, "取消"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child(
                    button(
                        "db-visible-confirm",
                        tr(cx, "确定"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click({
                        let picker_for_confirm = picker.clone();
                        move |_, window, cx| {
                            let (selected_len, total) = picker_for_confirm.read(cx).counts();
                            // 白名单语义（dbx 同一条）：勾满全部或一个没勾 = 不过滤。
                            // 存死快照会让之后新建的库永远进不了侧栏。
                            let selection = picker_for_confirm.read(cx).sorted_selection();
                            let form = picker_for_confirm.read(cx).form.clone();
                            let _ = form.update(cx, |form, cx| {
                                form.visible_databases =
                                    if selected_len == total || selected_len == 0 {
                                        None
                                    } else {
                                        Some(selection)
                                    };
                                cx.notify();
                            });
                            window.close_dialog(cx);
                        }
                    }),
                );

            dialog
                .w(px(VISIBLE_DBS_DIALOG_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .button_props(DialogButtonProps::default())
                .content(move |content, _, cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .min_h_0()
                            .child(modal_title_row(
                                "db-visible-title",
                                &drag_for_content,
                                tr(cx, "选择显示数据库"),
                                cx,
                            ))
                            .child(picker_for_content.clone()),
                    ))
                })
                .footer(footer)
        });
    }

    /// 「路径输入框 + 选择文件按钮」一行（TLS 证书与 SSH 私钥共用）。
    ///
    /// 文件面板走 [`crate::file_dialogs::open_file_in_app`]（gpui 异步面板，不重入
    /// 事件循环 —— 界面禁用 rfd 同步模态是硬纪律）；回调把路径写回对应输入框。
    fn path_row(
        &self,
        id: &str,
        label: &'static str,
        input: &Entity<InputState>,
        prompt: &'static str,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let input = input.clone();
        h_flex()
            .w_full()
            .items_center()
            .gap(tokens.spacing.lg)
            .child(field_label(tr(cx, label), cx))
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&input)
                        .h(theme.density().control)
                        .text_size(tokens.typography.sm.size)
                        .w_full(),
                ),
            )
            .child(
                toolbar_action_button(
                    SharedString::from(id.to_owned()),
                    tr(cx, "选择文件"),
                    Icon::new(DuckIcon::FolderOpen).size(theme.extended().icon.sm),
                    false,
                    false,
                    cx,
                )
                .on_click(move |_, window, cx| {
                    let input = input.clone();
                    crate::file_dialogs::open_file_in_app(
                        window,
                        cx,
                        tr(cx, prompt),
                        move |path, window, cx| {
                            input.update(cx, |state, cx| {
                                state.set_value(path.display().to_string(), window, cx)
                            });
                        },
                    );
                }),
            )
    }

    // ── 连接钩子 ──────────────────────────────────────────────────────

    /// 表单当前的「环境事实」（试跑钩子时喂给子进程；**不含口令**）。
    ///
    /// 用的是**表单里此刻的值**而不是已保存的档案：这样「改完主机顺手试跑一下钩子」
    /// 看到的就是改完之后的 `DUCK_CONN_HOST`，不必先保存再试。
    pub(super) fn hook_context(&self, cx: &App) -> HookContext {
        let text = |state: &Entity<InputState>| state.read(cx).value().trim().to_owned();
        let port = {
            let raw = text(&self.port);
            if raw.is_empty() {
                self.kind
                    .default_port()
                    .map_or_else(String::new, |port| port.to_string())
            } else {
                raw
            }
        };
        HookContext {
            // 新建态还没有 id：试跑时这个变量会是空串（钩子不该依赖它做判断）。
            connection_id: self.id.clone().unwrap_or_default(),
            connection_name: text(&self.name),
            kind: self.kind.wire_name().to_owned(),
            host: text(&self.host),
            port,
            user: text(&self.user),
            database: text(&self.database),
        }
    }

    /// 加一条空钩子。
    pub(super) fn add_hook(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let existing: Vec<String> = self.hook_rows.iter().map(|row| row.id.clone()).collect();
        let mut seq = existing.len() + 1;
        let id = loop {
            let candidate = format!("hook-{seq}");
            if !existing.contains(&candidate) {
                break candidate;
            }
            seq += 1;
        };
        let row = HookRow::new(id, None, window, cx);
        self.hook_rows.push(row);
        cx.notify();
    }

    /// 删一条钩子（连带清掉它的试跑结果）。
    pub(super) fn remove_hook(&mut self, id: &str, cx: &mut Context<Self>) {
        self.hook_rows.retain(|row| row.id != id);
        self.hook_results.remove(id);
        cx.notify();
    }

    /// 「试跑」一条钩子：**在后台单跑一次**（不建连、不落任何状态，只为了确认它跑得通）。
    pub(super) fn test_hook(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.hook_rows.get(index) else {
            return;
        };
        if row.is_incomplete(cx) {
            notify::warning(tr(cx, "这条钩子还没填完，先补上命令或脚本。"), window, cx);
            return;
        }
        let hook = row.to_hook(cx);
        let context = self.hook_context(cx);
        let phase = hook.phase;
        let id = hook.id.clone();
        let task = cx
            .background_executor()
            .spawn(async move { run_hook(&hook, phase, &context) });
        cx.spawn(async move |this, cx| {
            let outcome = task.await;
            let _ = this.update(cx, |this, cx| {
                this.hook_results.insert(id, outcome);
                cx.notify();
            });
        })
        .detach();
    }

    /// HOOKS 分区：列表 + 「添加钩子」。
    pub(super) fn hooks_section(&self, cx: &mut Context<Self>) -> AnyElement {
        let tokens = active_theme(cx).tokens().clone();
        let mut section = v_flex()
            .w_full()
            .gap(form_gap(cx))
            .child(db_section_header(DuckIcon::SquareTerminal, "HOOKS", cx))
            .child(field_hint(
                tr(
                    cx,
                    "在建连 / 断开的前后跑一条本机命令或脚本；失败默认只提示、不阻断。",
                ),
                cx,
            ));
        if self.hook_rows.is_empty() {
            section = section.child(field_hint(tr(cx, "还没有钩子。"), cx));
        }
        for index in 0..self.hook_rows.len() {
            section = section.child(self.render_hook_row(index, cx));
        }
        section
            .child(
                h_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(
                        button(
                            "db-hook-add",
                            tr(cx, "添加钩子"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .on_click(cx.listener(|this, _, window, cx| this.add_hook(window, cx))),
                    )
                    .child(field_hint(
                        tr(cx, "钩子只在「连接 / 断开」时跑，不会在测试连接时跑。"),
                        cx,
                    )),
            )
            .into_any_element()
    }

    /// 一行钩子：启用 / 相位 / 载体 / 内容 / 工作目录 / 超时 / 失败策略 / 试跑 / 删除。
    pub(super) fn render_hook_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();
        let Some(row) = self.hook_rows.get(index) else {
            return div().into_any_element();
        };
        let id = row.id.clone();
        let mode = row.mode;
        let phase = row.phase;
        let failure = row.failure;
        let enabled = row.enabled;
        let entity = cx.entity().downgrade();

        // 相位下拉（四档）。
        let phase_options: Vec<(SharedString, SharedString)> = HookPhase::ALL
            .iter()
            .map(|phase| {
                (
                    SharedString::from(phase.key()),
                    SharedString::from(hook_phase_label(*phase, cx)),
                )
            })
            .collect();
        let phase_select = {
            let entity = entity.clone();
            let key = id.clone();
            dropdown_select(
                SharedString::from(format!("db-hook-phase-{key}")),
                phase_options,
                SharedString::from(phase.key()),
                move |value: SharedString, _window, cx| {
                    let Some(phase) = HookPhase::from_key(&value) else {
                        return;
                    };
                    let _ = entity.update(cx, |this, cx| {
                        if let Some(row) = this.hook_rows.iter_mut().find(|row| row.id == key) {
                            row.phase = phase;
                        }
                        cx.notify();
                    });
                },
                cx,
            )
        };

        // 失败策略下拉（两档）。
        let failure_options = vec![
            (
                SharedString::from("warn"),
                SharedString::from(tr(cx, "失败只提示（继续）")),
            ),
            (
                SharedString::from("block"),
                SharedString::from(tr(cx, "失败则中断本次操作")),
            ),
        ];
        let failure_select = {
            let entity = entity.clone();
            let key = id.clone();
            let current = match failure {
                HookFailure::Warn => "warn",
                HookFailure::Block => "block",
            };
            dropdown_select(
                SharedString::from(format!("db-hook-failure-{key}")),
                failure_options,
                SharedString::from(current),
                move |value: SharedString, _window, cx| {
                    let failure = if value.as_ref() == "block" {
                        HookFailure::Block
                    } else {
                        HookFailure::Warn
                    };
                    let _ = entity.update(cx, |this, cx| {
                        if let Some(row) = this.hook_rows.iter_mut().find(|row| row.id == key) {
                            row.failure = failure;
                        }
                        cx.notify();
                    });
                },
                cx,
            )
        };

        // 载体两档（命令 / 脚本）。
        let mode_toggle = h_flex().flex_none().gap(tokens.spacing.xxs).children(
            [HookMode::Command, HookMode::Script].map(|candidate| {
                let selected = mode == candidate;
                let key = id.clone();
                button(
                    SharedString::from(format!("db-hook-mode-{}-{}", candidate.key(), key)),
                    SharedString::from(candidate.label(cx)),
                    if selected {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::Secondary
                    },
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(row) = this.hook_rows.iter_mut().find(|row| row.id == key) {
                        row.mode = candidate;
                    }
                    cx.notify();
                }))
            }),
        );

        // 启用 / 停用。
        let enable_toggle = {
            let key = id.clone();
            button(
                SharedString::from(format!("db-hook-enable-{key}")),
                if enabled {
                    tr(cx, "已启用")
                } else {
                    tr(cx, "已停用")
                },
                if enabled {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Secondary
                },
                cx,
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if let Some(row) = this.hook_rows.iter_mut().find(|row| row.id == key) {
                    row.enabled = !row.enabled;
                }
                cx.notify();
            }))
        };

        // 内容：命令档给一行命令，脚本档给「解释器 + 文件」。
        let content = match mode {
            HookMode::Command => field_row(cx, tr(cx, "命令"), &row.line),
            HookMode::Script => v_flex()
                .w_full()
                .gap(form_gap(cx))
                .child(field_row(cx, tr(cx, "解释器"), &row.interpreter))
                .child(field_row(cx, tr(cx, "脚本文件"), &row.path)),
        };

        let status = self
            .hook_results
            .get(&id)
            .map(|outcome| hook_outcome_line(outcome, cx));

        let remove = {
            let key = id.clone();
            toolbar_action_button(
                SharedString::from(format!("db-hook-remove-{key}")),
                tr(cx, "删除钩子"),
                Icon::new(DuckIcon::Trash2).size(extended.icon.sm),
                false,
                false,
                cx,
            )
            .on_click(cx.listener(move |this, _, _, cx| this.remove_hook(&key, cx)))
        };
        let run = toolbar_action_button(
            SharedString::from(format!("db-hook-test-{id}")),
            tr(cx, "试跑"),
            Icon::new(DuckIcon::Play).size(extended.icon.sm),
            false,
            false,
            cx,
        )
        .on_click(cx.listener(move |this, _, window, cx| this.test_hook(index, window, cx)));

        v_flex()
            .w_full()
            .gap(form_gap(cx))
            .p(tokens.spacing.sm)
            .rounded(tokens.radius.md)
            .border_1()
            .border_color(if enabled {
                tokens.colors.border
            } else {
                extended.colors.hairline
            })
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .flex_wrap()
                    .gap(tokens.spacing.xs)
                    .child(enable_toggle)
                    .child(phase_select)
                    .child(mode_toggle)
                    .child(div().flex_1())
                    .child(run)
                    .child(remove),
            )
            .child(content)
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.lg)
                    .child(field_row(cx, tr(cx, "工作目录"), &row.cwd))
                    .child(field_row(cx, tr(cx, "超时（秒）"), &row.timeout)),
            )
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.lg)
                    .child(field_label(tr(cx, "失败时"), cx))
                    .child(failure_select),
            )
            .children(status)
            .into_any_element()
    }
}

impl DriverPickerView {
    pub(super) fn new(
        page: WeakEntity<DatabasePage>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = text_input(window, cx, tr(cx, "按名称、驱动或端口筛选"));
        let subscription = cx.subscribe(
            &search,
            |this: &mut Self,
             _source: Entity<InputState>,
             event: &InputEvent,
             cx: &mut Context<Self>| {
                if matches!(event, InputEvent::Change) {
                    this.query = this.search.read(cx).value().trim().to_lowercase();
                    cx.notify();
                }
            },
        );
        Self {
            page,
            search,
            query: String::new(),
            // 缺省落在关系型：这是用户找库的第一直觉（也是卡最多的一档）。
            active_section: PickerSection::Sql,
            selected: None,
            _subscription: subscription,
        }
    }

    /// 当前选中的卡片（按 id 找回；id 已不在墙上时是 `None` —— 比如搜索换分类后
    /// 被去重规则挪走的那张）。
    fn selected_card(&self) -> Option<PickerCard> {
        let id = self.selected?;
        picker_cards().into_iter().find(|card| card.id() == id)
    }

    /// 「已选择：xxx」的名字（脚注用；没选时 `None`）。
    pub(super) fn selected_name(&self, cx: &App) -> Option<SharedString> {
        self.selected_card().map(|card| card.name(cx))
    }

    /// 「下一步」：把当前选中交给 [`Self::choose`]（目标不可连就沿用它的提示）。
    pub(super) fn proceed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(card) = self.selected_card() else {
            return;
        };
        self.choose(card.target(), window, cx);
    }

    /// 进详情表单：支持的目标关掉 picker、进表单；指路的目标切路由；其余只提示。
    ///
    /// 四条路的分岔在这里收口（`PickerTarget`），表单那边只知道「档位 + 驱动键 + 牌子」：
    ///
    /// - [`PickerTarget::Builtin`] → 内建七档之一，`driver` / `brand` 都为 `None`；
    /// - [`PickerTarget::Agent`] → 档位 [`DatabaseKind::Agent`]、驱动键带上
    ///   （缺省端口 / 展示名 / 品牌图都随它走）；
    /// - [`PickerTarget::Brand`] → **牌子才是选中物**：档位与驱动键都从它推出来
    ///   （OceanBase 的 MySQL 模式 → 内建 `mysql`，Oracle 模式 → agent 轨）；
    /// - [`PickerTarget::Page`] → 收掉弹窗、切到那条路由（Redis 有自己的页）；
    /// - [`PickerTarget::Unsupported`] → 只提示，不进表单。
    ///
    /// ⚠️ 走 agent 档**不检查驱动装没装**：表单里有「去装驱动」的提示，驱动管理弹窗
    /// 也在同一个选择器的脚注上 —— 先让用户把账号密码填完再告诉他缺驱动，比反过来
    /// 好（他会先去装驱动，回来还得重填一遍）。
    pub(super) fn choose(&self, target: PickerTarget, window: &mut Window, cx: &mut App) {
        let (kind, driver, brand) = match target {
            PickerTarget::Builtin(kind) => (kind, None, None),
            PickerTarget::Agent(key) => (DatabaseKind::Agent, Some(key), None),
            // 牌子卡：三样一起往下传。档位与驱动键**从牌子推**而不是从卡片旁边的
            // 上下文推 —— `mysql` 协议下十几个牌子共用同一个档位，只有牌子决定
            // 端口、账号与「要不要装驱动」（见 `PickerTarget::Brand`）。
            PickerTarget::Brand(brand) => (brand.kind(), brand.driver(), Some(brand)),
            // **先收弹窗再切路由**：`navigate_to` 换的是 shell 里那一格的内容，
            // 它不会替我们收弹窗 —— 而弹窗挂在**当前**窗口上，切完就成了上一页的遗留物。
            PickerTarget::Page(route) => {
                window.close_dialog(cx);
                crate::app::navigate_to(cx, route);
                return;
            }
            PickerTarget::Unsupported => {
                notify::info(tr(cx, "该数据库类型的支持即将到来。"), window, cx);
                return;
            }
        };
        window.close_dialog(cx);
        let _ = self.page.update(cx, |page, cx| {
            page.start_new_connection(kind, driver, brand, window, cx);
        });
    }

    /// 分类侧栏的一行（样式对齐驱动管理弹窗的 `rail_row`：选中底 + 计数）。
    fn rail_row(
        &self,
        section: PickerSection,
        count: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();
        let hover_bg = extended.colors.hover_tint;
        let active = self.active_section == section;
        h_flex()
            .id(SharedString::from(format!("db-picker-rail-{:?}", section)))
            .w_full()
            .items_center()
            .justify_between()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .rounded(tokens.radius.md)
            .cursor_pointer()
            .when(active, |el| el.bg(extended.colors.nav_selected))
            .hover(move |style| style.bg(hover_bg))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(tokens.typography.sm.size)
                    .child(section.label(cx)),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(tokens.typography.xs.size)
                    .text_color(tokens.colors.muted_foreground)
                    .font_features(tabular_numbers())
                    .child(SharedString::from(count.to_string())),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                this.active_section = section;
                cx.notify();
            }))
    }

    /// 一张驱动卡（选中态 = 强调色描边 + 选中底色 + 名字转强调色；`query` 过滤在调用点已做）。
    fn card(&self, card: &PickerCard, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();
        let id = card.id();
        let name = card.name(cx);
        let hint = card.hint(cx);
        let logo = card.logo();
        let target = card.target();
        // 「即将到来」的那几张在**墙上就看得出**，不必点一下才弹提示。
        // 弱化的只是名字的颜色：**品牌名与品牌图照常显示** —— 未支持说的是
        // 「本仓还没接」，不是「这不是一个正经引擎」。
        let supported = card.is_supported();
        let selected = self.selected == Some(id);
        // 选中底与悬停底分两档：选中卡不再被悬停的 muted 盖掉选中观感。
        let (normal_bg, hover_bg) = if selected {
            (extended.colors.nav_selected, extended.colors.nav_selected)
        } else {
            (tokens.colors.background, tokens.colors.muted)
        };
        let name_el = div()
            .truncate()
            .text_size(tokens.typography.sm.size)
            .font_weight(FontWeight::MEDIUM)
            .child(name);
        let name_el = if supported {
            if selected {
                name_el.text_color(extended.colors.accent_text)
            } else {
                name_el
            }
        } else {
            name_el.text_color(tokens.colors.muted_foreground)
        };
        h_flex()
            .id(SharedString::from(format!("db-driver-{id}")))
            .min_w_0()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .rounded(tokens.radius.md)
            .border_1()
            .border_color(if selected {
                tokens.colors.accent
            } else {
                tokens.colors.border
            })
            .bg(normal_bg)
            .cursor_pointer()
            .hover(move |style| style.bg(hover_bg))
            .child(match logo {
                Some(logo) => logo.element(px(PICKER_LOGO_SIZE)).into_any_element(),
                None => Icon::new(DuckIcon::Database)
                    .size(px(PICKER_LOGO_SIZE))
                    .text_color(tokens.colors.muted_foreground)
                    .into_any_element(),
            })
            .child(
                div().flex_1().min_w_0().child(name_el).child(
                    div()
                        .truncate()
                        .text_size(tokens.typography.xs.size)
                        .text_color(tokens.colors.muted_foreground)
                        .child(hint),
                ),
            )
            // 角标只挂在未支持的那几张上：`Option` 直接交给 `children`
            // （这里是个挂过 `.id()` 的 `Stateful`，没有 `.when()`）。
            .children((!supported).then(|| neutral_tag(SharedString::from(tr(cx, "未支持")), cx)))
            // dbx 同一条交互：**单击只选中**（下一步进表单），双击（或点已选中的卡）
            // 直达 —— 熟手省一步。
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                if !supported {
                    notify::info(tr(cx, "该数据库类型的支持即将到来。"), window, cx);
                    return;
                }
                let double_click =
                    matches!(event, ClickEvent::Mouse(mouse) if mouse.up.click_count > 1);
                if double_click || this.selected == Some(id) {
                    this.choose(target, window, cx);
                    return;
                }
                this.selected = Some(id);
                cx.notify();
            }))
    }
}

impl Render for DriverPickerView {
    /// 头部（提示 + 搜索框）常驻；主体两栏：左侧分类侧栏、右侧卡片网格，整体独立滚动。
    /// 搜索态下右侧跨分类显示命中（按分类标题分组），与 dbx 一致。
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let searching = !self.query.is_empty();

        // 标题行（`modal_title_row`）已经写了「新建连接」+ ✕，这里**不再重复标题**：
        // 第二行只放一句提示与搜索框（原实现两行标题叠着，是个肉眼可见的重复）。
        let header = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .justify_between()
            .gap(tokens.spacing.lg)
            .child(meta_text(tr(cx, "选择要连接的目标。"), cx))
            .child(
                h_flex()
                    .h(theme.density().control)
                    // 驱动选择器的**舒适宽度**：布局约束（不在 spacing 阶梯上），保留裸值。
                    .w(px(240.))
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    .px(tokens.spacing.sm)
                    .rounded(tokens.radius.md)
                    .border_1()
                    .border_color(tokens.colors.border)
                    .bg(tokens.colors.background)
                    .child(
                        Icon::new(DuckIcon::Search)
                            .size(theme.extended().icon.md)
                            .text_color(tokens.colors.muted_foreground),
                    )
                    .child(
                        div().flex_1().min_w_0().child(
                            Input::new(&self.search)
                                .appearance(false)
                                .with_size(Size::Small)
                                .text_size(tokens.typography.sm.size)
                                .w_full(),
                        ),
                    ),
            );

        // 手写的 14 张 + 驱动清单里能接成数据源的 44 条（见 `picker_cards`）。
        let all = picker_cards();
        let visible: Vec<&PickerCard> = all
            .iter()
            .filter(|card| card.matches(&self.query))
            .collect();

        // 侧栏计数是**全量**的（不是搜索命中数）：它回答「这一类里有什么」，
        // 跟当前搜什么无关。
        let rail = {
            let mut rail = v_flex().w_full().gap(tokens.spacing.xxs);
            for section in PICKER_SECTIONS {
                let count = all.iter().filter(|card| card.section() == *section).count();
                if count == 0 {
                    continue;
                }
                rail = rail.child(self.rail_row(*section, count, cx));
            }
            rail
        };

        // 搜索态跨分类（带分类小标题），否则只画当前分类 —— 两种状态共用一段网格拼装。
        let sections_for_grid: Vec<PickerSection> = if searching {
            PICKER_SECTIONS.to_vec()
        } else {
            vec![self.active_section]
        };
        let mut body = v_flex().w_full().min_h_0().gap(tokens.spacing.xs);
        for section in &sections_for_grid {
            let cards: Vec<&PickerCard> = visible
                .iter()
                .copied()
                .filter(|card| card.section() == *section)
                .collect();
            if cards.is_empty() {
                continue;
            }
            let mut grid = div()
                .grid()
                .grid_cols(PICKER_COLUMNS as u16)
                .gap(tokens.spacing.xs);
            for card in cards {
                grid = grid.child(self.card(card, cx));
            }
            body = body
                .when(searching, |body| {
                    body.child(caption_heading(section.label(cx), cx))
                })
                .child(grid);
        }
        if visible.is_empty()
            || (!searching && {
                !visible
                    .iter()
                    .any(|card| card.section() == self.active_section)
            })
        {
            body = body.child(
                div()
                    .w_full()
                    .py(tokens.spacing.lg)
                    .flex()
                    .justify_center()
                    .child(meta_text(tr(cx, "没有匹配的数据库类型。"), cx)),
            );
        }

        v_flex()
            .w_full()
            .min_h_0()
            .child(
                div()
                    .w_full()
                    .flex_none()
                    .px(tokens.spacing.lg)
                    .py(tokens.spacing.sm)
                    .border_b_1()
                    .border_color(tokens.colors.border)
                    .child(header),
            )
            .child(
                h_flex()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    // 侧栏顶对齐（`h_flex` 默认 cross-axis 居中，会把矮的侧栏垂直居中，
                    // 顶上留出一段空白）；网格自身从顶往下排。
                    .items_start()
                    .px(tokens.spacing.lg)
                    .py(tokens.spacing.sm)
                    // 侧栏宽度：布局约束（九档中文标签一行放得下），保留裸值。
                    .gap(tokens.spacing.lg)
                    .child(
                        div()
                            .id("db-picker-rail")
                            .w(px(PICKER_RAIL_WIDTH))
                            .flex_none()
                            .min_h_0()
                            .overflow_y_scroll()
                            .child(rail),
                    )
                    .child(
                        div()
                            .id("db-picker-grid")
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .child(body),
                    ),
            )
    }
}

impl Render for EditFormView {
    /// dbx 同形的详情正文：头部（返回 + 引擎 logo + 标题 / 副标题 + 连接名称 + 关闭）
    /// 常驻，三分区（SERVER / AUTHENTICATION / TRANSPORT）独立滚动。
    ///
    /// 脚注（测试连接 / 删除 / 取消 / 保存连接）挂在弹窗外壳上 —— 它不属于表单，
    /// 也不需要跟着表单状态重绘。
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let icon_md = theme.extended().icon.md;
        let is_new = self.id.is_none();
        // 展示名 / 品牌图 / 模式切换器 / 库名提示都从这个值问。
        //
        // ⚠️ 必须带上 `self.profile`，不能用 [`EngineRef::new`]：后者靠
        // `brands::infer(kind, driver)` **反推**牌子，而 MySQL 协议下站着十几个牌子、
        // 推不出来 ⇒ `profile = None` ⇒ 模式族切换器不画（OceanBase 点进来只剩
        // 「新建 MySQL 连接」，标题也退回协议名）—— 表单状态里明明记着牌子。
        // `self.profile` 的三个写入点（`with_profile` / `from_config` / `apply_variant`）
        // 都已对品牌表验过键，这里直接信它。
        let engine = EngineRef {
            kind: self.kind,
            driver: self.driver,
            profile: self.profile,
        };
        // 网络引擎才画主机/端口/认证/传输；**文件型引擎**（SQLite、DuckDB）只有一格
        // 「数据库文件」——`EngineRef::is_file_backed` 是这条判据的唯一来源。
        let network = self.kind.needs_host() && !engine.is_file_backed();
        let engine_label = SharedString::from(tr(cx, engine.label()));

        // ── 头部：返回 + 引擎 logo + 标题 / 副标题 + 连接名称 + 关闭 ──
        let header = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(
                toolbar_action_button(
                    "db-edit-back",
                    tr(cx, "返回"),
                    Icon::new(DuckIcon::ChevronLeft).size(icon_md),
                    false,
                    false,
                    cx,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    // 返回 = 回到 picker：新建态回上一步，编辑态直接关窗。
                    window.close_dialog(cx);
                    if is_new {
                        let _ = this
                            .page
                            .update(cx, |page, cx| page.open_new_connection_picker(window, cx));
                    }
                })),
            )
            .child(engine_logo(engine, px(EDIT_LOGO_SIZE), cx))
            .child(
                v_flex()
                    .min_w_0()
                    .gap(tokens.spacing.xxs)
                    .child(title_text(
                        trf(
                            cx,
                            if is_new {
                                "新建 {} 连接"
                            } else {
                                "编辑 {} 连接"
                            },
                            &[engine_label.as_ref()],
                        ),
                        cx,
                    ))
                    .child(meta_text(tr(cx, "标红的字段为必填项。"), cx)),
            )
            .child(div().flex_1())
            .child(
                // 连接名输入框宽度：布局约束（不在 spacing 阶梯上），保留裸值。
                div().w(px(196.)).child(
                    Input::new(&self.name)
                        .h(theme.density().control)
                        .text_size(tokens.typography.sm.size)
                        .w_full(),
                ),
            )
            .child(
                toolbar_action_button(
                    "db-edit-close",
                    tr(cx, "关闭"),
                    Icon::new(DuckIcon::X).size(icon_md),
                    false,
                    false,
                    cx,
                )
                .on_click(|_, window, cx| window.close_dialog(cx)),
            );

        // ── 子进程驱动没装时的提示条 ──
        //
        // 只对 agent 档、且**本机真没装**时出现。这条连接现在保存得下、连不上，
        // 得让用户知道差什么、并且就地有个地方去装 —— 否则他只能自己猜。
        // 用 `destructive` 描边而不是弹警告：它是一条**常驻状态**，不是一次性事件。
        let driver_notice = (!self.driver_ready()).then(|| {
            let page = self.page.clone();
            h_flex()
                .w_full()
                .items_center()
                .gap(tokens.spacing.sm)
                .px(tokens.spacing.sm)
                .py(tokens.spacing.xs)
                .rounded(tokens.radius.md)
                .border_1()
                .border_color(tokens.colors.destructive)
                .child(
                    Icon::new(DuckIcon::TriangleAlert)
                        .size(icon_md)
                        .text_color(tokens.colors.destructive),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(meta_text(tr(cx, "这条驱动还没装到本机，现在连不上。"), cx)),
                )
                .child(
                    button(
                        "db-edit-install-driver",
                        tr(cx, "去装驱动"),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        // 与选择器脚注那条入口同一条口径：先关掉当前弹窗再开商店，
                        // 同一窗口只留一层。
                        window.close_dialog(cx);
                        let _ = page.update(cx, |page, cx| page.open_driver_manager(window, cx));
                    }),
                )
        });

        // ── 环境：带语义色圆点的 pill，选中即改档案标签 ──
        let env_pills = h_flex()
            .flex_wrap()
            .items_center()
            .gap(tokens.spacing.xxs)
            .children(EnvTag::ALL.map(|tag| {
                let selected = self.env_tag == tag;
                let label = env_tag_label(tag, cx).unwrap_or_else(|| tr(cx, "无标签").into());
                let dot = div()
                    // 环境色点直径：视觉标记（不是间距步进），阶梯里没有对应档。
                    .size(px(8.))
                    .flex_none()
                    .rounded_full()
                    .bg(env_dot_color(tag, cx));
                icon_text_button(
                    SharedString::from(format!("db-env-{}", tag.wire_name())),
                    label,
                    dot,
                    if selected {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::Secondary
                    },
                    cx,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.env_tag = tag;
                    cx.notify();
                }))
            }));

        // ── SERVER：主机（必填）+ 端口、数据库、分组；SQLite 只有文件与分组 ──
        let mut server_fields = v_flex().w_full().gap(form_gap(cx)).child(db_section_header(
            DuckIcon::Server,
            "SERVER",
            cx,
        ));

        // ── 模式族：同一个牌子的一次连接要先选模式 ──
        //
        // 判据只有一条：品牌表里同族的牌子多于一个（`EngineRef::variants`）。单模式品牌
        // ——绝大多数 —— 根本不画这一行。
        //
        // 为什么排在 SERVER 的**第一行**而不是塞进「高级选项」：它决定下面那些字段的
        // 语义。OceanBase 的 Oracle 模式连的是另一套协议（agent 轨），端口与惯用账号
        // 也是另一套 —— 让人先填完主机端口再发现「模式选错了」，等于让他重填一遍。
        let variants = engine.variants();
        if variants.len() > 1 {
            let entity = cx.entity().downgrade();
            let mut pills = h_flex().flex_wrap().items_center().gap(tokens.spacing.xxs);
            for variant in variants {
                let brand = *variant;
                // 每个按钮各持一份弱引用（闭包要 `'static`，循环里不能共用同一个）。
                let entity = entity.clone();
                let selected = self.profile == Some(brand.key);
                // 模式名是中文源串（品牌表的 `mode`），空串时退到品牌名。
                let label = SharedString::from(tr(
                    cx,
                    if brand.mode.is_empty() {
                        brand.label
                    } else {
                        brand.mode
                    },
                ));
                pills = pills.child(
                    button(
                        SharedString::from(format!("db-variant-{}", brand.key)),
                        label,
                        if selected {
                            ButtonVariant::Primary
                        } else {
                            ButtonVariant::Secondary
                        },
                        cx,
                    )
                    .on_click(move |_, window, cx| {
                        let _ = entity.update(cx, |this, cx| {
                            this.apply_variant(brand, window, cx);
                        });
                    }),
                );
            }
            server_fields = server_fields.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.lg)
                    .child(field_label(tr(cx, "模式"), cx))
                    .child(pills),
            );
        }

        if network {
            server_fields = server_fields
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap(tokens.spacing.lg)
                        .child(
                            // 必填标签落危险色（与副标题「标红的字段为必填项」呼应）。
                            field_label(tr(cx, "主机"), cx).text_color(tokens.colors.destructive),
                        )
                        .child(
                            div().flex_1().min_w_0().child(
                                Input::new(&self.host)
                                    .h(theme.density().control)
                                    .text_size(tokens.typography.sm.size)
                                    .w_full(),
                            ),
                        )
                        .child(
                            // 端口输入框宽度：布局约束（4 位数足够，再宽浪费），保留裸值。
                            div().w(px(104.)).child(
                                Input::new(&self.port)
                                    .h(theme.density().control)
                                    .text_size(tokens.typography.sm.size)
                                    .w_full(),
                            ),
                        ),
                )
                .child(field_row(cx, tr(cx, "数据库"), &self.database))
                .child(field_hint(engine.database_hint(cx), cx));
            // URL 附加参数：只有 agent 轨消费它（内建驱动的连接串由各自的驱动拼），
            // 所以只在这一档画。少数引擎**少了它连不上**（星环 Argo / Impala 的
            // `auth=noSasl`、Kyuubi 的 `auth=NONE`），品牌表里有值就在开表时预填好了。
            if self.kind == DatabaseKind::Agent {
                // 服务名：GBase 8s / Informix 才有（两个 agent 引擎各自要一个
                // server 名，对端有缺省回落 —— 有格子教用户填，好过 908 报错）。
                if let Some(spec) = engine.server_name() {
                    server_fields = server_fields
                        .child(field_row(cx, tr(cx, spec.label()), &self.server_name))
                        .child(field_hint(tr(cx, spec.placeholder()), cx));
                }
                server_fields = server_fields
                    .child(field_row(cx, tr(cx, "URL 附参"), &self.url_params))
                    .child(field_hint(
                        tr(
                            cx,
                            "URL 附加参数（连接串 `?` 之后那截）。少数引擎必填：星环 Argo 用 auth=noSasl，Kyuubi 用 auth=NONE。",
                        ),
                        cx,
                    ));
            }
            server_fields = server_fields.child(field_row(cx, tr(cx, "分组"), &self.group));
        } else {
            server_fields = server_fields
                .child(field_row(cx, tr(cx, "数据库文件"), &self.database))
                .child(field_hint(engine.database_hint(cx), cx))
                .child(field_row(cx, tr(cx, "分组"), &self.group));
        }

        // ── QUERY：默认命名空间 + 连接级只读档 + 该连接的默认查询超时 ──
        //
        // 三项都是「连接级」而非「某次执行」的设置，与 SERVER / AUTHENTICATION 的
        // 物理连接参数分开排。读写档放这里而不是 TRANSPORT：它管的是**权限**而非传输。
        let read_only_toggle = button(
            "db-readonly-toggle",
            if self.read_only {
                tr(cx, "只读")
            } else {
                tr(cx, "可写")
            },
            if self.read_only {
                ButtonVariant::Primary
            } else {
                ButtonVariant::Secondary
            },
            cx,
        )
        .on_click(cx.listener(|this, _, _, cx| {
            this.read_only = !this.read_only;
            cx.notify();
        }));
        let query_section = v_flex()
            .w_full()
            .gap(form_gap(cx))
            .child(db_section_header(DuckIcon::Gauge, "QUERY", cx))
            .child(field_row(cx, tr(cx, "默认命名空间"), &self.schema))
            .child(field_hint(
                tr(cx, "连接后默认浏览的 schema / 库；留空用引擎当前命名空间。"),
                cx,
            ))
            .child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.lg)
                    .child(field_label(tr(cx, "读写档"), cx))
                    .child(read_only_toggle),
            )
            .child(field_hint(
                tr(
                    cx,
                    "只读档由服务器兜底拒绝写语句（不是界面软拦）；切到只读不需要重连。",
                ),
                cx,
            ))
            .child(field_row(cx, tr(cx, "查询超时（秒）"), &self.timeout))
            .child(field_hint(
                tr(
                    cx,
                    "只作用于读路径（执行 SQL、读表、AI 只读执行）；留空用全局默认 30 秒。",
                ),
                cx,
            ));

        // ── AUTHENTICATION：认证方式下拉 + 按档位显隐的输入。仅网络引擎。 ──
        //
        // dbx 同一条取向：档位是少数通用值（密码 / 无认证 / 连接串 / API Key），
        // 每个引擎只认得其中几档（`AuthMode::available_for`），下拉按它裁剪。
        let auth_section = network.then(|| {
            let options: Vec<(SharedString, SharedString)> = AuthMode::ALL
                .into_iter()
                .filter(|mode| mode.available_for(self.kind))
                .map(|mode| {
                    (
                        SharedString::from(mode.wire_name()),
                        SharedString::from(auth_mode_label(mode, cx)),
                    )
                })
                .collect();
            let entity = cx.entity().downgrade();
            let mode_select = dropdown_select(
                "db-auth-mode",
                options,
                SharedString::from(self.auth_mode.wire_name()),
                move |value: SharedString, _window, cx| {
                    let _ = entity.update(cx, |this, cx| {
                        this.auth_mode = AuthMode::from_wire(&value);
                        cx.notify();
                    });
                },
                cx,
            );
            let mut section = v_flex()
                .w_full()
                .gap(form_gap(cx))
                .child(db_section_header(DuckIcon::User, "AUTHENTICATION", cx))
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap(tokens.spacing.lg)
                        .child(field_label(tr(cx, "认证方式"), cx))
                        // 下拉宽度：布局约束（与传输档案下拉同宽），保留裸值。
                        .child(div().w(px(220.)).child(mode_select)),
                );
            match self.auth_mode {
                AuthMode::ConnectionString => {
                    section = section
                        .child(field_row(cx, tr(cx, "连接串"), &self.connection_string))
                        .child(field_hint(
                            tr(
                                cx,
                                "整条连接串直连；上面的主机 / 端口 / 账号在本档下全部忽略。",
                            ),
                            cx,
                        ));
                }
                AuthMode::ApiKey => {
                    section = section
                        .child(field_row(cx, tr(cx, "API Key"), &self.password))
                        .child(field_hint(
                            tr(
                                cx,
                                "Key 原文进 Authorization: ApiKey 头；与口令同一套凭据链路。",
                            ),
                            cx,
                        ));
                }
                AuthMode::None_ => {
                    section = section
                        .child(field_row_required(cx, tr(cx, "用户名"), &self.user))
                        .child(field_hint(
                            tr(
                                cx,
                                "无认证：不发口令（PostgreSQL trust / 未开安全的集群）。",
                            ),
                            cx,
                        ));
                }
                AuthMode::Password => {
                    section = section
                        .child(field_row_required(cx, tr(cx, "用户名"), &self.user))
                        .child(field_row(cx, tr(cx, "密码"), &self.password));
                }
            }
            section
        });

        // ── TLS/SSL：SSL 档位 + 证书三件套。仅网络引擎。 ──
        //
        // 档位三档沿用内核 `TlsMode`；证书字段只在**真消费它们**的两个驱动
        //（PostgreSQL / MySQL）下展开 —— 其余引擎填了也不会生效，不画的理由
        // 与「不支持的能力不画入口」同一条。
        let tls_section = network.then(|| {
            let tls_switch = h_flex()
                .flex_wrap()
                .items_center()
                .gap(tokens.spacing.xxs)
                .children(TlsMode::ALL.map(|mode| {
                    let selected = self.tls == mode;
                    let label = match mode {
                        TlsMode::Disable => tr(cx, "不用 TLS"),
                        TlsMode::Trust => tr(cx, "校验证书"),
                        TlsMode::DangerAcceptInvalid => tr(cx, "接受自签"),
                    };
                    button(
                        SharedString::from(format!("db-tls-{}", mode.wire_name())),
                        SharedString::from(label),
                        if selected {
                            ButtonVariant::Primary
                        } else {
                            ButtonVariant::Secondary
                        },
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.tls = mode;
                        cx.notify();
                    }))
                }));
            let mut section = v_flex()
                .w_full()
                .gap(form_gap(cx))
                .child(db_section_header(DuckIcon::ShieldCheck, "TLS/SSL", cx))
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap(tokens.spacing.lg)
                        .child(field_label(tr(cx, "SSL 模式"), cx))
                        .child(tls_switch),
                );
            if self.tls != TlsMode::Disable
                && matches!(self.kind, DatabaseKind::Postgres | DatabaseKind::Mysql)
            {
                section = section
                    .child(self.path_row(
                        "db-ca-pick",
                        "CA 证书",
                        &self.ca_cert_path,
                        "选择 CA 证书（PEM）",
                        cx,
                    ))
                    .child(self.path_row(
                        "db-client-cert-pick",
                        "客户端证书",
                        &self.client_cert_path,
                        "选择客户端证书（PEM）",
                        cx,
                    ))
                    .child(self.path_row(
                        "db-client-key-pick",
                        "客户端私钥",
                        &self.client_key_path,
                        "选择客户端私钥（PEM）",
                        cx,
                    ))
                    .child(field_hint(
                        tr(
                            cx,
                            "自签 CA 填这里可免「接受自签」；客户端证书 + 私钥用于双向 TLS（私钥需为未加密格式）。",
                        ),
                        cx,
                    ));
            }
            section
        });

        // ── TRANSPORT：SSH 隧道 + 代理 + SSM。仅网络引擎。（SSL 档位在 TLS/SSL 分区。） ──
        let transport_section = network.then(|| {
            let ssh_toggle = button(
                "db-ssh-toggle",
                if self.ssh_enabled {
                    tr(cx, "经 SSH 隧道")
                } else {
                    tr(cx, "直连")
                },
                if self.ssh_enabled {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Secondary
                },
                cx,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.ssh_enabled = !this.ssh_enabled;
                cx.notify();
            }));
            let mut ssh_block = v_flex().w_full().gap(form_gap(cx)).child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.lg)
                    .child(field_label(tr(cx, "SSH 隧道"), cx))
                    .child(ssh_toggle),
            );
            if self.ssh_enabled {
                // 认证方式下拉（口令 / 私钥 / 私钥 + 口令 / agent）。
                let auth_options: Vec<(SharedString, SharedString)> = SshAuthMethod::ALL
                    .iter()
                    .map(|method| {
                        (
                            SharedString::from(method.wire_name()),
                            SharedString::from(ssh_auth_label(*method, cx)),
                        )
                    })
                    .collect();
                let entity = cx.entity().downgrade();
                let ssh_auth_select = dropdown_select(
                    "db-ssh-auth",
                    auth_options,
                    SharedString::from(self.ssh_auth_method.wire_name()),
                    move |value: SharedString, _window, cx| {
                        let _ = entity.update(cx, |this, cx| {
                            this.ssh_auth_method = SshAuthMethod::from_wire(&value);
                            cx.notify();
                        });
                    },
                    cx,
                );
                ssh_block = ssh_block
                    .child(field_row(cx, tr(cx, "跳板机"), &self.ssh_host))
                    .child(field_row(cx, tr(cx, "SSH 端口"), &self.ssh_port))
                    .child(field_row(cx, tr(cx, "SSH 用户"), &self.ssh_user))
                    .child(field_row(cx, tr(cx, "转发目标"), &self.ssh_remote_host))
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap(tokens.spacing.lg)
                            .child(field_label(tr(cx, "SSH 认证"), cx))
                            .child(div().w(px(220.)).child(ssh_auth_select)),
                    );
                ssh_block = match self.ssh_auth_method {
                    SshAuthMethod::Password => {
                        ssh_block.child(field_row(cx, tr(cx, "SSH 口令"), &self.ssh_password))
                    }
                    SshAuthMethod::Key => ssh_block.child(self.path_row(
                        "db-ssh-key-pick",
                        "私钥文件",
                        &self.ssh_key_path,
                        "选择 SSH 私钥",
                        cx,
                    )),
                    SshAuthMethod::KeyAndPassword => ssh_block
                        .child(self.path_row(
                            "db-ssh-key-pick",
                            "私钥文件",
                            &self.ssh_key_path,
                            "选择 SSH 私钥",
                            cx,
                        ))
                        .child(field_row(cx, tr(cx, "私钥口令"), &self.ssh_key_passphrase)),
                    SshAuthMethod::Agent => ssh_block.child(field_hint(
                        tr(
                            cx,
                            "使用本机 ssh-agent（SSH_AUTH_SOCK 原样透传给 ssh 子进程）。",
                        ),
                        cx,
                    )),
                };
            }

            // 代理：协议两档按钮 + 主机 / 端口 / 认证。口令与 SSH 同一条纪律（进凭据文件）。
            let proxy_toggle = button(
                "db-proxy-toggle",
                if self.proxy_enabled {
                    tr(cx, "经代理")
                } else {
                    tr(cx, "不经代理")
                },
                if self.proxy_enabled {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Secondary
                },
                cx,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.proxy_enabled = !this.proxy_enabled;
                cx.notify();
            }));
            let proxy_kind_switch = h_flex()
                .flex_wrap()
                .items_center()
                .gap(tokens.spacing.xxs)
                .children(ProxyKind::ALL.map(|kind| {
                    let selected = self.proxy_kind == kind;
                    let label = match kind {
                        ProxyKind::Socks5 => tr(cx, "SOCKS5"),
                        ProxyKind::HttpConnect => tr(cx, "HTTP CONNECT"),
                    };
                    button(
                        SharedString::from(format!("db-proxy-kind-{}", kind.key())),
                        SharedString::from(label),
                        if selected {
                            ButtonVariant::Primary
                        } else {
                            ButtonVariant::Secondary
                        },
                        cx,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.proxy_kind = kind;
                        cx.notify();
                    }))
                }));
            let mut proxy_block = v_flex().w_full().gap(form_gap(cx)).child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.lg)
                    .child(field_label(tr(cx, "代理"), cx))
                    .child(proxy_toggle)
                    .child(proxy_kind_switch),
            );
            if self.proxy_enabled {
                proxy_block = proxy_block
                    .child(field_row(cx, tr(cx, "代理主机"), &self.proxy_host))
                    .child(field_row(cx, tr(cx, "代理端口"), &self.proxy_port))
                    .child(field_row(cx, tr(cx, "代理用户"), &self.proxy_user))
                    .child(field_row(cx, tr(cx, "代理口令"), &self.proxy_password))
                    .child(field_hint(
                        tr(cx, "只有需要认证的代理才要填用户名与口令。"),
                        cx,
                    ));
            }

            // AWS SSM：命令全部交给用户本机已装的 aws CLI，本项目不存任何 AWS 凭据。
            let ssm_toggle = button(
                "db-ssm-toggle",
                if self.ssm_enabled {
                    tr(cx, "经 AWS SSM")
                } else {
                    tr(cx, "不用 SSM")
                },
                if self.ssm_enabled {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Secondary
                },
                cx,
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.ssm_enabled = !this.ssm_enabled;
                cx.notify();
            }));
            let mut ssm_block = v_flex().w_full().gap(form_gap(cx)).child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.lg)
                    .child(field_label(tr(cx, "AWS SSM"), cx))
                    .child(ssm_toggle),
            );
            if self.ssm_enabled {
                ssm_block = ssm_block
                    .child(field_row(cx, tr(cx, "目标实例"), &self.ssm_instance))
                    .child(field_row(cx, tr(cx, "远端主机"), &self.ssm_remote_host))
                    .child(field_row(cx, tr(cx, "区域"), &self.ssm_region))
                    .child(field_row(cx, tr(cx, "AWS profile"), &self.ssm_profile))
                    .child(field_hint(
                        tr(
                            cx,
                            "需要本机已装 aws CLI 并完成过 aws sso login / configure。",
                        ),
                        cx,
                    ));
            }

            // 传输档案：一份可被多条连接引用的三档配置。选中之后，下面三档**就是这份
            // 档案的内容**（而不是连接自己的）——保存在 `save()` 里原样写回档案。
            let mut profile_options = vec![(
                SharedString::default(),
                SharedString::from(tr(cx, "自定义（用下面的配置）")),
            )];
            profile_options.extend(self.tunnel_profiles.iter().map(|profile| {
                let label = match profile.kind() {
                    Some(kind) => format!("{} · {}", profile.name, transport_kind_label(kind, cx)),
                    None => profile.name.clone(),
                };
                (
                    SharedString::from(profile.id.clone()),
                    SharedString::from(label),
                )
            }));
            let profile_entity = cx.entity().downgrade();
            let profile_select = dropdown_select(
                "db-tunnel-profile",
                profile_options,
                SharedString::from(self.tunnel_profile.clone().unwrap_or_default()),
                move |value: SharedString, window, cx| {
                    let id = value.to_string();
                    let _ = profile_entity.update(cx, |this, cx| {
                        this.select_tunnel_profile((!id.is_empty()).then_some(id), window, cx);
                    });
                },
                cx,
            );
            let mut profile_block = v_flex().w_full().gap(form_gap(cx)).child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.lg)
                    .child(field_label(tr(cx, "传输档案"), cx))
                    .child(div().flex_1().min_w_0().child(profile_select)),
            );
            match self.tunnel_profile.is_some() {
                true => {
                    profile_block = profile_block
                        .child(field_row(cx, tr(cx, "档案名称"), &self.profile_name))
                        .child(field_hint(
                            tr(
                                cx,
                                "下面的三档就是这份档案的内容；保存连接时会一并存回档案。",
                            ),
                            cx,
                        ))
                        .child(
                            h_flex().w_full().gap(tokens.spacing.xs).child(
                                button(
                                    "db-tunnel-profile-delete",
                                    tr(cx, "删除这份档案"),
                                    ButtonVariant::Secondary,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.remove_tunnel_profile(window, cx),
                                )),
                            ),
                        );
                }
                false => {
                    // 「自定义」态才给「存为新档案」：选中档案时再存一份，等于把档案复制一遍，
                    // 想要那份语义请先切到自定义。少一个按钮，少一种误解。
                    profile_block = profile_block
                        .child(field_row(cx, tr(cx, "档案名称"), &self.profile_name))
                        .child(
                            h_flex().w_full().gap(tokens.spacing.xs).child(
                                button(
                                    "db-tunnel-profile-add",
                                    tr(cx, "存为新档案"),
                                    ButtonVariant::Secondary,
                                    cx,
                                )
                                .on_click(cx.listener(
                                    |this, _, window, cx| this.add_tunnel_profile(window, cx),
                                )),
                            ),
                        );
                }
            }

            // 三档是**互斥**的：同时开着只说优先级，不说「叠加」——界面上说清楚，
            // 免得用户以为「SSH 出去再走代理」会生效。
            let active_count = usize::from(self.ssh_enabled)
                + usize::from(self.proxy_enabled)
                + usize::from(self.ssm_enabled);
            let overlap_hint = (active_count > 1).then(|| {
                field_hint(
                    tr(
                        cx,
                        "同时启用多档时，只有优先级最高的一档生效：SSM → SSH → 代理。",
                    ),
                    cx,
                )
            });

            v_flex()
                .w_full()
                .gap(form_gap(cx))
                .child(db_section_header(DuckIcon::Globe, "TRANSPORT", cx))
                .child(profile_block)
                .child(ssh_block)
                .child(proxy_block)
                .child(ssm_block)
                .children(overlap_hint)
        });

        // 头部与页签常驻、正文独立滚动：长表单（SSH 展开）不会把脚注挤出弹窗。
        //
        // 页签显隐对齐 dbx：TLS / 隧道两页签**仅网络引擎**有；下标超出可用范围
        // （换引擎 / 旧档案）时夹回第一页，不在渲染里改状态。
        let available_tabs: Vec<&str> = if network {
            FORM_TABS.to_vec()
        } else {
            FORM_TABS
                .iter()
                .copied()
                .filter(|tab| !matches!(*tab, "TLS/SSL" | "隧道/代理"))
                .collect()
        };
        let active_tab = self.tab.min(available_tabs.len() - 1);
        let tab_strip = {
            let entity = cx.entity().downgrade();
            segmented_tabs(
                "db-edit-tabs",
                available_tabs
                    .iter()
                    .map(|label| SharedString::from(tr(cx, label))),
                active_tab,
                move |index, _window, cx| {
                    let _ = entity.update(cx, |this, cx| {
                        this.tab = index;
                        cx.notify();
                    });
                },
                cx,
            )
        };
        // 每个页签装什么（`network` 为假时 TLS / 隧道两段本来就是 `None`）。
        let tab_body: AnyElement = match available_tabs[active_tab] {
            "TLS/SSL" => v_flex()
                .w_full()
                .gap(tokens.spacing.lg)
                .children(tls_section)
                .into_any_element(),
            "隧道/代理" => v_flex()
                .w_full()
                .gap(tokens.spacing.lg)
                .children(transport_section)
                .into_any_element(),
            "高级" => v_flex()
                .w_full()
                .gap(tokens.spacing.lg)
                .child(query_section)
                .child(self.hooks_section(cx))
                .into_any_element(),
            // 「连接信息」：环境 + 驱动提示 + 主机 / 模式族 / 分组 + 认证方式。
            _ => v_flex()
                .w_full()
                .gap(tokens.spacing.lg)
                .child(
                    h_flex()
                        .w_full()
                        .items_start()
                        .gap(tokens.spacing.lg)
                        .child(field_label(tr(cx, "环境"), cx))
                        .child(
                            v_flex()
                                .gap(tokens.spacing.xxs)
                                .child(env_pills)
                                .child(field_hint(tr(cx, "标明此连接指向的环境。"), cx)),
                        ),
                )
                .children(driver_notice)
                .child(server_fields)
                .children(auth_section)
                .into_any_element(),
        };
        div()
            .w_full()
            .min_h_0()
            .flex()
            .flex_col()
            .child(
                div()
                    .w_full()
                    .flex_none()
                    .px(tokens.spacing.lg)
                    .pt(tokens.spacing.sm)
                    .pb(tokens.spacing.xs)
                    .border_b_1()
                    .border_color(tokens.colors.border)
                    .child(header)
                    .child(div().mt(tokens.spacing.sm).child(tab_strip)),
            )
            .child(
                div()
                    .id("db-edit-body")
                    .w_full()
                    .min_h_0()
                    .max_h(px(EDIT_DIALOG_BODY_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .px(tokens.spacing.lg)
                    .py(tokens.spacing.md)
                    .child(tab_body),
            )
    }
}

/// 第三方档案的失败原因 → 给用户看的说明。
pub(super) fn import_error_message(error: duck_db::import::ImportError, cx: &App) -> &'static str {
    use duck_db::import::ImportError;
    match error {
        ImportError::NotJson => tr(cx, "文件内容不是合法的 JSON。"),
        ImportError::Unrecognised => tr(
            cx,
            "认不出这个连接档案；支持本应用导出的档案、DBeaver 的 data-sources.json 与 TablePlus 的 .tableplusconnection。",
        ),
        ImportError::NoSupportedConnections => tr(
            cx,
            "档案里没有本应用支持的引擎（目前支持 SQLite / PostgreSQL / MySQL）。",
        ),
    }
}

impl VisibleDatabasesView {
    /// （已勾选数，总库数）—— 标题行与「确定」的清空判定共用。
    fn counts(&self) -> (usize, usize) {
        (self.selected.len(), self.names.len())
    }

    /// 按清单顺序排好的勾选结果（白名单存的是稳定序，不是勾选序）。
    fn sorted_selection(&self) -> Vec<String> {
        self.names
            .iter()
            .filter(|name| self.selected.contains(*name))
            .cloned()
            .collect()
    }
}

impl Render for VisibleDatabasesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended().clone();
        let icon_sm = extended.icon.sm;
        let query = self.search.read(cx).value().trim().to_lowercase();

        let (selected_len, total) = self.counts();
        let summary = if total == 0 {
            tr(cx, "没有可勾选的库。").to_owned()
        } else if selected_len == total {
            trf(
                cx,
                "已勾选全部 {} 个库；保存后侧栏不再过滤。",
                &[&total.to_string()],
            )
            .to_owned()
        } else {
            trf(
                cx,
                "已勾选 {} / {} 个库；保存后侧栏只显示它们。",
                &[&selected_len.to_string(), &total.to_string()],
            )
            .to_owned()
        };

        let mut list = v_flex().w_full().gap(tokens.spacing.xxs);
        let mut matched = 0usize;
        for name in &self.names {
            if !query.is_empty() && !name.to_lowercase().contains(&query) {
                continue;
            }
            matched += 1;
            let selected = self.selected.contains(name);
            let row_name = name.clone();
            let row = h_flex()
                .id(SharedString::from(format!("db-visible-row-{name}")))
                .w_full()
                .items_center()
                .gap(tokens.spacing.sm)
                .px(tokens.spacing.sm)
                .py(tokens.spacing.xs)
                .rounded(tokens.radius.sm)
                .hover(move |style| style.bg(extended.colors.hover_tint))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if !this.selected.remove(&row_name) {
                        this.selected.insert(row_name.clone());
                    }
                    cx.notify();
                }))
                .child(if selected {
                    Icon::new(DuckIcon::Check)
                        .size(icon_sm)
                        .text_color(tokens.colors.accent)
                        .into_any_element()
                } else {
                    // 占位与勾选图标同宽，行文字左缘不因勾选状态横跳。
                    div().w(icon_sm).into_any_element()
                })
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .child(body_text(name.clone(), cx)),
                );
            list = list.child(row);
        }
        if matched == 0 {
            list = list.child(meta_text(tr(cx, "没有匹配的库。"), cx));
        }

        v_flex()
            .w_full()
            .min_h_0()
            .gap(tokens.spacing.sm)
            .child(meta_text(summary, cx))
            .child(
                div().w_full().child(
                    Input::new(&self.search)
                        .h(theme.density().control)
                        .text_size(tokens.typography.sm.size)
                        .w_full(),
                ),
            )
            .child(
                div()
                    .id("db-visible-list")
                    .w_full()
                    .min_h_0()
                    .max_h(px(VISIBLE_DBS_LIST_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .child(list),
            )
    }
}

/// 认证方式的中文名（下拉选项用；判据与持久化一律走 [`AuthMode::wire_name`]）。
fn auth_mode_label(mode: AuthMode, cx: &App) -> &'static str {
    match mode {
        AuthMode::Password => tr(cx, "密码"),
        AuthMode::None_ => tr(cx, "无认证"),
        AuthMode::ConnectionString => tr(cx, "连接串"),
        AuthMode::ApiKey => tr(cx, "API Key"),
    }
}

/// SSH 认证方式的中文名（下拉选项用）。
fn ssh_auth_label(method: SshAuthMethod, cx: &App) -> &'static str {
    match method {
        SshAuthMethod::Password => tr(cx, "口令"),
        SshAuthMethod::Key => tr(cx, "私钥文件"),
        SshAuthMethod::KeyAndPassword => tr(cx, "私钥 + 口令"),
        SshAuthMethod::Agent => tr(cx, "ssh-agent"),
    }
}

/// 「标签 + 输入框」一行（表单专用；标签列宽与设置页的 `option_row` 对齐）。
pub(super) fn field_row(
    cx: &App,
    label: impl Into<SharedString>,
    input: &Entity<InputState>,
) -> gpui::Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    h_flex()
        .w_full()
        .items_center()
        .gap(tokens.spacing.lg)
        .child(field_label(label, cx))
        .child(
            div().flex_1().min_w_0().child(
                Input::new(input)
                    .h(theme.density().control)
                    .text_size(tokens.typography.sm.size)
                    .w_full(),
            ),
        )
}

/// [`field_row`] 的必填变体：标签落危险色（与「标红的字段为必填项」呼应）。
pub(super) fn field_row_required(
    cx: &App,
    label: impl Into<SharedString>,
    input: &Entity<InputState>,
) -> gpui::Div {
    let theme = active_theme(cx);
    let tokens = theme.tokens();
    h_flex()
        .w_full()
        .items_center()
        .gap(tokens.spacing.lg)
        .child(field_label(label, cx).text_color(tokens.colors.destructive))
        .child(
            div().flex_1().min_w_0().child(
                Input::new(input)
                    .h(theme.density().control)
                    .text_size(tokens.typography.sm.size)
                    .w_full(),
            ),
        )
}

/// 环境标签的语义色圆点：生产=危险、预发布=警告、测试=强调、开发=信息、无=二级文字。
pub(super) fn env_dot_color(tag: EnvTag, cx: &App) -> Hsla {
    let theme = active_theme(cx);
    let colors = theme.tokens().colors;
    let extended = theme.extended().colors;
    match tag {
        EnvTag::Prod => colors.destructive,
        EnvTag::Staging => extended.warning,
        EnvTag::Test => extended.accent_text,
        EnvTag::Dev => extended.info,
        EnvTag::None => colors.muted_foreground,
    }
}

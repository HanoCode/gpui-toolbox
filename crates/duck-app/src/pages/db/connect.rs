//! 连接管理：驱动选择器（`DriverPickerView`）与新建 / 编辑连接表单（`EditFormView`）
//! —— 两个**独立视图**（弹窗的正文就是它们），外加档案导入的失败文案。
//!
//! 两个类型仍定义在 `mod.rs`（与 `pages::terminal` 同一口径：类型留模块根，子模块只放
//! `impl` 块），所以这里的 `impl` 能直接读写它们的私有字段，无需给字段加可见性。

use super::*;

impl EditFormView {
    /// 新建态：按引擎把可预判的字段直接填好（对齐 dbx 的 `defaultForm()`），少让用户手打；
    /// SQLite 走本地文件，三样都不填。
    pub(super) fn new(
        page: WeakEntity<DatabasePage>,
        kind: DatabaseKind,
        profiles: Vec<TunnelProfile>,
        window: &mut Window,
        cx: &mut Context<DatabasePage>,
    ) -> Self {
        // 预填对齐 dbx 的 `defaultForm()`（`ConnectionDialog.vue`）：host=`127.0.0.1`、
        // 端口=引擎缺省数字、用户=引擎惯用账号；SQLite 走本地文件，三样都不填。
        // host 输入框在网络引擎下恒被预填，占位符不出现，留空即可。
        let host = text_input(window, cx, "");
        let port = text_input(window, cx, tr(cx, "端口"));
        let user = text_input(window, cx, tr(cx, "用户名"));
        if kind.needs_host() {
            host.update(cx, |state, cx| state.set_value("127.0.0.1", window, cx));
            user.update(cx, |state, cx| {
                state.set_value(default_user(kind), window, cx)
            });
            if let Some(default_port) = kind.default_port() {
                port.update(cx, |state, cx| {
                    state.set_value(default_port.to_string(), window, cx)
                });
            }
        }
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
            tls: TlsMode::Disable,
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
            ssh_enabled: false,
            ssh_host: text_input(window, cx, tr(cx, "跳板机主机")),
            ssh_port: text_input(window, cx, tr(cx, "留空用 22")),
            ssh_user: text_input(window, cx, tr(cx, "跳板机用户")),
            ssh_remote_host: text_input(window, cx, tr(cx, "留空表示跳板机本机")),
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
        let mut form = Self::new(page, cfg.kind, profiles, window, cx);
        form.id = Some(cfg.id.clone());
        form.tls = cfg.tls;
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
        Ok(ConnectionConfig {
            id: self.id.clone().unwrap_or_else(|| {
                format!("conn-{}", id_suffix(self.name.read(cx).value().as_ref()))
            }),
            name: self.name.read(cx).value().trim().to_owned(),
            kind: self.kind,
            host: self.host.read(cx).value().trim().to_owned(),
            port,
            user: self.user.read(cx).value().trim().to_owned(),
            database: self.database.read(cx).value().trim().to_owned(),
            tls: self.tls,
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
        let password_text = self.password.read(cx).value().trim().to_owned();
        let db = if password_text.is_empty() {
            load_password(&cfg.id).unwrap_or(None)
        } else {
            Some(password_text)
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
        Ok((cfg, Credentials { db, ssh, proxy }))
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
            .to_string()
        } else {
            tr(cx, "确定删除这份传输档案吗？").to_owned()
        };
        if !duck_platform::confirm(tr(cx, "删除传输档案"), &message) {
            return;
        }
        let _ = self
            .page
            .update(cx, |page, cx| page.delete_tunnel_profile(&profile_id, cx));
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
            _subscription: subscription,
        }
    }

    /// 选中一张卡片：M1 支持的引擎关掉 picker、进详情表单；其余只提示。
    pub(super) fn choose(&self, kind: Option<DatabaseKind>, window: &mut Window, cx: &mut App) {
        let Some(kind) = kind else {
            notify::info(tr(cx, "该数据库类型的支持即将到来。"), window, cx);
            return;
        };
        window.close_dialog(cx);
        let _ = self.page.update(cx, |page, cx| {
            page.start_new_connection(kind, window, cx);
        });
    }
}

impl Render for DriverPickerView {
    /// 头部（提示 + 搜索框）常驻；下方按分区排卡片网格，整体独立滚动。
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let hover_bg = tokens.colors.muted;

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

        let visible: Vec<&PickerDriver> = PICKER_DRIVERS
            .iter()
            .filter(|driver| driver_matches_query(driver, &self.query))
            .collect();

        let mut body = v_flex().w_full().min_h_0().gap(tokens.spacing.xs);
        for section in PICKER_SECTIONS {
            let cards: Vec<&PickerDriver> = visible
                .iter()
                .copied()
                .filter(|driver| driver.section == *section)
                .collect();
            if cards.is_empty() {
                continue;
            }
            let mut grid = div()
                .grid()
                .grid_cols(PICKER_COLUMNS as u16)
                .gap(tokens.spacing.xs);
            for driver in cards {
                let name = SharedString::from(driver.name);
                let hint = SharedString::from(driver.hint);
                let logo = driver.logo;
                let kind = driver.kind;
                grid = grid.child(
                    h_flex()
                        .id(SharedString::from(format!("db-driver-{}", driver.id)))
                        .min_w_0()
                        .items_center()
                        .gap(tokens.spacing.sm)
                        .px(tokens.spacing.sm)
                        .py(tokens.spacing.xs)
                        .rounded(tokens.radius.md)
                        .border_1()
                        .border_color(tokens.colors.border)
                        .bg(tokens.colors.background)
                        .cursor_pointer()
                        .hover(move |style| style.bg(hover_bg))
                        .child(logo.element(px(PICKER_LOGO_SIZE)))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(tokens.typography.sm.size)
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .truncate()
                                        .text_size(tokens.typography.xs.size)
                                        .text_color(tokens.colors.muted_foreground)
                                        .child(hint),
                                ),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.choose(kind, window, cx);
                        })),
                );
            }
            body = body
                .child(caption_heading(section.label(cx), cx))
                .child(grid);
        }
        if visible.is_empty() {
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
                div()
                    .id("db-picker-grid")
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .max_h(px(PICKER_GRID_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .px(tokens.spacing.lg)
                    .py(tokens.spacing.sm)
                    .child(body),
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
        let network = self.kind.needs_host();

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
            .child(kind_logo(self.kind).element(px(EDIT_LOGO_SIZE)))
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
                            &[kind_label(self.kind)],
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
                .child(field_hint(database_hint(self.kind, cx), cx))
                .child(field_row(cx, tr(cx, "分组"), &self.group));
        } else {
            server_fields = server_fields
                .child(field_row(cx, tr(cx, "数据库文件"), &self.database))
                .child(field_hint(database_hint(self.kind, cx), cx))
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

        // ── AUTHENTICATION：用户名（必填）+ 口令（掩码）。仅网络引擎。 ──
        let auth_section = network.then(|| {
            v_flex()
                .w_full()
                .gap(form_gap(cx))
                .child(db_section_header(DuckIcon::User, "AUTHENTICATION", cx))
                .child(field_row_required(cx, tr(cx, "用户名"), &self.user))
                .child(field_row(cx, tr(cx, "密码"), &self.password))
        });

        // ── TRANSPORT：SSL 模式 + SSH 隧道。仅网络引擎。 ──
        let transport_section = network.then(|| {
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
                ssh_block = ssh_block
                    .child(field_row(cx, tr(cx, "跳板机"), &self.ssh_host))
                    .child(field_row(cx, tr(cx, "SSH 端口"), &self.ssh_port))
                    .child(field_row(cx, tr(cx, "SSH 用户"), &self.ssh_user))
                    .child(field_row(cx, tr(cx, "转发目标"), &self.ssh_remote_host))
                    .child(field_row(cx, tr(cx, "SSH 口令"), &self.ssh_password));
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
                .child(
                    h_flex()
                        .w_full()
                        .items_center()
                        .gap(tokens.spacing.lg)
                        .child(field_label(tr(cx, "SSL 模式"), cx))
                        .child(tls_switch),
                )
                .child(ssh_block)
                .child(proxy_block)
                .child(ssm_block)
                .children(overlap_hint)
        });

        // 头部常驻、正文独立滚动：长表单（SSH 展开）不会把脚注挤出弹窗。
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
                    .py(tokens.spacing.sm)
                    .border_b_1()
                    .border_color(tokens.colors.border)
                    .child(header),
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
                    .child(
                        v_flex()
                            .w_full()
                            .gap(tokens.spacing.lg)
                            .child(
                                h_flex()
                                    .w_full()
                                    .items_start()
                                    .gap(tokens.spacing.lg)
                                    .child(field_label(tr(cx, "环境"), cx))
                                    .child(
                                        v_flex().gap(tokens.spacing.xxs).child(env_pills).child(
                                            field_hint(tr(cx, "标明此连接指向的环境。"), cx),
                                        ),
                                    ),
                            )
                            .child(server_fields)
                            .child(query_section)
                            .children(auth_section)
                            .children(transport_section)
                            .child(self.hooks_section(cx)),
                    ),
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

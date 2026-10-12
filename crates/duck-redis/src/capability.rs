//! 分级锁写闸门：把「连接档案 + 解锁窗口 + ACL 探测」归一成「这个动作能不能做」。
//!
//! 这是本模块的安全基座（`docs/redis-design.md` §4.4）。**全部是纯函数**，
//! 不触碰网络与时间——当前时刻由调用方把「解锁剩余时长」传进来，
//! 因此可以用普通 `#[test]` 把每条规则钉死（对齐 `duck-db::sqlintel` 的纯函数纪律）。
//!
//! 三层输入：
//! 1. 连接的 **只读开关**（档案里的 `read_only`）；
//! 2. **生产档默认锁写**（`EnvTag::Prod` 且不在解锁窗口内）；
//! 3. **ACL 探测结果**（服务端用户本身没有写权限 → `StrictReadOnly`）。
//!
//! 三者的优先级是「越严格越优先」：ACL 只读是服务端事实，压过用户在档案里选的
//! ReadWrite；只读开关是用户显式意图，压过环境标签。见 [`Gate::mode`]。

use crate::config::EnvTag;

/// 生产档一次解锁的时长（秒）。
///
/// 15 分钟：够干完一件明确的事，又不至于让人把解锁忘在那过夜。
/// **常量定义在核心里**，UI 只读它，不在页面里写裸值。
pub const WRITE_UNLOCK_SECS: u64 = 15 * 60;

/// 当前访问模式。由 [`Gate::mode`] 依据三项输入推导，不做持久化。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    /// 读写全开。
    ReadWrite,
    /// 安全模式：连接只读开关打开，或生产档在解锁窗口之外。
    SafeMode,
    /// 严格只读：服务端 ACL 用户本身没有写权限（客户端拦不拦都一样写不进去）。
    StrictReadOnly,
}

impl AccessMode {
    /// 稳定 wire 名（日志 / 测试断言用）。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::ReadWrite => "read-write",
            Self::SafeMode => "safe-mode",
            Self::StrictReadOnly => "strict-read-only",
        }
    }

    /// 是否允许任何写操作。
    #[must_use]
    pub const fn allows_write(self) -> bool {
        matches!(self, Self::ReadWrite)
    }
}

/// 一个用户可见的动作。UI 拿它去问闸门「能不能做」，而不是自己判断环境。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// 读键（`GET` / `HGETALL` / `SCAN` / `TTL` / `TYPE` …）。
    ReadKey,
    /// 扫描键空间（`SCAN` / `DBSIZE`）。
    ScanKeys,
    /// 读服务端信息（`INFO` / `SLOWLOG GET` / `MEMORY USAGE` …）。
    ReadServerInfo,
    /// 读配置（`CONFIG GET`）。
    ReadConfig,
    /// 写键值 / 改集合元素。
    WriteKey,
    /// 删键。
    DeleteKey,
    /// 重命名键。
    RenameKey,
    /// 设置 / 移除过期时间。
    SetTtl,
    /// 清空当前库（`FLUSHDB`）。
    FlushDb,
    /// 清空所有库（`FLUSHALL`）。
    FlushAll,
    /// 写配置（`CONFIG SET` / `CONFIG REWRITE`）。
    ConfigWrite,
    /// 装载 / 删除脚本与函数（`SCRIPT LOAD` / `FUNCTION LOAD`）。
    ScriptLoad,
    /// 集群管理（`CLUSTER *` / `FAILOVER` / `REPLICAOF`）。
    ClusterAdmin,
    /// 服务端运行态管理（`CLIENT KILL` / `CLIENT PAUSE` / `SLOWLOG RESET`）。
    ///
    /// 与 [`Self::ConfigWrite`] 分开：改配置是「改将来的行为」，
    /// 而杀连接 / 清日志是**对正在跑的服务端下手**，权限口径不该混在一起。
    ServerAdmin,
    /// 向频道发布消息（`PUBLISH`）。
    PubSubPublish,
    /// 执行任意原始命令（redis-cli 终端）。
    RawCommand,
}

impl Capability {
    /// 全部动作（UI 遍历 / 测试覆盖用）。
    pub const ALL: &'static [Self] = &[
        Self::ReadKey,
        Self::ScanKeys,
        Self::ReadServerInfo,
        Self::ReadConfig,
        Self::WriteKey,
        Self::DeleteKey,
        Self::RenameKey,
        Self::SetTtl,
        Self::FlushDb,
        Self::FlushAll,
        Self::ConfigWrite,
        Self::ScriptLoad,
        Self::ClusterAdmin,
        Self::ServerAdmin,
        Self::PubSubPublish,
        Self::RawCommand,
    ];

    /// 是否为写动作（受访问模式约束）。
    #[must_use]
    pub const fn is_write(self) -> bool {
        !matches!(
            self,
            Self::ReadKey | Self::ScanKeys | Self::ReadServerInfo | Self::ReadConfig
        )
    }

    /// 是否为「特别危险」的动作。
    ///
    /// 危险动作在**通过闸门之后**还要过二次确认（见 [`crate::writegate`]）；
    /// 这一位只用来决定 UI 上是否加警示标记，不参与是否放行的判定。
    #[must_use]
    pub const fn is_dangerous(self) -> bool {
        matches!(
            self,
            Self::FlushDb | Self::FlushAll | Self::ClusterAdmin | Self::RawCommand
        )
    }
}

/// 拒绝对某个动作的原因。UI 用它生成「灰显 + 说明原因」的文案。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// 连接档案里选了「只读」。
    ConnectionReadOnly,
    /// 处于锁写状态（生产档默认锁写，或档案显式选了「锁定」）且不在解锁窗口内。
    WriteLocked,
    /// 服务端 ACL 用户没有写权限。
    AclDenied,
}

impl DenyReason {
    /// 稳定标识（**英文**：内核不依赖 i18n，界面文案由 `duck-app` 用 `tr` 组织）。
    #[must_use]
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::ConnectionReadOnly => "connection_read_only",
            Self::WriteLocked => "write_locked",
            Self::AclDenied => "acl_denied",
        }
    }
}

/// 闸门的输入，由调用方每次求值时装配。
///
/// 不持连接、不读时钟——`unlock_remaining` 由 UI 侧的解锁计时器算好传进来，
/// 所以本类型可以自由构造用于测试。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gate {
    /// 档案里的只读选择（`WritePolicy::ReadOnly`）。
    read_only: bool,
    /// 生产档默认锁写是否生效（只在策略为「跟随标签」且档位为生产时为真）。
    production_lock: bool,
    /// 档案里显式选了「锁定」（无论环境标签都锁，限时可解锁）。
    force_lock: bool,
    /// ACL 探测结果：服务端用户是否只读。
    acl_read_only: bool,
    /// 解锁窗口的剩余时长；`None` = 不在解锁窗口内。仅锁写状态下有意义。
    unlock_remaining: Option<std::time::Duration>,
}

impl Gate {
    /// 装配闸门输入（按「跟随标签」语义：生产档默认锁写）。
    #[must_use]
    pub const fn new(
        read_only: bool,
        env_tag: EnvTag,
        acl_read_only: bool,
        unlock_remaining: Option<std::time::Duration>,
    ) -> Self {
        Self {
            read_only,
            production_lock: env_tag.is_production(),
            force_lock: false,
            acl_read_only,
            unlock_remaining,
        }
    }

    /// 由档案的写入策略与 ACL 探测结果装配。
    ///
    /// 策略到锁位的换算：
    /// - `FollowTag`：锁不锁由环境标签决定（生产档默认锁）；
    /// - `Allow`：用户显式放行，**压过生产档的默认锁**；
    /// - `Lock`：无论环境标签都锁（限时可解锁）；
    /// - `ReadOnly`：连接本身只读。
    #[must_use]
    pub fn from_policy(
        policy: crate::config::WritePolicy,
        env_tag: EnvTag,
        acl_read_only: bool,
        unlock_remaining: Option<std::time::Duration>,
    ) -> Self {
        Self {
            read_only: policy.is_read_only(),
            production_lock: policy.follows_tag() && env_tag.is_production(),
            force_lock: policy.forces_lock(),
            acl_read_only,
            unlock_remaining,
        }
    }

    /// 由档案与探测结果装配的常见形态（未解锁，按跟随标签处理）。
    #[must_use]
    pub const fn from_config(read_only: bool, env_tag: EnvTag, acl_read_only: bool) -> Self {
        Self::new(read_only, env_tag, acl_read_only, None)
    }

    /// 推导当前访问模式。
    ///
    /// 优先级「越严格越优先」：
    /// 1. ACL 只读 —— 服务端事实，压过用户的一切选择（选 ReadWrite 也写不进去）；
    /// 2. 只读策略 —— 用户显式意图，压过环境标签；
    /// 3. 锁写（生产档默认锁，或显式选「锁定」）且不在解锁窗口。
    #[must_use]
    pub fn mode(&self) -> AccessMode {
        if self.acl_read_only {
            return AccessMode::StrictReadOnly;
        }
        if self.read_only {
            return AccessMode::SafeMode;
        }
        if self.lock_engaged() {
            return AccessMode::SafeMode;
        }
        AccessMode::ReadWrite
    }

    /// 解锁窗口是否生效（只在锁写状态下有意义；可写档位永远返回 `None`）。
    #[must_use]
    pub fn unlock_active(&self) -> Option<std::time::Duration> {
        if !(self.production_lock || self.force_lock) {
            return None;
        }
        self.unlock_remaining.filter(|left| !left.is_zero())
    }

    /// 当前是否处于「限时可解锁」的锁写状态（状态栏据此决定要不要显示「解锁」按钮）。
    #[must_use]
    pub fn is_locked(&self) -> bool {
        !self.read_only && !self.acl_read_only && self.lock_engaged()
    }

    /// 锁写位：生产档默认锁 / 显式锁定，且不在解锁窗口内。
    fn lock_engaged(&self) -> bool {
        (self.production_lock || self.force_lock) && self.unlock_active().is_none()
    }

    /// 这个动作能不能做。
    pub fn check(&self, capability: Capability) -> Result<(), DenyReason> {
        if !capability.is_write() {
            return Ok(());
        }
        match self.mode() {
            AccessMode::ReadWrite => Ok(()),
            // ACL 只读最优先——报它，用户才知道「解锁也没用，得换账号」。
            AccessMode::StrictReadOnly => Err(DenyReason::AclDenied),
            AccessMode::SafeMode => {
                if self.read_only {
                    Err(DenyReason::ConnectionReadOnly)
                } else {
                    Err(DenyReason::WriteLocked)
                }
            }
        }
    }

    /// [`Self::check`] 的布尔形态。
    #[must_use]
    pub fn can(&self, capability: Capability) -> bool {
        self.check(capability).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 非生产档 + 未开只读 + ACL 可写 → 全开。
    #[test]
    fn plain_connection_is_read_write() {
        let gate = Gate::from_config(false, EnvTag::Dev, false);
        assert_eq!(gate.mode(), AccessMode::ReadWrite);
        for cap in Capability::ALL {
            assert!(gate.can(*cap), "{cap:?} 应放行");
        }
    }

    /// 只读开关：写动作全拒，读动作照放。
    #[test]
    fn read_only_switch_blocks_writes_only() {
        let gate = Gate::from_config(true, EnvTag::Dev, false);
        assert_eq!(gate.mode(), AccessMode::SafeMode);
        assert_eq!(
            gate.check(Capability::WriteKey),
            Err(DenyReason::ConnectionReadOnly)
        );
        assert_eq!(
            gate.check(Capability::FlushAll),
            Err(DenyReason::ConnectionReadOnly)
        );
        assert!(gate.can(Capability::ReadKey));
        assert!(gate.can(Capability::ScanKeys));
        assert!(gate.can(Capability::ReadServerInfo));
        assert!(gate.can(Capability::ReadConfig));
    }

    /// 生产档默认锁写；解锁窗口内放行；窗口过期后重新上锁。
    #[test]
    fn production_locks_writes_until_unlocked() {
        let locked = Gate::from_config(false, EnvTag::Prod, false);
        assert!(locked.is_locked());
        assert_eq!(locked.mode(), AccessMode::SafeMode);
        assert_eq!(
            locked.check(Capability::DeleteKey),
            Err(DenyReason::WriteLocked)
        );

        let unlocked = Gate::new(
            false,
            EnvTag::Prod,
            false,
            Some(Duration::from_secs(WRITE_UNLOCK_SECS)),
        );
        assert_eq!(unlocked.mode(), AccessMode::ReadWrite);
        assert!(unlocked.can(Capability::DeleteKey));
        assert!(!unlocked.is_locked());

        // 剩余 0 秒 = 窗口刚刚到期，必须回到锁写，不能因为 Some(0) 就放行。
        let expired = Gate::new(false, EnvTag::Prod, false, Some(Duration::ZERO));
        assert_eq!(expired.mode(), AccessMode::SafeMode);
        assert_eq!(
            expired.check(Capability::WriteKey),
            Err(DenyReason::WriteLocked)
        );
    }

    /// 写入策略：显式允许压过生产锁；显式锁定在开发档也锁；只读是最直接的一档。
    #[test]
    fn write_policy_drives_the_gate() {
        use crate::config::WritePolicy;

        // Allow：生产档也放行。
        let allowed = Gate::from_policy(WritePolicy::Allow, EnvTag::Prod, false, None);
        assert_eq!(allowed.mode(), AccessMode::ReadWrite);
        assert!(!allowed.is_locked());

        // Lock：开发档也锁，但限时可解锁。
        let locked = Gate::from_policy(WritePolicy::Lock, EnvTag::Dev, false, None);
        assert_eq!(locked.mode(), AccessMode::SafeMode);
        assert!(locked.is_locked(), "显式锁定在非生产档也应显示解锁按钮");
        let unlocked = Gate::from_policy(
            WritePolicy::Lock,
            EnvTag::Dev,
            false,
            Some(Duration::from_secs(60)),
        );
        assert_eq!(unlocked.mode(), AccessMode::ReadWrite);

        // ReadOnly：只读原因。
        let ro = Gate::from_policy(WritePolicy::ReadOnly, EnvTag::Dev, false, None);
        assert_eq!(
            ro.check(Capability::WriteKey),
            Err(DenyReason::ConnectionReadOnly)
        );
        assert!(!ro.is_locked(), "只读不是锁写——不该显示解锁按钮");

        // FollowTag：跟 from_config 同一语义。
        let follow = Gate::from_policy(WritePolicy::FollowTag, EnvTag::Prod, false, None);
        assert_eq!(follow.mode(), AccessMode::SafeMode);
    }

    /// 解锁窗口只对生产档 / 显式锁写有意义：其余档位传了也不生效（防止调用方误传）。
    #[test]
    fn unlock_window_only_applies_to_production() {
        let gate = Gate::new(false, EnvTag::Dev, false, Some(Duration::from_secs(60)));
        assert_eq!(gate.unlock_active(), None);
        assert_eq!(gate.mode(), AccessMode::ReadWrite);
    }

    /// ACL 只读优先级最高：压过只读策略与生产档，且报的原因是「换账号也没用」的那条。
    #[test]
    fn acl_read_only_outranks_everything() {
        let acl = Gate::new(true, EnvTag::Prod, true, Some(Duration::from_secs(600)));
        assert_eq!(acl.mode(), AccessMode::StrictReadOnly);
        assert_eq!(acl.check(Capability::WriteKey), Err(DenyReason::AclDenied));
        assert!(acl.can(Capability::ReadKey));
        assert!(!acl.is_locked(), "ACL 只读不该显示解锁按钮——解锁也没用");
    }

    /// 读动作永远放行（连 ACL 只读都不拦读）。
    #[test]
    fn reads_are_never_blocked() {
        let gates = [
            Gate::from_config(true, EnvTag::Dev, false),
            Gate::from_config(false, EnvTag::Prod, false),
            Gate::from_config(false, EnvTag::Prod, true),
            Gate::new(false, EnvTag::Prod, false, Some(Duration::from_secs(1))),
        ];
        for cap in [
            Capability::ReadKey,
            Capability::ScanKeys,
            Capability::ReadServerInfo,
            Capability::ReadConfig,
        ] {
            assert!(!cap.is_write());
            for gate in gates {
                assert!(gate.can(cap), "{cap:?} 在 {gate:?} 下应放行");
            }
        }
    }

    /// 分类谓词：写面覆盖全部非读动作，危险面是正确的四类。
    #[test]
    fn capability_classification() {
        let writes: Vec<_> = Capability::ALL.iter().filter(|c| c.is_write()).collect();
        assert_eq!(writes.len(), Capability::ALL.len() - 4, "恰好四个只读动作");

        for cap in [
            Capability::FlushDb,
            Capability::FlushAll,
            Capability::ClusterAdmin,
            Capability::RawCommand,
        ] {
            assert!(cap.is_dangerous(), "{cap:?} 应标为危险");
        }
        for cap in [
            Capability::ReadKey,
            Capability::WriteKey,
            Capability::SetTtl,
        ] {
            assert!(!cap.is_dangerous(), "{cap:?} 不应标为危险");
        }
    }

    /// 稳定标识不随重构漂移。
    #[test]
    fn wire_names_are_stable() {
        assert_eq!(AccessMode::ReadWrite.wire_name(), "read-write");
        assert_eq!(AccessMode::SafeMode.wire_name(), "safe-mode");
        assert_eq!(AccessMode::StrictReadOnly.wire_name(), "strict-read-only");
        assert_eq!(
            DenyReason::ConnectionReadOnly.wire_name(),
            "connection_read_only"
        );
        assert_eq!(DenyReason::WriteLocked.wire_name(), "write_locked");
        assert_eq!(DenyReason::AclDenied.wire_name(), "acl_denied");
    }
}

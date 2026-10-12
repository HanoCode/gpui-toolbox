//! 凭据后端：**口令存在哪由宿主决定**，内核只认 `duck-platform::secret::SecretStore`。
//!
//! # 为什么不直接在 `accounts.rs` 里调 `duck_platform::secret::set()`
//!
//! 那条路写死了进程级的 `<应用数据目录>/credentials.json`
//! （`duck-platform::secret::default_store` 是个 `OnceLock`，路径进去就再改不了），
//! 于是单测没法把凭据关进临时目录 —— 跑一次 `cargo test` 就要动用户真实的凭据文件。
//!
//! 而 `SecretStore` 这个 trait 存在的理由恰好就是「让调用方注入别的落点」，
//! `duck-platform/src/secret.rs` 的注释原话是「例如测试里的临时目录，或将来把凭据
//! 托管到企业的密钥服务」。这里就是那个注入点：生产走 [`platform`]，测试走
//! [`memory`]（或指向临时目录的 `FileStore`）。
//!
//! # 为什么转发而不是直接持有 `default_store()`
//!
//! `default_store()` 返回的是 `&'static FileStore`——静态引用，不是 `Arc`；
//! 而 [`crate::MailCore`] 要能廉价 `Clone`，句柄必须是拥有所有权的 `Arc`。
//! 同时转发也保住了平台侧的键名校验（`set_with` 会先 `validate_key`）。

use std::sync::Arc;

use duck_platform::secret::{SecretError, SecretStore};

/// 凭据后端的共享句柄。
///
/// `Arc<dyn _>` 而不是泛型参数：`MailCore` 要 `Clone`、要被跨线程搬进
/// `background_executor`，泛型参数会把这些约束传染给每一个持有它的类型。
pub type Secrets = Arc<dyn SecretStore>;

/// 生产后端：`duck-platform::secret` 的进程级存储。
#[derive(Debug, Default)]
pub struct PlatformSecrets;

impl SecretStore for PlatformSecrets {
    fn set(&self, key: &str, value: &str) -> Result<(), SecretError> {
        duck_platform::secret::set(key, value)
    }

    fn get(&self, key: &str) -> Result<Option<String>, SecretError> {
        duck_platform::secret::get(key)
    }

    fn delete(&self, key: &str) -> Result<(), SecretError> {
        duck_platform::secret::delete(key)
    }
}

/// 生产环境的默认后端（写 `<应用数据目录>/credentials.json`）。
#[must_use]
pub fn platform() -> Secrets {
    Arc::new(PlatformSecrets)
}

/// 内存后端：**进程退出即丢**，只给测试用。
///
/// 用 `cfg(test)` 收着：它是「让上层逻辑能在不碰磁盘的前提下被测」的测试夹具，
/// 不是「凭据存不了时的回落方案」——那种回落会让用户以为自己存住了口令。
#[cfg(test)]
#[must_use]
pub fn memory() -> Secrets {
    Arc::new(duck_platform::secret::MemoryStore::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_backend_round_trips_and_is_idempotent_on_delete() {
        let store = memory();
        assert!(store.get("k").unwrap().is_none());
        store.set("k", "v").unwrap();
        assert_eq!(store.get("k").unwrap().as_deref(), Some("v"));
        store.delete("k").unwrap();
        // 删不存在的键是幂等的（trait 契约原文：「键不存在」是 `Ok(None)`，不是错误）。
        store.delete("k").unwrap();
        assert!(store.get("k").unwrap().is_none());
    }

    #[test]
    fn two_handles_do_not_share_state() {
        let a = memory();
        let b = memory();
        a.set("k", "v").unwrap();
        // 每个 `memory()` 都是独立实例——否则跨测试串数据。
        assert!(b.get("k").unwrap().is_none());
    }

    #[test]
    fn platform_backend_implements_the_trait() {
        // 只验类型可用，不碰磁盘（真写会落在用户的应用数据目录里）。
        let store = platform();
        let _: &dyn SecretStore = store.as_ref();
    }
}

//! API key 的加密存取：转发 `duck_platform::vault`。
//!
//! # 落点与纪律（2026-10-12 改版，推翻「key 进系统钥匙串」的旧口径）
//!
//! - **key 密文进档案**：存在 [`crate::ProviderProfile::api_key`] 里，随
//!   `kv: ai_providers` 一起落 SQLite。**kv 里没有明文**——解密用的密钥在
//!   `app_data_dir()/vault.key`（0600，不随备份包走），所以拷走数据库文件
//!   或者拿到导出的备份包都读不出明文。
//! - **不再有独立的键名**：旧实现按 `ai:{provider_id}:api_key` 逐条写系统
//!   钥匙串，键名这一层现在多余——`provider_id` 就是档案 id，
//!   「这条档案配了密钥没有」直接看 `ProviderProfile::api_key` 是不是 `None`。
//! - **不碰系统钥匙串**：与系统凭据存储绑定会把「应用数据」拆成两半
//!   （备份 / 迁移只能搬走一半），且各平台后端可用性不一致。
//!   与 `duck-platform::secret` 的分工见该模块文档。
//! - **解不开就当作没配**：换了机器、密钥文件被删、密文被人改过 —— 一律
//!   [`open`] 返回 `None`，界面显示「未配置」。**绝不回落成明文**。
//! - **零日志**：本模块不 `tracing` 任何键值内容。

use crate::AiError;

/// 加密一条 API key，产出可直接落档案的密文。
///
/// 每次调用都用新 nonce（同一个 key 两次封出来的密文不同），所以**不能拿密文
/// 当「key 有没有变」的判据**——要比就比解出来的明文。
pub fn seal(api_key: &str) -> Result<String, AiError> {
    duck_platform::vault::seal(api_key).map_err(|error| AiError::Secret(error.to_string()))
}

/// 解开档案里的密文。
///
/// 解不开（换了机器、密钥文件被删、密文被改坏）与「本来就是空串」都返回 `None`，
/// **不冒错**：调用点在「发请求前」和「渲染凭据状态」两处都要用它，而这两处的
/// 正确处置都是「当作没配密钥」，正好就是 `None`。弹一个用户无法处理的解密错误
/// 只会把界面卡住。
#[must_use]
pub fn open(sealed: &str) -> Option<String> {
    duck_platform::vault::open(sealed)
        .ok()
        .filter(|key| !key.is_empty())
}

/// 凭据存储此刻能不能用。
///
/// 探的是「密钥文件能不能读写」，不再问「本平台有没有编译进系统后端」——
/// 本机加密保管箱在三个平台上走的是同一套代码（`ring` + 一个密钥文件）。
///
/// 实现上真的封一个空串：首次调用会顺带生成密钥文件，那是**正常初始化**
/// 而不是副作用（第一次保存 key 时同样要生成）。密钥取到后走进程内缓存，
/// 之后每次调用都是纯内存操作。
#[must_use]
pub fn is_available() -> bool {
    seal("").is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 密封再打开必须原样往返（含非 ASCII 与空格这类真实 key 里会出现的字符）。
    #[test]
    fn a_sealed_key_opens_back() {
        for key in ["sk-proj-abc123", "含中文的 key", "with spaces"] {
            let sealed = seal(key).expect("加密");
            assert_ne!(sealed, key, "密文不该等于明文");
            assert_eq!(open(&sealed).as_deref(), Some(key));
        }
    }

    /// 密文里不该能直接搜到明文——这条是「kv 快照被拷走也不泄露」的底线。
    #[test]
    fn the_sealed_form_does_not_contain_the_key() {
        let key = "sk-very-distinctive-token-9876543210";
        let sealed = seal(key).expect("加密");
        assert!(!sealed.contains(key), "密文里出现了明文：{sealed}");
    }

    /// 坏密文一律 `None`，不 panic、不冒错。
    #[test]
    fn a_malformed_sealed_value_opens_as_none() {
        for bad in ["", "abc", "zzzz", "00", &"ab".repeat(12)] {
            assert_eq!(open(bad), None, "{bad:?} 不该解出东西来");
        }
    }

    /// 空串解出来也是 `None`：档案里存了个空 key 等于没配，
    /// 不能让它变成「配了个空字符串」而下发一个空的 `Authorization` 头。
    #[test]
    fn an_empty_key_reads_as_not_configured() {
        let sealed = seal("").expect("加密空串");
        assert_eq!(open(&sealed), None);
    }

    /// 同一个 key 两次封出来的密文不同（nonce 不复用），但都能解回原值。
    #[test]
    fn sealing_twice_produces_different_ciphertext() {
        let first = seal("same-key").expect("第一次加密");
        let second = seal("same-key").expect("第二次加密");
        assert_ne!(first, second, "nonce 复用等于 GCM 报废");
        assert_eq!(open(&first).as_deref(), Some("same-key"));
        assert_eq!(open(&second).as_deref(), Some("same-key"));
    }

    /// 可用性探测要能过（测试进程与真实应用共用 `app_data_dir()`，
    /// 这一条同时验证「密钥文件不存在时会自动创建」）。
    #[test]
    fn the_credential_store_reports_itself_available() {
        assert!(is_available(), "本机加密保管箱应当可用");
    }
}

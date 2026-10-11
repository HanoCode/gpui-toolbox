//! 校验更新源的**签名**：这份 manifest 有没有被我们信任的私钥签过。
//!
//! # 与 SHA-256 的分工
//!
//! 摘要在防的是**传输**：字节在路上被改了、下了一半、写到盘上坏掉了。它挡不住的
//! 是**源本身**被换掉——攻击者把 `update.json` 连同里面那个 `sha256` 一起改掉，
//! 摘要会忠实地为那份假包作证，因为它本来就是从同一份假文档里读来的。
//!
//! 签名补的正是这一块：manifest 的字节必须由持有对应私钥的人签过，否则整份文档
//! 不予采信。两者**都要过**，不是二选一——这一点是本模块存在的全部理由。
//!
//! # 格式
//!
//! [minisign](https://jedisct1.github.io/minisign/)：Ed25519 配 BLAKE2b-512 预哈希，
//! detached 签名（`update.json` 旁边放一份 `update.json.sig`）。三条理由：
//!
//! 1. 客户端只需要**验签**，而 `minisign-verify` 是零传递依赖的纯验签实现——
//!    私钥永远不进二进制。
//! 2. 同一套格式能被 `minisign` / `rsign` 等现成工具签，发布流程不必依赖我们自己的代码。
//! 3. 它是 Tauri 更新器采用的那一套，被大量真实部署检验过。
//!
//! # 公钥从哪来
//!
//! 编译期内建，见 [`builtin_public_key`]。**没有内建公钥时整个校验跳过**——那是给
//! 尚未配置密钥的开发构建与测试留的缺口。正式发布必须设置 `DUCK_UPDATE_PUBKEY`，
//! 否则这段代码等于不存在（这一条写在 `docs/architecture-gap-analysis.md` 的
//! P1-3 章节里，不在代码里重复第二遍）。

use minisign_verify::{PublicKey, Signature};

use crate::error::UpdateError;

/// 编译期内建的更新签名公钥（`minisign.pub` 里的那一行 base64）。
///
/// 构建时由环境变量 `DUCK_UPDATE_PUBKEY` 注入；没设置就是 `None`。
///
/// 用编译期常量而不是运行时读一个旁边的文件，是为了让「这一份二进制信任谁」在产物
/// 里就是定死的——换信任的公钥必须重新构建，而不是替换一个旁边能被随手改掉的文本。
#[must_use]
pub fn builtin_public_key() -> Option<&'static str> {
    option_env!("DUCK_UPDATE_PUBKEY")
}

/// 这份构建要不要校验签名。
///
/// 单独成一个函数，是为了让「跳过校验」这个决定在代码里**看得见**——
/// 而不是藏在某个 `if let Some(_)` 里。
#[must_use]
pub fn required() -> bool {
    builtin_public_key().is_some()
}

/// 用指定的公钥校验一份 detached 签名。
///
/// 拆出「给公钥」这一版而不是只留内建公钥，是为了让测试能喂一对固定的密钥，
/// 不必依赖构建环境。
///
/// # Errors
/// 公钥读不出来、签名读不出来、或验签不通过 → [`UpdateError::Signature`]。
pub fn verify_with(public_key: &str, data: &[u8], detached: &str) -> Result<(), UpdateError> {
    let key = PublicKey::from_base64(public_key.trim()).map_err(|error| {
        UpdateError::Signature(format!("the built-in public key is unusable: {error}"))
    })?;
    let signature = Signature::decode(detached)
        .map_err(|error| UpdateError::Signature(format!("the signature is unreadable: {error}")))?;

    // `allow_legacy = false`：只接受新版 Minisign 的预哈希签名。放行旧格式等于多留
    // 一条我们没打算支持的路径，而它换不来任何兼容性收益——更新源是我们自己的。
    key.verify(data, &signature, false)
        .map_err(|error| UpdateError::Signature(error.to_string()))
}

/// 用内建公钥校验；没有内建公钥时什么也不做。
///
/// # Errors
/// 同 [`verify_with`]。
pub fn verify_builtin(data: &[u8], detached: &str) -> Result<(), UpdateError> {
    match builtin_public_key() {
        Some(key) => verify_with(key, data, detached),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::{builtin_public_key, required, verify_with};

    /// 跨语言交叉验证用的固定向量，由 `sbin/minisign_sign.py` 生成。
    ///
    /// 复现方式：`seed = bytes(range(32))`、`key_id = bytes(range(8, 16))`，
    /// 对被签内容签名。它同时钉住两件事——Python 侧那套纯标准库实现算得对，
    /// 以及客户端**接受**它。任何一边跑偏（BLAKE2b 预哈希、global signature 覆盖的
    /// 范围、base64 的 padding）都会让下面几条当场红。
    const PUBLIC_KEY: &str = "RUQICQoLDA0ODwOhB7/zzhC+HXDdGOdLwJln5NYwm6UNXx3chmQSVTG4";
    /// 公钥里换了 key id 的一把（取自上游文档），用来证明「不是这把钥匙签的」会被拒。
    const FOREIGN_KEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";

    const MESSAGE: &[u8] = br#"{"manifest_version":1,"channel":"stable","version":"0.2.0"}"#;

    const SIGNATURE: &str = "untrusted comment: signature from duck test key\n\
        RUQICQoLDA0OD6z7GS2oeGDRm2BxHffoF3WvcEA1e8WtEkdWUWmlA7mHZleBXuXBBORmb+os/9R4jsak8L8y0po4tquz9oXkcwE=\n\
        trusted comment: timestamp:1760000000\tfile:update.json\n\
        9xcRxf9Ed/rzIPJXsiuXMSQXSaVPsjwCNRRZwqZYhRXy2WsVK5hL0ql207ZWbfYXgjFrmG2vLOuZFd/Y6EvhDA==\n";

    #[test]
    fn no_builtin_key_means_the_check_is_off() {
        assert_eq!(
            builtin_public_key(),
            None,
            "the test build must not have a key baked in"
        );
        assert!(!required());
        assert_eq!(
            super::verify_builtin(b"anything", "not even a signature"),
            Ok(()),
            "without a key there is nothing to check against, and nothing to refuse"
        );
    }

    /// 这条是整条链路的接口测试：我们自己的发布工具产出什么，客户端就得接受什么。
    #[test]
    fn a_signature_made_by_our_own_tooling_verifies() {
        assert_eq!(
            verify_with(PUBLIC_KEY, MESSAGE, SIGNATURE),
            Ok(()),
            "the client must accept what sbin/minisign_sign.py produces"
        );
    }

    /// 改了内容就验不过——这正是签名要拦的那件事。
    #[test]
    fn a_single_flipped_byte_breaks_the_signature() {
        let mut tampered = MESSAGE.to_vec();
        tampered[10] ^= 0x01;
        assert!(
            verify_with(PUBLIC_KEY, &tampered, SIGNATURE).is_err(),
            "one bit of difference must be enough to refuse"
        );
    }

    /// 换一把钥匙：key id 就对不上，客户端必须当场拒绝，而不是「大概能过」。
    #[test]
    fn another_key_does_not_verify_our_signature() {
        let error = verify_with(FOREIGN_KEY, MESSAGE, SIGNATURE)
            .expect_err("a signature names the key that made it");
        assert!(
            error.to_string().contains("different key"),
            "the reason should name the actual problem, got: {error}"
        );
    }

    /// trusted comment 也在签名覆盖面内：改它同样得失败。
    #[test]
    fn a_tampered_trusted_comment_breaks_the_global_signature() {
        let tampered = SIGNATURE.replace("timestamp:1760000000", "timestamp:9999999999");
        assert!(verify_with(PUBLIC_KEY, MESSAGE, &tampered).is_err());
    }

    #[test]
    fn an_unusable_public_key_is_reported_as_such() {
        let error = verify_with("this is not base64 !!!", b"data", SIGNATURE)
            .expect_err("a broken key must not be silently ignored");
        assert!(error.to_string().contains("public key"), "{error}");
    }

    #[test]
    fn an_unreadable_signature_is_reported_as_such() {
        let error = verify_with(PUBLIC_KEY, b"data", "just one line")
            .expect_err("a malformed signature must not be silently ignored");
        assert!(error.to_string().contains("signature"), "{error}");
    }
}

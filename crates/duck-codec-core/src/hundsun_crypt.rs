//! 恒生 LCPT 加解密内核——**逐字复刻**自参考仓
//! `ifm_ai_tools/src-tauri/src/hundsun_crypt.rs`（1,222 行，含全部测试向量）。
//!
//! 移植只改了两处，算法与填充逻辑一行未动：
//! 1. 摘掉 5 处 `#[tauri::command]`（Tauri 命令宏）——本仓这些函数由
//!    `duck-codec-tool`（process 轨）直接调用；
//! 2. 模块头补这段出处说明。
//!
//! 算法口径、默认密钥、填充方式与易错点见下方原文档（全部保留）。

//! 恒生 LCPT 加解密 —— `lcpt-core.jar` 中 `EncryptFactoryImpl` 支持的算法，逐字节 1:1 复刻。
//!
//! 分发模型对齐 Java：`lcpt.config.encryptMode` ∈ {DES, DES3, SM4}（空 → DES），
//! `lcpt.config.blockCipherMode` ∈ {ECB, CBC}（空 → ECB）。**DES 不读 blockCipherMode**，
//! 恒为 ECB。另有 CUSTOM 分支走反射加载硬件 PBC 类，无法本地复刻，故不提供。
//!
//! | 算法 | 算法串 | 密钥 | IV | 填充 | 输出 | 空串 |
//! |------|--------|------|----|------|------|------|
//! | DES  | `DES`（= DES/ECB/PKCS5Padding） | jar 内 `deskey` 8 字节（hex 输入） | — | PKCS#5 | **Base64** | 直接 PKCS#5(空) |
//! | DES(HsAndMoe) | `HSDES`（ECB only） | ASCII 8 字节，默认字面量 `HsAndMoe` | — | **零填充，整除 8 不补块** | **hex 大写** | 先替换为单字节 `\0` |
//! | DES3 | `DESede[/CBC]` + PKCS5Padding | ASCII 24 字节 | CBC: `12345678` | PKCS#5 | **hex 大写** | 先替换为单字节 `\0` |
//! | SM4  | 标准 SM4，ECB/CBC | ASCII 16 字节 | CBC: `0123456789ABCDEF` | **0x80 补位（ISO/IEC 9797-1 方法 2）** | **hex 小写** | 直接补位 |
//!
//! ### `DES` 与 `DES(HsAndMoe)` 是**同名不同义**的两套实现
//!
//! 二者都叫 "DES"，但来自恒生两个不同 jar，密钥/填充/输出编码全不相同：
//!   - `DES`（下方实现）＝ `lcpt-jres` 的 `jres.common.encrypt.DesEncrypt`：**jar 内 `deskey`** +
//!     PKCS#5 + **Base64**。在官方 GUI 工具 `enctools` 里叫「**数据库及RabbitMQ用DES**」。
//!   - `DES(HsAndMoe)` ＝ `lcpt-common` 的 `lcpt.algorithm.encrypt.DESEncryptImpl` →
//!     `DES.enc(str, "HsAndMoe")`：**硬编码字面量密钥**、**零填充**、**hex 大写**。
//!     在官方 GUI 工具里它就**直接叫「DES」**——这正是最容易踩的反直觉点。
//!
//! 所有向量均由真实 JVM 运行 `lcpt-core.jar` / `enctools-1.0-SNAPSHOT.jar` 产出并逐条比对通过。
//!
//! 四处易错点（已实测确认）：
//!   1. `DES3.byte2hex` 用的是大写 `Digit` 表 ⇒ DES3 输出是**大写** hex，不是小写。
//!   2. `DesEncrypt.getEncryptStr("")` 不做 `\0` 替换，直接对空数组做 PKCS#5 ⇒ 得到一个填充块；
//!      只有 `DES3`/`DES3.encCBC`/`DES.enc`（HsAndMoe）才做 `\0` 替换。
//!   3. LCPT 的 SM4 是**标准 SM4**（GB/T 32907-2016 向量命中）；"厂商改动 S-box"的说法源于
//!      测试向量取错（国标要求密钥与明文相同）。其非标之处只在填充方式。
//!   4. `DES(HsAndMoe)` 的"零填充"只在**末块不足 8 字节**时补 `0x00`，长度**整除 8 时不再追加整块**
//!      （与 PKCS#5 相反）；解密先对**末块**做 PKCS#5 风格扫描，再由 `byte2string` 在**首个 `0x00`**
//!      处截断 ⇒ **明文含 `0x00` 时不可逆**，属厂商实现的有损设计，此处如实复刻。
//!
//! 另含**恒生柜员密码**（`tsys_user`）：落库公式 `user_pwd = Encrypt(算法, userId + 密码)`，
//! 明文为 `userId + 密码` **无分隔符直接拼接**。算法由配置链 `sysconfig SMWay` >
//! `tbparam ENCRYPT_ALGORITHM`（默认 MD5）决定，支持 MD5 / SHA / SHA256 / SHA512 / SM4 / SM3。
//!
//! **编码口径**：六个算法统一按 **UTF-8** 取字节、输出 **大写** hex。
//! MD5 分支的权威依据是老 jar 内**唯一**的 MD5 实现
//! `com.hundsun.lcpt.algorithm.MD5Encryptor.getMD5ofStr`：它用 `inbuf.getBytes()`（平台默认编码，
//! 实测 UTF-8）+ 大写 `Digit` 表（`byteHEX`）；SHA 家族与 SM3 / SM4 同口径，一律 UTF-8。
//! 已用真实 JVM 逐条比对：`MD5Encryptor.getMD5ofStr(s)` 与 `MessageDigest("MD5").digest(s.getBytes("UTF-8"))`
//! 上大写在 5 组向量上完全一致。
//!
//! ⚠️ **口径分歧留档**：另一份 `SysUserService` 版本对 MD5/SHA 家族走
//! `src.getBytes("UnicodeLittleUnmarked")` = **UTF-16LE 无 BOM**、输出小写 hex。
//! 若对接的系统确为该口径，只需把 `teller_encrypt` 的 MD5/SHA 分支换回 UTF-16LE 取字节 +
//! `hex::encode`（小写）即可；本工具当前按 jar 口径实现。
//! 另注：`SysUserService` / `EncryptUtils` **不在** `lcpt-core.jar` 内（jar 是纯框架、无柜员业务逻辑），
//! 无法就地判定，故以 jar 内可见实现为准。实现见 `hundsun_teller_encrypt`；摘要**不可逆**，无解密命令。

// 复刻件保留参考仓的写法，两条 clippy 建议都不适用：
//   • `repeat().take(n)` → `repeat_n` 是 1.87 的 API（本仓 MSRV 1.85）；
//   • `len() % n == 0` → `is_multiple_of()` 同样是 1.87 的 API。
// 改写成别的等价形式就等于动了复刻件的字节，失去与参考仓的逐行可比性。
#![allow(clippy::manual_repeat_n, clippy::manual_is_multiple_of)]

use base64::Engine;
use des::cipher::generic_array::GenericArray;
use des::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use des::{Des, TdesEde3};
use md5::Md5;
use serde::Serialize;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use sm3::Sm3;
use sm4::Sm4;

type Block8 = GenericArray<u8, <Des as des::cipher::BlockSizeUser>::BlockSize>;
type Block16 = GenericArray<u8, <Sm4 as des::cipher::BlockSizeUser>::BlockSize>;

const DES_BLOCK: usize = 8;
const SM4_BLOCK: usize = 16;

/// jar 内 `deskey`（Java 序列化 `SecretKeySpec.getEncoded()`）对应的真实 DES 密钥字节。
const DES_DEFAULT_KEY_HEX: &str = "cbe080d620da3813";
const DES_DEFAULT_KEY: [u8; 8] = [0xcb, 0xe0, 0x80, 0xd6, 0x20, 0xda, 0x38, 0x13];
/// `DESEncryptImpl` 中硬编码的字面量密钥（8 字节 ASCII），对应 `DES.enc(str, "HsAndMoe")`。
const HSDES_DEFAULT_KEY: &str = "HsAndMoe";
/// `lcpt.config.encrypt-key` 默认值（24 字节，DESede 要求恰好 24）。
const DES3_DEFAULT_KEY: &str = "HsAndMoeHsAndMoeHsAndMoe";
/// `DES3Encryptor.iv`
const DES3_CBC_IV: &[u8] = b"12345678";
/// `SM4Utils` 的默认 `secretKey`（16 字节 ASCII）
const SM4_DEFAULT_KEY: &str = "HsAndMoeHsAndMoe";
/// `SM4Utils` 的默认 `iv`
const SM4_CBC_IV: &[u8] = b"0123456789ABCDEF";

/// 供界面渲染选择器使用的算法元信息（单一事实来源，避免前端硬编码漂移）。
#[derive(Serialize)]
pub struct HundsunSpec {
    /// 算法标识：DES / DES3 / SM4
    pub algorithm: String,
    /// 界面展示名
    pub label: String,
    /// 默认密钥（DES 为 hex，其余为 ASCII 原文）
    pub default_key: String,
    /// 密钥输入形态："hex" 或 "ascii"
    pub key_kind: String,
    /// 密钥字节长度约束
    pub key_bytes: usize,
    /// 是否支持 CBC（DES 恒为 ECB）
    pub supports_cbc: bool,
    /// 输出编码："base64" / "hex-upper" / "hex-lower"
    pub output: String,
    /// 算法与填充说明
    pub note: String,
}

pub fn hundsun_specs() -> Vec<HundsunSpec> {
    vec![
        HundsunSpec {
            algorithm: "DES".into(),
            label: "DES（默认）".into(),
            default_key: DES_DEFAULT_KEY_HEX.into(),
            key_kind: "hex".into(),
            key_bytes: 8,
            supports_cbc: false,
            output: "base64".into(),
            note: "DES/ECB/PKCS5Padding → Base64；密钥取自 jar 内 deskey".into(),
        },
        HundsunSpec {
            algorithm: "HSDES".into(),
            label: "DES(HsAndMoe)".into(),
            default_key: HSDES_DEFAULT_KEY.into(),
            key_kind: "ascii".into(),
            key_bytes: 8,
            supports_cbc: false,
            output: "hex-upper".into(),
            note: "恒生自研 DES/ECB，零填充（整除 8 不补块）→ hex(大写)；密钥默认字面量 HsAndMoe".into(),
        },
        HundsunSpec {
            algorithm: "DES3".into(),
            label: "3DES".into(),
            default_key: DES3_DEFAULT_KEY.into(),
            key_kind: "ascii".into(),
            key_bytes: 24,
            supports_cbc: true,
            output: "hex-upper".into(),
            note: "DESede/ECB|CBC/PKCS5Padding → hex(大写)；CBC IV 固定 12345678".into(),
        },
        HundsunSpec {
            algorithm: "SM4".into(),
            label: "SM4".into(),
            default_key: SM4_DEFAULT_KEY.into(),
            key_kind: "ascii".into(),
            key_bytes: 16,
            supports_cbc: true,
            output: "hex-lower".into(),
            note: "标准 SM4，ECB|CBC，0x80 补位 → hex(小写)；CBC IV 固定 0123456789ABCDEF".into(),
        },
    ]
}

// ─────────────────────────── 密钥解析 ───────────────────────────

fn norm_algo(a: &str) -> String {
    a.trim().to_uppercase()
}

fn is_cbc(mode: &str) -> bool {
    mode.trim().eq_ignore_ascii_case("CBC")
}

fn resolve_des_key(key: &str) -> Result<[u8; 8], String> {
    let k = key.trim();
    if k.is_empty() {
        return Ok(DES_DEFAULT_KEY);
    }
    let bytes = hex::decode(k).map_err(|e| format!("DES 密钥 Hex 解析失败: {e}"))?;
    if bytes.len() != DES_BLOCK {
        return Err(format!(
            "DES 密钥长度错误: 需要 8 字节 (16 位 hex)，实际 {} 字节",
            bytes.len()
        ));
    }
    let mut out = [0u8; DES_BLOCK];
    out.copy_from_slice(&bytes);
    Ok(out)
}

fn resolve_ascii_key(
    key: &str,
    algo: &str,
    expected: usize,
    default: &str,
) -> Result<Vec<u8>, String> {
    let k = if key.trim().is_empty() { default } else { key };
    let bytes = k.as_bytes();
    if bytes.len() != expected {
        return Err(format!(
            "{algo} 密钥长度错误: 需要 {expected} 字节，实际 {} 字节（按 ASCII 原文计长）",
            bytes.len()
        ));
    }
    Ok(bytes.to_vec())
}

// ─────────────────────────── 填充 ───────────────────────────

/// PKCS#5：长度恰为 8 的倍数时也补满一整块。
fn pkcs5_pad(data: &[u8]) -> Vec<u8> {
    let pad = DES_BLOCK - (data.len() % DES_BLOCK);
    let mut out = data.to_vec();
    out.extend(std::iter::repeat(pad as u8).take(pad));
    out
}

fn pkcs5_unpad(data: &[u8]) -> Result<Vec<u8>, String> {
    if data.is_empty() || data.len() % DES_BLOCK != 0 {
        return Err(format!("密文长度 {} 非法（应为 8 的倍数）", data.len()));
    }
    let pad = *data.last().unwrap() as usize;
    if pad == 0 || pad > DES_BLOCK || pad > data.len() {
        return Err(format!("非法填充字节 {pad}（通常是密钥不匹配）"));
    }
    if data[data.len() - pad..].iter().any(|&b| b as usize != pad) {
        return Err("填充字节不一致（通常是密钥错误或密文被截断）".into());
    }
    Ok(data[..data.len() - pad].to_vec())
}

/// LCPT 的 SM4 补位：`0x80` + 若干 `0x00`（ISO/IEC 9797-1 方法 2），**不是 PKCS#7**。
fn sm4_pad(data: &[u8]) -> Vec<u8> {
    let p = SM4_BLOCK - (data.len() % SM4_BLOCK);
    let mut out = data.to_vec();
    out.push(0x80);
    out.extend(std::iter::repeat(0u8).take(p - 1));
    out
}

/// 对齐 Java `SM4.padding(input, 0)`：在**十六进制串**中找最后一个 "80" 并截断。
fn sm4_unpad(data: &[u8]) -> Result<Vec<u8>, String> {
    let hexs = hex::encode(data);
    match hexs.rfind("80") {
        Some(i) => {
            if i % 2 != 0 {
                return Err("解密结果补位异常（0x80 未字节对齐）".into());
            }
            hex::decode(&hexs[..i]).map_err(|e| format!("补位截断失败: {e}"))
        }
        None => Ok(Vec::new()),
    }
}

/// 对齐 Java `DES3.byte2string`：按首个 `\0` 截断。
fn truncate_at_nul(data: &[u8]) -> &[u8] {
    match data.iter().position(|&b| b == 0) {
        Some(i) => &data[..i],
        None => data,
    }
}

// ─────────────────────────── DES ───────────────────────────

fn des_encrypt(key: &str, plain: &str) -> Result<String, String> {
    let k = resolve_des_key(key)?;
    // 注意：Java 此处不对空串做 `\0` 替换，直接对空数组做 PKCS#5。
    let padded = pkcs5_pad(plain.as_bytes());
    let cipher = Des::new_from_slice(&k).map_err(|e| format!("DES 初始化失败: {e:?}"))?;
    let mut out = Vec::with_capacity(padded.len());
    for chunk in padded.chunks(DES_BLOCK) {
        let mut b = Block8::clone_from_slice(chunk);
        cipher.encrypt_block(&mut b);
        out.extend_from_slice(&b);
    }
    Ok(base64::engine::general_purpose::STANDARD.encode(&out))
}

fn des_decrypt(key: &str, cipher_b64: &str) -> Result<String, String> {
    if cipher_b64.trim().is_empty() {
        return Ok(String::new());
    }
    let k = resolve_des_key(key)?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(cipher_b64.trim())
        .map_err(|e| format!("Base64 解码失败: {e}"))?;
    if raw.is_empty() || raw.len() % DES_BLOCK != 0 {
        return Err(format!(
            "Base64 解码后长度 {} 非法（应为 8 的倍数）",
            raw.len()
        ));
    }
    let cipher = Des::new_from_slice(&k).map_err(|e| format!("DES 初始化失败: {e:?}"))?;
    let mut out = Vec::with_capacity(raw.len());
    for chunk in raw.chunks(DES_BLOCK) {
        let mut b = Block8::clone_from_slice(chunk);
        cipher.decrypt_block(&mut b);
        out.extend_from_slice(&b);
    }
    let clear = pkcs5_unpad(&out)?;
    Ok(String::from_utf8_lossy(&clear).to_string())
}

// ─────────────────────── DES(HsAndMoe) ───────────────────────
//
// 对齐 `lcpt-common` 的 `lcpt.algorithm.DES` + `DESEncryptor`（879 行手写位运算 DES）。
// 与标准 DES 算法本体一致，非标之处全在**填充**与**去填充/截断**：
//   · 加密：末块不足 8 字节补 `0x00`；长度**整除 8 时不再追加整块**。
//   · 解密：末块做 PKCS#5 风格扫描去填充，再由 `byte2string` 在**首个 `0x00`** 处截断。

/// 对齐 `DESEncryptor.encrypt`：仅当 `len % 8 != 0` 时补 `0x00`，整除 8 **不补整块**。
fn hsdes_pad(data: &[u8]) -> Vec<u8> {
    let rem = data.len() % DES_BLOCK;
    let mut out = data.to_vec();
    if rem != 0 {
        out.extend(std::iter::repeat(0u8).take(DES_BLOCK - rem));
    }
    out
}

/// 对齐 `DESEncryptor.decrypt` 对**最后一个块**的 PKCS#5 风格去填充扫描。
///
/// Java 原逻辑：`j` 从 1 到 7，取 `bt = last[j]`；若 `last[j..8]` 全部等于 `bt`
/// **且** `j == 8 - bt`（即恰为合法 PKCS#5 尾块：`8-j` 个值为 `bt` 的填充字节）则截断到
/// `lastBase + j`；若遍历到 `j == 7` 仍未命中，则保留整块。未命中任何分支时原样返回，
/// 后续由 `byte2string`（`truncate_at_nul`）在首个 `0x00` 处截断。
fn hsdes_unpad(data: &[u8]) -> Vec<u8> {
    let n = data.len();
    if n < DES_BLOCK {
        return data.to_vec();
    }
    let base = n - DES_BLOCK; // Java 中 `olen` 的最终值 = 末块偏移
    let mut keep = n;
    for j in 1..DES_BLOCK {
        let bt = data[base + j];
        let mut k = j;
        while k < DES_BLOCK {
            if bt != data[base + k] {
                break;
            }
            k += 1;
        }
        if k == DES_BLOCK {
            // `bt as i8 as isize` 复刻 Java `byte` 的有符号算术（如 0x38 → 56，8-56 = -48）
            if j as isize == 8 - (bt as i8 as isize) {
                keep = base + j;
                break;
            }
            if j == DES_BLOCK - 1 {
                keep = base + DES_BLOCK;
            }
        }
    }
    data[..keep].to_vec()
}

fn hsdes_encrypt(key: &str, plain: &str) -> Result<String, String> {
    let k = resolve_ascii_key(key, "DES(HsAndMoe)", DES_BLOCK, HSDES_DEFAULT_KEY)?;
    // Java `DES.enc`：空串先替换为单字节 `\0`
    let data: Vec<u8> = if plain.is_empty() {
        vec![0u8]
    } else {
        plain.as_bytes().to_vec()
    };
    let padded = hsdes_pad(&data);
    let cipher = Des::new_from_slice(&k).map_err(|e| format!("DES(HsAndMoe) 初始化失败: {e:?}"))?;
    let mut out = Vec::with_capacity(padded.len());
    for chunk in padded.chunks(DES_BLOCK) {
        let mut b = Block8::clone_from_slice(chunk);
        cipher.encrypt_block(&mut b);
        out.extend_from_slice(&b);
    }
    Ok(hex::encode_upper(out))
}

fn hsdes_decrypt(key: &str, cipher_hex: &str) -> Result<String, String> {
    if cipher_hex.trim().is_empty() {
        return Ok(String::new());
    }
    let k = resolve_ascii_key(key, "DES(HsAndMoe)", DES_BLOCK, HSDES_DEFAULT_KEY)?;
    let raw = hex::decode(cipher_hex.trim()).map_err(|e| format!("Hex 解码失败: {e}"))?;
    if raw.is_empty() || raw.len() % DES_BLOCK != 0 {
        return Err(format!("密文长度 {} 非法（应为 8 的倍数）", raw.len()));
    }
    let cipher = Des::new_from_slice(&k).map_err(|e| format!("DES(HsAndMoe) 初始化失败: {e:?}"))?;
    let mut out = Vec::with_capacity(raw.len());
    for chunk in raw.chunks(DES_BLOCK) {
        let mut b = Block8::clone_from_slice(chunk);
        cipher.decrypt_block(&mut b);
        out.extend_from_slice(&b);
    }
    let clear = hsdes_unpad(&out);
    Ok(String::from_utf8_lossy(truncate_at_nul(&clear)).to_string())
}

// ─────────────────────────── 3DES ───────────────────────────

fn des3_encrypt(key: &str, mode: &str, plain: &str) -> Result<String, String> {
    let k = resolve_ascii_key(key, "3DES", 24, DES3_DEFAULT_KEY)?;
    // Java `DES3.enc`：空串先替换为单字节 `\0`
    let data: Vec<u8> = if plain.is_empty() {
        vec![0u8]
    } else {
        plain.as_bytes().to_vec()
    };
    let padded = pkcs5_pad(&data);
    let cipher = TdesEde3::new_from_slice(&k).map_err(|e| format!("3DES 初始化失败: {e:?}"))?;
    let mut out = Vec::with_capacity(padded.len());
    if is_cbc(mode) {
        let mut iv = [0u8; DES_BLOCK];
        iv.copy_from_slice(DES3_CBC_IV);
        for chunk in padded.chunks(DES_BLOCK) {
            let mut b = Block8::clone_from_slice(chunk);
            for i in 0..DES_BLOCK {
                b[i] ^= iv[i];
            }
            cipher.encrypt_block(&mut b);
            iv.copy_from_slice(&b);
            out.extend_from_slice(&b);
        }
    } else {
        for chunk in padded.chunks(DES_BLOCK) {
            let mut b = Block8::clone_from_slice(chunk);
            cipher.encrypt_block(&mut b);
            out.extend_from_slice(&b);
        }
    }
    Ok(hex::encode_upper(out))
}

fn des3_decrypt(key: &str, mode: &str, cipher_hex: &str) -> Result<String, String> {
    if cipher_hex.trim().is_empty() {
        return Ok(String::new());
    }
    let k = resolve_ascii_key(key, "3DES", 24, DES3_DEFAULT_KEY)?;
    let raw = hex::decode(cipher_hex.trim().to_uppercase())
        .map_err(|e| format!("Hex 解码失败: {e}"))?;
    if raw.is_empty() || raw.len() % DES_BLOCK != 0 {
        return Err(format!("密文长度 {} 非法（应为 8 的倍数）", raw.len()));
    }
    let cipher = TdesEde3::new_from_slice(&k).map_err(|e| format!("3DES 初始化失败: {e:?}"))?;
    let mut out = Vec::with_capacity(raw.len());
    if is_cbc(mode) {
        let mut iv = [0u8; DES_BLOCK];
        iv.copy_from_slice(DES3_CBC_IV);
        for chunk in raw.chunks(DES_BLOCK) {
            let mut b = Block8::clone_from_slice(chunk);
            let prev = b;
            cipher.decrypt_block(&mut b);
            for i in 0..DES_BLOCK {
                b[i] ^= iv[i];
            }
            iv.copy_from_slice(&prev);
            out.extend_from_slice(&b);
        }
    } else {
        for chunk in raw.chunks(DES_BLOCK) {
            let mut b = Block8::clone_from_slice(chunk);
            cipher.decrypt_block(&mut b);
            out.extend_from_slice(&b);
        }
    }
    let clear = pkcs5_unpad(&out)?;
    Ok(String::from_utf8_lossy(truncate_at_nul(&clear)).to_string())
}

// ─────────────────────────── SM4 ───────────────────────────

fn sm4_encrypt(key: &str, mode: &str, plain: &str) -> Result<String, String> {
    let k = resolve_ascii_key(key, "SM4", 16, SM4_DEFAULT_KEY)?;
    let padded = sm4_pad(plain.as_bytes());
    let cipher = Sm4::new_from_slice(&k).map_err(|e| format!("SM4 初始化失败: {e:?}"))?;
    let mut out = Vec::with_capacity(padded.len());
    if is_cbc(mode) {
        let mut iv = [0u8; SM4_BLOCK];
        iv.copy_from_slice(SM4_CBC_IV);
        for chunk in padded.chunks(SM4_BLOCK) {
            let mut b = Block16::clone_from_slice(chunk);
            for i in 0..SM4_BLOCK {
                b[i] ^= iv[i];
            }
            cipher.encrypt_block(&mut b);
            iv.copy_from_slice(&b);
            out.extend_from_slice(&b);
        }
    } else {
        for chunk in padded.chunks(SM4_BLOCK) {
            let mut b = Block16::clone_from_slice(chunk);
            cipher.encrypt_block(&mut b);
            out.extend_from_slice(&b);
        }
    }
    Ok(hex::encode(out))
}

fn sm4_decrypt(key: &str, mode: &str, cipher_hex: &str) -> Result<String, String> {
    if cipher_hex.trim().is_empty() {
        return Ok(String::new());
    }
    let k = resolve_ascii_key(key, "SM4", 16, SM4_DEFAULT_KEY)?;
    let raw = hex::decode(cipher_hex.trim()).map_err(|e| format!("Hex 解码失败: {e}"))?;
    if raw.is_empty() || raw.len() % SM4_BLOCK != 0 {
        return Err(format!("密文长度 {} 非法（应为 16 的倍数）", raw.len()));
    }
    let cipher = Sm4::new_from_slice(&k).map_err(|e| format!("SM4 初始化失败: {e:?}"))?;
    let mut out = Vec::with_capacity(raw.len());
    if is_cbc(mode) {
        let mut iv = [0u8; SM4_BLOCK];
        iv.copy_from_slice(SM4_CBC_IV);
        for chunk in raw.chunks(SM4_BLOCK) {
            let mut b = Block16::clone_from_slice(chunk);
            let prev = b;
            cipher.decrypt_block(&mut b);
            for i in 0..SM4_BLOCK {
                b[i] ^= iv[i];
            }
            iv.copy_from_slice(&prev);
            out.extend_from_slice(&b);
        }
    } else {
        for chunk in raw.chunks(SM4_BLOCK) {
            let mut b = Block16::clone_from_slice(chunk);
            cipher.decrypt_block(&mut b);
            out.extend_from_slice(&b);
        }
    }
    let clear = sm4_unpad(&out)?;
    Ok(String::from_utf8_lossy(&clear).to_string())
}

// ─────────────────────────── 命令 ───────────────────────────

/// 恒生 LCPT 加密。`algorithm` ∈ {DES, HSDES, DES3, SM4}；`mode` ∈ {ECB, CBC}
/// （DES / HSDES 恒为 ECB）。
pub fn hundsun_encrypt(
    algorithm: String,
    mode: String,
    key: String,
    plain: String,
) -> Result<String, String> {
    match norm_algo(&algorithm).as_str() {
        "DES" => des_encrypt(&key, &plain),
        "HSDES" | "DES-HS" | "DES-HM" | "DES(HSANDMOE)" => hsdes_encrypt(&key, &plain),
        "DES3" | "3DES" | "TDES" => des3_encrypt(&key, &mode, &plain),
        "SM4" => sm4_encrypt(&key, &mode, &plain),
        other => Err(format!(
            "不支持的算法: {other}（可选 DES / HSDES / DES3 / SM4）"
        )),
    }
}

/// 恒生 LCPT 解密。与 Java 的差异：Java 用 try-catch 吞异常返回空串，此处显式报错。
pub fn hundsun_decrypt(
    algorithm: String,
    mode: String,
    key: String,
    cipher: String,
) -> Result<String, String> {
    match norm_algo(&algorithm).as_str() {
        "DES" => des_decrypt(&key, &cipher),
        "HSDES" | "DES-HS" | "DES-HM" | "DES(HSANDMOE)" => hsdes_decrypt(&key, &cipher),
        "DES3" | "3DES" | "TDES" => des3_decrypt(&key, &mode, &cipher),
        "SM4" => sm4_decrypt(&key, &mode, &cipher),
        other => Err(format!(
            "不支持的算法: {other}（可选 DES / HSDES / DES3 / SM4）"
        )),
    }
}

// ─────────────────────────── 恒生柜员密码（tsys_user） ───────────────────────────

/// 柜员密码选项元信息（单一事实来源，供界面渲染选择器，避免前端硬编码漂移）。
///
/// `kind = "smway"` 的项对应 `SysUserService` 中
/// `ConsoleConfig.getConsoleConfig().getValue("smWay")` 的真实 if-else 分支（业务模式）；
/// `kind = "extended"` 的项是 `EncryptUtils` 方法族里存在、但**不属于** `smWay` 可选值的扩展算法。
#[derive(Serialize)]
pub struct TellerSpec {
    /// 选项标识（前端下拉 value）：MD5 / SM3 / SM4 / SPRING / SM2 / SME140 / SHA / SHA256 / SHA512
    pub algorithm: String,
    /// 界面展示名
    pub label: String,
    /// 分组：`smway`（业务模式）/ `extended`（扩展算法）
    pub kind: String,
    /// 对应的 smWay 参数值；扩展算法为空串；兜底默认项为 `(兜底)`
    pub smway: String,
    /// 触发条件（中文一句话，供提示面板展示）
    pub trigger: String,
    /// 输入字符集
    pub charset: String,
    /// 输出长度与编码说明
    pub output: String,
    /// 是否可逆
    pub reversible: bool,
    /// 本地能否离线复现（sm2 / smE140 为 false）
    pub reproducible: bool,
    /// 是否默认选中（smWay 未命中其它五值时的 MD5 兜底分支）
    pub is_default: bool,
    /// 说明（实现类与依据）
    pub note: String,
}

pub fn hundsun_teller_specs() -> Vec<TellerSpec> {
    vec![
        // ── smWay 业务模式（对齐 SysUserService.resetPassword 等 8 处 if-else 分派）──
        TellerSpec {
            algorithm: "MD5".into(),
            label: "MD5（兜底默认）".into(),
            kind: "smway".into(),
            smway: "(兜底)".into(),
            trigger: "smWay 不等于以下任一值时（含未配置）走此分支".into(),
            charset: "UTF-16LE".into(),
            output: "32 位小写 hex".into(),
            reversible: false,
            reproducible: true,
            is_default: true,
            note: "EncryptUtils.md5Encrypt：Java 走 src.getBytes(\"UnicodeLittleUnmarked\")（UTF-16LE 无 BOM）+ 小写 hex".into(),
        },
        TellerSpec {
            algorithm: "SM3".into(),
            label: "SM3".into(),
            kind: "smway".into(),
            smway: "sm3".into(),
            trigger: "smWay = sm3".into(),
            charset: "UTF-8".into(),
            output: "64 位小写 hex".into(),
            reversible: false,
            reproducible: true,
            is_default: false,
            note: "PwdSm3EncryptUtil.sm3Encrypt：输入平台默认编码（UTF-8），输出小写 hex".into(),
        },
        TellerSpec {
            algorithm: "SM4".into(),
            label: "SM4".into(),
            kind: "smway".into(),
            smway: "sm4".into(),
            trigger: "smWay = sm4".into(),
            charset: "UTF-8".into(),
            output: "变长小写 hex".into(),
            reversible: true,
            reproducible: true,
            is_default: false,
            note: "SM4EncryptImpl.enc（ECB，0x80 补位）；SM4SecretKey.privateKey 为空时用内置密钥 HsAndMoeHsAndMoe".into(),
        },
        TellerSpec {
            algorithm: "SPRING".into(),
            label: "Spring DES".into(),
            kind: "smway".into(),
            smway: "spring".into(),
            trigger: "smWay = spring".into(),
            charset: "UTF-8".into(),
            output: "变长 Base64".into(),
            reversible: true,
            reproducible: true,
            is_default: false,
            note: "Spring Bean「Encrypt」→ LCPT DesEncrypt（deskey + PKCS#5 + Base64），等价于本工具「恒生密码」的 DES-ECB".into(),
        },
        TellerSpec {
            algorithm: "SM2".into(),
            label: "SM2（不可复现）".into(),
            kind: "smway".into(),
            smway: "sm2".into(),
            trigger: "smWay = sm2".into(),
            charset: "UTF-8".into(),
            output: "—".into(),
            reversible: false,
            reproducible: false,
            is_default: false,
            note: "每次 SM2Utils.generateKeyPair() 随机生成密钥对后加密，落库密文无法离线复现".into(),
        },
        TellerSpec {
            algorithm: "SME140".into(),
            label: "加密机 smE140（不可复现）".into(),
            kind: "smway".into(),
            smway: "smE140".into(),
            trigger: "smWay = smE140".into(),
            charset: "—".into(),
            output: "—".into(),
            reversible: false,
            reproducible: false,
            is_default: false,
            note: "由行内硬件加密机 IEncryptService.encrypt 完成，依赖 smE140isDigit / accNo / mode / format / keyName 参数".into(),
        },
        // ── 扩展算法（EncryptUtils 方法族，非 smWay 可选值）──
        TellerSpec {
            algorithm: "SHA".into(),
            label: "SHA1（扩展）".into(),
            kind: "extended".into(),
            smway: "".into(),
            trigger: "非 smWay 可选值；EncryptUtils.shaEncrypt 的能力".into(),
            charset: "UTF-16LE".into(),
            output: "40 位小写 hex".into(),
            reversible: false,
            reproducible: true,
            is_default: false,
            note: "EncryptUtils.shaEncrypt：UTF-16LE 无 BOM + 小写 hex".into(),
        },
        TellerSpec {
            algorithm: "SHA256".into(),
            label: "SHA256（扩展）".into(),
            kind: "extended".into(),
            smway: "".into(),
            trigger: "非 smWay 可选值；EncryptUtils.sha256Encrypt 的能力".into(),
            charset: "UTF-16LE".into(),
            output: "64 位小写 hex".into(),
            reversible: false,
            reproducible: true,
            is_default: false,
            note: "EncryptUtils.sha256Encrypt：UTF-16LE 无 BOM + 小写 hex".into(),
        },
        TellerSpec {
            algorithm: "SHA512".into(),
            label: "SHA512（扩展）".into(),
            kind: "extended".into(),
            smway: "".into(),
            trigger: "非 smWay 可选值；EncryptUtils.sha512Encrypt 的能力".into(),
            charset: "UTF-16LE".into(),
            output: "128 位小写 hex".into(),
            reversible: false,
            reproducible: true,
            is_default: false,
            note: "EncryptUtils.sha512Encrypt：UTF-16LE 无 BOM + 小写 hex".into(),
        },
    ]
}

/// 对齐 Java `String.getBytes("UnicodeLittleUnmarked")`：**UTF-16LE、无 BOM**。
///
/// 代理对（U+10000 以上）按 UTF-16 两个 code unit 各自小端输出，与 JDK 行为一致。
///
/// 用于 MD5 / SHA / SHA256 / SHA512（`EncryptUtils` 的编码口径）；SM3 / SM4 / SPRING
/// 走平台默认编码（UTF-8），两者**不可混用**。
fn utf16le_bytes(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len() * 2);
    for unit in s.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

/// 摘要 → **小写** hex。
///
/// 对齐 `EncryptUtils` 内部 `Integer.toHexString` 的小写口径；SM3 同样小写。
/// 用 `hex::encode` 而非 `format!("{:x}")`：后者的 `LowerHex` 对泛型 `D` 无法满足
/// `OutputSize: Add` 约束，无法在泛型上下文中编译。
fn digest_hex_lower<D: Digest>(data: &[u8]) -> String {
    let mut hasher = D::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn teller_encrypt(algorithm: &str, user_id: &str, password: &str) -> Result<String, String> {
    // 落库明文：userId + 密码，无分隔符直接拼接
    let plain = format!("{user_id}{password}");
    match norm_algo(algorithm).as_str() {
        // ── smWay 业务模式（对齐 SysUserService 的 ConsoleConfig.getValue("smWay") 分派）──
        // 兜底默认：EncryptUtils.md5Encrypt（UnicodeLittleUnmarked = UTF-16LE 无 BOM）+ 小写 hex
        "MD5" => Ok(digest_hex_lower::<Md5>(&utf16le_bytes(&plain))),
        // smWay=sm3：PwdSm3EncryptUtil.sm3Encrypt（BouncyCastle SM3Digest，平台默认编码 UTF-8）
        "SM3" => Ok(digest_hex_lower::<Sm3>(plain.as_bytes())),
        // smWay=sm4：SM4EncryptImpl.enc（ECB，0x80 补位），未配 privateKey 时用内置密钥
        "SM4" => sm4_encrypt("", "ECB", &plain),
        // smWay=spring：Spring Bean Encrypt → DesEncrypt（deskey + PKCS#5 + Base64）
        "SPRING" => des_encrypt(DES_DEFAULT_KEY_HEX, &plain),
        // smWay=sm2：每次 generateKeyPair 随机密钥对，密文不可离线复现
        "SM2" => Err(
            "SM2 模式每次随机生成密钥对（SM2Utils.generateKeyPair），密文不可复现，本工具无法离线计算"
                .into(),
        ),
        // smWay=smE140：依赖行内硬件加密机
        "SME140" => Err(
            "smE140 模式由行内硬件加密机（IEncryptService）完成，本工具无法离线计算".into(),
        ),
        // ── 扩展算法（EncryptUtils 方法族，非 smWay 可选值）──
        "SHA" | "SHA1" => Ok(digest_hex_lower::<Sha1>(&utf16le_bytes(&plain))),
        "SHA256" => Ok(digest_hex_lower::<Sha256>(&utf16le_bytes(&plain))),
        "SHA512" => Ok(digest_hex_lower::<Sha512>(&utf16le_bytes(&plain))),
        other => Err(format!(
            "不支持的算法/模式: {other}（smWay 模式：MD5 / SM3 / SM4 / SPRING / SM2 / SME140；扩展算法：SHA / SHA256 / SHA512）"
        )),
    }
}

/// 恒生柜员密码加密（`tsys_user.user_pwd`）。
///
/// 明文 = `userId + 密码` **无分隔符直接拼接**；选项见 `hundsun_teller_specs`。
/// 摘要**不可逆**（SM4 / SPRING 除外），故不提供解密命令。
///
/// 说明：该规则来自上层 `SysUserService`（**不在** `lcpt-core.jar` 内，jar 是纯框架，
/// 无任何柜员业务逻辑）。8 处分派点（`resetPassword` / 新增用户 / 修改密码 / 验密等）
/// 结构完全一致，均按 `ConsoleConfig.getConsoleConfig().getValue("smWay")` 分派：
/// `sm2` → SM2 随机密钥对；`sm4` → SM4EncryptImpl；`sm3` → PwdSm3EncryptUtil；
/// `spring` → Spring Bean Encrypt；`smE140` → 硬件加密机；其余 → `EncryptUtils.md5Encrypt`。
/// 期望值由真实 JVM（`DesEncrypt`）+ Python hashlib 双源产出。
pub fn hundsun_teller_encrypt(
    algorithm: String,
    user_id: String,
    password: String,
) -> Result<String, String> {
    teller_encrypt(&algorithm, &user_id, &password)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(a: &str, m: &str, k: &str, p: &str) -> String {
        hundsun_encrypt(a.into(), m.into(), k.into(), p.into()).unwrap()
    }
    fn dec(a: &str, m: &str, k: &str, c: &str) -> String {
        hundsun_decrypt(a.into(), m.into(), k.into(), c.into()).unwrap()
    }

    // 期望值全部由真实 JVM 运行 lcpt-core.jar 产出
    #[test]
    fn des_matches_java() {
        let v = [
            ("12345678", "LJK/4lGXPs37eoAEz0PGMQ=="),
            ("hello", "3KNLLZ8j0Kw="),
            ("a", "f5XNE+pl5Ms="),
            ("1234567", "2IaWifVAkxI="),
            ("ABCDEFGHIJKLMNOP", "eGaApRi+3V4GXAvRObPatft6gATPQ8Yx"),
            ("中文测试abc", "04h0s1VpS6ydFLZCmTN/eg=="),
            (":", "lPO/tEaLst4="),
            ("~!@#$%^&*", "2JadEbuxnbPS8u60diDJUA=="),
            // 空串：Java 直接对空数组做 PKCS#5，不替换 \0
            ("", "+3qABM9DxjE="),
        ];
        for (p, e) in v {
            assert_eq!(enc("DES", "ECB", "", p), e, "encrypt {p:?}");
            if !p.is_empty() {
                assert_eq!(dec("DES", "ECB", "", e), p, "decrypt {e}");
            }
        }
    }

    /// `lcpt.algorithm.encrypt.DESEncryptImpl`（`DES.enc/dec`，key = 字面量 `"HsAndMoe"`）。
    /// 期望值由真实 JVM 运行 `enctools-1.0-SNAPSHOT.jar` 的 `DESEncryptImpl` 产出。
    #[test]
    fn hsdes_matches_java() {
        let v = [
            ("12345678", "FD94B0C9A7B4F84B"),
            ("hello", "C69132BEDA59176B"),
            ("a", "2BB1E8387F66A7BA"),
            ("abc", "E279FEAA8A930F74"),
            ("admin", "DDFE93EEF9C03AC9"),
            ("1234567", "DC078B1394A281E2"),
            ("ABCDEFGHIJKLMNOP", "3B6B2F385ACB3DDFB05141237F332C10"),
            ("中文测试", "254445414D6E4C3F7259AA1F0CF6E590"),
            ("中文测试abc", "254445414D6E4C3F3AB0232092878A14"),
            (":", "E08B50178B2B2EFD"),
            ("~!@#$%^&*", "2C5640B87DB3D0C71FC99F2DB9363FB7"),
            // 空串：Java 先替换为单字节 `\0`，再零填充到 8 字节
            ("", "9A8BBC63A14A4B0E"),
        ];
        for (p, e) in v {
            assert_eq!(enc("HSDES", "ECB", "", p), e, "encrypt {p:?}");
            assert_eq!(dec("HSDES", "ECB", "", e), p, "decrypt {e}");
        }
        // 别名与大小写
        assert_eq!(enc("hsdes", "ECB", "", "12345678"), "FD94B0C9A7B4F84B");
        assert_eq!(enc("DES-HM", "ECB", "", "12345678"), "FD94B0C9A7B4F84B");
        // 输出恒为大写 hex
        let c = enc("HSDES", "ECB", "", "12345678");
        assert_eq!(c, c.to_uppercase());
        // 8 字节整块明文不追加填充块 ⇒ 恒为 16 位 hex；不足 8 字节也只占 1 块
        assert_eq!(c.len(), 16);
        assert_eq!(enc("HSDES", "ECB", "", "1234567").len(), 16);
    }

    /// `HSDES` 密钥为 8 字节 ASCII，默认 `HsAndMoe`；长度不符须报错。可自定义密钥。
    #[test]
    fn hsdes_key_handling() {
        // 显式传入默认密钥 → 与不传等价
        assert_eq!(
            enc("HSDES", "ECB", "HsAndMoe", "12345678"),
            enc("HSDES", "ECB", "", "12345678")
        );
        // 自定义 8 字节密钥
        let c = enc("HSDES", "ECB", "12345678", "12345678");
        assert_ne!(c, "FD94B0C9A7B4F84B");
        assert_eq!(dec("HSDES", "ECB", "12345678", &c), "12345678");
        // 长度约束
        assert!(hundsun_encrypt("HSDES".into(), "ECB".into(), "HsAndMo".into(), "x".into()).is_err());
        assert!(hundsun_encrypt("HSDES".into(), "ECB".into(), "HsAndMoe!".into(), "x".into()).is_err());
        // HSDES 不读 mode，DEC 走 ECB
        assert_eq!(
            enc("HSDES", "ECB", "", "12345678"),
            enc("HSDES", "CBC", "", "12345678")
        );
        // 空密文 → 空明文
        assert_eq!(dec("HSDES", "ECB", "", ""), "");
        // 长度非 8 倍数的密文报错
        assert!(hundsun_decrypt("HSDES".into(), "ECB".into(), "".into(), "FD94".into()).is_err());
    }

    #[test]
    fn des3_ecb_matches_java() {
        let v = [
            ("12345678", "FD94B0C9A7B4F84B12D033DC46E1B700"),
            ("hello", "0A8906EDD51D5C9C"),
            ("a", "6770FF054E8913AE"),
            ("1234567", "9DAFB4059ED35D8C"),
            ("ABCDEFGHIJKLMNOP", "3B6B2F385ACB3DDFB05141237F332C1012D033DC46E1B700"),
            ("中文测试abc", "254445414D6E4C3F5E6A532F037D1BBD"),
            (":", "2B8BA78D4763AFAA"),
            ("~!@#$%^&*", "2C5640B87DB3D0C760B75683AF38A5E5"),
            ("", "25709FFAAABFF61B"),
        ];
        for (p, e) in v {
            assert_eq!(enc("DES3", "ECB", "", p), e, "encrypt {p:?}");
            assert_eq!(dec("DES3", "ECB", "", e), p, "decrypt {e}");
        }
    }

    #[test]
    fn des3_cbc_matches_java() {
        let v = [
            ("12345678", "9A8BBC63A14A4B0E16DB17C843ECBAD5"),
            ("hello", "6EC45604E8AC0741"),
            ("a", "C600D241793004BA"),
            ("1234567", "12C982874DADCF36"),
            ("ABCDEFGHIJKLMNOP", "5545890F30F7DF5502202E4DE57B809B749651D62EF0E8F7"),
            ("中文测试abc", "CBB5AC08396DF72391AC080377C4882E"),
            (":", "92BFD0F8383DFC80"),
            ("~!@#$%^&*", "D238B5DA35CB13E5D412F58B2A1D5AEC"),
            ("", "C070598A0C9F46B2"),
        ];
        for (p, e) in v {
            assert_eq!(enc("DES3", "CBC", "", p), e, "encrypt {p:?}");
            assert_eq!(dec("DES3", "CBC", "", e), p, "decrypt {e}");
        }
    }

    #[test]
    fn sm4_ecb_matches_java() {
        let v = [
            ("12345678", "1ded3c5f9c1fab49ef919f2b99983fba"),
            ("hello", "e720c6b0dd30ebda26bf8433c1f57ea6"),
            ("a", "163ac57a2a7f5dd15127b397772571d8"),
            ("1234567", "028d448e6ef48a37ffcb8f0424b49707"),
            (
                "ABCDEFGHIJKLMNOP",
                "d107b1a2ef5a29f59100a8c04bdc461f2a99d32fa2c2ac284c2067d0c50cd4f0",
            ),
            ("中文测试abc", "cd83d8d939abb047faedb1f6e1538dda"),
            (":", "e222e9145e25a175f2d2b960d414c100"),
            ("~!@#$%^&*", "aba5dea24b819bdf2d9b58897015ddab"),
            ("", "2a99d32fa2c2ac284c2067d0c50cd4f0"),
        ];
        for (p, e) in v {
            assert_eq!(enc("SM4", "ECB", "", p), e, "encrypt {p:?}");
            assert_eq!(dec("SM4", "ECB", "", e), p, "decrypt {e}");
        }
    }

    #[test]
    fn sm4_cbc_matches_java() {
        let v = [
            ("12345678", "d2a7f7720ac5fbd883a878b634807b1a"),
            ("hello", "44419f6d751073ebf3158d929aa54c96"),
            ("a", "672ab5b88d4bd9205e531784d3ff1582"),
            ("1234567", "6e2b1d396b76cdf4a12f570ec8d7d281"),
            (
                "ABCDEFGHIJKLMNOP",
                "c6ebe6e6ccfe764babfb8eaccb5b0558cd4d99cf5a47f213296ffb719fda8e56",
            ),
            ("中文测试abc", "b8643d5108ceeaf6128eb8af58a965ed"),
            (":", "208d88d6695a0bdb25af84ef9bc760cb"),
            ("~!@#$%^&*", "8b37d4c6a9a03c511c2897b3749e2447"),
            ("", "e57c65e94f919e25466e62ffce16af95"),
        ];
        for (p, e) in v {
            assert_eq!(enc("SM4", "CBC", "", p), e, "encrypt {p:?}");
            assert_eq!(dec("SM4", "CBC", "", e), p, "decrypt {e}");
        }
    }

    #[test]
    fn correct_output_case_per_algorithm() {
        let d3 = enc("DES3", "ECB", "", "12345678");
        assert_eq!(d3, d3.to_uppercase());
        let s4 = enc("SM4", "ECB", "", "12345678");
        assert_eq!(s4, s4.to_lowercase());
        // DES3 解密兼容小写输入（Java 的 indexOf 大小写敏感会失败，此处做健壮性放宽）
        assert_eq!(
            dec("DES3", "ECB", "", "fd94b0c9a7b4f84b12d033dc46e1b700"),
            "12345678"
        );
    }

    #[test]
    fn custom_keys_work_and_length_is_enforced() {
        let c = enc("DES", "ECB", "0011223344556677", "12345678");
        assert_ne!(c, "LJK/4lGXPs37eoAEz0PGMQ==");
        assert_eq!(dec("DES", "ECB", "0011223344556677", &c), "12345678");
        assert!(hundsun_encrypt("DES".into(), "ECB".into(), "0011".into(), "x".into()).is_err());
        assert!(
            hundsun_encrypt("DES3".into(), "ECB".into(), "HsAndMoe".into(), "x".into()).is_err()
        );
        assert!(hundsun_encrypt(
            "DES3".into(),
            "ECB".into(),
            "1234567890123456".into(),
            "x".into()
        )
        .is_err());
        assert!(hundsun_encrypt("SM4".into(), "ECB".into(), "short".into(), "x".into()).is_err());
    }

    #[test]
    fn des_ignores_cbc_mode() {
        assert_eq!(
            enc("DES", "ECB", "", "12345678"),
            enc("DES", "CBC", "", "12345678")
        );
    }

    #[test]
    fn unknown_algorithm_errors() {
        assert!(hundsun_encrypt("AES".into(), "ECB".into(), "".into(), "x".into()).is_err());
        assert!(hundsun_decrypt("AES".into(), "ECB".into(), "".into(), "00".into()).is_err());
    }

    /// 柜员密码：明文 = userId + 密码 直拼，模式由 `smWay` 决定（8 处分派点结构一致）。
    ///
    /// 期望值源：MD5/SHA/SHA256/SHA512 = Python hashlib（UTF-16LE 无 BOM，小写 hex）；
    /// SM3 = Python hashlib（UTF-8，小写 hex）；SM4 = 真实 JVM `SM4Utils`；
    /// SPRING = 真实 JVM `DesEncrypt.getEncryptStr`（deskey + PKCS#5 + Base64）。
    #[test]
    fn teller_matches_sources() {
        // 官方锚点：SQL 脚本注解样例（admin + 11111111 → SM3，64 位小写 hex）
        assert_eq!(
            teller_encrypt("SM3", "admin", "11111111").unwrap(),
            "8d494213390cf3c1e3392a630fbe82cd4dc992e2f4bf4d766a23d7961133d27c",
            "官方锚点 SM3"
        );

        // MD5（smWay 兜底默认）：UTF-16LE 无 BOM + 小写 hex（EncryptUtils.md5Encrypt 口径）
        for (u, p, e) in [
            ("admin", "11111111", "9ee348ff78bba3d66f5ea862c9905f3d"),
            ("01013", "888888", "fd6f79904e59f4a02df9a14ab0cff36c"),
            ("00001", "123456", "a077764982a9142775598dc20a239aa1"),
            ("张三", "123456", "822bace155ab5f283e492852660d6a25"),
            ("", "", "d41d8cd98f00b204e9800998ecf8427e"),
        ] {
            let got = teller_encrypt("MD5", u, p).unwrap();
            assert_eq!(got, e, "MD5 {u:?}+{p:?}");
            assert_eq!(got, got.to_lowercase(), "MD5 输出必须小写");
        }

        // SHA1（扩展）：UTF-16LE 无 BOM + 小写 hex
        for (u, p, e) in [
            ("admin", "11111111", "7ed63a45c63295b98dfa98ef6aa0b7daf38a3b08"),
            ("01013", "888888", "20d78783658414dfd30d3f60bfa682f779d4e2d1"),
            ("00001", "123456", "ca482f7d4a8e8156e68ca64f3f452c8065349b6c"),
            ("张三", "123456", "85895d68e45281bcc316965ef9a9e04b514ebd02"),
            ("", "", "da39a3ee5e6b4b0d3255bfef95601890afd80709"),
        ] {
            let got = teller_encrypt("SHA", u, p).unwrap();
            assert_eq!(got, e, "SHA {u:?}+{p:?}");
            assert_eq!(got, got.to_lowercase(), "SHA 输出必须小写");
        }

        // SHA256（扩展）：UTF-16LE 无 BOM + 小写 hex
        for (u, p, e) in [
            ("admin", "11111111", "950055cacb8ac26fad2c202f7965be2b2a2d794fb5c09f6a003e7871f6ef8c85"),
            ("01013", "888888", "946acba017f74bc5e83bff8656bff5a5dcb516eb73babf5b3060c7ede2249d28"),
            ("00001", "123456", "095d7b7c3d288ca5992bf1156259a74e33976891e4cb7b0d379e41175eab3f01"),
            ("张三", "123456", "8a44533f8bad0a2dc67a94364fca42bb451789739213b3f61ee2e51a28055a8f"),
            ("", "", "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
        ] {
            let got = teller_encrypt("SHA256", u, p).unwrap();
            assert_eq!(got, e, "SHA256 {u:?}+{p:?}");
            assert_eq!(got, got.to_lowercase(), "SHA256 输出必须小写");
        }

        // SHA512（扩展）：UTF-16LE 无 BOM + 小写 hex
        for (u, p, e) in [
            ("admin", "11111111", "ba789e7e0ef831c96bddf451694afa5152a11521f35521d647a8181ac81df79fcdc15e75297ff28be909949655c53dc077252b96a7f39ae5abef8c4ae57d61e0"),
            ("01013", "888888", "6cf08c473c3fd1d6f2e95b7d28064bf035f3fe8acc19ad0800623abeccc4b56b23ae758f4261a1fd55690555499ded10304e63474e1db8681b58d58713c260c5"),
            ("00001", "123456", "cb5f7b4d2d12aaeb359b010540e7093bc6e0ba26c26a750d2470d663deb3949ad6dd4f469599a6b2166817f60d15ff2eb1578bfcfce9134a290c82a65c4f97f0"),
            ("张三", "123456", "48f5dc8634a38447dfaac6299ecf1c88bc81c9abd7bf26021ef2d35d85e558553ff555a8900c9c7c66665b9a79761bc42c3b9d6539811ccd0351293368a94b2e"),
            ("", "", "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"),
        ] {
            let got = teller_encrypt("SHA512", u, p).unwrap();
            assert_eq!(got, e, "SHA512 {u:?}+{p:?}");
            assert_eq!(got, got.to_lowercase(), "SHA512 输出必须小写");
        }

        // SM3（smWay=sm3）：UTF-8 取字节 + **小写** hex
        for (u, p, e) in [
            ("admin", "11111111", "8d494213390cf3c1e3392a630fbe82cd4dc992e2f4bf4d766a23d7961133d27c"),
            ("01013", "888888", "6ca8f91c5027fb5ed1ae5d4401f236f834b05f2d80cbb0bdd5aa8f7fbed93b88"),
            ("00001", "123456", "b7d9fc2b1ccdd058b6e7ff4463e16d60888f95349ce5f21895b48bcb736ea738"),
            ("张三", "123456", "0ba8874fbc84d32154832d0c6b2412b0f4d4f8f447b28ad6fe3b1a59459e395f"),
            ("", "", "1ab21d8355cfa17f8e61194831e81a8f22bec8c728fefb747ed035eb5082aa2b"),
        ] {
            let got = teller_encrypt("SM3", u, p).unwrap();
            assert_eq!(got, e, "SM3 {u:?}+{p:?}");
            assert_eq!(got, got.to_lowercase(), "SM3 输出必须小写");
        }

        // SM4（smWay=sm4）：UTF-8 + ECB（0x80 补位），真实 JVM 真值
        for (u, p, e) in [
            ("admin", "11111111", "b698f7fa62a50a7dd495d84b9cdc051f"),
            ("01013", "888888", "a9f86bfc6bd89ac7e6a86a45e85f3a97"),
            ("00001", "123456", "beb1629bc8c341e03cd8569ed64fc71c"),
            ("张三", "123456", "6623b919fba480f67836fed145202f9c"),
            ("", "", "2a99d32fa2c2ac284c2067d0c50cd4f0"),
        ] {
            assert_eq!(teller_encrypt("SM4", u, p).unwrap(), e, "SM4 {u:?}+{p:?}");
        }

        // SPRING（smWay=spring）：真实 JVM `DesEncrypt.getEncryptStr`（deskey + PKCS#5 + Base64）
        for (u, p, e) in [
            ("admin", "11111111", "AatLJCHXZ1CLzVcs1M/Cfg=="),
            ("01013", "888888", "+7qG4PrHQU15/CtXTVMkrg=="),
            ("00001", "123456", "8ZLi+/r8uvJtfsEhxpagqQ=="),
            ("张三", "123456", "IScVwgTE/e3XBB0HMNWhKw=="),
            ("", "", "+3qABM9DxjE="),
        ] {
            assert_eq!(teller_encrypt("SPRING", u, p).unwrap(), e, "SPRING {u:?}+{p:?}");
        }

        // 不可复现模式必须显式报错（绝不静默返回空串）
        assert!(hundsun_teller_encrypt("SM2".into(), "a".into(), "b".into()).is_err());
        assert!(hundsun_teller_encrypt("SME140".into(), "a".into(), "b".into()).is_err());
        // 未知算法报错（DES / AES 均不在柜员支持列表内）
        assert!(hundsun_teller_encrypt("DES".into(), "a".into(), "b".into()).is_err());
        assert!(hundsun_teller_encrypt("AES".into(), "a".into(), "b".into()).is_err());
    }

    /// UTF-16LE（无 BOM）编码对齐 JDK `UnicodeLittleUnmarked`，含代理对。
    ///
    /// 该口径**已启用**：MD5 / SHA / SHA256 / SHA512 四个分支都走它。
    #[test]
    fn teller_utf16le_encoding() {
        assert!(utf16le_bytes("").is_empty());
        assert_eq!(utf16le_bytes("A"), vec![0x41u8, 0x00]); // U+0041
        assert_eq!(utf16le_bytes("中"), vec![0x2du8, 0x4e]); // U+4E2D
        assert_eq!(utf16le_bytes("😀"), vec![0x3du8, 0xd8, 0x00, 0xde]); // U+1F600 → D83D DE00

        // 启用口径真值：UTF-16LE 下 MD5(admin11111111)
        assert_eq!(
            digest_hex_lower::<Md5>(&utf16le_bytes("admin11111111")),
            "9ee348ff78bba3d66f5ea862c9905f3d"
        );
        // 与 UTF-8 口径**必须不同**，防止两种口径被无意识混用
        assert_ne!(
            digest_hex_lower::<Md5>(&utf16le_bytes("admin11111111")),
            digest_hex_lower::<Md5>(b"admin11111111")
        );
    }

    /// 选项列表：6 个 smWay 业务模式 + 3 个扩展算法，默认选中 MD5 兜底分支。
    #[test]
    fn teller_specs_cover_modes() {
        let s = hundsun_teller_specs();
        assert_eq!(s.len(), 9, "6 个 smWay 模式 + 3 个扩展算法");

        let smway: Vec<_> = s.iter().filter(|x| x.kind == "smway").collect();
        let extended: Vec<_> = s.iter().filter(|x| x.kind == "extended").collect();
        assert_eq!(smway.len(), 6);
        assert_eq!(extended.len(), 3);

        // smWay 值必须覆盖 SysUserService 全部分支
        let vals: Vec<&str> = smway.iter().map(|x| x.smway.as_str()).collect();
        for v in ["(兜底)", "sm3", "sm4", "spring", "sm2", "smE140"] {
            assert!(vals.contains(&v), "缺少 smWay 分支 {v}");
        }

        // 默认唯一且为 MD5 兜底分支
        assert_eq!(s.iter().filter(|x| x.is_default).count(), 1);
        let def = s.iter().find(|x| x.is_default).unwrap();
        assert_eq!(def.algorithm, "MD5");
        assert_eq!(def.smway, "(兜底)");

        // 可复现性：仅 sm2 / smE140 为 false
        for x in &s {
            let want = !matches!(x.algorithm.as_str(), "SM2" | "SME140");
            assert_eq!(x.reproducible, want, "reproducible for {}", x.algorithm);
        }

        // 编码口径：MD5/SHA 家族 UTF-16LE；SM3/SM4/SPRING/SM2 UTF-8；SME140 无（硬件）
        for x in &s {
            let want = if matches!(x.algorithm.as_str(), "MD5" | "SHA" | "SHA256" | "SHA512") {
                "UTF-16LE"
            } else if matches!(x.algorithm.as_str(), "SM3" | "SM4" | "SPRING" | "SM2") {
                "UTF-8"
            } else {
                "—"
            };
            assert_eq!(x.charset, want, "charset for {}", x.algorithm);
        }

        // 可逆项：仅 SM4 / SPRING
        for x in &s {
            let want = matches!(x.algorithm.as_str(), "SM4" | "SPRING");
            assert_eq!(x.reversible, want, "reversible for {}", x.algorithm);
        }
    }

    #[test]
    fn specs_cover_four_algorithms() {
        let s = hundsun_specs();
        assert_eq!(s.len(), 4);
        // 顺序：DES / DES(HsAndMoe) / DES3 / SM4
        assert_eq!(s[0].algorithm, "DES");
        assert!(!s[0].supports_cbc);
        assert_eq!(s[0].key_kind, "hex");
        assert_eq!(s[0].output, "base64");

        assert_eq!(s[1].algorithm, "HSDES");
        assert_eq!(s[1].label, "DES(HsAndMoe)");
        assert_eq!(s[1].default_key, "HsAndMoe");
        assert_eq!(s[1].key_kind, "ascii");
        assert_eq!(s[1].key_bytes, 8);
        assert!(!s[1].supports_cbc);
        assert_eq!(s[1].output, "hex-upper");

        assert_eq!(s[2].algorithm, "DES3");
        assert_eq!(s[2].key_bytes, 24);
        assert!(s[2].supports_cbc);
        assert_eq!(s[3].default_key, "HsAndMoeHsAndMoe");
        assert_eq!(s[3].output, "hex-lower");

        // 两个 "DES" 字面量必须是不同的算法标识，避免前端选择器歧义
        assert_ne!(s[0].algorithm, s[1].algorithm);
    }
}

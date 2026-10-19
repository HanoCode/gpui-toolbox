//! 加解密工具包（`duck.codec.crypto-bundle`）的 **process 轨入口**。
//!
//! 一个插件九个「标签页」：参考仓 `CryptoBundle.vue` 里那排 `AppSegmented`
//! 的分段切换，在本仓由 `[io]` 的 `mode` 选择器 + `visible_when` 条件显隐表达
//! （宿主不认「标签页」这回事，只认参数可见性；效果相同：当前模式下只看得见
//! 该模式用得上的旋钮）。
//!
//! 本文件只做三件事：把 `mode` 分派到内核、把参数摊平成内核入参、把结果拼成
//! 一段可复制的文本。任何算法都不在这里——它们全在 `duck-codec-core`。

use duck_codec_core::{aes_cbc, base64, file_hash, hash, hex, hundsun_crypt, jwt, password, url};
use duck_plugin::PluginLocales;
use duck_plugin_process::render::Renderer;
use duck_plugin_process::{PluginOutput, RunRequest, serve};
use std::path::Path;
use std::process::ExitCode;

/// 参数区：宿主把 `[io]` 表单收集到的值序列化成键值对。
struct Params(serde_json::Value);

impl Params {
    fn new(value: &serde_json::Value) -> Self {
        Self(value.clone())
    }

    /// 取字符串（数字 / 布尔也接受）。
    fn text(&self, key: &str) -> String {
        match self.0.get(key) {
            Some(serde_json::Value::String(text)) => text.trim().to_owned(),
            Some(serde_json::Value::Number(number)) => number.to_string(),
            Some(serde_json::Value::Bool(flag)) => flag.to_string(),
            _ => String::new(),
        }
    }

    /// 取值，空串时用 `fallback`（选项没送到时回落参考仓的默认值）。
    fn text_or(&self, key: &str, fallback: &str) -> String {
        let value = self.text(key);
        if value.is_empty() {
            fallback.to_owned()
        } else {
            value
        }
    }

    /// 取整数（缺省 / 无法解析时退回 `fallback`）。
    fn number(&self, key: &str, fallback: usize) -> usize {
        self.text(key).parse::<usize>().unwrap_or(fallback)
    }

    /// 取布尔（`"true"` / `"false"`，也认 JSON 布尔）。
    fn flag(&self, key: &str, fallback: bool) -> bool {
        match self.0.get(key) {
            Some(serde_json::Value::Bool(flag)) => *flag,
            Some(serde_json::Value::String(text)) => match text.trim() {
                "true" => true,
                "false" => false,
                _ => fallback,
            },
            _ => fallback,
        }
    }
}

/// 取词器：`locales/` 由宿主通过环境变量指路，缺失时原样返回中文源串。
struct Text {
    locales: PluginLocales,
    lang: String,
}

impl Text {
    fn load() -> Self {
        let lang = std::env::var(duck_plugin::ENV_LOCALE).unwrap_or_else(|_| "zh-Hans".to_owned());
        // 宿主给的 `DUCK_LOCALES_DIR` **就是** `locales/` 这一级：走 `load` 会把
        // `locales` 再拼一次，读不到任何文件（英文界面静默显示中文）。
        let locales = std::env::var(duck_plugin::ENV_LOCALES_DIR)
            .map(|dir| PluginLocales::load_locales_dir(Path::new(&dir), "zh-Hans"))
            .unwrap_or_default();
        Self { locales, lang }
    }

    /// 内核产出的是**已插值**中文，要过模板反查才能翻（与 merge / sql 同一套）。
    fn renderer(&self) -> Renderer<'_> {
        Renderer::new(&self.locales, &self.lang)
    }
}

fn main() -> ExitCode {
    serve(handle)
}

fn handle(
    request: &RunRequest,
    _out: &mut PluginOutput<std::io::Stdout>,
) -> Result<String, duck_plugin::PluginError> {
    let text = Text::load();
    let params = Params::new(&request.params);
    let outcome = match params.text_or("mode", "base64").as_str() {
        "base64" => convert(
            request,
            &params,
            "b64_action",
            base64::encode,
            base64::decode,
        ),
        "url" => convert(request, &params, "url_action", url::encode, url::decode),
        "hex" => convert(request, &params, "hex_action", hex::encode, hex::decode),
        "hash" => hash_run(request, &params),
        "aes" => aes_run(request, &params),
        "jwt" => jwt_run(request),
        "password" => password_run(&params),
        "hundsun" => hundsun_run(request, &params),
        "hundsun-teller" => teller_run(&params),
        other => Err(format!("未知的功能：{}", other)),
    };
    outcome.map_err(|error| fail(text.renderer().translate(&error)))
}

fn fail(message: String) -> duck_plugin::PluginError {
    duck_plugin::PluginError::Execution(message)
}

/// 「编码 / 解码」两个函数指针：三条分支共用同一套骨架。
type Encode = fn(&str) -> String;
type Decode = fn(&str) -> Result<String, String>;

/// Base64 / URL / Hex：同一形状——一个动作开关 + 一段输入。
fn convert(
    request: &RunRequest,
    params: &Params,
    action_key: &str,
    encode: Encode,
    decode: Decode,
) -> Result<String, String> {
    let input = request.input.as_str();
    if input.is_empty() {
        return Err("输入是空的".to_owned());
    }
    // 老清单 / 第三方调用可能只给 `mode` 不给动作键，按「编码」走（源仓默认那条）。
    if params.text(action_key) == "decode" {
        decode(input)
    } else {
        Ok(encode(input))
    }
}

/// 哈希：文本 → 四种 SHA；文件 → 五种摘要（含 MD5，内核复刻件的固定顺序）。
fn hash_run(request: &RunRequest, params: &Params) -> Result<String, String> {
    if params.text("hash_target") == "file" {
        let path = params.text("hash_file");
        if path.is_empty() {
            return Err("请先选择要计算哈希的文件".to_owned());
        }
        let results = file_hash::compute_file_hash(&path)?;
        return Ok(rows(results.into_iter().map(|item| (item.alg, item.value))));
    }
    let input = request.input.as_str();
    if input.is_empty() {
        return Err("输入是空的".to_owned());
    }
    Ok(rows(
        hash::hash_text(input)
            .into_iter()
            .map(|item| (item.alg, item.value)),
    ))
}

/// AES-CBC：明文 → Base64 / Base64 → 明文。
fn aes_run(request: &RunRequest, params: &Params) -> Result<String, String> {
    let mode = params.text_or("aes_mode", aes_cbc::MODES[0]);
    let key = params.text("aes_key");
    if key.is_empty() {
        return Err("AES 需要密钥（16 / 24 / 32 字节）".to_owned());
    }
    let iv = params.text("aes_iv");
    let input = request.input.as_str();
    if input.is_empty() {
        return Err("输入是空的".to_owned());
    }
    if params.text("aes_action") == "decrypt" {
        aes_cbc::decrypt(&mode, &key, &iv, input)
    } else {
        aes_cbc::encrypt(&mode, &key, &iv, input)
    }
}

/// JWT：只拆 Header / Payload，不验签。
fn jwt_run(request: &RunRequest) -> Result<String, String> {
    let input = request.input.trim();
    if input.is_empty() {
        return Err("输入是空的".to_owned());
    }
    let parts = jwt::decode(input)?;
    Ok(format!(
        "Header\n{}\n\nPayload\n{}\n\n{}",
        parts.header, parts.payload, "（签名段未校验，本工具只解析内容）"
    ))
}

/// 随机密码：一行一个，制表符后面跟强度档位。
fn password_run(params: &Params) -> Result<String, String> {
    let options = password::Options {
        length: params.number("pwd_length", 16),
        count: params.number("pwd_count", 5),
        upper: params.flag("pwd_upper", true),
        lower: params.flag("pwd_lower", true),
        digits: params.flag("pwd_digits", true),
        symbols: params.flag("pwd_symbols", true),
    };
    let list = password::generate(&options)?;
    Ok(list
        .iter()
        .map(|item| format!("{}\t{}", item.value, item.strength))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// 恒生 LCPT：DES / DES(HsAndMoe) / DES3 / SM4，ECB 或 CBC。
///
/// `hs_key` 留空 = 用算法默认密钥——参考仓那个「重置默认」按钮的等价物：
/// 清空即可回落默认，比再记一个按钮更好记。
fn hundsun_run(request: &RunRequest, params: &Params) -> Result<String, String> {
    let algorithm = params.text_or("hs_algorithm", "DES");
    let mode = params.text_or("hs_mode", "ECB");
    let key = params.text("hs_key");
    let input = request.input.trim();
    if input.is_empty() {
        return Err("输入是空的".to_owned());
    }
    if params.text("hs_action") == "decrypt" {
        hundsun_crypt::hundsun_decrypt(algorithm, mode, key, input.to_owned())
    } else {
        hundsun_crypt::hundsun_encrypt(algorithm, mode, key, input.to_owned())
    }
}

/// 恒生柜员密码：`Encrypt(算法, userId + 密码)`，落库口径。
fn teller_run(params: &Params) -> Result<String, String> {
    let algorithm = params.text_or("tl_algorithm", "MD5");
    let user_id = params.text("tl_user_id");
    let pwd = params.text("tl_password");
    if user_id.is_empty() && pwd.is_empty() {
        return Err("柜员号与密码不能都为空".to_owned());
    }
    hundsun_crypt::hundsun_teller_encrypt(algorithm, user_id, pwd)
}

/// 把「算法 → 值」的列表排成对齐的两列（算法名左对齐补到 7 格）。
fn rows(items: impl Iterator<Item = (String, String)>) -> String {
    items
        .map(|(alg, value)| format!("{alg:<7}{value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

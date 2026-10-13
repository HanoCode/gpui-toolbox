//! AI text-to-SQL：**只留 SQL 领域的提示词**，协议层走 `duck-ai`。
//!
//! 设计要点（`docs/database-design.md` §4.8）：Provider trait 先立住，安全闸门
//! 在调用方——模型产出的 SQL 必须过 `sqlintel` 风险分级，只读才允许自动执行，
//! 写语句一律退回用户编辑器人工审批。本模块不做审批、不执行 SQL。
//!
//! **收敛（`docs/ai-design.md` §4.9）**：OpenAI 兼容协议的 HTTP 实现、错误归一、
//! 用量解析全部上移到 [`duck_ai`]，本模块只做两件事——保留对外的 Provider 面，
//! 以及组装 SQL 领域的提示词。
//!
//! **动作工厂**：DB 场景要的不只是「自然语言 → SQL」。这里把三种动作收敛成
//! [`AiAction`] 与一组**纯函数**（[`pick_action`] / [`AiAction::system_prompt`] /
//! [`build_action_prompt`]）：
//!
//! | 动作 | 输入 | 产出 |
//! | --- | --- | --- |
//! | [`AiAction::Generate`] | schema + 自然语言问题 | 一条 SQL |
//! | [`AiAction::Fix`] | schema + 失败 SQL + **数据库错误原文** | 修正后的 SQL |
//! | [`AiAction::Explain`] | schema + SQL（± 错误原文） | 解释文本（**不执行**） |
//!
//! 动作选择走**规则**（[`pick_action`]：手上有没有错误上下文），**不引入** LLM
//! 意图分类——那既多一次往返，又多一份不确定性。

use duck_ai::{ChatMessage, ChatRequest, ProviderProfile};

use crate::error::DbError;

/// 一次 AI 补全的产出。
#[derive(Debug, Clone)]
pub struct AiCompletion {
    /// 模型回复正文：`generate` / `fix` 约定为一条 SQL，`explain` 是解释文本；
    /// 具体怎么用由调用方按动作决定。
    pub text: String,
    /// 本次请求消耗的总 token 数（拿不到时为 0）。
    pub total_tokens: u32,
}

/// DB 场景下的一次 AI 动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AiAction {
    /// 自然语言 → SQL（现状语义）。
    Generate,
    /// 给定失败 SQL + 数据库错误原文 → 修正后的 SQL。
    Fix,
    /// 解释 SQL / 错误含义（只产出文本，**不产出可执行 SQL**）。
    Explain,
}

impl AiAction {
    /// 全部动作（界面按钮的展示顺序）。
    pub const ALL: [Self; 3] = [Self::Generate, Self::Fix, Self::Explain];

    /// 该动作的系统提示词。
    #[must_use]
    pub const fn system_prompt(self) -> &'static str {
        match self {
            Self::Generate => GENERATE_SYSTEM_PROMPT,
            Self::Fix => FIX_SYSTEM_PROMPT,
            Self::Explain => EXPLAIN_SYSTEM_PROMPT,
        }
    }
}

/// 规则选动作：有**错误上下文**就是修错；只给了 SQL 就是解释；都没有就是生成。
///
/// 纯规则、不调 LLM：调用点手上有没有错误原文 / 原 SQL 本身就可判定，
/// 再让模型做一次意图分类只是白花一次往返。
#[must_use]
pub fn pick_action(sql: Option<&str>, error: Option<&str>) -> AiAction {
    let has_sql = sql.is_some_and(|sql| !sql.trim().is_empty());
    let has_error = error.is_some_and(|error| !error.trim().is_empty());
    match (has_sql, has_error) {
        (_, true) => AiAction::Fix,
        (true, false) => AiAction::Explain,
        (false, false) => AiAction::Generate,
    }
}

/// AI 提供方契约（M3 扩展多家实现时的公共面）。
pub trait AiProvider: Send {
    /// 用 `generate` 的默认系统提示词发一段提示词，拿回正文与用量。
    fn complete(&self, prompt: &str) -> Result<AiCompletion, DbError> {
        self.complete_with(AiAction::Generate.system_prompt(), prompt)
    }

    /// 指定系统提示词发一段提示词（`generate` / `fix` / `explain` 各给各的）。
    fn complete_with(&self, system_prompt: &str, prompt: &str) -> Result<AiCompletion, DbError>;
}

/// OpenAI 兼容提供方（`POST {base_url}/chat/completions`，非流式）。
#[derive(Debug, Clone)]
pub struct OpenAiCompatible {
    /// 形如 `https://api.openai.com/v1`（不带尾斜杠）。
    pub base_url: String,
    /// API key（明文只在内存；持久化走凭据文件，值为 vault 密文）。
    pub api_key: String,
    /// 模型名（如 `gpt-4o-mini`）。
    pub model: String,
    /// 请求超时秒数。
    pub timeout_secs: u64,
}

impl AiProvider for OpenAiCompatible {
    fn complete_with(&self, system_prompt: &str, prompt: &str) -> Result<AiCompletion, DbError> {
        if self.base_url.trim().is_empty() || self.api_key.trim().is_empty() {
            return Err(DbError::Config(
                "AI provider base_url/api_key is empty".into(),
            ));
        }
        if self.model.trim().is_empty() {
            return Err(DbError::Config("AI provider model is empty".into()));
        }
        // 档案是本模块私有的临时值，只为把参数喂给 `duck_ai`；
        // 不落盘、不进 kv，所以 `api_key` 留 `None`（明文单独作为参数传递）。
        let profile = ProviderProfile {
            id: "duck-db-ai".to_owned(),
            template_id: "openai_compatible".to_owned(),
            name: String::new(),
            base_url: self.base_url.trim().trim_end_matches('/').to_owned(),
            model: self.model.clone(),
            is_local: false,
            api_key: None,
        };
        let request = ChatRequest {
            model: self.model.clone(),
            messages: vec![
                ChatMessage::system(system_prompt),
                ChatMessage::user(prompt),
            ],
            // 温度钉 0：text-to-SQL 要的是确定性输出，不是发挥。
            temperature: Some(0.0),
            max_tokens: None,
            tools: Vec::new(),
        };
        let (text, usage) = duck_ai::provider::complete_with_usage(
            &profile,
            self.api_key.trim(),
            &request,
            self.timeout_secs.max(1),
        )
        .map_err(map_ai_error)?;
        Ok(AiCompletion {
            text,
            total_tokens: usage.total_tokens,
        })
    }
}

/// `duck_ai::AiError` → `DbError`：调用方（`db.rs`）只认后者。
fn map_ai_error(error: duck_ai::AiError) -> DbError {
    match error {
        duck_ai::AiError::Config(detail) => DbError::Config(detail),
        duck_ai::AiError::Cancelled => DbError::Cancelled("AI request was cancelled".into()),
        other => DbError::Query(other.to_string()),
    }
}

/// `generate` 的系统提示词：只产 SQL，不解释，多条用分号分隔。
const GENERATE_SYSTEM_PROMPT: &str = "You are a senior SQL assistant. Given a database schema and a \
question in any language, reply with ONLY the SQL statement (or statements separated by \
semicolons) that answers the question. Prefer read-only SELECT queries. Never wrap the \
answer in markdown fences. No explanations.";

/// `fix` 的系统提示词：给定失败 SQL + 数据库错误原文，只产修正后的 SQL。
const FIX_SYSTEM_PROMPT: &str = "You are a senior SQL assistant. Given a database schema, a SQL \
statement that failed, and the exact database error message, reply with ONLY the corrected SQL \
statement. Keep the original intent; fix syntax, identifiers, or types as the error indicates. \
Prefer read-only SELECT queries. Never wrap the answer in markdown fences. No explanations.";

/// `explain` 的系统提示词：解释 SQL / 错误，产出自然语言，不再产出可执行 SQL。
const EXPLAIN_SYSTEM_PROMPT: &str = "You are a senior SQL assistant. Given a database schema and a \
SQL statement (and possibly a database error message), explain in plain language what the \
statement does or why it failed, and how to fix it. Reply with a concise explanation in the same \
language as the user; do NOT reply with a full SQL statement to execute.";

/// 组 `generate` 动作的提示词（schema 摘要 + 用户问题）。
///
/// 保留这个名字与签名：它是外部调用点（`pages/db.rs`）与既有单测的入口。
#[must_use]
pub fn build_prompt(schema_summary: &str, question: &str) -> String {
    build_action_prompt(AiAction::Generate, schema_summary, question, None, None)
}

/// 组某个动作的用户提示词（**纯函数**，可单测）。
///
/// 三个动作各自决定注入哪些上下文：`Generate` 注入问题；`Fix` 注入原 SQL + **错误原文**；
/// `Explain` 两者都注入。schema 摘要为空时整段省略（不发一句「Database schema:」空头）。
#[must_use]
pub fn build_action_prompt(
    action: AiAction,
    schema_summary: &str,
    question: &str,
    sql: Option<&str>,
    error: Option<&str>,
) -> String {
    let schema = schema_summary.trim();
    let mut out = String::new();
    if !schema.is_empty() {
        out.push_str("Database schema:\n");
        out.push_str(schema);
        out.push('\n');
    }
    match action {
        AiAction::Generate => {
            out.push_str("\nQuestion: ");
            out.push_str(question.trim());
            out.push_str("\n\nReply with SQL only.");
        }
        AiAction::Fix => {
            out.push_str("\nFailed SQL:\n");
            out.push_str(sql.map(str::trim).unwrap_or_default());
            out.push_str("\n\nDatabase error:\n");
            out.push_str(error.map(str::trim).unwrap_or_default());
            out.push_str("\n\nReply with a corrected SQL statement only.");
        }
        AiAction::Explain => {
            if let Some(sql) = sql.map(str::trim).filter(|sql| !sql.is_empty()) {
                out.push_str("\nSQL:\n");
                out.push_str(sql);
                out.push('\n');
            }
            if let Some(error) = error.map(str::trim).filter(|error| !error.is_empty()) {
                out.push_str("\nDatabase error:\n");
                out.push_str(error);
                out.push('\n');
            }
            out.push_str(
                "\nExplain in plain language what it does or why it failed, and how to fix it.",
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 提示词组装：schema 与问题都进正文。
    #[test]
    fn prompt_layout() {
        let prompt = build_prompt("t(id INT)", "最近 10 行");
        assert!(prompt.contains("t(id INT)"));
        assert!(prompt.contains("最近 10 行"));
    }

    /// 空 base_url / api_key 直接报配置错误（不发请求）。
    #[test]
    fn rejects_empty_config() {
        let provider = OpenAiCompatible {
            base_url: " ".into(),
            api_key: " ".into(),
            model: "m".into(),
            timeout_secs: 5,
        };
        let error = provider.complete("p").unwrap_err();
        assert!(matches!(error, DbError::Config(_)));
    }

    /// 缺模型名也走配置错误，不会带着空 model 发出去。
    #[test]
    fn rejects_empty_model() {
        let provider = OpenAiCompatible {
            base_url: "https://example.com/v1".into(),
            api_key: "k".into(),
            model: "  ".into(),
            timeout_secs: 5,
        };
        let error = provider.complete("p").unwrap_err();
        assert!(matches!(error, DbError::Config(_)));
    }

    /// 规则选动作：有错误上下文 → fix；只有 SQL → explain；都没有 → generate；
    /// 空白输入等同缺省。
    #[test]
    fn pick_action_uses_error_context() {
        assert_eq!(
            pick_action(Some("SELECT 1"), Some("syntax error")),
            AiAction::Fix
        );
        assert_eq!(pick_action(Some("SELECT 1"), None), AiAction::Explain);
        assert_eq!(pick_action(None, None), AiAction::Generate);
        assert_eq!(pick_action(Some("  "), None), AiAction::Generate);
        assert_eq!(
            pick_action(Some("SELECT 1"), Some("   ")),
            AiAction::Explain
        );
    }

    /// 三个动作的系统提示词各不相同，且指向各自的产出形态。
    #[test]
    fn action_system_prompts_differ() {
        let [generate, fix, explain] = AiAction::ALL.map(AiAction::system_prompt);
        assert!(generate.contains("SQL"));
        assert!(fix.contains("error"));
        assert!(explain.contains("explain"));
        assert_ne!(generate, fix);
        assert_ne!(fix, explain);
        assert_ne!(generate, explain);
    }

    /// `fix` 的提示词必须把**原 SQL** 与**错误原文**都注入正文（lastError 注入的落点）。
    #[test]
    fn fix_prompt_injects_sql_and_error() {
        let prompt = build_action_prompt(
            AiAction::Fix,
            "users(id INT)",
            "",
            Some("SELECT * FROM users WHERE"),
            Some("near \"WHERE\": syntax error"),
        );
        assert!(prompt.contains("users(id INT)"));
        assert!(prompt.contains("SELECT * FROM users WHERE"));
        assert!(prompt.contains("near \"WHERE\": syntax error"));
    }

    /// `explain` 的提示词带 SQL 与错误，但没有「产出 SQL」的指令。
    #[test]
    fn explain_prompt_carries_context() {
        let prompt = build_action_prompt(
            AiAction::Explain,
            "t(id INT)",
            "",
            Some("SELECT * FROM t"),
            Some("permission denied for table t"),
        );
        assert!(prompt.contains("SELECT * FROM t"));
        assert!(prompt.contains("permission denied for table t"));
        assert!(prompt.contains("Explain"));
        assert!(!prompt.contains("Reply with SQL only"));
    }
}

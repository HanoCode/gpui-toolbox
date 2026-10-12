//! AI text-to-SQL：OpenAI 兼容 `/chat/completions`（ureq，零新增依赖）。
//!
//! 设计要点（`docs/database-design.md` §4.8）：Provider trait 先立住，首版只实现
//! OpenAI 兼容协议（OpenAI / DeepSeek / Moonshot / 本地 vLLM 都吃这套）；**安全闸门
//! 在调用方**——模型产出的 SQL 必须过 `sqlintel` 风险分级，只读才允许自动执行，
//! 写语句一律退回用户编辑器人工审批。本模块不做审批、不执行 SQL。

use crate::error::DbError;

/// 一次 AI 补全的产出。
#[derive(Debug, Clone)]
pub struct AiCompletion {
    /// 模型回复正文（约定为一条 SQL 语句；解析由调用方做）。
    pub sql: String,
    /// 本次请求消耗的总 token 数（拿不到时为 0）。
    pub total_tokens: u32,
}

/// AI 提供方契约（M3 扩展多家实现时的公共面）。
pub trait AiProvider: Send {
    /// 发一段提示词，拿回 SQL 文本与用量。
    fn complete(&self, prompt: &str) -> Result<AiCompletion, DbError>;
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
    fn complete(&self, prompt: &str) -> Result<AiCompletion, DbError> {
        if self.base_url.trim().is_empty() || self.api_key.trim().is_empty() {
            return Err(DbError::Config(
                "AI provider base_url/api_key is empty".into(),
            ));
        }
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let body = serde_json::json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": SYSTEM_PROMPT},
                {"role": "user", "content": prompt},
            ],
            "temperature": 0.0,
        });
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .timeout_global(Some(std::time::Duration::from_secs(
                    self.timeout_secs.max(1),
                )))
                .build(),
        );
        // 本仓 ureq 未开 `json` 特性：手工序列化 + Content-Type，能力等价。
        let payload = serde_json::to_vec(&body)
            .map_err(|error| DbError::Query(format!("AI request encode failed: {error}")))?;
        let mut response = agent
            .post(&url)
            .header("Authorization", &format!("Bearer {}", self.api_key.trim()))
            .header("Content-Type", "application/json")
            .send(payload.as_slice())
            .map_err(|error| DbError::Query(format!("AI request failed: {error}")))?;
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|error| DbError::Query(format!("AI response read failed: {error}")))?;
        let json: serde_json::Value = serde_json::from_str(&text)
            .map_err(|error| DbError::Query(format!("AI response parse failed: {error}")))?;
        let content = json
            .pointer("/choices/0/message/content")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        if content.is_empty() {
            return Err(DbError::Query("AI response has no message content".into()));
        }
        let total_tokens = json
            .pointer("/usage/total_tokens")
            .and_then(serde_json::Value::as_u64)
            .and_then(|tokens| u32::try_from(tokens).ok())
            .unwrap_or_default();
        Ok(AiCompletion {
            sql: content,
            total_tokens,
        })
    }
}

/// 系统提示词：只产 SQL，不解释，多条用分号分隔。
const SYSTEM_PROMPT: &str = "You are a senior SQL assistant. Given a database schema and a \
question in any language, reply with ONLY the SQL statement (or statements separated by \
semicolons) that answers the question. Prefer read-only SELECT queries. Never wrap the \
answer in markdown fences. No explanations.";

/// 组 text-to-SQL 提示词（schema 摘要 + 用户问题）。
#[must_use]
pub fn build_prompt(schema_summary: &str, question: &str) -> String {
    format!("Database schema:\n{schema_summary}\n\nQuestion: {question}\n\nReply with SQL only.")
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
}

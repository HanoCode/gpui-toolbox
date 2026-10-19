//! 金融工具族内核（`duck.finance.*`，计划 §6.10／里程碑 M6）。
//!
//! 两个工具、一份内核：
//!
//! | 工具 | 模块 | 参考仓来源 |
//! | --- | --- | --- |
//! | PDM 阅读器 | [`pdm`] | `src-tauri/src/pdm/`（`parser` / `sql_gen` / `export`，1,344 行里的只读部分） |
//! | 渠道网关模型生成 | [`channel_gateway`] | `src-tauri/src/channel_gateway/`（`excel_parser` / `code_gen` / `mod.rs`，2,360 行） |
//!
//! 纪律与前几族一致：**零 gpui、零 tauri、全同步、内核不出中文**。错误走枚举 /
//! 稳定码，整句与细节由插件层（`crates/duck-finance-tool`）按当前语言拼。
//!
//! ## 这一族砍掉了什么
//!
//! - `pdm/db_import.rs`（437 行）与 `pdm/generator.rs`（121 行）：从数据库连接反向
//!   导出 PDM。本仓的 process 轨拿不到数据库连接池（也没迁数据库工具），
//!   且这两个文件只服务「生成 PDM」这一个动作，与「阅读器」无关。
//! - `channel_gateway/excel_parser::generate_template_excel`：写一份 `.xlsx` 模板，
//!   参考仓用 `rust_xlsxwriter`。为一个「发一份空表给银行填」的动作引新依赖不值得，
//!   插件侧改为用 `headers` 动作把期望的 sheet / 列布局打成文本。
//!
//! ⚠️ **`channel_gateway/db.rs`（710 行）不再是「砍掉」，而是换了个载体**：
//! 它那套 SQLite（接口库 / 版本快照 / 变更记录 / 自定义模板）在 [`channel_gateway::store`]
//! 里用「一个目录 + 几个 JSON 文件」完整复刻了——读写语义逐条对齐，只是没有事务与索引。
//! 理由见那个模块的文档头：真实体量是「几十个接口 × 几十个字段」，一份 JSON 几百 KB。
//!
//! ## 两处主动修复（参考仓的真 bug，不是风格差异）
//!
//! 1. **建表 SQL 的表名**：源仓 `sql_gen.rs` 先写 `CREATE TABLE {q} (` 再写
//!    `\n    {code} (\n` —— 两个左括号、一个右括号，且表名根本没进语句
//!    （产出形如 `CREATE TABLE " ( / T_ACCOUNT (`）。这里按显而易见的意图
//!    写成 `CREATE TABLE {q}{code}{q} (`，其余排版（缩进、主键行、`;`）保持不变。
//! 2. **数组字段的 ESB 名**：`post_process_arrays` 把起始行换成数组字段时，源仓把
//!    `esb_field_name` 清成空串，Bank 模板又不吐 `esb_name`（只有 Host 模板用），
//!    所以对 Bank 侧无影响；这里原样保留，避免改变 Host 侧已依赖的行为。
//!
//! ## 生成的代码里为什么还有中文
//!
//! 两个 Java 模板（`channel_gateway/templates/*.java.tera`）逐字节复制自参考仓：
//! 文件头的「系统名称 / 软件版权 / 功能说明」与异常文案是**写进 `.java` 文件的内容**，
//! 属于恒生 LCPT 网关的既有约定，不是界面文案——与合并族 `merge_upd.xml` 注释头同一口径
//! （见 `duck-theme` 守卫里「含转义序列的字面量是写进文件的内容」那段说明）。

pub mod channel_gateway;
pub mod pdm;

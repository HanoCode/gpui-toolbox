//! 接口库 / 版本快照 / 自定义模板：**文件存储**（替代参考仓的 SQLite）。
//!
//! 参考仓把这三样存在自己的 `channel_gateway.db` 里（`rusqlite`，6 张表）。
//! 本仓不引数据库：插件只有一个字符串 KV（ADR-4，且宿主侧还没接），关系模型塞不进去；
//! 而这三样东西的**真实体量**是「几十个接口 × 几十个字段」——一份 JSON 几百 KB，
//! 用不着事务、索引、并发写。
//!
//! 所以换成一个目录 + 几个 JSON 文件：
//!
//! ```text
//! <root>/
//!   banks.json                  银行清单（元信息，不含接口）
//!   banks/<code>.json           一家的全部接口（含字段）
//!   banks/<code>.versions.json  这家的版本快照（含整份接口，便于任意两版对比）
//!   templates.json              自定义模板
//! ```
//!
//! 为什么版本快照存**整份**而不是逐接口散表：参考仓的 `cg_version_snapshots` 一行一个接口，
//! 对比时再按 `function_id` 拼回去。整份存的读法更简单（不用 join），代价是同一接口在
//! 多个版本里重复——对几十个接口的项目，这点冗余远小于「拼错了却看不出来」的风险。
//!
//! ## 三条硬约束
//!
//! 1. **`bank_code` 直接当文件名**，所以必须过 [`check_code`]：只许 ASCII 字母数字与
//!    `_` `-`。不校验的话 `../../etc/x` 能写到数据目录外面去。
//! 2. **写文件一律原子**（写 `.tmp` 再 `rename`）：插件进程可能被用户随时取消并
//!    `SIGKILL`，半截 JSON 会让接口库整个读不出来。
//! 3. **内核不出中文**：变更记录里的「请求 / 应答」是枚举（[`Direction`]），
//!    自动备份的版本名由插件层给（`backup_prefix`）。数据里落下的中文只可能是
//!    用户自己输入的内容。

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// `Direction` 复用 `mod.rs` 的那一份（不是这里再定义一个）：`xml_template` 也用它，
// 两处各定义一次的话 `Direction::Request` 会变成两个不同类型，混用时报的错是
// 「expected Direction, found Direction」，比不报错还难查。
use super::{Direction, GatewayError, InterfaceDef};

/// 内置模板的两个文件名（与参考仓 `code_gen.rs` 的常量同名）。
pub const BUILTIN_TEMPLATES: [&str; 2] = ["BankImpl.java", "HostImpl.java"];

/// 一家银行（元信息）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BankMeta {
    pub bank_code: String,
    #[serde(default)]
    pub bank_name: String,
    #[serde(default)]
    pub package_base: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

/// 一家银行的完整工作区：元信息 + 全部接口。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Bank {
    #[serde(flatten)]
    pub meta: BankMeta,
    #[serde(default)]
    pub interfaces: Vec<InterfaceDef>,
}

/// 一次版本快照。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Version {
    pub id: u64,
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub interfaces: Vec<InterfaceDef>,
}

/// 一个模板。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Template {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub content: String,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    /// 内置模板标记。**不落盘**：`templates.json` 里只存用户自己的模板，
    /// 内置的两份来自二进制里的 `.tera` 源串，读的时候现合并。
    #[serde(skip)]
    pub builtin: bool,
}

fn default_language() -> String {
    "java".to_owned()
}

/// 变更的种类（对应参考仓 `ChangeRecord.change_type` 的五个字符串）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// 整个接口新增。
    InterfaceAdded,
    /// 整个接口消失。
    InterfaceRemoved,
    /// 某个字段新增。
    FieldAdded,
    /// 某个字段消失。
    FieldRemoved,
    /// 字段还在，但类型 / ESB 名变了。
    FieldChanged,
}

/// 字段级变更改的是哪个属性。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldAttr {
    DataType,
    EsbName,
    Meaning,
}

/// 一条变更记录。
///
/// 参考仓用五个 `String` 装下所有情况（`change_type` 是字符串，`field_name` 里塞
/// 「请求:serialNo.类型」这种拼好的路径）。这里拆成枚举 + 可选字段：插件层要拼
/// 中英双语，靠字符串前缀反解会很难看，且一旦措辞改了就解不出来。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub function_id: String,
    pub kind: ChangeKind,
    /// 字段级变更才有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<Direction>,
    /// 字段级变更才有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_name: Option<String>,
    /// `FieldChanged` 才有。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attr: Option<FieldAttr>,
    #[serde(default)]
    pub old_value: String,
    #[serde(default)]
    pub new_value: String,
    /// 接口级变更才有：`(请求字段数, 应答字段数)`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_counts: Option<(usize, usize)>,
}

/// 一条「改字段」的意图。
///
/// 全是 `Option`：只想改类型就只给 `data_type`，其余留 `None`（不是空串——空串是
/// 「把它改成空」，两者语义完全不同）。工具层用 `params.trim()` 判空后转换。
#[derive(Debug, Clone, Copy, Default)]
pub struct FieldEdit<'a> {
    /// 交易码；空 = 所有接口里同名字段都改（批量修正常见）。
    pub function_id: &'a str,
    /// 字段名（`field_name`，不是 ESB 名）。
    pub field_name: &'a str,
    /// 只改某一侧；`None` = 两侧都改。
    pub direction: Option<Direction>,
    pub data_type: Option<&'a str>,
    pub esb_field_name: Option<&'a str>,
    pub meaning: Option<&'a str>,
    pub required: Option<bool>,
}

/// 一条批量改字段的补丁（拥有所有权）。
///
/// 与 [`FieldEdit`] 语义一致，只是**能跨层传**：它来自宿主的表格编辑
/// （`params.__edits`），要经过 宿主 → 插件 → 内核 三跳，借不到引用。
/// `None` = 不改；`Some("")` = 改成空串（两件事，别混）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldPatch {
    /// 交易码；空 = 所有接口里同名字段都改。
    pub function_id: String,
    /// 字段名（`field_name`，不是 ESB 名）。
    pub field_name: String,
    /// 只改某一侧；`None` = 两侧都改。
    pub direction: Option<Direction>,
    pub data_type: Option<String>,
    pub meaning: Option<String>,
}

/// 数据目录句柄。
///
/// 每次动作调用现建一个，不缓存任何状态——插件是**一次一进程**，
/// 缓存只会变成「下次进程里读到的是上次的内存快照」。
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn banks_file(&self) -> PathBuf {
        self.root.join("banks.json")
    }

    fn bank_file(&self, code: &str) -> PathBuf {
        self.root.join("banks").join(format!("{code}.json"))
    }

    fn versions_file(&self, code: &str) -> PathBuf {
        self.root
            .join("banks")
            .join(format!("{code}.versions.json"))
    }

    fn templates_file(&self) -> PathBuf {
        self.root.join("templates.json")
    }

    /// 银行清单（`updated_at` 降序，与参考仓 `ORDER BY updated_at DESC` 同）。
    ///
    /// # Errors
    /// 文件读不动或不是合法 JSON。
    pub fn banks(&self) -> Result<Vec<BankMeta>, GatewayError> {
        let mut banks: Vec<BankMeta> = self.read_json(&self.banks_file())?;
        banks.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(banks)
    }

    /// 读一家银行的接口清单；不存在返回 `None`（不是错误——「还没存过」是常态）。
    ///
    /// # Errors
    /// 文件读不动或不是合法 JSON。
    pub fn bank(&self, code: &str) -> Result<Option<Bank>, GatewayError> {
        check_code(code)?;
        let path = self.bank_file(code);
        if !path.is_file() {
            return Ok(None);
        }
        self.read_json(&path).map(Some)
    }

    /// 把一批接口存进某家银行，返回**相对旧数据的变更**。
    ///
    /// 与参考仓 `save_interfaces` 一致：旧数据存在且确有变更时，先给旧数据打一份
    /// 快照（`backup_prefix` + 时间戳），再覆盖。
    ///
    /// # Errors
    /// `bank_code` 不合法、读旧数据失败、写盘失败。
    pub fn save_bank(
        &self,
        meta: BankMeta,
        interfaces: &[InterfaceDef],
        backup_prefix: &str,
    ) -> Result<Vec<Change>, GatewayError> {
        check_code(&meta.bank_code)?;
        let existing = self.bank(&meta.bank_code)?;
        let changes = existing
            .as_ref()
            .map(|old| diff_interfaces(&old.interfaces, interfaces))
            .unwrap_or_default();

        // 「旧数据非空且确有变更」才备份：首次导入不备份（没有可备份的东西），
        // 原样重存同一份也不备份（否则每点一次保存就多一个版本）。
        //
        // ⚠️ 写成 `filter` + `if let` 而不是 `if A && let Some(x) = b`：后者是
        // `let_chains`，2024 起稳定但**需要 1.88**，本仓 MSRV 硬门是 1.85。
        let backup_source = existing
            .as_ref()
            .filter(|old| !old.interfaces.is_empty())
            .filter(|_| !changes.is_empty());
        if let Some(old) = backup_source {
            let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
            let mut versions = self.versions(&meta.bank_code)?;
            let id = versions.iter().map(|v| v.id).max().unwrap_or(0) + 1;
            versions.push(Version {
                id,
                label: format!("{backup_prefix}-{stamp}"),
                description: backup_prefix.to_owned(),
                created_at: now(),
                interfaces: old.interfaces.clone(),
            });
            self.write_json(&self.versions_file(&meta.bank_code), &versions)?;
        }

        let created_at = existing
            .as_ref()
            .map(|old| old.meta.created_at.clone())
            .filter(|stamp| !stamp.is_empty())
            .unwrap_or_else(now);
        let bank = Bank {
            meta: BankMeta {
                bank_code: meta.bank_code.clone(),
                bank_name: meta.bank_name,
                package_base: meta.package_base,
                created_at,
                updated_at: now(),
            },
            interfaces: interfaces.to_vec(),
        };
        self.write_json(&self.bank_file(&meta.bank_code), &bank)?;

        // 索引与正体分两个文件 ⇒ 顺序是「先正体、后索引」：反过来的话，
        // 正体写失败时索引里会多一条指向空文件的记录，界面上点进去就是空的。
        let mut banks = self.banks()?;
        banks.retain(|entry| entry.bank_code != bank.meta.bank_code);
        banks.push(bank.meta);
        self.write_json(&self.banks_file(), &banks)?;
        Ok(changes)
    }

    /// 删掉一家银行（接口 + 版本 + 索引项）。返回是否真的删了。
    ///
    /// # Errors
    /// `bank_code` 不合法或写盘失败。
    pub fn delete_bank(&self, code: &str) -> Result<bool, GatewayError> {
        check_code(code)?;
        let mut banks = self.banks()?;
        let before = banks.len();
        banks.retain(|bank| bank.bank_code != code);
        if banks.len() == before {
            return Ok(false);
        }
        self.write_json(&self.banks_file(), &banks)?;
        // 正体与版本**删不动不算失败**：索引已经没了，界面上也点不进去。
        // 这里报错只会让用户以为删除失败，实际是「删了但留了几个孤儿文件」。
        let _ = fs::remove_file(self.bank_file(code));
        let _ = fs::remove_file(self.versions_file(code));
        Ok(true)
    }

    /// 某家银行的全部版本（`id` 降序，与参考仓 `ORDER BY id DESC` 同）。
    ///
    /// # Errors
    /// `bank_code` 不合法或文件读不动。
    pub fn versions(&self, code: &str) -> Result<Vec<Version>, GatewayError> {
        check_code(code)?;
        let mut versions: Vec<Version> = self.read_json(&self.versions_file(code))?;
        // 按 `id` 降序（与参考仓 `ORDER BY id DESC` 同）：`Reverse` 比 `sort_by(|a, b|
        // b.id.cmp(&a.id))` 少一次手工比较，语义一样。
        versions.sort_by_key(|version| std::cmp::Reverse(version.id));
        Ok(versions)
    }

    /// 给某家银行**当前已存的接口**打一个快照。
    ///
    /// `label` 留空 = 用时间戳（`YYYYmmdd-HHMMSS`，与自动备份同一格式）：
    /// 时间戳在**内核**里取，因为时间依赖只在这里（`chrono`），
    /// 工具层为了一个文件名再引一次日期库不值得。
    ///
    /// # Errors
    /// 银行不存在（`NoSuchBank`）、`bank_code` 不合法或写盘失败。
    pub fn snapshot(
        &self,
        code: &str,
        label: &str,
        description: &str,
    ) -> Result<Version, GatewayError> {
        let bank = self
            .bank(code)?
            .ok_or_else(|| GatewayError::NoSuchBank(code.to_owned()))?;
        let mut versions = self.versions(code)?;
        let version = Version {
            id: versions.iter().map(|v| v.id).max().unwrap_or(0) + 1,
            label: if label.trim().is_empty() {
                chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
            } else {
                label.trim().to_owned()
            },
            description: description.to_owned(),
            created_at: now(),
            interfaces: bank.interfaces,
        };
        versions.push(version.clone());
        self.write_json(&self.versions_file(code), &versions)?;
        Ok(version)
    }

    /// 取某个版本（含整份接口）。
    ///
    /// # Errors
    /// `bank_code` 不合法或文件读不动。
    pub fn version(&self, code: &str, id: u64) -> Result<Option<Version>, GatewayError> {
        Ok(self.versions(code)?.into_iter().find(|v| v.id == id))
    }

    /// 改库里一个字段（类型 / ESB 名 / 含义 / 必填），返回改到了几处。
    ///
    /// 这条对应参考仓 `ChannelGatewayFieldTable.vue` 的「就地改」：那个页面允许在表格里
    /// 逐行改 `data_type` / `meaning` 再生成代码。本仓没有可编辑表格（`[io]` 的 `table`
    /// 输出只支持排序与导 CSV），于是把它收成一次运行——改的是**库里的数据，不动 Excel**，
    /// 正好是「银行给的原始表不动，我们自己补一层修正」的用法。
    ///
    /// 返回 0 表示没命中：这时候**不写盘**（避免把 `updated_at` 白翻一次，
    /// 让清单里的排序莫名其妙地变）。
    ///
    /// # Errors
    /// 银行不存在、`bank_code` 不合法或写盘失败。
    pub fn edit_field(&self, code: &str, edit: &FieldEdit<'_>) -> Result<usize, GatewayError> {
        let Some(mut bank) = self.bank(code)? else {
            return Err(GatewayError::NoSuchBank(code.to_owned()));
        };
        let mut hits = 0usize;
        for iface in &mut bank.interfaces {
            if !edit.function_id.is_empty() && iface.function_id != edit.function_id {
                continue;
            }
            let both = iface
                .request_fields
                .iter_mut()
                .map(|field| (Direction::Request, field))
                .chain(
                    iface
                        .response_fields
                        .iter_mut()
                        .map(|field| (Direction::Response, field)),
                );
            for (direction, field) in both {
                if field.field_name != edit.field_name {
                    continue;
                }
                // 方向给了就必须对上：同一个字段名在请求与应答里各有一份是常态
                // （`serialNo` 两边都有），不区分就会一次改掉两条。
                if edit.direction.is_some_and(|wanted| wanted != direction) {
                    continue;
                }
                if let Some(value) = edit.data_type {
                    field.data_type = value.to_owned();
                }
                if let Some(value) = edit.esb_field_name {
                    field.esb_field_name = value.to_owned();
                }
                if let Some(value) = edit.meaning {
                    field.meaning = value.to_owned();
                }
                if let Some(value) = edit.required {
                    field.required = value;
                }
                hits += 1;
            }
        }
        if hits == 0 {
            return Ok(0);
        }
        bank.meta.updated_at = now();
        self.write_json(&self.bank_file(code), &bank)?;
        // 索引里的 `updated_at` 也要跟上：清单按它降序排，不同步就会把刚改过的
        // 这家排到改之前的位置去，看起来像「没保存成功」。
        let mut banks = self.banks()?;
        banks.retain(|entry| entry.bank_code != bank.meta.bank_code);
        banks.push(bank.meta);
        self.write_json(&self.banks_file(), &banks)?;
        Ok(hits)
    }

    /// 一次批量改字段（宿主表格编辑 `params.__edits` 的落地口）。
    ///
    /// 与 [`Self::edit_field`] 语义完全一致，区别只有一个：**读一次、写一次**。
    /// 逐条调 `edit_field` 也能改对，但 50 个格子的编辑会写 50 遍正体与索引，
    /// 每遍都要重写整份银行文件——用户点一次「应用」等半秒就不该有。
    ///
    /// 未命中的 patch 静默跳过（表格里改错一行的列名，不该让整批失败），
    /// 返回真正改到的处数。
    ///
    /// # Errors
    /// 银行不存在、`bank_code` 不合法或写盘失败。
    pub fn apply_patches(&self, code: &str, patches: &[FieldPatch]) -> Result<usize, GatewayError> {
        if patches.is_empty() {
            return Ok(0);
        }
        let Some(mut bank) = self.bank(code)? else {
            return Err(GatewayError::NoSuchBank(code.to_owned()));
        };
        let mut hits = 0usize;
        for patch in patches {
            for iface in &mut bank.interfaces {
                if !patch.function_id.is_empty() && iface.function_id != patch.function_id {
                    continue;
                }
                let both = iface
                    .request_fields
                    .iter_mut()
                    .map(|field| (Direction::Request, field))
                    .chain(
                        iface
                            .response_fields
                            .iter_mut()
                            .map(|field| (Direction::Response, field)),
                    );
                for (direction, field) in both {
                    if field.field_name != patch.field_name {
                        continue;
                    }
                    // 方向给了就必须对上：同名同改是常态，不区分会一次改掉两条。
                    if patch.direction.is_some_and(|wanted| wanted != direction) {
                        continue;
                    }
                    if let Some(value) = &patch.data_type {
                        field.data_type = value.clone();
                    }
                    if let Some(value) = &patch.meaning {
                        field.meaning = value.clone();
                    }
                    hits += 1;
                }
            }
        }
        if hits == 0 {
            return Ok(0);
        }
        bank.meta.updated_at = now();
        self.write_json(&self.bank_file(code), &bank)?;
        let mut banks = self.banks()?;
        banks.retain(|entry| entry.bank_code != bank.meta.bank_code);
        banks.push(bank.meta);
        self.write_json(&self.banks_file(), &banks)?;
        Ok(hits)
    }

    /// 全部模板：用户存的在前（按 name 升序），内置的补在后面。
    ///
    /// 同名的以**用户那份**为准（参考仓 `list_templates` 也是「`result` 里已有就不补内置」），
    /// 这样「把内置模板另存一份改掉」在列表里不会出现两条一模一样名字的。
    ///
    /// # Errors
    /// `templates.json` 读不动。
    pub fn templates(&self) -> Result<Vec<Template>, GatewayError> {
        let mut templates: Vec<Template> = self.read_json(&self.templates_file())?;
        templates.sort_by(|a, b| a.name.cmp(&b.name));
        for name in BUILTIN_TEMPLATES {
            if !templates.iter().any(|t| t.name == name) {
                templates.push(builtin_template(name));
            }
        }
        Ok(templates)
    }

    /// 按名字取模板。**内置优先**——与参考仓 `get_template` 一致：
    /// 内置的名字是写死的两种 Java 侧，用户就算存了同名文件也盖不掉它。
    ///
    /// # Errors
    /// `templates.json` 读不动。
    pub fn template(&self, name: &str) -> Result<Option<Template>, GatewayError> {
        if BUILTIN_TEMPLATES.contains(&name) {
            return Ok(Some(builtin_template(name)));
        }
        Ok(self.templates()?.into_iter().find(|t| t.name == name))
    }

    /// 存一个模板（同名覆盖，`created_at` 保留原值）。
    ///
    /// # Errors
    /// 名字不合法（空 / 带路径分隔符）、或写盘失败。
    pub fn save_template(&self, mut template: Template) -> Result<(), GatewayError> {
        check_template_name(&template.name)?;
        let mut templates: Vec<Template> = self.read_json(&self.templates_file())?;
        // 覆盖同一项时保留 created_at：只改内容不该把「建于」也一起刷新。
        template.created_at = templates
            .iter()
            .find(|t| t.name == template.name)
            .map(|t| t.created_at.clone())
            .filter(|stamp| !stamp.is_empty())
            .unwrap_or_else(now);
        template.updated_at = now();
        templates.retain(|t| t.name != template.name);
        templates.push(template);
        self.write_json(&self.templates_file(), &templates)
    }

    /// 删一个模板。内置的不许删（返回 `Ok(false)`），与参考仓一致——
    /// 那边内置模板根本不在表里，删了也没反应。
    ///
    /// # Errors
    /// 写盘失败。
    pub fn delete_template(&self, name: &str) -> Result<bool, GatewayError> {
        if BUILTIN_TEMPLATES.contains(&name) {
            return Ok(false);
        }
        let mut templates: Vec<Template> = self.read_json(&self.templates_file())?;
        let before = templates.len();
        templates.retain(|t| t.name != name);
        if templates.len() == before {
            return Ok(false);
        }
        self.write_json(&self.templates_file(), &templates)?;
        Ok(true)
    }

    /// 读一份 JSON；**文件不存在当作空**（首次运行的常态）。
    fn read_json<T: for<'de> Deserialize<'de> + Default>(
        &self,
        path: &Path,
    ) -> Result<T, GatewayError> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| GatewayError::StoreParse(format!("{}: {error}", path.display()))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
            Err(error) => Err(GatewayError::StoreRead(format!(
                "{}: {error}",
                path.display()
            ))),
        }
    }

    /// 原子写一份 JSON（缩进 2 空格：这文件用户可能直接打开看）。
    fn write_json<T: Serialize>(&self, path: &Path, value: &T) -> Result<(), GatewayError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                GatewayError::StoreWrite(format!("{}: {error}", parent.display()))
            })?;
        }
        let body = serde_json::to_vec_pretty(value)
            .map_err(|error| GatewayError::StoreWrite(error.to_string()))?;
        // `.tmp` 与目标同目录：跨目录 rename 不是原子的（还可能是跨文件系统）。
        // 后缀带 pid 是防两个插件进程同时写——它们写的是同一份文件，
        // 固定名字会让两个进程抢同一个临时文件，结果是一份混合内容。
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        fs::write(&tmp, &body)
            .map_err(|error| GatewayError::StoreWrite(format!("{}: {error}", tmp.display())))?;
        fs::rename(&tmp, path).map_err(|error| {
            let _ = fs::remove_file(&tmp);
            GatewayError::StoreWrite(format!("{}: {error}", path.display()))
        })
    }
}

/// `bank_code` 会变成文件名 ⇒ 只许 ASCII 字母数字与 `_` `-`，长度 1..=64。
///
/// 挡住的是 `../../x` 这种「写到数据目录外面去」，以及 `a/b` 这种「悄悄建子目录」。
/// 银行代码本来就是 `hzcb` / `boc` 这类短标识，限制不影响真实用法。
fn check_code(code: &str) -> Result<(), GatewayError> {
    let trimmed = code.trim();
    let ok = !trimmed.is_empty()
        && trimmed.len() <= 64
        && trimmed
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(GatewayError::BadBankCode(code.to_owned()))
    }
}

/// 模板名会变成 `templates.json` 里的键（不落成文件名），但仍挡住路径分隔符与空名：
/// 空名在界面上是一行看不见的模板，带斜杠的名字纯属误用。
fn check_template_name(name: &str) -> Result<(), GatewayError> {
    let trimmed = name.trim();
    let ok = !trimmed.is_empty()
        && trimmed.len() <= 120
        && !trimmed.contains(['/', '\\'])
        && trimmed != ".."
        && trimmed != ".";
    if ok {
        Ok(())
    } else {
        Err(GatewayError::BadTemplateName(name.to_owned()))
    }
}

/// 内置模板（内容来自二进制里的 `.tera` 源串）。
fn builtin_template(name: &str) -> Template {
    let content = if name == "HostImpl.java" {
        super::HOST_TEMPLATE_SOURCE
    } else {
        super::BANK_TEMPLATE_SOURCE
    };
    Template {
        name: name.to_owned(),
        // 描述留空：内核不出中文，界面上由插件层按 `builtin` 标记给一句译文。
        description: String::new(),
        content: content.to_owned(),
        language: default_language(),
        created_at: String::new(),
        updated_at: String::new(),
        builtin: true,
    }
}

fn now() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 两批接口的差异（对应参考仓 `diff_interface_lists`）。
///
/// 与参考仓唯一的差别是**顺序稳定**：那边遍历 `HashMap` / `HashSet`，每次跑出来的
/// 变更顺序都不一样；这里按 `function_id` 排序后再比，字段按原顺序比。
/// 语义没变，只是让「同一份输入跑两次」的输出可以直接 diff。
pub fn diff_interfaces(old: &[InterfaceDef], new: &[InterfaceDef]) -> Vec<Change> {
    let old_map: std::collections::BTreeMap<&str, &InterfaceDef> =
        old.iter().map(|i| (i.function_id.as_str(), i)).collect();
    let new_map: std::collections::BTreeMap<&str, &InterfaceDef> =
        new.iter().map(|i| (i.function_id.as_str(), i)).collect();
    let all_ids: BTreeSet<&str> = old_map.keys().chain(new_map.keys()).copied().collect();

    let mut changes = Vec::new();
    for function_id in all_ids {
        match (old_map.get(function_id), new_map.get(function_id)) {
            (None, Some(newer)) => changes.push(Change {
                function_id: function_id.to_owned(),
                kind: ChangeKind::InterfaceAdded,
                direction: None,
                field_name: None,
                attr: None,
                old_value: String::new(),
                new_value: String::new(),
                field_counts: Some((newer.request_fields.len(), newer.response_fields.len())),
            }),
            (Some(older), None) => changes.push(Change {
                function_id: function_id.to_owned(),
                kind: ChangeKind::InterfaceRemoved,
                direction: None,
                field_name: None,
                attr: None,
                old_value: String::new(),
                new_value: String::new(),
                field_counts: Some((older.request_fields.len(), older.response_fields.len())),
            }),
            (Some(older), Some(newer)) => {
                compare_fields(
                    &mut changes,
                    function_id,
                    Direction::Request,
                    &older.request_fields,
                    &newer.request_fields,
                );
                compare_fields(
                    &mut changes,
                    function_id,
                    Direction::Response,
                    &older.response_fields,
                    &newer.response_fields,
                );
            }
            (None, None) => {}
        }
    }
    changes
}

/// 两个版本之间的差异（对应参考仓 `compare_versions`）。
///
/// 与 [`diff_interfaces`] 的区别只在入参形态：那边比的是「库里的 vs 刚解析的」，
/// 这边比的是「两版快照」。算法完全一样，所以直接借道——
/// 分开写迟早会有一边漏掉「ESB 名变更」。
pub fn diff_versions(older: &Version, newer: &Version) -> Vec<Change> {
    diff_interfaces(&older.interfaces, &newer.interfaces)
}

/// 单个接口在新旧两版之间的差异（对应参考仓 `get_interface_diff`）。
pub fn diff_one_interface(
    function_id: &str,
    older: Option<&InterfaceDef>,
    newer: Option<&InterfaceDef>,
) -> Vec<Change> {
    let mut changes = Vec::new();
    let wrap = |iface: &InterfaceDef| vec![iface.clone()];
    match (older, newer) {
        (None, Some(newer)) => changes.extend(diff_interfaces(&[], &wrap(newer))),
        (Some(older), None) => changes.extend(diff_interfaces(&wrap(older), &[])),
        (Some(older), Some(newer)) => {
            compare_fields(
                &mut changes,
                function_id,
                Direction::Request,
                &older.request_fields,
                &newer.request_fields,
            );
            compare_fields(
                &mut changes,
                function_id,
                Direction::Response,
                &older.response_fields,
                &newer.response_fields,
            );
        }
        (None, None) => {}
    }
    changes
}

/// 一组字段的差异（对应参考仓 `compare_field_list`）。
///
/// 三种变更都报：新增、删除、以及同名但**类型 / ESB 名**变了。
/// 类型变了必须报——那正是「重新导一份 Excel」最常见、也最容易漏掉的事。
fn compare_fields(
    changes: &mut Vec<Change>,
    function_id: &str,
    direction: Direction,
    old_fields: &[super::FieldDef],
    new_fields: &[super::FieldDef],
) {
    let old_map: std::collections::BTreeMap<&str, &super::FieldDef> = old_fields
        .iter()
        .map(|f| (f.field_name.as_str(), f))
        .collect();
    let new_map: std::collections::BTreeMap<&str, &super::FieldDef> = new_fields
        .iter()
        .map(|f| (f.field_name.as_str(), f))
        .collect();

    let field_change = |kind: ChangeKind,
                        name: &str,
                        attr: Option<FieldAttr>,
                        old_value: String,
                        new_value: String| Change {
        function_id: function_id.to_owned(),
        kind,
        direction: Some(direction),
        field_name: Some(name.to_owned()),
        attr,
        old_value,
        new_value,
        field_counts: None,
    };

    for field in new_fields {
        if !old_map.contains_key(field.field_name.as_str()) {
            changes.push(field_change(
                ChangeKind::FieldAdded,
                &field.field_name,
                None,
                String::new(),
                field.data_type.clone(),
            ));
        }
    }
    for field in old_fields {
        if !new_map.contains_key(field.field_name.as_str()) {
            changes.push(field_change(
                ChangeKind::FieldRemoved,
                &field.field_name,
                None,
                field.data_type.clone(),
                String::new(),
            ));
        }
    }
    for field in new_fields {
        let Some(older) = old_map.get(field.field_name.as_str()) else {
            continue;
        };
        if older.data_type != field.data_type {
            changes.push(field_change(
                ChangeKind::FieldChanged,
                &field.field_name,
                Some(FieldAttr::DataType),
                older.data_type.clone(),
                field.data_type.clone(),
            ));
        }
        if older.esb_field_name != field.esb_field_name {
            changes.push(field_change(
                ChangeKind::FieldChanged,
                &field.field_name,
                Some(FieldAttr::EsbName),
                older.esb_field_name.clone(),
                field.esb_field_name.clone(),
            ));
        }
        // 含义也报：`field-edit` 能改它，改了却不报，用户会以为「没生效」。
        // 参考仓的 `compare_field_list` 只比类型与 ESB 名——那边没有「改含义再落库」
        // 的入口，所以少这一条不影响它；这边有，就得报。
        if older.meaning != field.meaning {
            changes.push(field_change(
                ChangeKind::FieldChanged,
                &field.field_name,
                Some(FieldAttr::Meaning),
                older.meaning.clone(),
                field.meaning.clone(),
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channel_gateway::FieldDef;

    fn field(name: &str, data_type: &str, esb: &str) -> FieldDef {
        FieldDef {
            field_name: name.to_owned(),
            esb_field_name: esb.to_owned(),
            data_type: data_type.to_owned(),
            required: false,
            meaning: String::new(),
            is_array: false,
            array_items: Vec::new(),
        }
    }

    fn iface(id: &str, request: Vec<FieldDef>, response: Vec<FieldDef>) -> InterfaceDef {
        InterfaceDef {
            function_id: id.to_owned(),
            description: format!("{id} 描述"),
            interface_type: "bank".to_owned(),
            business_domain: String::new(),
            request_fields: request,
            response_fields: response,
        }
    }

    /// 临时目录（不引 `tempfile`：本 crate 的依赖清单是刻意收紧的）。
    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("duck-fin-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create scratch");
        dir
    }

    #[test]
    fn bank_code_that_could_escape_the_directory_is_rejected() {
        for bad in ["", "  ", "../etc", "a/b", "a\\b", "账户", &"x".repeat(65)] {
            assert!(check_code(bad).is_err(), "{bad:?} 不该被当成合法的银行代码");
        }
        for good in ["hzcb", "BOC_2024", "abc-1"] {
            assert!(check_code(good).is_ok(), "{good:?} 应当合法");
        }
    }

    #[test]
    fn saving_a_bank_writes_both_the_body_and_the_index() {
        let root = scratch("save");
        let store = Store::new(&root);
        let interfaces = vec![iface(
            "B100001",
            vec![field("accNo", "string", "accNo")],
            vec![],
        )];
        let changes = store
            .save_bank(
                BankMeta {
                    bank_code: "hzcb".into(),
                    bank_name: "杭州银行".into(),
                    package_base: "com.example".into(),
                    ..Default::default()
                },
                &interfaces,
                "backup",
            )
            .expect("save");
        // 首次导入没有旧数据可比 ⇒ 没有变更、也没有备份版本。
        assert!(changes.is_empty());
        assert!(store.versions("hzcb").expect("versions").is_empty());

        let banks = store.banks().expect("banks");
        assert_eq!(banks.len(), 1);
        assert_eq!(banks[0].bank_code, "hzcb");
        assert!(!banks[0].created_at.is_empty());

        let bank = store.bank("hzcb").expect("read").expect("exists");
        assert_eq!(bank.interfaces.len(), 1);
        // 覆盖保存不该把「建于」也刷新：那条时间戳是这家银行的历史。
        let created = banks[0].created_at.clone();
        store
            .save_bank(
                BankMeta {
                    bank_code: "hzcb".into(),
                    ..Default::default()
                },
                &interfaces,
                "backup",
            )
            .expect("resave");
        assert_eq!(store.banks().expect("banks")[0].created_at, created);
    }

    #[test]
    fn a_real_change_snapshots_the_old_data_first() {
        let root = scratch("backup");
        let store = Store::new(&root);
        let before = vec![iface(
            "B100001",
            vec![field("accNo", "string", "accNo")],
            vec![],
        )];
        let meta = BankMeta {
            bank_code: "hzcb".into(),
            ..Default::default()
        };
        store
            .save_bank(meta.clone(), &before, "backup")
            .expect("seed");

        // 类型 string → number：这正是「银行重发一份 Excel」最常见的改动。
        let after = vec![iface(
            "B100001",
            vec![field("accNo", "number", "accNo")],
            vec![],
        )];
        let changes = store
            .save_bank(meta.clone(), &after, "backup")
            .expect("save");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::FieldChanged);
        assert_eq!(changes[0].attr, Some(FieldAttr::DataType));
        assert_eq!(changes[0].direction, Some(Direction::Request));
        assert_eq!(changes[0].old_value, "string");
        assert_eq!(changes[0].new_value, "number");

        let versions = store.versions("hzcb").expect("versions");
        assert_eq!(versions.len(), 1, "变更前的旧数据应当留一个快照");
        assert!(versions[0].label.starts_with("backup-"));
        // 快照里是**旧**内容：这正是「导入前备份」的意义。
        assert_eq!(
            versions[0].interfaces[0].request_fields[0].data_type,
            "string"
        );
        // 同一份再存一次：没有变更 ⇒ 不该再多一个版本。
        store.save_bank(meta, &after, "backup").expect("resave");
        assert_eq!(store.versions("hzcb").expect("versions").len(), 1);
    }

    #[test]
    fn diff_reports_added_removed_and_both_kinds_of_field_change() {
        let older = vec![iface(
            "B1",
            vec![
                field("accNo", "string", "accNo"),
                field("gone", "string", "gone"),
            ],
            vec![],
        )];
        let newer = vec![
            iface(
                "B1",
                vec![
                    field("accNo", "number", "accNoV2"),
                    field("fresh", "string", "fresh"),
                ],
                vec![],
            ),
            iface("B2", vec![], vec![]),
        ];
        let changes = diff_interfaces(&older, &newer);
        let kinds: Vec<ChangeKind> = changes.iter().map(|c| c.kind).collect();
        assert!(kinds.contains(&ChangeKind::InterfaceAdded));
        assert!(kinds.contains(&ChangeKind::FieldAdded));
        assert!(kinds.contains(&ChangeKind::FieldRemoved));
        // 类型与 ESB 名各报一条：两条都不报的话，「ESB 名改了」会被当成没变更。
        assert_eq!(
            kinds
                .iter()
                .filter(|k| **k == ChangeKind::FieldChanged)
                .count(),
            2
        );
        // 顺序稳定：同一份输入跑两次结果必须一样（参考仓用 HashMap，做不到这点）。
        assert_eq!(changes, diff_interfaces(&older, &newer));
    }

    #[test]
    fn templates_merge_builtins_and_let_user_versions_win_the_name() {
        let root = scratch("templates");
        let store = Store::new(&root);
        let names: Vec<String> = store
            .templates()
            .expect("list")
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, ["BankImpl.java", "HostImpl.java"]);

        store
            .save_template(Template {
                name: "BankImpl.java".into(),
                content: "custom".into(),
                ..Default::default()
            })
            .expect("save");
        let all = store.templates().expect("list");
        // 同名不出现两条。
        assert_eq!(all.iter().filter(|t| t.name == "BankImpl.java").count(), 1);
        // 但按名字取仍然是**内置优先**（与参考仓 `get_template` 一致）。
        let got = store.template("BankImpl.java").expect("get").expect("some");
        assert!(got.builtin);
        // 内置模板里的 tera 占位形态（`{{ package_base }}`）。
        assert!(got.content.contains("{{"));

        // 内置的不许删。
        assert!(!store.delete_template("HostImpl.java").expect("delete"));
        store
            .save_template(Template {
                name: "My.java".into(),
                content: "x".into(),
                ..Default::default()
            })
            .expect("save");
        assert!(store.delete_template("My.java").expect("delete"));
        assert!(!store.delete_template("My.java").expect("delete again"));
    }

    #[test]
    fn deleting_a_bank_removes_it_from_the_index_and_the_list() {
        let root = scratch("delete");
        let store = Store::new(&root);
        let meta = BankMeta {
            bank_code: "hzcb".into(),
            ..Default::default()
        };
        store
            .save_bank(meta, &[iface("B1", vec![], vec![])], "backup")
            .expect("seed");
        store.snapshot("hzcb", "v1", "").expect("snapshot");

        assert!(store.delete_bank("hzcb").expect("delete"));
        assert!(store.banks().expect("banks").is_empty());
        assert!(store.bank("hzcb").expect("read").is_none());
        assert!(store.versions("hzcb").expect("versions").is_empty());
        // 删不存在的不报错，只是「没删到」。
        assert!(!store.delete_bank("hzcb").expect("delete again"));
    }

    /// 批量改：一次读一次写，未命中的跳过，命中时按 patch 逐条算。
    #[test]
    fn applying_patches_changes_every_hit_in_one_pass() {
        let root = scratch("patches");
        let store = Store::new(&root);
        store
            .save_bank(
                BankMeta {
                    bank_code: "hzcb".into(),
                    ..Default::default()
                },
                &[iface(
                    "B1",
                    vec![
                        field("tradeDate", "string", "tradeDate"),
                        field("amount", "string", "amount"),
                    ],
                    vec![field("tradeDate", "string", "tradeDate")],
                )],
                "b",
            )
            .expect("seed");

        let hits = store
            .apply_patches(
                "hzcb",
                &[
                    FieldPatch {
                        function_id: "B1".into(),
                        field_name: "tradeDate".into(),
                        direction: Some(Direction::Request),
                        data_type: Some("number".into()),
                        meaning: Some("交易日".into()),
                    },
                    FieldPatch {
                        function_id: "B1".into(),
                        field_name: "amount".into(),
                        direction: None,
                        data_type: Some("amount".into()),
                        meaning: None,
                    },
                    // 改错名字：静默跳过，不能连累整批。
                    FieldPatch {
                        function_id: "B1".into(),
                        field_name: "nope".into(),
                        direction: None,
                        data_type: Some("string".into()),
                        meaning: None,
                    },
                ],
            )
            .expect("apply");
        assert_eq!(hits, 2);

        let bank = store.bank("hzcb").expect("read").expect("bank");
        let request = &bank.interfaces[0].request_fields;
        assert_eq!(request[0].data_type, "number");
        assert_eq!(request[0].meaning, "交易日");
        assert_eq!(request[1].data_type, "amount");
        // 只改了请求侧：应答侧同名的那条必须原样。
        assert_eq!(bank.interfaces[0].response_fields[0].data_type, "string");
    }

    #[test]
    fn editing_a_field_can_target_a_single_direction() {
        let root = scratch("edit");
        let store = Store::new(&root);
        let meta = BankMeta {
            bank_code: "hzcb".into(),
            ..Default::default()
        };
        // 请求与应答各有一个 `serialNo` —— 这是 ESB 接口的真实形态，
        // 也是「不区分方向就会一次改掉两条」的由来。
        store
            .save_bank(
                meta,
                &[iface(
                    "B1",
                    vec![field("serialNo", "string", "serialNo")],
                    vec![field("serialNo", "string", "serialNo")],
                )],
                "b",
            )
            .expect("seed");

        // 不给方向 ⇒ 两侧都改。
        let hits = store
            .edit_field(
                "hzcb",
                &FieldEdit {
                    function_id: "B1",
                    field_name: "serialNo",
                    data_type: Some("number"),
                    ..Default::default()
                },
            )
            .expect("edit");
        assert_eq!(hits, 2);

        // 给了方向 ⇒ 只改那一侧。
        let hits = store
            .edit_field(
                "hzcb",
                &FieldEdit {
                    function_id: "B1",
                    field_name: "serialNo",
                    direction: Some(Direction::Response),
                    data_type: Some("amount"),
                    ..Default::default()
                },
            )
            .expect("edit one side");
        assert_eq!(hits, 1);
        let bank = store.bank("hzcb").expect("read").expect("exists");
        assert_eq!(bank.interfaces[0].request_fields[0].data_type, "number");
        assert_eq!(bank.interfaces[0].response_fields[0].data_type, "amount");

        // 没命中 ⇒ 一处都不写（`updated_at` 不能白翻一次，否则清单排序会莫名其妙地变）。
        let before = store.banks().expect("banks")[0].updated_at.clone();
        // 时间戳精确到秒：不等一下的话「同秒内重写」与「没重写」分不出来。
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let hits = store
            .edit_field(
                "hzcb",
                &FieldEdit {
                    function_id: "B1",
                    field_name: "noSuchField",
                    data_type: Some("x"),
                    ..Default::default()
                },
            )
            .expect("edit nothing");
        assert_eq!(hits, 0);
        assert_eq!(store.banks().expect("banks")[0].updated_at, before);
    }

    #[test]
    fn snapshot_pins_the_current_interfaces_under_a_new_id() {
        let root = scratch("snapshot");
        let store = Store::new(&root);
        let meta = BankMeta {
            bank_code: "hzcb".into(),
            ..Default::default()
        };
        store
            .save_bank(
                meta,
                &[iface("B1", vec![field("a", "string", "a")], vec![])],
                "b",
            )
            .expect("seed");
        let first = store.snapshot("hzcb", "v1", "初版").expect("v1");
        let second = store.snapshot("hzcb", "v2", "").expect("v2");
        assert_eq!(first.id, 1);
        assert_eq!(second.id, 2);
        // 列表按 id 降序。
        let listed = store.versions("hzcb").expect("list");
        assert_eq!(listed[0].id, 2);
        assert_eq!(
            store.version("hzcb", 1).expect("get").expect("some").label,
            "v1"
        );
        // 银行还没存过就打不了快照。
        assert!(matches!(
            store.snapshot("nope", "x", ""),
            Err(GatewayError::NoSuchBank(_))
        ));
    }
}

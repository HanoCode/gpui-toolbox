//! 表结构编辑器（`Mode::Structure`，dbx `TableStructureEditor` 对位）。
//!
//! 布局对齐 dbx：**目标条 → Tabs 卡片（DDL / 字段 / 索引 / 外键 / 触发器）→
//! SQL 预览卡 → 底部应用条**。字段标签是主编辑面（改名 / 改类型 / 可空 / 默认值 /
//! 注释 / 字符集 / 增删列 / 拖拽排序都在网格上完成，搜索框客户端过滤），其余四个
//! 标签只读——内核没有生成对应 DDL 的能力，「能编辑」的假象比没有编辑更糟（dbx
//! 同款：按能力开放）。
//!
//! 元数据口径：内核 [`duck_db::TableColumnMeta`] 给齐「可空 / 默认值 / 注释 /
//! 字符集 / 排序规则」，拿不到的属性是 `None` / 空串——UI 按引擎能力闸门画成
//! 不可编辑，**不冒充**。主键只有 MySQL 生成得了 DDL，其余引擎勾选框禁用并给
//! 口径提示；「复制其他表字段」弹窗整表候选字段（勾选后追加为新增行）。
//!
//! SQL 方言：一条改动生成一条独立 `ALTER`（逐条也是失败定位最细的写法）。
//! MySQL 能力最全（`MODIFY` + `AFTER` 物理重排 + 主键 DDL）；PG 按属性逐条出
//! 子句；SQLite 的列属性变更要重建表，如实出注释不出假语句。

use super::*;
use duck_components::{switch_control, toolbar_dropdown_button};
use duck_db::TableColumnMeta;
use gpui::Pixels;

impl DatabasePage {
    /// 结构标签的内容面：取列级元数据、主键、DDL、索引、外键、触发器（后台）。
    ///
    /// 先清旧草稿再发查询：上一张表的列名留在这一张的编辑器里就是事故，
    /// 宁可让用户看一帧「载入中」。六个元数据槽位互相独立（右栏 `TableMeta`
    /// 同款口径）：列元数据失败才报错，其余失败只影响自己那个标签。
    pub(super) fn open_structure(&mut self, table: TableRef, cx: &mut Context<Self>) {
        self.mode = Mode::Structure(table.clone());
        self.structure = None;
        self.copy_fields = None;
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        // 来源表快照：建连 / 刷新时内省好的库内表（排除当前表——自己复制自己没有意义）。
        let source_tables = session
            .catalog
            .iter()
            .filter(|entry| entry.kind == TableKind::Table)
            .map(|entry| entry.name.clone())
            .filter(|name| *name != table.name)
            .collect();
        let kind = session.kind;
        self.spawn_db(
            cx,
            move || -> Result<StructureMeta, String> {
                let mut guard = conn.lock().expect("connection lock poisoned");
                let columns = guard
                    .table_column_meta(&table)
                    .map_err(|error| error.to_string())?;
                // 主键拿不到不挡编辑器：改名 / 改类型 / 加列都不依赖它，只有
                // 「主键勾选展示」受影响（画成没有）。
                let primary_key = guard.table_primary_key(&table).unwrap_or_default();
                // 五个次要槽位哪个内省失败都不拖垮编辑器：索引 / 外键 / 触发器
                // 落空清单、DDL 落 `None`，对应标签各自显示空态。
                let indexes = guard.table_indexes(&table).unwrap_or_default();
                let foreign_keys = guard.table_foreign_keys(&table).unwrap_or_default();
                let triggers = guard.table_triggers(&table).unwrap_or_default();
                let ddl = guard.table_ddl(&table).ok();
                Ok(StructureMeta {
                    columns,
                    primary_key,
                    indexes,
                    foreign_keys,
                    triggers,
                    ddl,
                })
            },
            move |this, result, window, cx| match result {
                Ok(meta) => {
                    this.build_structure_draft(&meta, source_tables, kind, window, cx);
                    cx.notify();
                }
                Err(error) => {
                    notify::error(trf(cx, "读取表结构失败：{}", &[&error]), window, cx);
                }
            },
        );
        cx.notify();
    }

    /// 把取回的元数据装成草稿（每个输入框一个实体：网格行自己管自己的重绘）。
    fn build_structure_draft(
        &mut self,
        meta: &StructureMeta,
        source_tables: Vec<String>,
        kind: DatabaseKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let table = match &self.mode {
            Mode::Structure(table) => table.clone(),
            // 取数回来前用户切走了：这份列清单没有去处，丢掉（标签再激活会重取）。
            _ => return,
        };
        let text_input = |value: String, window: &mut Window, cx: &mut Context<Self>| {
            let state = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
            state.update(cx, |state, cx| state.set_value(value, window, cx));
            state
        };
        let columns = meta
            .columns
            .iter()
            .map(|column| {
                let default = column.default.clone().unwrap_or_default();
                let comment = column.comment.clone();
                let charset = column.character_set.clone().unwrap_or_default();
                let collation = column.collation.clone().unwrap_or_default();
                StructureColumnDraft {
                    original: Some(column.name.clone()),
                    original_type: column.type_name.clone(),
                    original_nullable: column.nullable.unwrap_or(true),
                    name: text_input(column.name.clone(), window, cx),
                    kind: text_input(column.type_name.clone(), window, cx),
                    nullable: column.nullable.unwrap_or(true),
                    original_default: default.clone(),
                    default: text_input(default, window, cx),
                    original_comment: comment.clone(),
                    comment: text_input(comment, window, cx),
                    original_charset: charset.clone(),
                    charset: text_input(charset, window, cx),
                    original_collation: collation.clone(),
                    collation: text_input(collation, window, cx),
                    primary: meta.primary_key.iter().any(|key| *key == column.name),
                    dropped: false,
                }
            })
            .collect();
        // 字段搜索框：一版一个实体（刷新 / 换表时随草稿重建，旧词不残留）。
        // 占位符先取好（`tr` 返回 `&'static str`，不占借用）——`cx.new` 的闭包
        // 里不能再借 `cx`。
        let search_placeholder = tr(cx, "搜索字段");
        let search = cx.new(|search_cx| {
            InputState::new(&mut *window, search_cx).placeholder(search_placeholder)
        });
        // 搜索词变了就重渲一次页面（页面是 AnyView，外壳重绘不连带它）。旧草稿
        // 重建时旧实体随草稿一起释放，挂在 `_subscriptions` 里的旧订阅随之失效。
        self._subscriptions.push(cx.subscribe(
            &search,
            |_this: &mut Self, _source: Entity<InputState>, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            },
        ));
        let original_order = meta
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect();
        self.structure = Some(StructureDraft {
            table,
            active_tab: StructureTab::Columns,
            density: StructureDensity::Standard,
            search,
            ddl: meta.ddl.clone(),
            indexes: meta.indexes.clone(),
            foreign_keys: meta.foreign_keys.clone(),
            triggers: meta.triggers.clone(),
            source_tables,
            original_order,
            original_primary: meta.primary_key.clone(),
            // 主键可编辑性：只有 MySQL 生成得了主键 DDL；其余引擎改主键 = 重建表。
            primary_editable: kind == DatabaseKind::Mysql,
            kind,
            columns,
        });
    }

    /// 列级属性对当前引擎可不可编辑（内核拿不到 = 不可编辑，UI 画成禁用）。
    ///
    /// 返回 `(可空, 默认值, 注释, 字符集)`：Mongo / ES 的「列」只是采样字段名；
    /// 注释只有四家文本引擎有；字符集只有 MySQL 有（PG 按库统一、T-SQL 的
    /// collation 不冒充成字符集）。新增列不受此闸门——属性是用户现场给的。
    fn attrs_known(kind: DatabaseKind) -> (bool, bool, bool, bool) {
        let doc = !matches!(kind, DatabaseKind::Mongo | DatabaseKind::Elasticsearch);
        let comment = matches!(
            kind,
            DatabaseKind::Mysql
                | DatabaseKind::Postgres
                | DatabaseKind::ClickHouse
                | DatabaseKind::SqlServer
        );
        let charset = kind == DatabaseKind::Mysql;
        (doc, doc, comment, charset)
    }

    /// 由草稿生成 `ALTER` 语句；没有任何改动（或引擎没有 SQL 标识符概念）时 `None`。
    ///
    /// 每次渲染预览都现算：改动数是个位数、输入框直读，不值得做增量。
    /// MySQL 的顺序变更生成物理重排（`AFTER` / `FIRST`），其余引擎保持本地显示序。
    pub(super) fn structure_alter_sql(&self, cx: &App) -> Option<String> {
        let kind = self.active_config()?.kind;
        let quote = kind.identifier_quote()?;
        let draft = self.structure.as_ref()?;
        let table_text = quote_qualified(&draft.table, quote).ok()?;
        let order_changed = self.structure_order_changed();
        let (nullable_known, default_known, comment_known, charset_known) = Self::attrs_known(kind);
        let mut lines: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        for (index, column) in draft.columns.iter().enumerate() {
            let name = column.name.read(cx).value().trim().to_owned();
            let kind_text = column.kind.read(cx).value().trim().to_owned();
            let default_text = column.default.read(cx).value().trim().to_owned();
            let comment_text = column.comment.read(cx).value().trim().to_owned();
            let charset_text = column.charset.read(cx).value().trim().to_owned();
            let collation_text = column.collation.read(cx).value().trim().to_owned();
            let after = order_changed.then(|| {
                self.after_clause(index, quote)
                    .map(|clause| format!(" {clause}"))
                    .unwrap_or_default()
            });
            match &column.original {
                None => {
                    // 新增列：名字与类型都要有才算一条能执行的语句。
                    if !name.is_empty() && !kind_text.is_empty() && !column.dropped {
                        let definition = self.column_definition(
                            &name,
                            &kind_text,
                            column.nullable,
                            &default_text,
                            &comment_text,
                            &charset_text,
                            &collation_text,
                        );
                        lines.push(format!(
                            "ALTER TABLE {table_text} ADD COLUMN {definition}{};",
                            after.unwrap_or_default()
                        ));
                    }
                }
                Some(original) => {
                    let quoted = quote_identifier(original, quote).ok()?;
                    if column.dropped {
                        lines.push(format!("ALTER TABLE {table_text} DROP COLUMN {quoted};"));
                        continue;
                    }
                    let renamed = !name.is_empty() && name != *original;
                    let type_changed = !kind_text.is_empty() && kind_text != column.original_type;
                    let nullable_changed =
                        nullable_known && column.nullable != column.original_nullable;
                    let default_changed = default_known && default_text != column.original_default;
                    let comment_changed = comment_known && comment_text != column.original_comment;
                    let charset_changed = charset_known
                        && (charset_text != column.original_charset
                            || collation_text != column.original_collation);
                    if !renamed
                        && !type_changed
                        && !nullable_changed
                        && !default_changed
                        && !comment_changed
                        && !charset_changed
                    {
                        continue;
                    }
                    match kind {
                        // MySQL 一条 `MODIFY / CHANGE` 吃下全部属性；顺序变更一起带上。
                        DatabaseKind::Mysql => {
                            let definition = self.column_definition(
                                if renamed { &name } else { original },
                                &kind_text,
                                column.nullable,
                                &default_text,
                                &comment_text,
                                &charset_text,
                                &collation_text,
                            );
                            let keyword = if renamed {
                                format!("CHANGE COLUMN {quoted} ")
                            } else {
                                "MODIFY COLUMN ".to_owned()
                            };
                            lines.push(format!(
                                "ALTER TABLE {table_text} {keyword}{definition}{};",
                                after.unwrap_or_default()
                            ));
                        }
                        // PG 按属性逐条出子句：一条 ALTER 只带一个动作，失败定位最细。
                        DatabaseKind::Postgres => {
                            if renamed {
                                let quoted_new = quote_identifier(&name, quote).ok()?;
                                lines.push(format!(
                                    "ALTER TABLE {table_text} RENAME COLUMN {quoted} TO {quoted_new};"
                                ));
                            }
                            if type_changed {
                                lines.push(format!(
                                    "ALTER TABLE {table_text} ALTER COLUMN {quoted} TYPE {kind_text};"
                                ));
                            }
                            if nullable_changed {
                                let action = if column.nullable {
                                    "DROP NOT NULL"
                                } else {
                                    "SET NOT NULL"
                                };
                                lines.push(format!(
                                    "ALTER TABLE {table_text} ALTER COLUMN {quoted} {action};"
                                ));
                            }
                            if default_changed {
                                let action = if default_text.is_empty() {
                                    "DROP DEFAULT".to_owned()
                                } else {
                                    format!("SET DEFAULT {}", literal_sql(&default_text))
                                };
                                lines.push(format!(
                                    "ALTER TABLE {table_text} ALTER COLUMN {quoted} {action};"
                                ));
                            }
                            if comment_changed {
                                let column_path = quote_identifier(
                                    &format!("{}.{}", draft.table.name, name),
                                    quote,
                                )
                                .ok()?;
                                lines.push(format!(
                                    "COMMENT ON COLUMN {column_path} IS '{}';",
                                    comment_text.replace('\'', "''")
                                ));
                            }
                        }
                        // SQL Server：类型 + 可空一条 `ALTER COLUMN`；默认值是约束
                        // 对象（要拿约束名才能改），注释走扩展属性——都如实出注释。
                        DatabaseKind::SqlServer => {
                            if renamed {
                                lines.push(format!(
                                    "EXEC sp_rename '{table_text}.{original}', '{name}', 'COLUMN';"
                                ));
                            }
                            if type_changed || nullable_changed {
                                let null_text = if column.nullable {
                                    String::new()
                                } else {
                                    " NOT NULL".to_owned()
                                };
                                lines.push(format!(
                                    "ALTER TABLE {table_text} ALTER COLUMN {quoted} {kind_text}{null_text};"
                                ));
                            }
                            if default_changed {
                                notes.push(
                                    "-- SQL Server 的默认值是约束对象，请在查询区手工处理；此改动未生成语句。"
                                        .to_owned(),
                                );
                            }
                            if comment_changed {
                                notes.push(
                                    "-- SQL Server 的列注释走扩展属性（sp_addextendedproperty），此改动未生成语句。"
                                        .to_owned(),
                                );
                            }
                        }
                        // ClickHouse：类型（含 Nullable( 前缀）/ 默认值 / 注释都能 MODIFY。
                        DatabaseKind::ClickHouse => {
                            if renamed {
                                let quoted_new = quote_identifier(&name, quote).ok()?;
                                lines.push(format!(
                                    "ALTER TABLE {table_text} RENAME COLUMN {quoted} TO {quoted_new};"
                                ));
                            }
                            let mut definition = format!("{quoted} {kind_text}");
                            if default_changed && !default_text.is_empty() {
                                definition
                                    .push_str(&format!(" DEFAULT {}", literal_sql(&default_text)));
                            }
                            lines.push(format!(
                                "ALTER TABLE {table_text} MODIFY COLUMN {definition};"
                            ));
                            if comment_changed {
                                lines.push(format!(
                                    "ALTER TABLE {table_text} COMMENT COLUMN {quoted} '{}';",
                                    comment_text.replace('\'', "''")
                                ));
                            }
                        }
                        // SQLite：改名 / 加列 / 删列是原生的；其余列属性变更要重建表。
                        DatabaseKind::Sqlite => {
                            if renamed {
                                let quoted_new = quote_identifier(&name, quote).ok()?;
                                lines.push(format!(
                                    "ALTER TABLE {table_text} RENAME COLUMN {quoted} TO {quoted_new};"
                                ));
                            }
                            if type_changed || nullable_changed || default_changed {
                                notes.push(format!(
                                    "-- SQLite 不能直接改「{original}」的列属性，需要重建表；此改动未生成语句。"
                                ));
                            }
                        }
                        // 没有列类型概念的引擎不会走到这里（identifier_quote 为 None）。
                        DatabaseKind::Mongo | DatabaseKind::Elasticsearch => {}
                    }
                }
            }
        }
        // 主键变更：MySQL 生成 `DROP / ADD PRIMARY KEY`；其余引擎勾了就如实说明。
        let current_pk: Vec<String> = draft
            .columns
            .iter()
            .filter(|column| column.primary && !column.dropped)
            .filter_map(|column| {
                let name = column.name.read(cx).value().trim().to_owned();
                (!name.is_empty()).then_some(name)
            })
            .collect();
        if current_pk != draft.original_primary {
            if draft.primary_editable {
                if !draft.original_primary.is_empty() {
                    lines.push(format!("ALTER TABLE {table_text} DROP PRIMARY KEY;"));
                }
                if !current_pk.is_empty() {
                    let quoted: Vec<String> = current_pk
                        .iter()
                        .filter_map(|name| quote_identifier(name, quote).ok())
                        .collect();
                    lines.push(format!(
                        "ALTER TABLE {table_text} ADD PRIMARY KEY ({});",
                        quoted.join(", ")
                    ));
                }
            } else {
                notes.push("-- 改主键需要重建表（涉及数据迁移），此改动未生成语句。".to_owned());
            }
        }
        // 顺序变更只有 MySQL 物理生效；其余引擎如实说明。
        if order_changed && kind != DatabaseKind::Mysql {
            notes.push("-- 顺序变更仅 MySQL 物理生效，其余引擎保持本地显示序。".to_owned());
        }
        for note in notes {
            lines.push(note);
        }
        (!lines.is_empty()).then(|| lines.join("\n"))
    }

    /// 拼一列的**完整定义**（MySQL 语法：类型 + 可空 + 默认值 + 注释 + 字符集）。
    #[allow(clippy::too_many_arguments)]
    fn column_definition(
        &self,
        name: &str,
        type_name: &str,
        nullable: bool,
        default: &str,
        comment: &str,
        charset: &str,
        collation: &str,
    ) -> String {
        let quote = self
            .active_config()
            .and_then(|config| config.kind.identifier_quote())
            .unwrap_or('`');
        let quoted = quote_identifier(name, quote).unwrap_or_else(|_| name.to_owned());
        let mut definition = format!("{quoted} {type_name}");
        if !nullable {
            definition.push_str(" NOT NULL");
        }
        if !default.is_empty() {
            definition.push_str(&format!(" DEFAULT {}", literal_sql(default)));
        }
        if !comment.is_empty() {
            definition.push_str(&format!(" COMMENT '{}'", comment.replace('\'', "''")));
        }
        if !charset.is_empty() {
            definition.push_str(&format!(" CHARACTER SET {charset}"));
            if !collation.is_empty() {
                definition.push_str(&format!(" COLLATE {collation}"));
            }
        }
        definition
    }

    /// 物理重排的落点子句：落在**前一个还在表里**的原列之后；没有就排最前。
    fn after_clause(&self, index: usize, quote: char) -> Option<String> {
        let draft = self.structure.as_ref()?;
        let previous = draft.columns[..index]
            .iter()
            .rev()
            .find(|column| column.original.is_some() && !column.dropped)?;
        let previous = previous.original.as_ref()?;
        Some(match quote_identifier(previous, quote) {
            Ok(quoted) => format!("AFTER {quoted}"),
            Err(_) => "FIRST".to_owned(),
        })
    }

    /// 顺序变没变：只比**还在表里的原列**的相对序（删列不算重排，否则删一列
    /// 会让所有改动都背上 `AFTER`，噪音大于信息）。
    fn structure_order_changed(&self) -> bool {
        let Some(draft) = self.structure.as_ref() else {
            return false;
        };
        let dropped: std::collections::BTreeSet<&str> = draft
            .columns
            .iter()
            .filter(|column| column.dropped)
            .filter_map(|column| column.original.as_deref())
            .collect();
        let baseline: Vec<&String> = draft
            .original_order
            .iter()
            .filter(|name| !dropped.contains(name.as_str()))
            .collect();
        let current: Vec<&String> = draft
            .columns
            .iter()
            .filter(|column| column.original.is_some() && !column.dropped)
            .filter_map(|column| column.original.as_ref())
            .collect();
        baseline != current
    }

    /// 字段搜索词（客户端过滤字段名 / 类型 / 注释；空串 = 不过滤）。
    fn structure_search_text(&self, cx: &App) -> String {
        self.structure
            .as_ref()
            .map(|draft| draft.search.read(cx).value().trim().to_owned())
            .unwrap_or_default()
    }

    /// 选中一个结构标签。
    fn select_structure_tab(&mut self, tab: StructureTab, cx: &mut Context<Self>) {
        if let Some(draft) = self.structure.as_mut()
            && draft.active_tab != tab
        {
            draft.active_tab = tab;
            cx.notify();
        }
    }

    /// 行密度档位（下拉三选；页内记忆）。
    fn select_structure_density(&mut self, density: StructureDensity, cx: &mut Context<Self>) {
        if let Some(draft) = self.structure.as_mut()
            && draft.density != density
        {
            draft.density = density;
            cx.notify();
        }
    }

    /// 行密度的两把尺：控件高度与行内边距（dbx 三档同款体感）。
    fn density_metrics(&self, cx: &App) -> (Pixels, Pixels) {
        let base = active_theme(cx).density().control;
        match self
            .structure
            .as_ref()
            .map(|draft| draft.density)
            .unwrap_or(StructureDensity::Standard)
        {
            StructureDensity::Compact => (px(24.0), px(1.0)),
            StructureDensity::Standard => (base, px(3.0)),
            StructureDensity::Comfortable => (base + px(8.0), px(6.0)),
        }
    }

    /// 拖拽一列到目标位（**活重排**：拖过哪行就落在哪行前面，dbx 同款体感）。
    fn move_structure_column(&mut self, from: usize, to: usize, cx: &mut Context<Self>) {
        let Some(draft) = self.structure.as_mut() else {
            return;
        };
        if from == to || from >= draft.columns.len() || to >= draft.columns.len() {
            return;
        }
        let column = draft.columns.remove(from);
        draft.columns.insert(to, column);
        cx.notify();
    }

    /// 复制一行字段为新增行（原列属性一起带过来；追加在被复制行之后）。
    fn copy_structure_column(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        // 先把源行值全部读出来（借用在 insert 前结束）。
        let source = {
            let Some(draft) = self.structure.as_ref() else {
                return;
            };
            let Some(source) = draft.columns.get(index) else {
                return;
            };
            let read = |state: &Entity<InputState>| state.read(cx).value().trim().to_owned();
            (
                source.nullable && !source.primary,
                read(&source.kind),
                read(&source.default),
                read(&source.comment),
                read(&source.charset),
                read(&source.collation),
                source.name.read(cx).value().trim().to_owned(),
            )
        };
        let (nullable, kind_text, default, comment, charset, collation, source_name) = source;
        let new_name = if source_name.is_empty() {
            String::new()
        } else {
            format!("{source_name}_copy")
        };
        let Some(draft) = self.structure.as_mut() else {
            return;
        };
        let column = StructureColumnDraft {
            original: None,
            original_type: String::new(),
            original_nullable: true,
            name: cx.new(|cell_cx| InputState::new(&mut *window, cell_cx)),
            kind: cx.new(|cell_cx| InputState::new(&mut *window, cell_cx)),
            nullable,
            original_default: String::new(),
            default: cx.new(|cell_cx| InputState::new(&mut *window, cell_cx)),
            original_comment: String::new(),
            comment: cx.new(|cell_cx| InputState::new(&mut *window, cell_cx)),
            original_charset: String::new(),
            charset: cx.new(|cell_cx| InputState::new(&mut *window, cell_cx)),
            original_collation: String::new(),
            collation: cx.new(|cell_cx| InputState::new(&mut *window, cell_cx)),
            primary: false,
            dropped: false,
        };
        column
            .name
            .update(cx, |state, cx| state.set_value(new_name, window, cx));
        column
            .kind
            .update(cx, |state, cx| state.set_value(kind_text, window, cx));
        column
            .default
            .update(cx, |state, cx| state.set_value(default, window, cx));
        column
            .comment
            .update(cx, |state, cx| state.set_value(comment, window, cx));
        column
            .charset
            .update(cx, |state, cx| state.set_value(charset, window, cx));
        column
            .collation
            .update(cx, |state, cx| state.set_value(collation, window, cx));
        draft.columns.insert(index + 1, column);
        cx.notify();
    }

    /// 目标条：数据库图标 + 限定表名 + 引擎类别徽标 + 刷新按钮。
    fn render_structure_target_bar(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let Some(draft) = self.structure.as_ref() else {
            return div().into_any_element();
        };
        let kind_label = self
            .active_config()
            .map(|config| kind_label(config.kind).to_owned())
            .unwrap_or_default();
        let table_label = draft
            .table
            .schema
            .as_deref()
            .map(|schema| format!("{schema}.{}", draft.table.name))
            .unwrap_or_else(|| draft.table.name.clone());
        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .rounded(tokens.radius.md)
            .bg(extended.colors.chrome)
            .border_1()
            .border_color(tokens.colors.border)
            .child(
                Icon::new(DuckIcon::Database)
                    .size(extended.icon.sm)
                    .text_color(tokens.colors.muted_foreground),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .text_size(tokens.typography.sm.size)
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(tokens.colors.foreground)
                    .truncate()
                    .child(SharedString::from(table_label)),
            )
            .child(neutral_tag(SharedString::from(kind_label), cx))
            .child(
                button(
                    "db-structure-refresh",
                    tr(cx, "刷新"),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(self.busy)
                .on_click({
                    let view = view.clone();
                    let table = draft.table.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| {
                            this.open_structure(table.clone(), cx);
                        });
                    }
                }),
            )
            .into_any_element()
    }

    /// Tabs 卡片的标签栏（左：五个标签；右：随标签切换的工具区）。
    fn render_structure_tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let Some(draft) = self.structure.as_ref() else {
            return div().into_any_element();
        };
        let tabs = [
            (StructureTab::Ddl, SharedString::from("DDL"), None::<usize>),
            (
                StructureTab::Columns,
                tr(cx, "字段").into(),
                Some(draft.columns.len()),
            ),
            (
                StructureTab::Indexes,
                tr(cx, "索引").into(),
                Some(draft.indexes.len()),
            ),
            (
                StructureTab::ForeignKeys,
                tr(cx, "外键").into(),
                Some(draft.foreign_keys.len()),
            ),
            (
                StructureTab::Triggers,
                tr(cx, "触发器").into(),
                Some(draft.triggers.len()),
            ),
        ];
        let mut tab_row = h_flex().flex_none().items_center().gap(tokens.spacing.xxs);
        for (tab, label, count) in tabs {
            let active = draft.active_tab == tab;
            let id = SharedString::from(format!("db-structure-tab-{:?}", tab));
            let mut tab_el = h_flex()
                .id(id)
                .flex_none()
                .items_center()
                .gap(tokens.spacing.xxs)
                .px(tokens.spacing.sm)
                .py(tokens.spacing.xxs)
                .rounded(tokens.radius.sm)
                .cursor_pointer()
                .hover(|style| style.bg(extended.colors.row_hover))
                .on_click({
                    let view = view.clone();
                    move |_, _, cx| {
                        let _ = view.update(cx, |this, cx| this.select_structure_tab(tab, cx));
                    }
                })
                .child(
                    div()
                        .text_size(tokens.typography.sm.size)
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(if active {
                            tokens.colors.primary
                        } else {
                            tokens.colors.muted_foreground
                        })
                        .child(label),
                );
            if let Some(count) = count {
                tab_el = tab_el.child(
                    div()
                        .px(tokens.spacing.xs)
                        .rounded(tokens.radius.full)
                        .bg(if active {
                            extended.colors.nav_selected
                        } else {
                            extended.colors.chrome
                        })
                        .text_size(tokens.typography.xs.size)
                        .text_color(if active {
                            tokens.colors.primary
                        } else {
                            extended.colors.text_tertiary
                        })
                        .child(SharedString::from(count.to_string())),
                );
            }
            tab_row = tab_row.child(tab_el);
        }
        h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .justify_between()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.xs)
            .py(tokens.spacing.xs)
            .border_b_1()
            .border_color(tokens.colors.border)
            .child(tab_row)
            .child(self.render_structure_tab_tools(cx))
            .into_any_element()
    }

    /// 标签栏右端的工具区（随激活标签切换，dbx 同款）。
    fn render_structure_tab_tools(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let Some(draft) = self.structure.as_ref() else {
            return div().into_any_element();
        };
        let tools = h_flex().flex_none().items_center().gap(tokens.spacing.xxs);
        match draft.active_tab {
            // DDL 标签：复制整段 DDL。
            StructureTab::Ddl => tools
                .child(
                    button(
                        "db-structure-copy-ddl",
                        tr(cx, "复制 DDL"),
                        ButtonVariant::Secondary,
                        cx,
                    )
                    .disabled(draft.ddl.is_none())
                    .on_click({
                        let view = view.clone();
                        move |_, _, cx| {
                            let _ = view.update(cx, |this, cx| {
                                let ddl =
                                    this.structure.as_ref().and_then(|draft| draft.ddl.clone());
                                if let Some(ddl) = ddl {
                                    this.put_on_clipboard(ddl, tr(cx, "已复制 DDL。"), cx);
                                }
                            });
                        }
                    }),
                )
                .into_any_element(),
            // 字段标签：搜索框 + 新增字段 + 复制其他表字段 + 行密度。
            StructureTab::Columns => {
                let source_options: Vec<(SharedString, SharedString)> = draft
                    .source_tables
                    .iter()
                    .map(|name| {
                        (
                            SharedString::from(name.clone()),
                            SharedString::from(name.clone()),
                        )
                    })
                    .collect();
                tools
                    .child(
                        h_flex()
                            .id("db-structure-search")
                            .w(px(160.0))
                            .flex_none()
                            .items_center()
                            .gap(tokens.spacing.xxs)
                            .px(tokens.spacing.xs)
                            .py(px(2.0))
                            .rounded(tokens.radius.md)
                            .border_1()
                            .border_color(tokens.colors.border)
                            .bg(tokens.colors.background)
                            // 页面是 AnyView，输入框要自己拦住鼠标按下（结果区搜索框同款）。
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .child(
                                Icon::new(DuckIcon::Search)
                                    .size(extended.icon.sm)
                                    .text_color(tokens.colors.muted_foreground),
                            )
                            .child(
                                div().flex_1().min_w_0().child(
                                    Input::new(&draft.search)
                                        .appearance(false)
                                        .with_size(Size::Small)
                                        .text_size(tokens.typography.sm.size)
                                        .w_full(),
                                ),
                            ),
                    )
                    .child(
                        button(
                            "db-structure-add",
                            tr(cx, "新增字段"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                let _ = view
                                    .update(cx, |this, cx| this.add_structure_column(window, cx));
                            }
                        }),
                    )
                    .child(
                        // 下拉的「当前值」不在选项里时显示原样文案 —— 正好当按钮标签用。
                        dropdown_select(
                            "db-structure-copy-fields",
                            source_options,
                            tr(cx, "复制其他表字段"),
                            {
                                let view = view.clone();
                                move |table, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.open_copy_fields(&table, window, cx);
                                    });
                                }
                            },
                            cx,
                        ),
                    )
                    .child(dropdown_select(
                        "db-structure-density",
                        [
                            (SharedString::from("compact"), tr(cx, "紧凑").into()),
                            (SharedString::from("standard"), tr(cx, "标准").into()),
                            (SharedString::from("comfortable"), tr(cx, "舒适").into()),
                        ],
                        SharedString::from(match draft.density {
                            StructureDensity::Compact => "compact",
                            StructureDensity::Standard => "standard",
                            StructureDensity::Comfortable => "comfortable",
                        }),
                        {
                            let view = view.clone();
                            move |value, _, cx| {
                                let density = match value.as_ref() {
                                    "compact" => StructureDensity::Compact,
                                    "comfortable" => StructureDensity::Comfortable,
                                    _ => StructureDensity::Standard,
                                };
                                let _ = view.update(cx, |this, cx| {
                                    this.select_structure_density(density, cx);
                                });
                            }
                        },
                        cx,
                    ))
                    .into_any_element()
            }
            // 三个只读标签没有工具。
            StructureTab::Indexes | StructureTab::ForeignKeys | StructureTab::Triggers => {
                tools.into_any_element()
            }
        }
    }

    /// 字段网格（九列：操作 / 字段名 / 类型 / 可为空 / 主键 / 默认值 / 注释 /
    /// 字符集 / 排序规则；搜索词客户端过滤，拖拽活重排，整块横向滚动）。
    fn render_structure_grid(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let Some(draft) = self.structure.as_ref() else {
            return empty_state(DuckIcon::Clock, tr(cx, "正在载入表结构…"), None, None, cx)
                .into_any_element();
        };
        let (control_h, row_py) = self.density_metrics(cx);
        let (nullable_known, default_known, comment_known, charset_known) =
            Self::attrs_known(draft.kind);
        let search = self.structure_search_text(cx);
        let matches = |name: &str, kind_text: &str, comment: &str| {
            let needle = search.to_lowercase();
            search.is_empty()
                || name.to_lowercase().contains(&needle)
                || kind_text.to_lowercase().contains(&needle)
                || comment.to_lowercase().contains(&needle)
        };
        // 表头（列宽与行单元格一一对应）。
        let head = |text: SharedString, width: Pixels| {
            div().flex_none().w(width).child(caption_heading(text, cx))
        };
        let header = h_flex()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.xs)
            .pb(tokens.spacing.xxs)
            .border_b_1()
            .border_color(tokens.colors.border)
            .child(head(tr(cx, "操作").into(), px(96.0)))
            .child(head(tr(cx, "字段名").into(), px(150.0)))
            .child(head(tr(cx, "类型").into(), px(140.0)))
            .child(head(tr(cx, "可为空").into(), px(56.0)))
            .child(head(tr(cx, "主键").into(), px(56.0)))
            .child(head(tr(cx, "默认值").into(), px(150.0)))
            .child(head(tr(cx, "注释").into(), px(160.0)))
            .child(head(tr(cx, "字符集").into(), px(110.0)))
            .child(head(tr(cx, "排序规则").into(), px(130.0)));
        let mut rows = v_flex().w_full().flex_none().gap(tokens.spacing.xxs);
        let mut visible = 0usize;
        for (index, column) in draft.columns.iter().enumerate() {
            let name = column.name.read(cx).value().trim().to_owned();
            let kind_text = column.kind.read(cx).value().trim().to_owned();
            let comment_text = column.comment.read(cx).value().trim().to_owned();
            if !matches(&name, &kind_text, &comment_text) {
                continue;
            }
            visible += 1;
            let changed = match &column.original {
                None => true,
                Some(original) => {
                    column.dropped
                        || name != *original
                        || kind_text != column.original_type
                        || column.nullable != column.original_nullable
                        || column.default.read(cx).value().trim() != column.original_default
                        || comment_text != column.original_comment
                        || column.charset.read(cx).value().trim() != column.original_charset
                        || column.collation.read(cx).value().trim() != column.original_collation
                }
            };
            let is_new = column.original.is_none();
            let dropped = column.dropped;
            // 开关禁用闸门：内核拿不到对应属性 / 该引擎改不了主键时，开关画成
            // 淡出并**在回调里直接拦**（gpui-base 的 `Switch` 没实现
            // `Disableable`，没有 bool 版 `.disabled()`）。
            let nullable_locked = dropped || (!nullable_known && !is_new);
            let pk_locked = dropped || (!is_new && !draft.primary_editable);
            let row = div()
                .flex()
                .flex_row()
                .flex_none()
                .items_center()
                .gap(tokens.spacing.xs)
                .px(tokens.spacing.xs)
                .py(row_py)
                .rounded(tokens.radius.sm)
                // 新增行薄绿底、标记删除的行整行淡化：删除是**草稿态**，撤得回来。
                .when(dropped, |row| row.opacity(extended.opacity.disabled))
                .when(is_new && !dropped, |row| {
                    row.bg(extended.colors.success.opacity(extended.opacity.tint))
                })
                // 拖拽落点：拖着列划过这一行就活重排到这一位（dbx 同款体感）。
                .on_drag_move::<StructureRowDrag>({
                    let drag_view = view.clone();
                    move |event, _, cx| {
                        let dragged = event.drag(cx).index;
                        let _ = drag_view.update(cx, |this, cx| {
                            this.move_structure_column(dragged, index, cx);
                        });
                    }
                })
                // 操作位：拖拽把手 + 复制 + 删除 / 撤销。
                .child(
                    div()
                        .flex_none()
                        .w(px(96.0))
                        .flex()
                        .items_center()
                        .gap(px(2.0))
                        .child(
                            div()
                                .id(SharedString::from(format!("db-structure-drag-{index}")))
                                .flex_none()
                                .w(px(20.0))
                                .h(control_h)
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(tokens.radius.sm)
                                .cursor_grab()
                                .hover(|style| style.bg(extended.colors.row_hover))
                                .child(
                                    Icon::new(DuckIcon::ArrowRightLeft)
                                        .size(extended.icon.sm)
                                        .text_color(tokens.colors.muted_foreground),
                                )
                                .on_drag(StructureRowDrag { index }, |drag, _, _, cx| {
                                    cx.stop_propagation();
                                    cx.new(|_| drag.clone())
                                }),
                        )
                        .child(
                            icon_button(
                                SharedString::from(format!("db-structure-copy-{index}")),
                                tr(cx, "复制字段"),
                                Icon::new(DuckIcon::Copy).size(extended.icon.sm),
                                ButtonVariant::Ghost,
                                cx,
                            )
                            .disabled(dropped)
                            .on_click({
                                let click_view = view.clone();
                                move |_, window, cx| {
                                    let _ = click_view.update(cx, |this, cx| {
                                        this.copy_structure_column(index, window, cx);
                                    });
                                }
                            }),
                        )
                        .child(
                            icon_button(
                                SharedString::from(format!("db-structure-drop-{index}")),
                                if dropped {
                                    tr(cx, "撤销删除")
                                } else if is_new {
                                    tr(cx, "移除")
                                } else {
                                    tr(cx, "删除")
                                },
                                if dropped {
                                    Icon::new(DuckIcon::RotateCcw).size(extended.icon.sm)
                                } else {
                                    Icon::new(DuckIcon::Trash2).size(extended.icon.sm)
                                },
                                ButtonVariant::Ghost,
                                cx,
                            )
                            .on_click({
                                let click_view = view.clone();
                                move |_, _, cx| {
                                    let _ = click_view.update(cx, |this, cx| {
                                        this.toggle_drop_column(index, cx);
                                    });
                                }
                            }),
                        ),
                )
                .child(grid_input(
                    &column.name,
                    control_h,
                    tokens.typography.sm.size,
                    dropped,
                ))
                .child(grid_input(
                    &column.kind,
                    control_h,
                    tokens.typography.sm.size,
                    dropped,
                ))
                .child(
                    div()
                        .flex_none()
                        .w(px(56.0))
                        .flex()
                        .justify_center()
                        .when(nullable_locked, |style| {
                            style.opacity(extended.opacity.disabled)
                        })
                        .child(switch_control(
                            SharedString::from(format!("db-structure-null-{index}")),
                            column.nullable,
                            {
                                let toggle_view = view.clone();
                                move |checked, _, cx| {
                                    if nullable_locked {
                                        return;
                                    }
                                    let _ = toggle_view.update(cx, |this, cx| {
                                        this.set_column_nullable(index, *checked, cx);
                                    });
                                }
                            },
                        )),
                )
                .child(
                    div()
                        .flex_none()
                        .w(px(56.0))
                        .flex()
                        .justify_center()
                        .when(pk_locked, |style| style.opacity(extended.opacity.disabled))
                        .child(switch_control(
                            SharedString::from(format!("db-structure-pk-{index}")),
                            column.primary,
                            {
                                let toggle_view = view.clone();
                                move |checked, _, cx| {
                                    if pk_locked {
                                        return;
                                    }
                                    let _ = toggle_view.update(cx, |this, cx| {
                                        this.set_column_primary(index, *checked, cx);
                                    });
                                }
                            },
                        )),
                )
                .child(
                    div()
                        .flex_none()
                        .w(px(150.0))
                        .flex()
                        .items_center()
                        .gap(px(2.0))
                        .child(grid_input(
                            &column.default,
                            control_h,
                            tokens.typography.mono_md.size,
                            dropped || !default_known && !is_new,
                        ))
                        .child(self.default_preset_button(index, is_new, cx)),
                )
                .child(grid_input(
                    &column.comment,
                    control_h,
                    tokens.typography.sm.size,
                    dropped || !comment_known && !is_new,
                ))
                .child(grid_input(
                    &column.charset,
                    control_h,
                    tokens.typography.mono_md.size,
                    dropped || !charset_known && !is_new,
                ))
                .child(grid_input(
                    &column.collation,
                    control_h,
                    tokens.typography.mono_md.size,
                    dropped || !charset_known && !is_new,
                ))
                .children(changed.then(|| neutral_tag(tr(cx, "已改"), cx)));
            rows = rows.child(row);
        }
        if visible == 0 && !search.is_empty() {
            return empty_state(DuckIcon::Search, tr(cx, "没有匹配的字段"), None, None, cx)
                .into_any_element();
        }
        // 整块横向滚动：九列在窄窗口下不挤压（dbx 网格同款）。
        div()
            .id("db-structure-columns")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_x_scroll()
            .child(
                v_flex()
                    .w(px(1130.0))
                    .flex_none()
                    .gap(tokens.spacing.xxs)
                    .child(header)
                    .child(rows),
            )
            .into_any_element()
    }

    /// 默认值快捷预设（NULL / CURRENT_TIMESTAMP / 0 / 空串；dbx 同款菜单）。
    fn default_preset_button(
        &self,
        index: usize,
        is_new: bool,
        // 只读：`&App` 即可——行循环里 `tokens` 还占着 cx 的不可变借用，
        // 这里要 `&mut` 就撞 E0502 了。
        cx: &App,
    ) -> AnyElement {
        let view = self.view.clone();
        let theme = active_theme(cx);
        let extended = theme.extended();
        let (_, default_known, _, _) = self
            .structure
            .as_ref()
            .map(|draft| Self::attrs_known(draft.kind))
            .unwrap_or((true, true, true, true));
        let id = SharedString::from(format!("db-structure-preset-{index}"));
        let label = tr(cx, "默认值预设");
        let icon = Icon::new(DuckIcon::ChevronDown).size(extended.icon.sm);
        // 既有引擎拿不到默认值元数据时画成停用（`toolbar_dropdown_button` 不带
        // 停用档——停用的按钮也不该弹菜单）。
        if !is_new && !default_known {
            return icon_button(id, label, icon, ButtonVariant::Ghost, cx)
                .disabled(true)
                .into_any_element();
        }
        // duck 的 chrome 按钮是 gpui-base 的 `Button`，没有
        // `dropdown_menu_with_anchor`（那只在 gpui-component 的 Button 上）——
        // 走 kit 的下拉触发按钮（kit.rs 注释里就是为这个场景准备的）。
        toolbar_dropdown_button(
            id,
            label,
            icon,
            Anchor::TopRight,
            move |menu, _, _| {
                let presets: [(&str, &str); 4] = [
                    ("NULL", "NULL"),
                    ("CURRENT_TIMESTAMP", "CURRENT_TIMESTAMP"),
                    ("0", "0"),
                    ("''（空串）", "''"),
                ];
                presets.iter().fold(menu, |menu, (label, value)| {
                    let item_view = view.clone();
                    let value = SharedString::from(*value);
                    menu.item(PopupMenuItem::new(SharedString::from(*label)).on_click(
                        move |_, window, cx| {
                            let _ = item_view.update(cx, |this, cx| {
                                if let Some(draft) = this.structure.as_mut()
                                    && let Some(column) = draft.columns.get_mut(index)
                                {
                                    column.default.update(cx, |state, cx| {
                                        state.set_value(value.to_string(), window, cx)
                                    });
                                }
                                cx.notify();
                            });
                        },
                    ))
                })
            },
            cx,
        )
        .into_any_element()
    }

    /// 只读清单标签的公共外壳（索引 / 外键 / 触发器三标签共用）。
    fn render_structure_readonly_list(
        &self,
        empty_message: impl Into<SharedString>,
        rows: Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if rows.is_empty() {
            return empty_state(DuckIcon::Layers, empty_message, None, None, cx).into_any_element();
        }
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        v_flex()
            .w_full()
            .flex_1()
            .min_h_0()
            .gap(tokens.spacing.xxs)
            .children(rows)
            .into_any_element()
    }

    /// 索引标签：名称 + 唯一 / 主键徽标 + 组成列。
    fn render_structure_indexes(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let Some(draft) = self.structure.as_ref() else {
            return div().into_any_element();
        };
        let rows = draft
            .indexes
            .iter()
            .map(|index| {
                let mut badge_row = h_flex().gap(tokens.spacing.xxs);
                if index.primary {
                    badge_row = badge_row.child(neutral_tag(tr(cx, "主键"), cx));
                }
                if index.unique {
                    badge_row = badge_row.child(neutral_tag(tr(cx, "唯一"), cx));
                }
                let columns = if index.columns.is_empty() {
                    tr(cx, "（表达式列）").to_owned()
                } else {
                    index.columns.join(", ")
                };
                card(cx)
                    .w_full()
                    .flex_none()
                    .px(tokens.spacing.sm)
                    .py(tokens.spacing.xs)
                    .child(
                        v_flex()
                            .w_full()
                            .gap(tokens.spacing.xxs)
                            .child(
                                h_flex()
                                    .w_full()
                                    .items_center()
                                    .gap(tokens.spacing.sm)
                                    .child(
                                        div()
                                            .min_w_0()
                                            .text_size(tokens.typography.sm.size)
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(tokens.colors.foreground)
                                            .truncate()
                                            .child(SharedString::from(index.name.clone())),
                                    )
                                    .child(badge_row),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .text_size(tokens.typography.mono_md.size)
                                    .text_color(tokens.colors.muted_foreground)
                                    .truncate()
                                    .child(SharedString::from(columns)),
                            ),
                    )
                    .into_any_element()
            })
            .collect();
        self.render_structure_readonly_list(SharedString::from(tr(cx, "暂无索引")), rows, cx)
    }

    /// 外键标签：约束名 + 列映射 + 更新 / 删除动作。
    fn render_structure_foreign_keys(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let Some(draft) = self.structure.as_ref() else {
            return div().into_any_element();
        };
        let rows = draft
            .foreign_keys
            .iter()
            .map(|foreign_key| {
                let name = if foreign_key.name.is_empty() {
                    tr(cx, "（无约束名）").to_owned()
                } else {
                    foreign_key.name.clone()
                };
                let mapping = format!(
                    "{} → {}.{}",
                    foreign_key.columns.join(", "),
                    foreign_key.ref_table,
                    foreign_key.ref_columns.join(", ")
                );
                let actions = format!(
                    "{} {} · {} {}",
                    tr(cx, "更新"),
                    foreign_key.on_update,
                    tr(cx, "删除"),
                    foreign_key.on_delete
                );
                card(cx)
                    .w_full()
                    .flex_none()
                    .px(tokens.spacing.sm)
                    .py(tokens.spacing.xs)
                    .child(
                        v_flex()
                            .w_full()
                            .gap(tokens.spacing.xxs)
                            .child(
                                div()
                                    .min_w_0()
                                    .text_size(tokens.typography.sm.size)
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(tokens.colors.foreground)
                                    .truncate()
                                    .child(SharedString::from(name)),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .text_size(tokens.typography.mono_md.size)
                                    .text_color(tokens.colors.muted_foreground)
                                    .truncate()
                                    .child(SharedString::from(mapping)),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .text_size(tokens.typography.xs.size)
                                    .text_color(tokens.colors.muted_foreground)
                                    .child(SharedString::from(actions)),
                            ),
                    )
                    .into_any_element()
            })
            .collect();
        self.render_structure_readonly_list(SharedString::from(tr(cx, "暂无外键")), rows, cx)
    }

    /// 触发器标签：名称 + 时机 + 事件。
    fn render_structure_triggers(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let Some(draft) = self.structure.as_ref() else {
            return div().into_any_element();
        };
        let rows = draft
            .triggers
            .iter()
            .map(|trigger| {
                let timing = format!("{} {}", trigger.timing, trigger.event);
                card(cx)
                    .w_full()
                    .flex_none()
                    .px(tokens.spacing.sm)
                    .py(tokens.spacing.xs)
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap(tokens.spacing.sm)
                            .child(
                                div()
                                    .min_w_0()
                                    .flex_1()
                                    .text_size(tokens.typography.sm.size)
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(tokens.colors.foreground)
                                    .truncate()
                                    .child(SharedString::from(trigger.name.clone())),
                            )
                            .child(neutral_tag(SharedString::from(timing), cx)),
                    )
                    .into_any_element()
            })
            .collect();
        self.render_structure_readonly_list(SharedString::from(tr(cx, "暂无触发器")), rows, cx)
    }

    /// DDL 标签：打开时读到的建表语句（只读 + 复制入口在标签栏）。
    fn render_structure_ddl(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let Some(draft) = self.structure.as_ref() else {
            return div().into_any_element();
        };
        match &draft.ddl {
            Some(ddl) => div()
                .id("db-structure-ddl")
                .w_full()
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .p(tokens.spacing.sm)
                .text_size(tokens.typography.mono_md.size)
                .text_color(tokens.colors.foreground)
                .child(SharedString::from(ddl.clone()))
                .into_any_element(),
            None => empty_state(DuckIcon::Code, tr(cx, "未能读取建表 DDL"), None, None, cx)
                .into_any_element(),
        }
    }

    /// 「复制其他表字段」：后台取源表列元数据，回来开候选弹窗。
    fn open_copy_fields(&mut self, table: &str, _window: &mut Window, cx: &mut Context<Self>) {
        // 后台任务与回调都是 'static：先把表名落成 owned，别让入参借用逃出去。
        let table = table.to_owned();
        let Some(session) = self.active_session() else {
            return;
        };
        let target = TableRef {
            name: table.to_owned(),
            schema: self
                .structure
                .as_ref()
                .and_then(|draft| draft.table.schema.clone()),
        };
        let conn = session.conn.clone();
        self.spawn_db(
            cx,
            move || -> Result<Vec<(String, String)>, String> {
                let mut guard = conn.lock().expect("connection lock poisoned");
                let columns = guard
                    .table_column_meta(&target)
                    .map_err(|error| error.to_string())?;
                Ok(columns
                    .into_iter()
                    .map(|column| (column.name, column.type_name))
                    .collect())
            },
            move |this, result, window, cx| match result {
                Ok(fields) => {
                    if fields.is_empty() {
                        notify::warning(tr(cx, "该表没有可复制的字段。"), window, cx);
                        return;
                    }
                    this.copy_fields = Some(CopyFieldsDraft {
                        table: table.clone(),
                        fields: fields
                            .into_iter()
                            .map(|(name, type_name)| (name, type_name, true))
                            .collect(),
                    });
                    this.show_copy_fields_dialog(window, cx);
                }
                Err(error) => {
                    notify::error(trf(cx, "读取表结构失败：{}", &[&error]), window, cx);
                }
            },
        );
    }

    /// 候选字段弹窗（勾选 → 确认后整批追加为新增行；全选 / 清除一键翻转）。
    fn show_copy_fields_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.view.clone();
        let drag = modal_drag(cx);
        let confirm_view = self.view.clone();
        let confirm: ConfirmHandler = Rc::new(move |window: &mut Window, cx: &mut App| {
            let mut accepted = true;
            let _ = confirm_view.update(cx, |this, cx| {
                let Some(draft) = this.copy_fields.as_ref() else {
                    accepted = false;
                    return;
                };
                let picked: Vec<(String, String)> = draft
                    .fields
                    .iter()
                    .filter(|(_, _, selected)| *selected)
                    .map(|(name, type_name, _)| (name.clone(), type_name.clone()))
                    .collect();
                if picked.is_empty() {
                    notify::warning(tr(cx, "请至少勾选一个字段。"), window, cx);
                    accepted = false;
                    return;
                }
                let count = picked.len();
                let source = draft.table.clone();
                let count_text = count.to_string();
                this.copy_fields = None;
                this.append_copied_fields(picked, &source, &count_text, window, cx);
            });
            accepted
        });
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let confirm = Rc::clone(&confirm);
            dialog
                .w(px(480.0))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                // `content` 闭包是 `Fn`（每次重渲染都会重跑）：候选行跟着
                // `copy_fields` 草稿的勾选态走，必须**现读现建**——`Div`
                // 不可 Clone，把建好的 `body` 捕获进来 Fn 搬不动（E0507）。
                .content({
                    let drag_for_content = drag.clone();
                    let content_view = view.clone();
                    move |content, _, cx| {
                        let theme = active_theme(cx);
                        let tokens = theme.tokens();
                        let draft = content_view
                            .upgrade()
                            .and_then(|page| page.read(cx).copy_fields.clone());
                        let body = match draft {
                            Some(draft) => {
                                let mut rows = v_flex().w_full().gap(tokens.spacing.xxs);
                                for (index, (name, type_name, selected)) in
                                    draft.fields.iter().enumerate()
                                {
                                    let row_view = content_view.clone();
                                    rows = rows.child(
                                        h_flex()
                                            .w_full()
                                            .flex_none()
                                            .items_center()
                                            .gap(tokens.spacing.xs)
                                            .child(
                                                switch_control(
                                                    SharedString::from(format!(
                                                        "db-copy-field-{index}"
                                                    )),
                                                    *selected,
                                                    {
                                                        let row_view = row_view.clone();
                                                        move |checked, _, cx| {
                                                            let _ =
                                                                row_view.update(cx, |this, cx| {
                                                                    if let Some(draft) =
                                                                        this.copy_fields.as_mut()
                                                                        && let Some(entry) = draft
                                                                            .fields
                                                                            .get_mut(index)
                                                                    {
                                                                        entry.2 = *checked;
                                                                    }
                                                                    cx.notify();
                                                                });
                                                        }
                                                    },
                                                )
                                                .with_size(Size::XSmall),
                                            )
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .flex_1()
                                                    .text_size(tokens.typography.sm.size)
                                                    .font_weight(FontWeight::MEDIUM)
                                                    .text_color(tokens.colors.foreground)
                                                    .truncate()
                                                    .child(SharedString::from(name.clone())),
                                            )
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .text_size(tokens.typography.mono_md.size)
                                                    .text_color(tokens.colors.muted_foreground)
                                                    .child(SharedString::from(type_name.clone())),
                                            ),
                                    );
                                }
                                let select_view = content_view.clone();
                                let clear_view = content_view.clone();
                                v_flex()
                                    .w_full()
                                    .gap(form_gap(cx))
                                    .child(modal_title_row(
                                        "db-copy-fields-title",
                                        &drag_for_content,
                                        trf(cx, "从「{}」复制字段", &[&draft.table]),
                                        cx,
                                    ))
                                    .child(
                                        h_flex()
                                            .w_full()
                                            .items_center()
                                            .gap(tokens.spacing.xs)
                                            .child(
                                                button(
                                                    "db-copy-fields-all",
                                                    tr(cx, "全选"),
                                                    ButtonVariant::Secondary,
                                                    cx,
                                                )
                                                .on_click({
                                                    let select_view = select_view.clone();
                                                    move |_, _, cx| {
                                                        let _ =
                                                            select_view.update(cx, |this, cx| {
                                                                if let Some(draft) =
                                                                    this.copy_fields.as_mut()
                                                                {
                                                                    for entry in &mut draft.fields {
                                                                        entry.2 = true;
                                                                    }
                                                                }
                                                                cx.notify();
                                                            });
                                                    }
                                                }),
                                            )
                                            .child(
                                                button(
                                                    "db-copy-fields-clear",
                                                    tr(cx, "清除"),
                                                    ButtonVariant::Secondary,
                                                    cx,
                                                )
                                                .on_click({
                                                    let clear_view = clear_view.clone();
                                                    move |_, _, cx| {
                                                        let _ =
                                                            clear_view.update(cx, |this, cx| {
                                                                if let Some(draft) =
                                                                    this.copy_fields.as_mut()
                                                                {
                                                                    for entry in &mut draft.fields {
                                                                        entry.2 = false;
                                                                    }
                                                                }
                                                                cx.notify();
                                                            });
                                                    }
                                                }),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .id("db-copy-fields-rows")
                                            .max_h(px(280.0))
                                            .overflow_y_scroll()
                                            .child(rows),
                                    )
                            }
                            None => v_flex()
                                .w_full()
                                .child(meta_text(tr(cx, "该表没有可复制的字段。"), cx)),
                        };
                        content.child(modal_drag_area(&drag_for_content, body))
                    }
                })
                .footer(dialog_footer(
                    "db-copy-fields",
                    tr(cx, "复制选中字段"),
                    ButtonVariant::Primary,
                    confirm,
                    cx,
                ))
        });
    }

    /// 把勾选的源表字段追加为新增行（名字 / 类型照抄源表；其余属性取默认）。
    fn append_copied_fields(
        &mut self,
        picked: Vec<(String, String)>,
        source: &str,
        count_text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for (name, type_name) in picked {
            self.add_structure_column(window, cx);
            let Some(draft) = self.structure.as_mut() else {
                return;
            };
            let Some(column) = draft.columns.last_mut() else {
                return;
            };
            column
                .name
                .update(cx, |state, cx| state.set_value(name.clone(), window, cx));
            column
                .kind
                .update(cx, |state, cx| state.set_value(type_name, window, cx));
        }
        notify::success(
            trf(cx, "已从「{}」复制 {} 个字段。", &[source, count_text]),
            window,
            cx,
        );
    }

    /// 结构编辑器整页（目标条 + Tabs 卡片 + SQL 预览 + 底部应用条）。
    ///
    /// ⚠️ 各子渲染都要 `&mut Context`（要读输入框、要发 `&mut` 的守卫），而
    /// `active_theme(cx)` 的借用贯穿整页 —— 先把**可变借用**的子块算完，再取主题。
    pub(super) fn render_structure_page(&self, cx: &mut Context<Self>) -> AnyElement {
        let target_bar = self.render_structure_target_bar(cx);
        let tabs_bar = self.render_structure_tabs(cx);
        let tab_body = {
            let tab = self
                .structure
                .as_ref()
                .map(|draft| draft.active_tab)
                .unwrap_or(StructureTab::Columns);
            match tab {
                StructureTab::Ddl => self.render_structure_ddl(cx),
                StructureTab::Columns => self.render_structure_grid(cx),
                StructureTab::Indexes => self.render_structure_indexes(cx),
                StructureTab::ForeignKeys => self.render_structure_foreign_keys(cx),
                StructureTab::Triggers => self.render_structure_triggers(cx),
            }
        };
        let preview = self.structure_alter_sql(cx);
        let apply_view = self.view.clone();
        // 预览文本在预览卡里被 move 掉，底部按钮的禁用态先用布尔存下。
        let has_preview = preview.is_some();
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let statement_count = preview
            .as_ref()
            .map(|sql| {
                sql.lines()
                    .filter(|line| {
                        let line = line.trim_start();
                        line.starts_with("ALTER")
                            || line.starts_with("EXEC")
                            || line.starts_with("COMMENT")
                    })
                    .count()
            })
            .unwrap_or(0);
        v_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .gap(tokens.spacing.sm)
            .bg(tokens.colors.background)
            .p(tokens.spacing.sm)
            // 目标条。
            .child(target_bar)
            // Tabs 卡片（标签栏 + 内容面）。
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .rounded(tokens.radius.md)
                    .border_1()
                    .border_color(tokens.colors.border)
                    .bg(tokens.colors.background)
                    .overflow_hidden()
                    .child(tabs_bar)
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .min_w_0()
                            .p(tokens.spacing.xs)
                            .child(tab_body),
                    ),
            )
            // SQL 预览卡（标题 + 语句数徽标 + 复制；内容可滚动）。
            .child(
                v_flex()
                    .w_full()
                    .flex_none()
                    .max_h(px(200.0))
                    .rounded(tokens.radius.md)
                    .border_1()
                    .border_color(tokens.colors.border)
                    .overflow_hidden()
                    .child(
                        h_flex()
                            .w_full()
                            .flex_none()
                            .items_center()
                            .justify_between()
                            .px(tokens.spacing.sm)
                            .py(tokens.spacing.xxs)
                            .border_b_1()
                            .border_color(tokens.colors.border)
                            .child(
                                h_flex()
                                    .items_center()
                                    .gap(tokens.spacing.xs)
                                    .child(caption_heading(tr(cx, "SQL 预览"), cx))
                                    .children((statement_count > 0).then(|| {
                                        neutral_tag(
                                            SharedString::from(format!(
                                                "{} {}",
                                                statement_count,
                                                tr(cx, "条语句")
                                            )),
                                            cx,
                                        )
                                    })),
                            )
                            .child(
                                button(
                                    "db-structure-copy",
                                    tr(cx, "复制 SQL"),
                                    ButtonVariant::Ghost,
                                    cx,
                                )
                                .disabled(preview.is_none())
                                .on_click({
                                    let view = apply_view.clone();
                                    move |_, _, cx| {
                                        let _ = view.update(cx, |this, cx| {
                                            if let Some(sql) = this.structure_alter_sql(cx) {
                                                this.put_on_clipboard(
                                                    sql,
                                                    tr(cx, "已复制 SQL。"),
                                                    cx,
                                                );
                                            }
                                        });
                                    }
                                }),
                            ),
                    )
                    .child(match preview {
                        Some(sql) => div()
                            .id("db-structure-preview")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .p(tokens.spacing.sm)
                            .text_size(tokens.typography.mono_md.size)
                            .text_color(tokens.colors.foreground)
                            .child(SharedString::from(sql))
                            .into_any_element(),
                        None => div()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(meta_text(tr(cx, "暂无变更"), cx))
                            .into_any_element(),
                    }),
            )
            // 底部应用条：左侧口径提示（编辑是草稿，应用才落库），右侧主按钮。
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .gap(tokens.spacing.sm)
                    .child(div().min_w_0().truncate().child(meta_text(
                        tr(cx, "编辑仅保存在本地草稿，应用前不写入数据库"),
                        cx,
                    )))
                    .child(
                        button(
                            "db-structure-apply",
                            tr(cx, "应用变更"),
                            ButtonVariant::Primary,
                            cx,
                        )
                        .disabled(self.busy || !has_preview)
                        .on_click({
                            let view = apply_view.clone();
                            move |_, window, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.apply_structure(window, cx);
                                });
                            }
                        }),
                    ),
            )
            .into_any_element()
    }

    /// 追加一行空字段（库里还没有；「应用变更」时走 `ADD COLUMN`）。
    pub(super) fn add_structure_column(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.structure.as_mut() else {
            return;
        };
        let name = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
        let kind = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
        let default = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
        let comment = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
        let charset = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
        let collation = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
        draft.columns.push(StructureColumnDraft {
            original: None,
            original_type: String::new(),
            original_nullable: true,
            name,
            kind,
            nullable: true,
            original_default: String::new(),
            default,
            original_comment: String::new(),
            comment,
            original_charset: String::new(),
            charset,
            original_collation: String::new(),
            collation,
            primary: false,
            dropped: false,
        });
        cx.notify();
    }

    /// 标记 / 恢复删除一列（新增列是直接移除——库里还没有，谈不上恢复）。
    fn toggle_drop_column(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(draft) = self.structure.as_mut() else {
            return;
        };
        let Some(column) = draft.columns.get_mut(index) else {
            return;
        };
        if column.original.is_none() {
            draft.columns.remove(index);
        } else {
            column.dropped = !column.dropped;
        }
        cx.notify();
    }

    /// 可空勾选（主键勾着的时候联动关掉——主键列不可空）。
    fn set_column_nullable(&mut self, index: usize, nullable: bool, cx: &mut Context<Self>) {
        if let Some(draft) = self.structure.as_mut()
            && let Some(column) = draft.columns.get_mut(index)
        {
            column.nullable = nullable;
            cx.notify();
        }
    }

    /// 主键勾选（仅 MySQL 可编辑；勾上联动取消可空）。
    fn set_column_primary(&mut self, index: usize, primary: bool, cx: &mut Context<Self>) {
        if let Some(draft) = self.structure.as_mut()
            && let Some(column) = draft.columns.get_mut(index)
        {
            column.primary = primary;
            if primary {
                column.nullable = false;
            }
            cx.notify();
        }
    }

    /// 「应用变更」：确认后走**与编辑器同一条**执行路径（历史 / 审计 / 超时 / 危险
    /// 确认只有一份真相）。执行完结果装回查询区（`execute_sql` 自己会切 `Mode::Query`）。
    pub(super) fn apply_structure(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sql) = self.structure_alter_sql(cx) else {
            return;
        };
        let view = self.view.clone();
        open_confirm(
            tr(cx, "应用表结构变更"),
            tr(
                cx,
                "将执行生成的 ALTER 语句。表结构变更不会自动回滚，确定继续吗？",
            ),
            tr(cx, "执行"),
            Rc::new(move |window, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.execute_sql(sql.clone(), None, window, cx);
                });
            }),
            window,
            cx,
        );
    }
}

/// 网格单元格输入框（统一高度 / 字号 / 禁用态）。
fn grid_input(
    state: &Entity<InputState>,
    height: Pixels,
    text_size: Pixels,
    disabled: bool,
) -> AnyElement {
    let mut input = Input::new(state).h(height).text_size(text_size).w_full();
    if disabled {
        input = input.disabled(true);
    }
    input.into_any_element()
}

/// 文本默认值按字符串字面量包引号；`NULL` / `CURRENT_TIMESTAMP` / 纯数字 /
/// 已经是引号字面量的原样。
fn literal_sql(value: &str) -> String {
    let numeric = !value.is_empty() && value.chars().all(|c| c.is_ascii_digit() || c == '.');
    if value.eq_ignore_ascii_case("NULL")
        || value.eq_ignore_ascii_case("CURRENT_TIMESTAMP")
        || numeric
        || (value.len() >= 2 && value.starts_with('\'') && value.ends_with('\''))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "''"))
    }
}

/// [`DatabasePage::open_structure`] 后台取回的结构元数据包。
///
/// 六个槽位互相独立：列元数据失败才整体报错；索引 / 外键 / 触发器 / DDL 各自
/// 失败只影响自己那个标签（右栏 `TableMeta` 同款口径）。
struct StructureMeta {
    columns: Vec<TableColumnMeta>,
    primary_key: Vec<String>,
    indexes: Vec<TableIndex>,
    foreign_keys: Vec<TableForeignKey>,
    triggers: Vec<TableTrigger>,
    ddl: Option<String>,
}

/// 字段行拖拽的载荷：只带**行下标**（0 起；活重排：拖过哪行就落在哪行前）。
#[derive(Clone)]
struct StructureRowDrag {
    index: usize,
}

impl Render for StructureRowDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

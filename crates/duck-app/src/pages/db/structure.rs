//! 表结构编辑器（`Mode::Structure`，dbx `TableStructureEditor` 对位）。
//!
//! 布局对齐 dbx：**目标条 → 表级属性（注释 / 存储引擎）→ Tabs 卡片（DDL / 字段 /
//! 索引 / 外键 / 触发器）→ SQL 预览卡 → 底部应用条**。字段标签是主编辑面（改名 /
//! 类型 + 长度两格 / 可空 / 主键 / 默认值 / 注释 / 字符集 / 排序规则 / 增删列 /
//! 拖拽排序，表头可拖拽调列宽，搜索框客户端过滤）；**索引与外键也是可编辑的**——
//! 行内改、可新增、可标记删除、索引可重建，全部生成真实 DDL。能不能编辑按引擎能力
//! 闸门（[`structure_capabilities`]）开放：拿不到的能力画成口径空态，「能编辑」的
//! 假象比没有编辑更糟。**DDL 标签同样是可编辑的 SQL 编辑器**（高亮 + 行号；手改的
//! DDL 优先于生成的 `ALTER`，两段都进预览、一起执行，见
//! [`DatabasePage::structure_apply_sql`]）；触发器标签保持只读。
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
use duck_components::{
    MenuItemSpec, dropdown_select_searchable, switch_control, toolbar_searchable_dropdown,
    toolbar_text_searchable_dropdown,
};
use duck_db::TableColumnMeta;
use gpui::Pixels;
use gpui_component::scroll::{Scrollbar, ScrollbarMode};

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

    /// 字段搜索框实体（建表 / 编辑两路草稿共用）：占位符 + `Change` 订阅一起挂。
    ///
    /// 占位符先取好（`tr` 返回 `&'static str`，不占借用）——`cx.new` 的闭包
    /// 里不能再借 `cx`。搜索词变了就重渲一次页面（页面是 AnyView，外壳重绘
    /// 不连带它）。旧草稿重建时旧实体随草稿一起释放，挂在 `_subscriptions`
    /// 里的旧订阅随之失效。
    fn structure_search_entity(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<InputState> {
        let search_placeholder = tr(cx, "搜索字段 / 注释");
        let search = cx.new(|search_cx| {
            InputState::new(&mut *window, search_cx).placeholder(search_placeholder)
        });
        self.track_structure_inputs(std::iter::once(&search), cx);
        search
    }

    /// 给一批输入框挂 `Change → 重渲` 订阅（字段 / 索引 / 外键 / 表注释共用）。
    fn track_structure_inputs<'a>(
        &mut self,
        inputs: impl Iterator<Item = &'a Entity<InputState>>,
        cx: &mut Context<Self>,
    ) {
        for state in inputs {
            self._subscriptions.push(cx.subscribe(
                state,
                |_this: &mut Self, _source: Entity<InputState>, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                },
            ));
        }
    }

    /// 打开一个**建表模式**的结构编辑器：不起内省（服务器上还没有这张表），
    /// 直接给一份空草稿——表名现场填、字段从零加（dbx 的 create mode 同口径）。
    pub(super) fn open_new_table(
        &mut self,
        schema: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mode = Mode::Structure(TableRef {
            schema: schema.clone(),
            name: String::new(),
        });
        self.structure = None;
        self.copy_fields = None;
        let Some(session) = self.active_session() else {
            return;
        };
        let kind = session.kind;
        // 「复制其他表字段」的来源表清单：建表时没有「当前表」，全量快照。
        let source_tables = session
            .catalog
            .iter()
            .filter(|entry| entry.kind == TableKind::Table)
            .map(|entry| entry.name.clone())
            .collect();
        self.build_new_table_draft(schema, source_tables, kind, window, cx);
        cx.notify();
    }

    /// 建表模式的空草稿：表名 / 注释 / 引擎默认值都按引擎能力预置
    /// （注释给得出就给输入框、引擎默认 InnoDB；字段列表为空，行为全
    /// `original: None`，应用时走 `CREATE TABLE` 汇总）。
    fn build_new_table_draft(
        &mut self,
        schema: Option<String>,
        source_tables: Vec<String>,
        kind: DatabaseKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text_input = |value: String, window: &mut Window, cx: &mut Context<Self>| {
            let state = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
            state.update(cx, |state, cx| state.set_value(value, window, cx));
            state
        };
        // 表级属性按引擎能力预置：注释给得出就给框（建表时是**空值**而非原值）；
        // 存储引擎只有 MySQL 有这个概念，默认 InnoDB（库里常见的默认档）。
        let (_, _, comment_known, _) = Self::attrs_known(kind);
        let mysql = kind == DatabaseKind::Mysql;
        let table_comment = comment_known.then(|| text_input(String::new(), window, cx));
        if let Some(comment) = &table_comment {
            self.track_structure_inputs(std::iter::once(comment), cx);
        }
        let search = self.structure_search_entity(window, cx);
        let (index_editable, foreign_key_editable, trigger_editable) = structure_capabilities(kind);
        // 预览编辑器：与 DDL 页同款（SQL 高亮 + 可编辑），内容在草稿建好后播种。
        let preview_editor = cx.new(|editor_cx| {
            EditorState::new(&mut *window, editor_cx)
                .language(SQL_LANGUAGE)
                .soft_wrap(true)
        });
        self.structure = Some(StructureDraft {
            table: TableRef {
                schema,
                name: String::new(),
            },
            is_create: true,
            new_table_name: Some(text_input(String::new(), window, cx)),
            active_tab: StructureTab::Columns,
            density: StructureDensity::Standard,
            search,
            ddl: None,
            ddl_editor: None,
            table_comment,
            original_table_comment: None,
            table_engine: mysql.then(|| "InnoDB".to_owned()),
            original_table_engine: None,
            engine_options: ENGINE_OPTIONS.map(ToOwned::to_owned).to_vec(),
            indexes: Vec::new(),
            foreign_keys: Vec::new(),
            index_editable,
            foreign_key_editable,
            trigger_editable,
            ref_columns: BTreeMap::new(),
            column_widths: STRUCTURE_DEFAULT_WIDTHS.map(px),
            preview_collapsed: false,
            preview_editor: Some(preview_editor.clone()),
            preview_editing: false,
            preview_manual: false,
            triggers: Vec::new(),
            new_triggers: Vec::new(),
            source_tables,
            original_order: Vec::new(),
            original_primary: Vec::new(),
            primary_editable: mysql,
            kind,
            columns: Vec::new(),
        });
        // 播种预览：建表模式首帧为空草稿（生成不出语句），这里只是留好入口。
        if let Some(sql) = self.structure_apply_sql(cx) {
            preview_editor.update(cx, |state, cx| state.set_value(sql, window, cx));
        }
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
        let columns: Vec<StructureColumnDraft> = meta
            .columns
            .iter()
            .map(|column| {
                // 类型名拆成「基础类型 + 长度修饰」：两格各自编辑，生成 SQL 时再拼回
                // `varchar(50)`（拆开的唯一理由是 demo 的「长度」独立列，且长度对
                // 部分类型无意义——见 `type_takes_length`）。
                let (base_type, length) = split_type_name(&column.type_name);
                let default = column.default.clone().unwrap_or_default();
                let comment = column.comment.clone();
                let charset = column.character_set.clone().unwrap_or_default();
                let collation = column.collation.clone().unwrap_or_default();
                StructureColumnDraft {
                    original: Some(column.name.clone()),
                    original_type: base_type.clone(),
                    original_length: length.clone(),
                    original_nullable: column.nullable.unwrap_or(true),
                    name: text_input(column.name.clone(), window, cx),
                    kind: text_input(base_type, window, cx),
                    length: text_input(length, window, cx),
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
        // 索引 / 外键草稿：与字段同一条「打开时的基线 + 就地改」口径。
        let indexes: Vec<StructureIndexDraft> = meta
            .indexes
            .iter()
            .map(|index| StructureIndexDraft {
                original: Some(index.name.clone()),
                original_columns: index.columns.clone(),
                original_unique: index.unique,
                system: is_system_index(&index.name, index.primary),
                name: text_input(index.name.clone(), window, cx),
                unique: index.unique,
                columns: index.columns.clone(),
                rebuild: false,
                dropped: false,
            })
            .collect();
        let foreign_keys: Vec<StructureForeignKeyDraft> = meta
            .foreign_keys
            .iter()
            .map(|foreign_key| StructureForeignKeyDraft {
                original: Some(foreign_key.name.clone()),
                original_columns: foreign_key.columns.clone(),
                original_ref_table: foreign_key.ref_table.clone(),
                original_ref_columns: foreign_key.ref_columns.clone(),
                original_on_update: normalize_action(&foreign_key.on_update),
                original_on_delete: normalize_action(&foreign_key.on_delete),
                name: text_input(foreign_key.name.clone(), window, cx),
                columns: foreign_key.columns.clone(),
                ref_table: foreign_key.ref_table.clone(),
                ref_columns: foreign_key.ref_columns.clone(),
                on_update: normalize_action(&foreign_key.on_update),
                on_delete: normalize_action(&foreign_key.on_delete),
                dropped: false,
            })
            .collect();
        // 表级属性：只有 MySQL 的 `SHOW CREATE TABLE` 尾部带 `ENGINE=… COMMENT='…'`，
        // 拿不到就是 `None`——表级属性行整行不显示（宁可不摆，也不摆一个空框）。
        let props = parse_table_props(kind, meta.ddl.as_deref());
        let table_comment = props
            .comment
            .as_ref()
            .map(|comment| text_input(comment.clone(), window, cx));
        let engine_options = props.engine.as_ref().map_or_else(Vec::new, |engine| {
            let mut options = ENGINE_OPTIONS.map(ToOwned::to_owned).to_vec();
            if !options
                .iter()
                .any(|option| option.eq_ignore_ascii_case(engine))
            {
                options.insert(0, engine.clone());
            }
            options
        });
        // 字段搜索框：一版一个实体（刷新 / 换表时随草稿重建，旧词不残留）。
        let search = self.structure_search_entity(window, cx);
        // DDL 编辑器：SQL 高亮 + 软换行（DDL 行普遍很长，横滚比折行更难读）。
        // 播种用打开时读到的建表 DDL；编辑过没有由 `structure_ddl_edited` 对
        // 原文比出来。`EditorState` 构造要 `&mut Window`，这里正好有。
        let ddl_editor = meta.ddl.as_ref().map(|ddl| {
            let state = cx.new(|editor_cx| {
                EditorState::new(&mut *window, editor_cx)
                    .language(SQL_LANGUAGE)
                    .soft_wrap(true)
            });
            state.update(cx, |state, cx| state.set_value(ddl.clone(), window, cx));
            state
        });
        // 预览编辑器（高亮 + 可编辑）：与 DDL 页同款组件，内容由草稿生成后播种。
        let preview_editor = cx.new(|editor_cx| {
            EditorState::new(&mut *window, editor_cx)
                .language(SQL_LANGUAGE)
                .soft_wrap(true)
        });
        if let Some(editor) = &ddl_editor {
            // DDL 改一个字就重渲：预览 / 标签上的未存标记都跟着走（与单元格
            // 输入框同一条事件驱动口径）。
            self._subscriptions.push(cx.subscribe(
                editor,
                |_this: &mut Self, _source: Entity<EditorState>, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                },
            ));
        }
        let original_order = meta
            .columns
            .iter()
            .map(|column| column.name.clone())
            .collect();
        let (index_editable, foreign_key_editable, trigger_editable) = structure_capabilities(kind);
        // 单元格改一个字符就重渲一次页面：SQL 预览 / 改动高亮 / 类型配色都跟着走
        // （demo 是「改动后防抖重算 SQL」，这里是事件驱动——网格就几十行，不为它
        // 再养一个定时器，也免得预览比手慢半拍）。
        let cell_inputs = columns
            .iter()
            .flat_map(|column| {
                [
                    &column.name,
                    &column.kind,
                    &column.length,
                    &column.default,
                    &column.comment,
                    &column.charset,
                    &column.collation,
                ]
            })
            .chain(indexes.iter().map(|index| &index.name))
            .chain(foreign_keys.iter().map(|foreign_key| &foreign_key.name))
            .chain(table_comment.iter());
        self.track_structure_inputs(cell_inputs, cx);
        self.structure = Some(StructureDraft {
            table,
            is_create: false,
            new_table_name: None,
            active_tab: StructureTab::Columns,
            density: StructureDensity::Standard,
            search,
            ddl: meta.ddl.clone(),
            ddl_editor,
            table_comment,
            original_table_comment: props.comment,
            table_engine: props.engine.clone(),
            original_table_engine: props.engine,
            engine_options,
            indexes,
            foreign_keys,
            index_editable,
            foreign_key_editable,
            trigger_editable,
            ref_columns: BTreeMap::new(),
            column_widths: STRUCTURE_DEFAULT_WIDTHS.map(px),
            preview_collapsed: false,
            preview_editor: Some(preview_editor.clone()),
            preview_editing: false,
            preview_manual: false,
            triggers: meta.triggers.clone(),
            new_triggers: Vec::new(),
            source_tables,
            original_order,
            original_primary: meta.primary_key.clone(),
            // 主键可编辑性：只有 MySQL 生成得了主键 DDL；其余引擎改主键 = 重建表。
            primary_editable: kind == DatabaseKind::Mysql,
            kind,
            columns,
        });
        // 播种预览：首帧就有内容，不用等一轮同步（空草稿生成不出语句时留空）。
        if let Some(sql) = self.structure_apply_sql(cx) {
            preview_editor.update(cx, |state, cx| state.set_value(sql, window, cx));
        }
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

    /// DDL 页的文本被编辑过没有（与打开时的原始 DDL 比尾随空白 trimmed 后的全文）。
    ///
    /// 比对放 `trim_end`：只敲了几个换行不算「编辑过」，免得预览无端端挂出
    /// 「DDL 已编辑」。
    fn structure_ddl_edited(&self, cx: &App) -> bool {
        self.structure
            .as_ref()
            .is_some_and(|draft| match (&draft.ddl_editor, &draft.ddl) {
                (Some(editor), Some(original)) => {
                    editor.read(cx).value().trim_end() != original.trim_end()
                }
                _ => false,
            })
    }

    /// 预览编辑态开关（「编辑 SQL」↔「完成」）。
    ///
    /// 退出时**定稿**：内容与生成结果不同就落成手改（此后应用 / 复制都用手改
    /// 文本，网格再改动不覆盖它）；一样则回到「草稿投影」态。
    fn set_structure_preview_editing(
        &mut self,
        editing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.structure.as_ref() else {
            return;
        };
        let editor = draft.preview_editor.clone();
        if editing {
            if let Some(draft) = self.structure.as_mut() {
                draft.preview_editing = true;
            }
            if let Some(editor) = editor.as_ref() {
                editor.update(cx, |state, cx| state.focus(window, cx));
            }
        } else {
            // 先算生成结果（不可变借用）再改草稿：`structure_generated_sql` 读
            // 整份草稿，不能和 `as_mut()` 的借用叠在一起。
            let generated = self.structure_generated_sql(cx).unwrap_or_default();
            let text = editor
                .as_ref()
                .map(|editor| editor.read(cx).value().trim_end().to_owned())
                .unwrap_or_default();
            if let Some(draft) = self.structure.as_mut() {
                draft.preview_editing = false;
                draft.preview_manual = text != generated.trim_end();
            }
        }
        cx.notify();
    }

    /// 「重新生成」：丢掉预览手改，回到草稿投影态（生成结果重新写进编辑器）。
    fn regenerate_structure_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(draft) = self.structure.as_mut() {
            draft.preview_manual = false;
            draft.preview_editing = false;
        }
        let generated = self.structure_generated_sql(cx).unwrap_or_default();
        if let Some(editor) = self
            .structure
            .as_ref()
            .and_then(|draft| draft.preview_editor.clone())
        {
            editor.update(cx, |state, cx| state.set_value(generated, window, cx));
        }
        cx.notify();
    }

    /// 「应用」实际要执行的语句 = 编辑后的 DDL（若 DDL 页改过）+ 结构草稿生成的
    /// `ALTER`（若有改动），按这个顺序拼成一段。
    ///
    /// 两段**都能在预览里看到**、确认弹窗执行的就是这段——手写 DDL 与网格草稿
    /// 是两条独立的改动来源，谁都不悄悄吞掉谁；冲突（比如两边都加同一列）由用户
    /// 在预览里自行取舍。
    pub(super) fn structure_apply_sql(&self, cx: &App) -> Option<String> {
        let draft = self.structure.as_ref()?;
        // 预览被**手改**（或正在编辑）：编辑器里的那段就是最终稿 —— 应用 / 复制 /
        // 关系数徽标全走它，生成结果让位（要回到生成态按「重新生成」）。
        let manual = draft.preview_editing || draft.preview_manual;
        if manual && let Some(editor) = draft.preview_editor.as_ref() {
            let text = editor.read(cx).value().trim().to_owned();
            if !text.is_empty() {
                return Some(text);
            }
        }
        self.structure_generated_sql(cx)
    }

    /// 由草稿**生成**的语句（不含预览手改）：预览同步与应用回落的基准。
    ///
    /// 「应用」真正执行的是 [`Self::structure_apply_sql`]（手改优先）；这里只算
    /// 生成结果，供「预览是否被改过」「重新生成」两处比较用。
    pub(super) fn structure_generated_sql(&self, cx: &App) -> Option<String> {
        let draft = self.structure.as_ref()?;
        // 建表模式：整条链换成 `CREATE TABLE` 汇总（没有「原表」可比对，ALTER
        // 那套增量口径无从谈起），DDL 页也不存在（没有原 DDL 可编辑）。
        if draft.is_create {
            return self.structure_create_sql(cx);
        }
        let mut parts: Vec<String> = Vec::new();
        if self.structure_ddl_edited(cx)
            && let Some(editor) = self
                .structure
                .as_ref()
                .and_then(|draft| draft.ddl_editor.as_ref())
        {
            let text = editor.read(cx).value().trim().to_owned();
            if !text.is_empty() {
                parts.push(text);
            }
        }
        if let Some(alter) = self.structure_alter_sql(cx) {
            parts.push(alter);
        }
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    /// 由草稿生成 `ALTER` 语句；没有任何改动（或引擎没有 SQL 标识符概念）时 `None`。
    ///
    /// 每次渲染预览都现算：改动数是个位数、输入框直读，不值得做增量。
    /// MySQL 的顺序变更生成物理重排（`AFTER` / `FIRST`），其余引擎保持本地显示序。
    pub(super) fn structure_alter_sql(&self, cx: &App) -> Option<String> {
        let kind = self.active_config()?.kind;
        // 引号取会话的真值（agent 档只有连上才知道）。
        let quote = self.active_quote()?;
        let draft = self.structure.as_ref()?;
        let table_text = quote_qualified(&draft.table, quote).ok()?;
        let order_changed = self.structure_order_changed();
        let (nullable_known, default_known, comment_known, charset_known) = Self::attrs_known(kind);
        let mut lines: Vec<String> = Vec::new();
        let mut notes: Vec<String> = Vec::new();
        // 表级属性（只有 MySQL 有这两项，且值要真改了才出语句）。
        if let Some((comment, original)) = draft
            .table_comment
            .as_ref()
            .zip(draft.original_table_comment.as_ref())
        {
            let text = comment.read(cx).value().trim().to_owned();
            if text != *original {
                lines.push(format!(
                    "ALTER TABLE {table_text} COMMENT = '{}';",
                    text.replace('\'', "''")
                ));
            }
        }
        if let Some((engine, original)) = draft
            .table_engine
            .as_ref()
            .zip(draft.original_table_engine.as_ref())
            && engine != original
        {
            lines.push(format!("ALTER TABLE {table_text} ENGINE = {engine};"));
        }
        for (index, column) in draft.columns.iter().enumerate() {
            let name = column.name.read(cx).value().trim().to_owned();
            // 类型与长度分两格编辑，生成 SQL 时拼回 `varchar(50)`——拼回来的这一个
            // 字符串就是「类型改没改」的比较基准。
            let kind_text = full_type(
                &column.kind.read(cx).value(),
                &column.length.read(cx).value(),
            );
            let original_type = full_type(&column.original_type, &column.original_length);
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
                    let type_changed = !kind_text.is_empty() && kind_text != original_type;
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
                        // 没有列类型概念的引擎不会走到这里（引号那一步返回 `None`）。
                        DatabaseKind::Mongo | DatabaseKind::Elasticsearch => {}
                        // agent 档：**按「如实出注释」的口径办**，与上面 SQL Server / SQLite
                        // 同一档。理由不是偷懒：这一档背后是几十个引擎，改列的写法各不相同
                        // （改名 Oracle / DB2 用 `RENAME COLUMN`、Hive 用 `CHANGE COLUMN`、
                        // 达梦又和 Oracle 一样），而驱动清单里**没有方言字段**可查。
                        // 猜一条多半跑不通的 ALTER，比明确告诉用户「请手工写」更坏。
                        DatabaseKind::Agent => {
                            notes.push(
                                "-- 子进程驱动这一档还没有方言信息，此改动未生成语句；请在查询区手工编写 ALTER。"
                                    .to_owned(),
                            );
                        }
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
        // 索引变更：系统索引（主键 / 约束隐式）只展示；改名 / 改列 / 改唯一性都按
        // 「先 DROP 再 CREATE」重建——MySQL 的 `RENAME INDEX` 只在 5.7+ 可用，
        // 统一成一个语义最清楚，也顺手实现了「重建索引」这个动作。
        if draft.index_editable {
            for index in &draft.indexes {
                if index.system {
                    continue;
                }
                let name = index.name.read(cx).value().trim().to_owned();
                if index.dropped {
                    if let Some(original) = &index.original
                        && let Some(statement) = drop_index_sql(kind, &table_text, original, quote)
                    {
                        lines.push(statement);
                    }
                    continue;
                }
                let changed = match &index.original {
                    None => true,
                    Some(original) => {
                        name != *original
                            || index.unique != index.original_unique
                            || index.columns != index.original_columns
                    }
                };
                if !changed && !index.rebuild {
                    continue;
                }
                // 列名要按**当前草稿**解析：同一批语句里列可能刚被改名，索引语句
                // 排在列变更之后，必须用新名字。
                let columns: Vec<String> = index
                    .columns
                    .iter()
                    .map(|column| resolved_column_name(draft, column, cx))
                    .collect();
                if name.is_empty() || columns.is_empty() {
                    notes.push(format!(
                        "-- 索引「{}」还没有名称或组成列，未生成语句。",
                        if name.is_empty() {
                            tr(cx, "未命名")
                        } else {
                            name.as_str()
                        }
                    ));
                    continue;
                }
                if let Some(original) = &index.original
                    && let Some(statement) = drop_index_sql(kind, &table_text, original, quote)
                {
                    lines.push(statement);
                }
                lines.push(create_index_sql(
                    &table_text,
                    &name,
                    &columns,
                    index.unique,
                    quote,
                ));
            }
        }
        // 外键变更：改约束名 / 列 / 引用 / 动作都按「先 DROP 再 ADD」重建。
        if draft.foreign_key_editable {
            for foreign_key in &draft.foreign_keys {
                let name = foreign_key.name.read(cx).value().trim().to_owned();
                if foreign_key.dropped {
                    if let Some(original) = &foreign_key.original
                        && let Some(statement) =
                            drop_foreign_key_sql(kind, &table_text, original, quote)
                    {
                        lines.push(statement);
                    }
                    continue;
                }
                let changed = match &foreign_key.original {
                    None => true,
                    Some(original) => {
                        name != *original
                            || foreign_key.columns != foreign_key.original_columns
                            || foreign_key.ref_table != foreign_key.original_ref_table
                            || foreign_key.ref_columns != foreign_key.original_ref_columns
                            || foreign_key.on_update != foreign_key.original_on_update
                            || foreign_key.on_delete != foreign_key.original_on_delete
                    }
                };
                if !changed {
                    continue;
                }
                if foreign_key.ref_table.trim().is_empty()
                    || foreign_key.columns.is_empty()
                    || foreign_key.columns.len() != foreign_key.ref_columns.len()
                {
                    notes.push(
                        "-- 外键需要「引用表 + 两边列数一致」才生成语句（复合外键按列序对齐）。"
                            .to_owned(),
                    );
                    continue;
                }
                let columns: Vec<String> = foreign_key
                    .columns
                    .iter()
                    .map(|column| resolved_column_name(draft, column, cx))
                    .collect();
                if let Some(original) = &foreign_key.original
                    && let Some(statement) =
                        drop_foreign_key_sql(kind, &table_text, original, quote)
                {
                    lines.push(statement);
                }
                lines.push(add_foreign_key_sql(
                    &table_text,
                    &name,
                    &columns,
                    &foreign_key.ref_table,
                    &foreign_key.ref_columns,
                    &foreign_key.on_update,
                    &foreign_key.on_delete,
                    quote,
                ));
            }
        }
        // 触发器：本轮新增的草稿生成 `CREATE TRIGGER`（与建表路径共用同一段，
        // 见 [`Self::push_new_trigger_sql`]）。
        self.push_new_trigger_sql(draft, kind, &table_text, quote, &mut lines, &mut notes, cx);
        // 顺序变更只有 MySQL 物理生效；其余引擎如实说明。
        if order_changed && kind != DatabaseKind::Mysql {
            notes.push("-- 顺序变更仅 MySQL 物理生效，其余引擎保持本地显示序。".to_owned());
        }
        for note in notes {
            lines.push(note);
        }
        (!lines.is_empty()).then(|| lines.join("\n"))
    }

    /// 本轮新增触发器的 `CREATE TRIGGER` 语句（编辑 / 建表两路共用；已有触发器
    /// 只读，不改不删）。触发体按引擎：MySQL / SQLite 是 `FOR EACH ROW` 后跟的
    /// 内联语句；PG 是函数名，这里补 `EXECUTE FUNCTION …()`（用户已写括号就不重复补）。
    fn push_new_trigger_sql(
        &self,
        draft: &StructureDraft,
        kind: DatabaseKind,
        table_text: &str,
        quote: char,
        lines: &mut Vec<String>,
        notes: &mut Vec<String>,
        cx: &App,
    ) {
        for trigger in &draft.new_triggers {
            let name = trigger.name.read(cx).value().trim().to_owned();
            let statement = trigger.statement.read(cx).value().trim().to_owned();
            if name.is_empty() || statement.is_empty() {
                // 名字 / 触发体没填完就不出语句：半条 CREATE TRIGGER 执行必炸。
                continue;
            }
            // 名字带引擎引号字符（或 NUL）就拒绝——拼进语句会被拆散。这里**只跳这一条**：
            // 整个应用路径 `?` 出去会把已经攒好的列 / 外键语句一起弄丢，预览变空更费解。
            let quoted_name = match quote_identifier(&name, quote) {
                Ok(quoted) => quoted,
                Err(_) => {
                    notes.push(
                        "-- 触发器名含引擎引号字符，拼进语句会被拆散，已跳过该触发器。".to_owned(),
                    );
                    continue;
                }
            };
            let body = match kind {
                DatabaseKind::Postgres => {
                    let call = if statement.ends_with(')') {
                        statement
                    } else {
                        format!("{statement}()")
                    };
                    format!("EXECUTE FUNCTION {call}")
                }
                _ => statement,
            };
            lines.push(format!(
                "CREATE TRIGGER {quoted_name} {} {} ON {table_text} FOR EACH ROW {body};",
                trigger.timing, trigger.event,
            ));
        }
    }

    /// 由建表草稿生成 `CREATE TABLE`（建表模式的「应用变更」与预览共这一条口径）。
    ///
    /// 列定义拼进建表语句本体；主键内联 `PRIMARY KEY (…)`；MySQL 追加
    /// `ENGINE / COMMENT` 表级选项；索引 / 外键 / 触发器跟在建表语句后面独立执行
    /// （三家的公共语法，MySQL / PG / SQLite 都吃得下）。一个字段都没有就不出语句
    ///（dbx 同款校验：空表建不得）。
    pub(super) fn structure_create_sql(&self, cx: &App) -> Option<String> {
        let kind = self.active_config()?.kind;
        // 引号取会话的真值（agent 档只有连上才知道）。
        let quote = self.active_quote()?;
        let draft = self.structure.as_ref()?;
        let name = draft
            .new_table_name
            .as_ref()?
            .read(cx)
            .value()
            .trim()
            .to_owned();
        if name.is_empty() {
            return None;
        }
        let table_ref = TableRef {
            schema: draft.table.schema.clone(),
            name,
        };
        let table_text = quote_qualified(&table_ref, quote).ok()?;
        let (_, default_known, _, _) = Self::attrs_known(kind);
        let mysql = kind == DatabaseKind::Mysql;
        let mut defs: Vec<String> = Vec::new();
        let mut primary_key: Vec<String> = Vec::new();
        for column in &draft.columns {
            if column.dropped {
                continue;
            }
            let column_name = column.name.read(cx).value().trim().to_owned();
            // 类型与长度分两格编辑，生成 SQL 时拼回 `varchar(50)`（与编辑路径同一拼法）。
            let kind_text = full_type(
                &column.kind.read(cx).value(),
                &column.length.read(cx).value(),
            );
            if column_name.is_empty() || kind_text.is_empty() {
                continue;
            }
            // 属性只下发**该引擎吃得住**的档位：非 MySQL 的注释 / 字符集内联进列
            // 定义是 MySQL 语法，别处执行必炸——传空串让 [`Self::column_definition`]
            // 自己略掉。默认值同理（内核拿不到默认值概念的引擎不下发）。
            let default_text = if default_known {
                column.default.read(cx).value().trim().to_owned()
            } else {
                String::new()
            };
            let (comment_text, charset_text, collation_text) = if mysql {
                (
                    column.comment.read(cx).value().trim().to_owned(),
                    column.charset.read(cx).value().trim().to_owned(),
                    column.collation.read(cx).value().trim().to_owned(),
                )
            } else {
                (String::new(), String::new(), String::new())
            };
            let definition = self.column_definition(
                &column_name,
                &kind_text,
                column.nullable,
                &default_text,
                &comment_text,
                &charset_text,
                &collation_text,
            );
            if column.primary
                && let Ok(quoted) = quote_identifier(&column_name, quote)
            {
                primary_key.push(quoted);
            }
            defs.push(definition);
        }
        // dbx 同款：至少一个字段才生成语句（空表建不得）。
        if defs.is_empty() {
            return None;
        }
        if !primary_key.is_empty() {
            defs.push(format!("PRIMARY KEY ({})", primary_key.join(", ")));
        }
        let mut create = format!("CREATE TABLE {table_text} (\n  {},\n)", defs.join(",\n  "));
        if mysql {
            let mut options = String::new();
            if let Some(engine) = draft.table_engine.as_deref() {
                options.push_str(&format!(" ENGINE={engine}"));
            }
            if let Some(comment) = draft.table_comment.as_ref() {
                let text = comment.read(cx).value().trim().to_owned();
                if !text.is_empty() {
                    options.push_str(&format!(" COMMENT='{}'", text.replace('\'', "''")));
                }
            }
            if !options.is_empty() {
                create = format!(
                    "CREATE TABLE {table_text} (\n  {}\n){options};",
                    defs.join(",\n  ")
                );
            }
        }
        let mut lines = vec![create];
        let mut notes: Vec<String> = Vec::new();
        // 索引：与编辑路径同一「独立 CREATE INDEX」口径（MySQL 内联 `KEY` 与它
        // 语义等价，统一成一种最好核对）。列名按当前草稿解析（刚改过的名字生效）。
        if draft.index_editable {
            for index in &draft.indexes {
                if index.system {
                    continue;
                }
                let index_name = index.name.read(cx).value().trim().to_owned();
                let columns: Vec<String> = index
                    .columns
                    .iter()
                    .map(|column| resolved_column_name(draft, column, cx))
                    .collect();
                if index_name.is_empty() || columns.is_empty() {
                    notes.push(format!(
                        "-- 索引「{}」还没有名称或组成列，未生成语句。",
                        if index_name.is_empty() {
                            tr(cx, "未命名")
                        } else {
                            index_name.as_str()
                        }
                    ));
                    continue;
                }
                lines.push(create_index_sql(
                    &table_text,
                    &index_name,
                    &columns,
                    index.unique,
                    quote,
                ));
            }
        }
        // 外键：MySQL / PG / T-SQL 的公共 `ALTER TABLE … ADD CONSTRAINT` 语法
        //（SQLite 本来就不在这一档，见 `structure_capabilities`）。
        if draft.foreign_key_editable {
            for foreign_key in &draft.foreign_keys {
                if foreign_key.dropped {
                    continue;
                }
                let foreign_key_name = foreign_key.name.read(cx).value().trim().to_owned();
                if foreign_key.ref_table.trim().is_empty()
                    || foreign_key.columns.is_empty()
                    || foreign_key.columns.len() != foreign_key.ref_columns.len()
                {
                    notes.push(
                        "-- 外键需要「引用表 + 两边列数一致」才生成语句（复合外键按列序对齐）。"
                            .to_owned(),
                    );
                    continue;
                }
                let columns: Vec<String> = foreign_key
                    .columns
                    .iter()
                    .map(|column| resolved_column_name(draft, column, cx))
                    .collect();
                lines.push(add_foreign_key_sql(
                    &table_text,
                    &foreign_key_name,
                    &columns,
                    &foreign_key.ref_table,
                    &foreign_key.ref_columns,
                    &foreign_key.on_update,
                    &foreign_key.on_delete,
                    quote,
                ));
            }
        }
        // 触发器：与编辑路径共用同一段（建表语句在前，触发器落点表已存在）。
        self.push_new_trigger_sql(draft, kind, &table_text, quote, &mut lines, &mut notes, cx);
        for note in notes {
            lines.push(note);
        }
        Some(lines.join("\n"))
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
        let quote = self.active_quote().unwrap_or('`');
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

    /// 当前停在「字段」标签上（行级快捷键只在这个标签里有意义）。
    fn structure_active_columns(&self) -> bool {
        self.structure
            .as_ref()
            .is_some_and(|draft| draft.active_tab == StructureTab::Columns)
    }

    /// ⌘D / ⌘⌫ 的作用对象：**有焦点的那个单元格所在的行**——demo 用
    /// `document.activeElement.closest("tr")` 找行，这里是同一口径（行上七种输入
    /// 框任一带焦点即算该行）。没焦点时返回 `None`，快捷键不动作。
    fn focused_structure_row(&self, window: &Window, cx: &App) -> Option<usize> {
        let draft = self.structure.as_ref()?;
        draft.columns.iter().position(|column| {
            [
                &column.name,
                &column.kind,
                &column.length,
                &column.default,
                &column.comment,
                &column.charset,
                &column.collation,
            ]
            .iter()
            .any(|state| state.focus_handle(cx).is_focused(window))
        })
    }

    /// 选中一个结构标签。
    fn select_structure_tab(&mut self, tab: StructureTab, cx: &mut Context<Self>) {
        if let Some(draft) = self.structure.as_mut()
            && draft.active_tab != tab
            // 建表模式没有 DDL 页（标签条也不画它）：兜一道，防止状态漂移。
            && !(draft.is_create && tab == StructureTab::Ddl)
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

    /// 行重排的落点（`StructureRowDrag` 的 `on_drag_move`）：拖过哪行就把源行搬到
    /// 哪一位。按**身份**找源行（同一行上会反复触发，按下标搬会越拖越乱）。
    fn move_structure_row(&mut self, row: gpui::EntityId, to: usize, cx: &mut Context<Self>) {
        let Some(draft) = self.structure.as_mut() else {
            return;
        };
        let Some(from) = draft
            .columns
            .iter()
            .position(|column| column.name.entity_id() == row)
        else {
            return;
        };
        if from == to || to >= draft.columns.len() {
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
                read(&source.length),
                read(&source.default),
                read(&source.comment),
                read(&source.charset),
                read(&source.collation),
                source.name.read(cx).value().trim().to_owned(),
            )
        };
        let (nullable, kind_text, length_text, default, comment, charset, collation, source_name) =
            source;
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
            original_length: String::new(),
            original_nullable: true,
            name: cx.new(|cell_cx| InputState::new(&mut *window, cell_cx)),
            kind: cx.new(|cell_cx| InputState::new(&mut *window, cell_cx)),
            length: cx.new(|cell_cx| InputState::new(&mut *window, cell_cx)),
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
            .length
            .update(cx, |state, cx| state.set_value(length_text, window, cx));
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
        // 引擎名走 `EngineRef`（agent 档的真名在驱动清单里）。
        let engine_label = self.active_config().map_or_else(String::new, |config| {
            tr(cx, EngineRef::from_config(config).label()).to_owned()
        });
        // 建表模式：目标条换成「表名输入框」（服务器上还没有这张表，标签是
        // 现场填的）；刷新按钮也不给——没有「重新内省」的对象可刷。
        if draft.is_create {
            let control_h = theme.density().control;
            return h_flex()
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
                        .flex_none()
                        .text_size(tokens.typography.sm.size)
                        .text_color(tokens.colors.muted_foreground)
                        .child(tr(cx, "表名")),
                )
                .child(div().flex_1().min_w_0().max_w(px(360.0)).child(grid_input(
                    draft.new_table_name.as_ref().unwrap(),
                    control_h,
                    tokens.typography.sm.size,
                    false,
                )))
                .child(neutral_tag(SharedString::from(engine_label), cx))
                .into_any_element();
        }
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
                    .child(selectable_text(SharedString::from(table_label))),
            )
            .child(neutral_tag(SharedString::from(engine_label), cx))
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

    /// 表级属性行：表注释 + 存储引擎（demo 的第二行）。
    ///
    /// 只有 MySQL 拿得到这两个值（从 `SHOW CREATE TABLE` 的收尾行解析，见
    /// [`parse_table_props`]）；拿不到就**整行不出现**——摆一个空框等于假承诺。
    fn render_structure_table_props(&self, cx: &App) -> Option<AnyElement> {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let control_h = theme.density().control;
        let draft = self.structure.as_ref()?;
        if draft.table_comment.is_none() && draft.table_engine.is_none() {
            return None;
        }
        let mut rows = v_flex().w_full().flex_none().gap(tokens.spacing.sm);
        if let Some(comment) = draft.table_comment.as_ref() {
            rows = rows.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .child(
                        div()
                            .flex_none()
                            .w(px(72.0))
                            .text_size(tokens.typography.sm.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(tr(cx, "注释")),
                    )
                    .child(div().flex_none().w(px(320.0)).child(grid_input(
                        comment,
                        control_h,
                        tokens.typography.sm.size,
                        false,
                    ))),
            );
        }
        if let Some(engine) = draft.table_engine.as_ref() {
            let view = self.view.clone();
            rows = rows.child(
                h_flex()
                    .w_full()
                    .items_center()
                    .gap(tokens.spacing.sm)
                    .child(
                        div()
                            .flex_none()
                            .w(px(72.0))
                            .text_size(tokens.typography.sm.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(tr(cx, "存储引擎")),
                    )
                    .child(
                        div().flex_none().w(px(220.0)).child(dropdown_select(
                            "db-structure-engine",
                            draft
                                .engine_options
                                .iter()
                                .map(|option| {
                                    (
                                        SharedString::from(option.clone()),
                                        SharedString::from(option.clone()),
                                    )
                                })
                                .collect::<Vec<_>>(),
                            SharedString::from(engine.clone()),
                            move |engine, _, cx| {
                                let _ = view.update(cx, |this, cx| {
                                    this.set_structure_table_engine(engine.to_string(), cx);
                                });
                            },
                            cx,
                        )),
                    ),
            );
        }
        Some(rows.into_any_element())
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
                Some(
                    draft
                        .columns
                        .iter()
                        .filter(|column| !column.dropped)
                        .count(),
                ),
            ),
            (
                StructureTab::Indexes,
                tr(cx, "索引").into(),
                Some(draft.indexes.iter().filter(|index| !index.dropped).count()),
            ),
            (
                StructureTab::ForeignKeys,
                tr(cx, "外键").into(),
                Some(
                    draft
                        .foreign_keys
                        .iter()
                        .filter(|item| !item.dropped)
                        .count(),
                ),
            ),
            (
                StructureTab::Constraints,
                tr(cx, "约束").into(),
                Some(self.structure_constraint_count()),
            ),
            (
                StructureTab::Partitions,
                tr(cx, "分区").into(),
                Some(usize::from(
                    draft
                        .ddl
                        .as_deref()
                        .is_some_and(|ddl| parse_partition_clause(ddl).is_some()),
                )),
            ),
            (
                StructureTab::Triggers,
                tr(cx, "触发器").into(),
                Some(draft.triggers.len()),
            ),
        ];
        let mut tab_row = h_flex().flex_none().items_center().gap(tokens.spacing.xxs);
        // DDL 页手改过还没应用：标签上挂一枚未存圆点（demo 的 `.ddl-dirty .dot`）。
        let ddl_dirty = self.structure_ddl_edited(cx);
        for (tab, label, count) in tabs {
            // 建表模式没有「原 DDL」可看：DDL 标签不出现（dbx 同款）。
            if draft.is_create && tab == StructureTab::Ddl {
                continue;
            }
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
                // 自绘的可点元素必须给 role + 选中态（与 shell 的标签条同一条纪律，
                // 见 `render_tabs.rs`）：读屏才知道这是一组标签、当前看的是哪一个。
                .role(Role::Tab)
                .aria_selected(active)
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
            if tab == StructureTab::Ddl && ddl_dirty {
                tab_el = tab_el.child(
                    div()
                        .flex_none()
                        .size(px(6.0))
                        .rounded_full()
                        .bg(extended.colors.warning),
                );
            }
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
    ///
    /// 「查看数据」五个标签都出现（它跳去数据标签，与当前看哪个标签无关）；行密度
    /// 只跟字段网格有关，所以在别的标签上不出现 —— 摆一个按了没反应的控件比不摆更糟。
    fn render_structure_tab_tools(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let view = self.view.clone();
        let Some(draft) = self.structure.as_ref() else {
            return div().into_any_element();
        };
        let mut tools = h_flex().flex_none().items_center().gap(tokens.spacing.xxs);
        tools = tools.child(
            button(
                "db-structure-view-data",
                tr(cx, "查看数据"),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click({
                let click_view = view.clone();
                let table = draft.table.clone();
                move |_, window, cx| {
                    let _ = click_view.update(cx, |this, cx| {
                        this.open_table(table.clone(), window, cx);
                    });
                }
            }),
        );
        match draft.active_tab {
            // DDL 标签：复制整段 DDL。
            StructureTab::Ddl => tools = tools.child(self.copy_ddl_button(cx)),
            // 字段标签：密度 + 搜索框（带命中计数）+ 新增字段 + 复制其他表字段 + 复制全部列名。
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
                let matched = self.structure_search_match_count(cx);
                tools = tools
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
                    .child(
                        h_flex()
                            .id("db-structure-search")
                            .w(px(190.0))
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
                            )
                            .children(matched.map(|count| {
                                div()
                                    .flex_none()
                                    .text_size(tokens.typography.xs.size)
                                    .text_color(extended.colors.text_tertiary)
                                    .child(SharedString::from(count.to_string()))
                            })),
                    )
                    .child(
                        icon_text_button(
                            "db-structure-add",
                            tr(cx, "新增字段"),
                            Icon::new(DuckIcon::Plus).size(extended.icon.sm),
                            ButtonVariant::Primary,
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
                        // 表清单可能上百张：走可搜索档（菜单限高 + 顶部搜索框）。
                        dropdown_select_searchable(
                            "db-structure-copy-fields",
                            source_options,
                            tr(cx, "复制其他表字段"),
                            tr(cx, "搜索…"),
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
                    .child(
                        button(
                            "db-structure-copy-names",
                            tr(cx, "复制全部列名"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .on_click({
                            let click_view = view.clone();
                            move |_, window, cx| {
                                let _ = click_view.update(cx, |this, cx| {
                                    this.toggle_structure_copy_names(cx);
                                    this.show_structure_copy_names_dialog(window, cx);
                                });
                            }
                        }),
                    );
            }
            // 索引标签：「新增索引」入口**只有**标签栏右侧这一个（内容区不再摆
            // 第二枚，免得同一个动作在同一屏出现两次）。外键同款。
            StructureTab::Indexes => {
                tools = tools.child(
                    icon_text_button(
                        "db-structure-add-index-top",
                        tr(cx, "新增索引"),
                        Icon::new(DuckIcon::Plus).size(extended.icon.sm),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click({
                        let add_view = view.clone();
                        move |_, window, cx| {
                            let _ = add_view
                                .update(cx, |this, cx| this.add_structure_index(window, cx));
                        }
                    }),
                );
            }
            StructureTab::ForeignKeys => {
                tools = tools.child(
                    icon_text_button(
                        "db-structure-add-fk-top",
                        tr(cx, "新增外键"),
                        Icon::new(DuckIcon::Plus).size(extended.icon.sm),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click({
                        let add_view = view.clone();
                        move |_, window, cx| {
                            let _ = add_view
                                .update(cx, |this, cx| this.add_structure_foreign_key(window, cx));
                        }
                    }),
                );
            }
            // 触发器：引擎生成得了 `CREATE TRIGGER` 才给新增入口（已有触发器仍只读）。
            StructureTab::Triggers
                if self
                    .structure
                    .as_ref()
                    .is_some_and(|draft| draft.trigger_editable) =>
            {
                tools = tools.child(
                    icon_text_button(
                        "db-structure-add-trigger-top",
                        tr(cx, "新增触发器"),
                        Icon::new(DuckIcon::Plus).size(extended.icon.sm),
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click({
                        let add_view = view.clone();
                        move |_, window, cx| {
                            let _ = add_view
                                .update(cx, |this, cx| this.add_structure_trigger(window, cx));
                        }
                    }),
                );
            }
            StructureTab::Triggers => {}
            // 约束 / 分区是只读总览，工具区不放任何按钮 —— 要改约束去字段 / 索引 /
            // 外键三页（那里才是生成 DDL 的地方）。
            StructureTab::Constraints | StructureTab::Partitions => {}
        }
        tools.into_any_element()
    }

    /// 「复制 DDL」按钮（DDL 标签的工具；只读，`&App` 即可）。
    fn copy_ddl_button(&self, cx: &App) -> AnyElement {
        let view = self.view.clone();
        let disabled = self
            .structure
            .as_ref()
            .is_none_or(|draft| draft.ddl.is_none());
        button(
            "db-structure-copy-ddl",
            tr(cx, "复制 DDL"),
            ButtonVariant::Secondary,
            cx,
        )
        .disabled(disabled)
        .on_click({
            let click_view = view.clone();
            move |_, _, cx| {
                let _ = click_view.update(cx, |this, cx| {
                    let ddl = this.structure.as_ref().and_then(|draft| draft.ddl.clone());
                    if let Some(ddl) = ddl {
                        this.put_on_clipboard(ddl, tr(cx, "已复制 DDL"), cx);
                    }
                });
            }
        })
        .into_any_element()
    }

    /// 搜索词命中的字段数（搜索框右侧的计数；空词 = 不显示）。
    ///
    /// 与 [`Self::structure_column_matches`] 同一条判据——计数与实际过滤必须是
    /// 同一份口径，否则「显示 3 条却只筛出 2 行」。
    fn structure_search_match_count(&self, cx: &App) -> Option<usize> {
        let draft = self.structure.as_ref()?;
        let needle = draft.search.read(cx).value().trim().to_lowercase();
        if needle.is_empty() {
            return None;
        }
        Some(
            draft
                .columns
                .iter()
                .filter(|column| {
                    Self::structure_column_matches(
                        &needle,
                        &column.name.read(cx).value(),
                        &column.kind.read(cx).value(),
                        &column.comment.read(cx).value(),
                    )
                })
                .count(),
        )
    }

    /// 字段搜索的判据（字段名 / 类型 / 注释，大小写不敏感；空词全中）。
    fn structure_column_matches(needle: &str, name: &str, kind: &str, comment: &str) -> bool {
        needle.is_empty()
            || name.to_lowercase().contains(needle)
            || kind.to_lowercase().contains(needle)
            || comment.to_lowercase().contains(needle)
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
        // 列宽（可拖拽）与十列总宽：内容宽度取十列之和，与 `min_w_full()` 一起
        // 决定「窄窗口横向滚动、宽窗口右侧留白」。
        let widths = draft.column_widths;
        let content_width: Pixels = widths.iter().fold(px(0.), |sum, width| sum + *width);
        let type_options = structure_type_options(draft.kind);
        let matches = |name: &str, kind_text: &str, comment: &str| {
            let needle = search.to_lowercase();
            search.is_empty()
                || name.to_lowercase().contains(&needle)
                || kind_text.to_lowercase().contains(&needle)
                || comment.to_lowercase().contains(&needle)
        };
        // 表头：十列（第一列左端是 `#` 序号位）+ 每列右端的拖拽调宽热区。
        let labels: [(SharedString, bool); STRUCTURE_COLUMNS] = [
            (tr(cx, "操作").into(), false),
            (tr(cx, "字段名").into(), false),
            (tr(cx, "类型").into(), false),
            (tr(cx, "长度").into(), false),
            (tr(cx, "可为空").into(), true),
            (tr(cx, "主键").into(), true),
            (tr(cx, "默认值").into(), false),
            (tr(cx, "注释").into(), false),
            (tr(cx, "字符集").into(), false),
            (tr(cx, "排序规则").into(), false),
        ];
        let mut header = h_flex()
            .flex_none()
            .w_full()
            .items_center()
            .pb(tokens.spacing.xxs)
            .border_b_1()
            .border_color(tokens.colors.border);
        for (column, (label, centered)) in labels.into_iter().enumerate() {
            let width = widths[column];
            let handle = div()
                .id(SharedString::from(format!("db-structure-resize-{column}")))
                .flex_none()
                .w(px(5.0))
                .h(control_h)
                .cursor(CursorStyle::ResizeLeftRight)
                .hover(|style| style.bg(extended.colors.drag_border))
                .on_drag(
                    StructureColResize {
                        index: column,
                        start_w: width,
                        start_x: px(0.),
                    },
                    |drag, position, _, cx| {
                        cx.stop_propagation();
                        let drag = StructureColResize {
                            start_x: position.x,
                            ..drag.clone()
                        };
                        cx.new(|_| drag)
                    },
                );
            let cell = if column == 0 {
                // 序号位与「操作」共一列：`#` 占 26px，右侧留 1px 竖线分隔。
                h_flex()
                    .flex_none()
                    .w(width)
                    .items_center()
                    .child(
                        div()
                            .flex_none()
                            .w(px(26.0))
                            .text_size(tokens.typography.xs.size)
                            .text_color(extended.colors.text_tertiary)
                            .child(SharedString::from("#")),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .px(tokens.spacing.xs)
                            .child(caption_heading(label, cx)),
                    )
            } else if centered {
                h_flex()
                    .flex_none()
                    .w(width)
                    .justify_center()
                    .child(caption_heading(label, cx))
            } else {
                h_flex()
                    .flex_none()
                    .w(width)
                    .items_center()
                    .px(tokens.spacing.xs)
                    .child(div().flex_1().min_w_0().child(caption_heading(label, cx)))
            };
            header = header.child(cell.child(handle));
        }
        // 调宽监听挂在**整个网格**上（不是只挂在表头上）：`on_drag_move` 只在指针
        // 压着的元素上触发，只挂表头的话指针往下一飘宽度就不再跟手了。
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
                        || kind_text != full_type(&column.original_type, &column.original_length)
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
            // 十列定宽单元格（序号+操作 / 字段名 / 类型 / 长度 / 可为空 / 主键 /
            // 默认值 / 注释 / 字符集 / 排序规则），与表头一一对应。
            let text_cell = |state: &Entity<InputState>, width: Pixels, size: Pixels, off: bool| {
                h_flex()
                    .flex_none()
                    .w(width)
                    .min_w_0()
                    .items_center()
                    // 标记删除的行：值文字打删除线（demo `tr.f-del .cell-input` 同款；
                    // 只打值，操作列的图标不打）。
                    .when(dropped, |cell| cell.line_through())
                    .child(grid_input(state, control_h, size, off))
            };
            // 类型不带长度时长度框禁用（`varchar(50)` 的 50 有意义，`bigint(20)`
            // 的 20 只是显示宽度——但值非空仍照原样回写，认错也不丢长度）。
            let length_lock = dropped || !type_takes_length(&column.kind.read(cx).value());
            let row = h_flex()
                .id(SharedString::from(format!("db-structure-row-{index}")))
                .w_full()
                .flex_none()
                .items_center()
                .py(row_py)
                // 标记删除的行红底、新增行绿底（demo 同款）：删除是**草稿态**，
                // 撤得回来，所以整行只是换个底色而不是消失。
                .when(dropped, |row| {
                    row.bg(tokens.colors.destructive.opacity(extended.opacity.tint))
                })
                .when(is_new && !dropped, |row| {
                    row.bg(extended.colors.success.opacity(extended.opacity.tint))
                })
                .when(!dropped && !is_new, |row| {
                    row.hover(|style| style.bg(extended.colors.row_hover))
                })
                // 拖拽落点：拖着列划过这一行就活重排到这一位（dbx 同款体感）。
                .on_drag_move::<StructureRowDrag>({
                    let drag_view = view.clone();
                    let this_row = column.name.entity_id();
                    move |event, _, cx| {
                        let source = event.drag(cx).row;
                        // 指针还压在自己身上时不用搬。
                        if source == this_row {
                            return;
                        }
                        let _ = drag_view.update(cx, |this, cx| {
                            this.move_structure_row(source, index, cx);
                        });
                    }
                })
                // ① 序号 + 操作位：序号（主键行加 PK 角标）+ 拖拽把手 + 复制 + 删除。
                .child(
                    h_flex()
                        .flex_none()
                        .w(widths[0])
                        .items_center()
                        .child(
                            h_flex()
                                .flex_none()
                                .w(px(30.0))
                                .items_center()
                                .gap(px(3.0))
                                .child(
                                    div()
                                        .flex_none()
                                        .text_size(tokens.typography.xs.size)
                                        .text_color(if changed {
                                            tokens.colors.primary
                                        } else {
                                            extended.colors.text_tertiary
                                        })
                                        .child(SharedString::from((index + 1).to_string())),
                                )
                                .when(column.primary && !dropped, |cell| {
                                    cell.child(
                                        div()
                                            .flex_none()
                                            .text_size(tokens.typography.xs.size)
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(tokens.colors.primary)
                                            .child(SharedString::from("PK")),
                                    )
                                }),
                        )
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
                                // 自绘的可点元素必须给 role + 无障碍名；那个左右箭头
                                // 图标本身看不出是「拖我排序」，鼠标用户要的是悬停提示
                                // （提示与无障碍名共用同一份文案）。
                                .role(Role::Button)
                                .aria_label(tr(cx, "拖拽排序"))
                                .tooltip({
                                    let tip = SharedString::from(tr(cx, "拖拽排序"));
                                    move |window, cx| Tooltip::new(tip.clone()).build(window, cx)
                                })
                                .hover(|style| style.bg(extended.colors.row_hover))
                                .child(
                                    Icon::new(DuckIcon::ArrowRightLeft)
                                        .size(extended.icon.sm)
                                        .text_color(tokens.colors.muted_foreground),
                                )
                                // 载荷带本行身份：`on_drag_move` 在别的行上按身份找源行，
                                // 反复触发也不会搬乱（见 `StructureRowDrag` 注释）。
                                .on_drag(
                                    StructureRowDrag {
                                        row: column.name.entity_id(),
                                    },
                                    |drag, _, _, cx| {
                                        cx.stop_propagation();
                                        cx.new(|_| drag.clone())
                                    },
                                ),
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
                // ② 字段名。
                .child(text_cell(
                    &column.name,
                    widths[1],
                    tokens.typography.sm.size,
                    dropped,
                ))
                // ③ 类型（可编辑下拉：候选只是快捷入口，框里仍可自由输入）。
                .child(
                    h_flex()
                        .flex_none()
                        .w(widths[2])
                        .min_w_0()
                        .items_center()
                        .when(dropped, |cell| cell.line_through())
                        .child(grid_input(
                            &column.kind,
                            control_h,
                            tokens.typography.sm.size,
                            dropped,
                        ))
                        .child(self.cell_menu_button(
                            index,
                            CellMenu::Type,
                            type_color(&column.kind.read(cx).value(), theme),
                            type_options,
                            "",
                            cx,
                        )),
                )
                // ④ 长度（类型不带长度时禁用：值非空仍照原样回写，不会悄悄丢）。
                .child(text_cell(
                    &column.length,
                    widths[3],
                    tokens.typography.mono_md.size,
                    length_lock,
                ))
                // ⑤ 可为空（主键列联动取消且禁用，demo 同款）。
                .child(
                    h_flex()
                        .flex_none()
                        .w(widths[4])
                        .justify_center()
                        .when(nullable_locked, |style| {
                            style.opacity(extended.opacity.disabled)
                        })
                        .child(
                            switch_control(
                                SharedString::from(format!("db-structure-null-{index}")),
                                column.nullable,
                                {
                                    let toggle_view = view.clone();
                                    move |checked, _, cx| {
                                        // gpui-base 的 `Switch` 没实现 `Disableable`，
                                        // 没有 bool 版 `.disabled()` —— 闸门在回调里拦。
                                        if nullable_locked {
                                            return;
                                        }
                                        let _ = toggle_view.update(cx, |this, cx| {
                                            this.set_column_nullable(index, *checked, cx);
                                        });
                                    }
                                },
                            )
                            // 开关本身没有文字：悬停提示补上「这个开关是什么」。
                            .tooltip(tr(cx, "可为空")),
                        ),
                )
                // ⑥ 主键。
                .child(
                    h_flex()
                        .flex_none()
                        .w(widths[5])
                        .justify_center()
                        .when(pk_locked, |style| style.opacity(extended.opacity.disabled))
                        .child(
                            switch_control(
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
                            )
                            .tooltip(tr(cx, "主键")),
                        ),
                )
                // ⑦ 默认值 + 常用值预设。
                .child(
                    h_flex()
                        .flex_none()
                        .w(widths[6])
                        .min_w_0()
                        .items_center()
                        .child(grid_input(
                            &column.default,
                            control_h,
                            tokens.typography.mono_md.size,
                            dropped || (!default_known && !is_new),
                        ))
                        .child(self.default_preset_button(index, is_new, cx)),
                )
                // ⑧ 注释 + 放大编辑。
                .child(
                    h_flex()
                        .flex_none()
                        .w(widths[7])
                        .min_w_0()
                        .items_center()
                        .child(grid_input(
                            &column.comment,
                            control_h,
                            tokens.typography.sm.size,
                            dropped || (!comment_known && !is_new),
                        ))
                        .child(
                            icon_button(
                                SharedString::from(format!("db-structure-comment-{index}")),
                                tr(cx, "放大编辑注释"),
                                Icon::new(DuckIcon::Pencil).size(extended.icon.sm),
                                ButtonVariant::Ghost,
                                cx,
                            )
                            .disabled(dropped)
                            .on_click({
                                let click_view = view.clone();
                                move |_, window, cx| {
                                    let _ = click_view.update(cx, |this, cx| {
                                        this.open_structure_comment(index, window, cx);
                                    });
                                }
                            }),
                        ),
                )
                // ⑨ 字符集（只有 MySQL 拿得到；空 = 表默认）。
                .child(
                    h_flex()
                        .flex_none()
                        .w(widths[8])
                        .min_w_0()
                        .items_center()
                        .child(grid_input(
                            &column.charset,
                            control_h,
                            tokens.typography.mono_md.size,
                            dropped || (!charset_known && !is_new),
                        ))
                        .when(charset_known || is_new, |cell| {
                            cell.child(self.cell_menu_button(
                                index,
                                CellMenu::Charset,
                                tokens.colors.muted_foreground,
                                &CHARSET_OPTIONS,
                                tr(cx, "表默认"),
                                cx,
                            ))
                        }),
                )
                // ⑩ 排序规则。
                .child(
                    h_flex()
                        .flex_none()
                        .w(widths[9])
                        .min_w_0()
                        .items_center()
                        .child(grid_input(
                            &column.collation,
                            control_h,
                            tokens.typography.mono_md.size,
                            dropped || (!charset_known && !is_new),
                        ))
                        .when(charset_known || is_new, |cell| {
                            cell.child(self.cell_menu_button(
                                index,
                                CellMenu::Collation,
                                tokens.colors.muted_foreground,
                                &COLLATION_OPTIONS,
                                tr(cx, "表默认"),
                                cx,
                            ))
                        }),
                );
            rows = rows.child(row);
        }
        if visible == 0 && !search.is_empty() {
            return empty_state(DuckIcon::Search, tr(cx, "没有匹配的字段"), None, None, cx)
                .into_any_element();
        }
        // 建表模式一张白纸：明说「从哪开始」，别让人对着一格空网格发呆。
        if visible == 0 && draft.is_create {
            return empty_state(
                DuckIcon::Plus,
                tr(cx, "还没有字段，点上方「新增字段」开始建表。"),
                None,
                None,
                cx,
            )
            .into_any_element();
        }
        // 两个方向都要能滚（demo `.grid-scroller{overflow:auto}`）：
        // 外层横向滚动区包住「固定表头 + 纵向滚动的行区」——横向滚动时表头与行
        // 共用同一份偏移，不会错位；纵向滚动只走行区，表头钉在顶部。
        //
        // ⚠️ 网格根必须是 **flex 容器**（`v_flex`，不是 `div()`）：块级父层不给子元素
        // 确定高度，下面那层 `size_full()` / 行区的 `flex_1` 就都拿不到确定高度 ⇒
        // 内容按自然高撑开、`overflow_y_scroll` 永不触发（表看起来「不滚动」）。
        //
        // ⚠️ gpui 的滚动容器**不画系统滚动条**，必须自己叠 `Scrollbar`（同
        // `data_table` 与 `tests/db_grid_horizontal_scroll.rs` 的形态），否则
        // 超宽 / 超高的内容没有任何可拖动的东西。句柄由页面状态持有（跨帧持久）。
        v_flex()
            .flex_1()
            .min_h_0()
            .min_w_0()
            // 调宽监听：拖动从列宽热区上开始，起始 x / 起始宽从拖拽载荷里带过来，
            // 每次移动按位移算绝对宽度（反复触发也是幂等的）。
            .on_drag_move::<StructureColResize>({
                let resize_view = view.clone();
                move |event, _, cx| {
                    let drag = event.drag(cx);
                    let index = drag.index;
                    let start_x = drag.start_x;
                    let start_w = drag.start_w;
                    let width = (start_w + (event.event.position.x - start_x))
                        .max(px(MIN_STRUCTURE_COLUMN_WIDTH));
                    let _ = resize_view.update(cx, |this, cx| {
                        this.resize_structure_column(index, width, cx);
                    });
                }
            })
            .child(
                // ⚠️ 承托滚动条的外壳必须是 **flex 容器**（`v_flex`，不是 `div()`）：
                // 块级父层不给子元素确定高度，里面那层 x 滚动区的 `flex_1` 拿不到
                // 确定高度 ⇒ 内容按自然高撑开、行区 `overflow_y_scroll` 永不触发
                // （竖向看起来「不滚动」）。绝对定位的滚动条叠加层不占布局，flex 化
                // 不影响它们的定位。
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .relative()
                    .child(
                        div()
                            .id("db-structure-cols-x")
                            .w_full()
                            .flex_1()
                            .min_h_0()
                            .flex()
                            .flex_row()
                            .overflow_x_scroll()
                            // 锁轴：竖向滚轮只归行区纵向滚动，不会被映射成横向滚动。
                            .restrict_scroll_to_axis()
                            .track_scroll(&self.structure_grid_x_scroll)
                            .child(
                                v_flex()
                                    .flex_none()
                                    .h_full()
                                    .min_h_0()
                                    .w(content_width)
                                    .min_w_full()
                                    .child(header)
                                    .child(
                                        div()
                                            .id("db-structure-cols-y")
                                            .flex_1()
                                            .min_h_0()
                                            .overflow_y_scroll()
                                            .track_scroll(&self.structure_grid_y_scroll)
                                            .child(rows),
                                    ),
                            ),
                    )
                    // 横向滚动条：常驻（Hover 档在数据网格里等于不可发现）；无溢出时
                    // Scrollbar 自己会隐藏，不会摆一条死轨道。`viewport_from_layout`
                    // 让轨道避开内容实际占用的区域。
                    .child(
                        div().absolute().inset_0().child(
                            Scrollbar::horizontal(&self.structure_grid_x_scroll)
                                .id("db-structure-cols-x-scrollbar")
                                .mode(ScrollbarMode::Always)
                                .viewport_from_layout(),
                        ),
                    )
                    // 纵向滚动条：滚动时现身、停下淡出（与 data_table 行区同档）。
                    .child(
                        div().absolute().inset_0().child(
                            Scrollbar::vertical(&self.structure_grid_y_scroll)
                                .id("db-structure-cols-y-scrollbar")
                                .mode(ScrollbarMode::Scrolling),
                        ),
                    ),
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
        let presets: [(&str, &str); 5] = [
            ("NULL", "NULL"),
            ("CURRENT_TIMESTAMP", "CURRENT_TIMESTAMP"),
            ("0", "0"),
            (tr(cx, "''（空串）"), "''"),
            ("1970-01-01 00:00:01", "'1970-01-01 00:00:01'"),
        ];
        let specs: Vec<MenuItemSpec> = presets
            .iter()
            .map(|(label, value)| MenuItemSpec::new(*value, *label))
            .collect();
        let on_pick: Rc<dyn Fn(SharedString, &mut Window, &mut App)> =
            Rc::new(move |value, window, cx| {
                let _ = view.update(cx, |this, cx| {
                    if let Some(draft) = this.structure.as_mut()
                        && let Some(column) = draft.columns.get_mut(index)
                    {
                        column.default.update(cx, |state, cx| {
                            state.set_value(value.to_string(), window, cx)
                        });
                    }
                    cx.notify();
                });
            });
        toolbar_searchable_dropdown(
            id,
            label,
            icon,
            Anchor::TopRight,
            specs,
            Some(SharedString::from(tr(cx, "搜索…"))),
            on_pick,
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
            .id("db-structure-readonly")
            .w_full()
            .flex_1()
            .min_h_0()
            // 触发器条数没有上限，清单得自己滚（编辑面高度已被分栏钉住）。
            .overflow_y_scroll()
            .gap(tokens.spacing.xxs)
            .children(rows)
            .into_any_element()
    }

    /// 索引标签：每条索引一张**可编辑**卡片（名称 / 唯一 / 组成列 + 重建 / 删除）。
    ///
    /// 「新增索引」入口只有标签栏右上角那一个（`render_structure_tab_tools`）——
    /// 内容区里再摆一枚同款按钮，同一动作两处入口只会让人迷惑。
    ///
    /// 系统索引（主键 / SQLite 约束隐式索引）只展示：删它等于删主键，名称与列也
    /// 不给改；引擎生成不了 `CREATE / DROP INDEX` 时整页换成口径说明，不摆改不动
    /// 的编辑器。
    fn render_structure_indexes(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let Some(draft) = self.structure.as_ref() else {
            return div().into_any_element();
        };
        if !draft.index_editable {
            return empty_state(
                DuckIcon::Layers,
                tr(cx, "该引擎不支持在界面上增删索引"),
                None,
                None,
                cx,
            )
            .into_any_element();
        }
        let column_options = self.structure_column_options(cx);
        let mut cards = v_flex().w_full().flex_none().gap(tokens.spacing.sm);
        for (index, item) in draft.indexes.iter().enumerate() {
            cards = cards.child(self.index_card(index, item, &column_options, cx));
        }
        let empty =
            empty_state(DuckIcon::Layers, tr(cx, "暂无索引"), None, None, cx).into_any_element();
        div()
            .id("db-structure-indexes")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_y_scroll()
            .children(if draft.indexes.is_empty() {
                vec![empty, cards.into_any_element()]
            } else {
                vec![cards.into_any_element()]
            })
            .into_any_element()
    }

    /// 一条索引的编辑卡片。
    fn index_card(
        &self,
        index: usize,
        item: &StructureIndexDraft,
        column_options: &[String],
        cx: &App,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let control_h = theme.density().control;
        let view = self.view.clone();
        let name_lock = item.system;
        let mut badges = h_flex().flex_none().items_center().gap(tokens.spacing.xxs);
        if item.system {
            badges = badges.child(neutral_tag(tr(cx, "系统"), cx));
        }
        badges = badges.child(neutral_tag(
            if item.unique {
                tr(cx, "UNIQUE")
            } else {
                tr(cx, "NORMAL")
            },
            cx,
        ));
        if item.dropped {
            badges = badges.child(neutral_tag(tr(cx, "待删除"), cx));
        } else if item.rebuild {
            badges = badges.child(neutral_tag(tr(cx, "待重建"), cx));
        }
        let columns_text = if item.columns.is_empty() {
            tr(cx, "（未选列）").to_owned()
        } else {
            item.columns.join(", ")
        };
        let mut actions = h_flex().flex_none().items_center().gap(tokens.spacing.xxs);
        if !item.system {
            actions = actions.child(
                icon_button(
                    SharedString::from(format!("db-structure-rebuild-{index}")),
                    tr(cx, "重建索引"),
                    Icon::new(DuckIcon::RotateCw).size(extended.icon.sm),
                    ButtonVariant::Ghost,
                    cx,
                )
                .disabled(item.dropped || item.rebuild)
                .on_click({
                    let click_view = view.clone();
                    move |_, _, cx| {
                        let _ = click_view.update(cx, |this, cx| {
                            this.set_index_rebuild(index, true, cx);
                        });
                    }
                }),
            );
        }
        actions = actions.child(
            icon_button(
                SharedString::from(format!("db-structure-index-drop-{index}")),
                if item.dropped {
                    tr(cx, "撤销删除")
                } else {
                    tr(cx, "删除索引")
                },
                if item.dropped {
                    Icon::new(DuckIcon::RotateCcw).size(extended.icon.sm)
                } else {
                    Icon::new(DuckIcon::Trash2).size(extended.icon.sm)
                },
                ButtonVariant::Ghost,
                cx,
            )
            .disabled(item.system)
            .on_click({
                let click_view = view.clone();
                move |_, _, cx| {
                    let _ = click_view.update(cx, |this, cx| this.toggle_index_drop(index, cx));
                }
            }),
        );
        let mut head = h_flex()
            .w_full()
            .items_center()
            .gap(tokens.spacing.sm)
            .child(div().flex_none().w(px(200.0)).child(grid_input(
                &item.name,
                control_h,
                tokens.typography.sm.size,
                name_lock,
            )))
            .child(badges)
            .child(div().flex_1().min_w_0());
        if !item.system {
            let unique_view = view.clone();
            head = head.child(
                h_flex()
                    .flex_none()
                    .items_center()
                    .gap(tokens.spacing.xs)
                    .child(meta_text(tr(cx, "唯一"), cx))
                    .child(
                        switch_control(
                            SharedString::from(format!("db-structure-index-unique-{index}")),
                            item.unique,
                            move |value, _, cx| {
                                let value = *value;
                                let _ = unique_view
                                    .update(cx, |this, cx| this.set_index_unique(index, value, cx));
                            },
                        )
                        .tooltip(tr(cx, "唯一")),
                    ),
            );
            let select_view = view.clone();
            let options = column_options.to_vec();
            head = head.child(multi_select_button(
                SharedString::from(format!("db-structure-index-cols-{index}")),
                options,
                item.columns.clone(),
                tr(cx, "选择组成列"),
                move |column, _, cx| {
                    let _ = select_view.update(cx, |this, cx| {
                        this.toggle_index_column(index, column, cx);
                    });
                },
                cx,
            ));
        }
        card(cx)
            .w_full()
            .flex_none()
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .when(item.dropped, |card| {
                card.bg(tokens.colors.destructive.opacity(extended.opacity.tint))
            })
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(head.child(actions))
                    .child(
                        div()
                            .min_w_0()
                            .text_size(tokens.typography.mono_md.size)
                            .text_color(tokens.colors.muted_foreground)
                            .truncate()
                            .child(selectable_text(SharedString::from(columns_text))),
                    ),
            )
            .into_any_element()
    }

    /// 外键标签：每条外键一张**可编辑**卡片（约束名 / 本表列 / 引用表 / 引用列 /
    /// 更新删除动作）。新增入口只有标签栏右上角那一个（与索引标签同款）。
    fn render_structure_foreign_keys(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let Some(draft) = self.structure.as_ref() else {
            return div().into_any_element();
        };
        if !draft.foreign_key_editable {
            return empty_state(
                DuckIcon::Layers,
                tr(cx, "该引擎不支持在界面上增删外键"),
                None,
                None,
                cx,
            )
            .into_any_element();
        }
        // 引用表候选：库内其他表 + 本表（自引用外键是合法结构：树 / 上级链）。
        let mut table_options: Vec<String> = draft.source_tables.clone();
        let self_table = draft
            .table
            .schema
            .as_deref()
            .map(|schema| format!("{schema}.{}", draft.table.name))
            .unwrap_or_else(|| draft.table.name.clone());
        if !table_options.iter().any(|table| *table == self_table) {
            table_options.push(self_table);
        }
        let column_options = self.structure_column_options(cx);
        let mut cards = v_flex().w_full().flex_none().gap(tokens.spacing.sm);
        for (index, item) in draft.foreign_keys.iter().enumerate() {
            cards = cards.child(self.foreign_key_card(
                index,
                item,
                &column_options,
                &table_options,
                cx,
            ));
        }
        let empty =
            empty_state(DuckIcon::Layers, tr(cx, "暂无外键"), None, None, cx).into_any_element();
        div()
            .id("db-structure-foreign-keys")
            .flex_1()
            .min_h_0()
            .min_w_0()
            .overflow_y_scroll()
            .children(if draft.foreign_keys.is_empty() {
                vec![empty, cards.into_any_element()]
            } else {
                vec![cards.into_any_element()]
            })
            .into_any_element()
    }

    /// 一条外键的编辑卡片。
    fn foreign_key_card(
        &self,
        index: usize,
        item: &StructureForeignKeyDraft,
        column_options: &[String],
        table_options: &[String],
        cx: &App,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let control_h = theme.density().control;
        let view = self.view.clone();
        // 引用列候选：缓存里该表的列；还没取到就只有已选项（后台取回来会重渲）。
        let mut ref_options: Vec<String> = self
            .structure
            .as_ref()
            .and_then(|draft| draft.ref_columns.get(&item.ref_table).cloned())
            .unwrap_or_default();
        for column in &item.ref_columns {
            if !ref_options.iter().any(|option| option == column) {
                ref_options.push(column.clone());
            }
        }
        let mut badges = h_flex().flex_none().items_center().gap(tokens.spacing.xxs);
        if item.dropped {
            badges = badges.child(neutral_tag(tr(cx, "待删除"), cx));
        }
        if item.ref_table.trim().is_empty() {
            badges = badges.child(neutral_tag(tr(cx, "未选引用表"), cx));
        }
        let path = if item.columns.is_empty() || item.ref_columns.is_empty() {
            tr(cx, "（未选列）").to_owned()
        } else {
            format!(
                "{} → {}({})",
                item.columns.join(", "),
                item.ref_table,
                item.ref_columns.join(", ")
            )
        };
        let action_options = FK_ACTIONS
            .iter()
            .map(|action| (SharedString::from(*action), SharedString::from(*action)))
            .collect::<Vec<_>>();
        let select_view = view.clone();
        let options = column_options.to_vec();
        let actions_view = view.clone();
        let delete_actions_view = view.clone();
        let update_options = action_options.clone();
        let delete_options = action_options.clone();
        let table_select_view = view.clone();
        let table_options_owned = table_options.to_vec();
        let ref_view = view.clone();
        let ref_options_owned = ref_options.clone();
        card(cx)
            .w_full()
            .flex_none()
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .when(item.dropped, |card| {
                card.bg(tokens.colors.destructive.opacity(extended.opacity.tint))
            })
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap(tokens.spacing.sm)
                            .child(div().flex_none().w(px(200.0)).child(grid_input(
                                &item.name,
                                control_h,
                                tokens.typography.sm.size,
                                item.dropped,
                            )))
                            .child(badges)
                            .child(multi_select_button(
                                SharedString::from(format!("db-structure-fk-cols-{index}")),
                                options,
                                item.columns.clone(),
                                tr(cx, "本表列"),
                                move |column, _, cx| {
                                    let _ = select_view.update(cx, |this, cx| {
                                        this.toggle_foreign_key_column(index, column, cx);
                                    });
                                },
                                cx,
                            ))
                            .child(dropdown_select_searchable(
                                SharedString::from(format!("db-structure-fk-ref-{index}")),
                                table_options_owned
                                    .iter()
                                    .map(|table| {
                                        (
                                            SharedString::from(table.clone()),
                                            SharedString::from(table.clone()),
                                        )
                                    })
                                    .collect::<Vec<_>>(),
                                SharedString::from(item.ref_table.clone()),
                                tr(cx, "搜索…"),
                                move |table, _, cx| {
                                    let _ = table_select_view.update(cx, |this, cx| {
                                        this.set_foreign_key_ref_table(
                                            index,
                                            table.to_string(),
                                            cx,
                                        );
                                    });
                                },
                                cx,
                            ))
                            .child(multi_select_button(
                                SharedString::from(format!("db-structure-fk-ref-cols-{index}")),
                                ref_options_owned,
                                item.ref_columns.clone(),
                                tr(cx, "引用列"),
                                move |column, _, cx| {
                                    let _ = ref_view.update(cx, |this, cx| {
                                        this.toggle_foreign_key_ref_column(index, column, cx);
                                    });
                                },
                                cx,
                            ))
                            .child(dropdown_select(
                                SharedString::from(format!("db-structure-fk-on-update-{index}")),
                                update_options,
                                SharedString::from(item.on_update.clone()),
                                move |action, _, cx| {
                                    let _ = actions_view.update(cx, |this, cx| {
                                        this.set_foreign_key_action(
                                            index,
                                            true,
                                            action.to_string(),
                                            cx,
                                        );
                                    });
                                },
                                cx,
                            ))
                            .child(dropdown_select(
                                SharedString::from(format!("db-structure-fk-on-delete-{index}")),
                                delete_options,
                                SharedString::from(item.on_delete.clone()),
                                move |action, _, cx| {
                                    let _ = delete_actions_view.update(cx, |this, cx| {
                                        this.set_foreign_key_action(
                                            index,
                                            false,
                                            action.to_string(),
                                            cx,
                                        );
                                    });
                                },
                                cx,
                            ))
                            .child(div().flex_1().min_w_0())
                            .child(
                                icon_button(
                                    SharedString::from(format!("db-structure-fk-drop-{index}")),
                                    if item.dropped {
                                        tr(cx, "撤销删除")
                                    } else {
                                        tr(cx, "删除外键")
                                    },
                                    if item.dropped {
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
                                            this.toggle_foreign_key_drop(index, cx);
                                        });
                                    }
                                }),
                            ),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .text_size(tokens.typography.mono_md.size)
                            .text_color(tokens.colors.muted_foreground)
                            .truncate()
                            .child(selectable_text(SharedString::from(path))),
                    ),
            )
            .into_any_element()
    }

    /// 约束页的角标数：主键算 1，每个唯一索引 / 检查约束 / 外键各算 1。
    ///
    /// 与 [`Self::render_structure_constraints`] 的取数完全同源（同一批过滤器），
    /// 否则会出现「角标写 3、页里只有 2 行」这种对不上的账。
    fn structure_constraint_count(&self) -> usize {
        let Some(draft) = self.structure.as_ref() else {
            return 0;
        };
        let pk = draft
            .columns
            .iter()
            .any(|column| column.primary && !column.dropped);
        let unique = draft
            .indexes
            .iter()
            .filter(|index| index.unique && !index.system && !index.dropped)
            .count();
        let checks = draft
            .ddl
            .as_deref()
            .map(|ddl| parse_check_constraints(ddl).len())
            .unwrap_or(0);
        let foreign_keys = draft
            .foreign_keys
            .iter()
            .filter(|item| !item.dropped)
            .count();
        usize::from(pk) + unique + checks + foreign_keys
    }

    /// 约束标签：主键 / 唯一 / 检查 / 外键四类**只读**总览。
    ///
    /// 数据全部是**投影**，不新造内省：主键来自字段草稿的 `primary` 标记，唯一约束来自
    /// 索引草稿里 `unique` 的那些，外键来自外键草稿，检查约束从建表 DDL 里解析
    ///（`parse_check_constraints`）。四类各自的编辑入口在别的页 —— 这里每行右侧都写明
    /// 来源，看完知道该去哪改。
    fn render_structure_constraints(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(draft) = self.structure.as_ref() else {
            return div().into_any_element();
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        let pk: Vec<String> = draft
            .columns
            .iter()
            .filter(|column| column.primary && !column.dropped)
            .map(|column| column.name.read(cx).value().to_string())
            .collect();
        if !pk.is_empty() {
            rows.push(self.constraint_row(
                cx,
                tr(cx, "主键"),
                None,
                pk.join(", "),
                tr(cx, "字段页"),
            ));
        }
        for index in draft
            .indexes
            .iter()
            .filter(|index| index.unique && !index.system && !index.dropped)
        {
            let name = index.name.read(cx).value().to_string();
            rows.push(self.constraint_row(
                cx,
                tr(cx, "唯一"),
                Some(name),
                index.columns.join(", "),
                tr(cx, "索引页"),
            ));
        }
        if let Some(ddl) = draft.ddl.as_deref() {
            for (name, expression) in parse_check_constraints(ddl) {
                rows.push(self.constraint_row(
                    cx,
                    tr(cx, "检查"),
                    (!name.is_empty()).then_some(name),
                    expression,
                    tr(cx, "建表 DDL"),
                ));
            }
        }
        for foreign_key in draft.foreign_keys.iter().filter(|item| !item.dropped) {
            let name = foreign_key.name.read(cx).value().to_string();
            let detail = format!(
                "({}) → {}({})",
                foreign_key.columns.join(", "),
                foreign_key.ref_table,
                foreign_key.ref_columns.join(", ")
            );
            rows.push(self.constraint_row(
                cx,
                tr(cx, "外键"),
                (!name.is_empty()).then_some(name),
                detail,
                tr(cx, "外键页"),
            ));
        }
        self.render_structure_readonly_list(
            SharedString::from(tr(cx, "这张表没有任何约束")),
            rows,
            cx,
        )
    }

    /// 约束页的一行：`类型` 标签 + 约束名 + 明细 + 来源。
    fn constraint_row(
        &self,
        cx: &App,
        kind: &'static str,
        name: Option<String>,
        detail: String,
        source: &'static str,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
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
                    .child(neutral_tag(SharedString::from(kind), cx))
                    .children(name.map(|name| {
                        div()
                            .max_w(px(200.))
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.sm.size)
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(tokens.colors.foreground)
                            .child(selectable_text(SharedString::from(name)))
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(tokens.typography.mono_md.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(selectable_text(SharedString::from(detail))),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(tokens.typography.xs.size)
                            .text_color(extended.colors.text_tertiary)
                            .child(source),
                    ),
            )
            .into_any_element()
    }

    /// 分区标签：从建表 DDL 里认 `PARTITION BY …`，原样贴出来（可选中 / 可复制）。
    ///
    /// 不解析成结构化字段：分区的写法在各引擎之间差得太多（MySQL 的
    /// `PARTITION p0 VALUES LESS THAN (…)` 列表、PG 的 `FOR VALUES FROM … TO …`、
    /// 子分区模板），拆成统一结构只会把原句拆丢。这里给的是「引擎怎么说的」。
    fn render_structure_partitions(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let Some(draft) = self.structure.as_ref() else {
            return div().into_any_element();
        };
        let Some(clause) = draft.ddl.as_deref().and_then(parse_partition_clause) else {
            return self.render_structure_readonly_list(
                SharedString::from(tr(cx, "这张表没有分区（或该引擎的建表 DDL 不含分区信息）")),
                Vec::new(),
                cx,
            );
        };
        v_flex()
            .w_full()
            .gap(tokens.spacing.xs)
            .child(
                div()
                    .text_size(tokens.typography.xs.size)
                    .text_color(extended.colors.text_tertiary)
                    .child(tr(
                        cx,
                        "以下分区子句取自建表 DDL（只读；改分区请写 ALTER TABLE）",
                    )),
            )
            .child(
                div()
                    .w_full()
                    .p(tokens.spacing.sm)
                    .rounded(tokens.radius.md)
                    .border_1()
                    .border_color(extended.colors.hairline)
                    .bg(tokens.colors.background)
                    .text_size(tokens.typography.mono_md.size)
                    .text_color(tokens.colors.foreground)
                    .child(selectable_text(SharedString::from(clause))),
            )
            .into_any_element()
    }

    /// 触发器标签：已有触发器只读清单（名称 + 时机 + 事件），本轮新增的草稿
    /// 是可编辑卡片（名称 / 时机 / 事件 / 触发体）。
    fn render_structure_triggers(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let Some(draft) = self.structure.as_ref() else {
            return div().into_any_element();
        };
        let mut rows: Vec<AnyElement> = draft
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
                                    .child(selectable_text(SharedString::from(
                                        trigger.name.clone(),
                                    ))),
                            )
                            .child(neutral_tag(SharedString::from(timing), cx)),
                    )
                    .into_any_element()
            })
            .collect();
        rows.extend(
            draft
                .new_triggers
                .iter()
                .enumerate()
                .map(|(index, item)| self.trigger_draft_card(index, item, cx)),
        );
        self.render_structure_readonly_list(SharedString::from(tr(cx, "暂无触发器")), rows, cx)
    }

    /// 一条**新增触发器**的编辑卡片。
    ///
    /// 触发体按引擎语义不同：MySQL / SQLite 是 `FOR EACH ROW` 后跟的内联语句；
    /// PG 是函数名（生成时补 `EXECUTE FUNCTION …()`）——占位提示写清楚，免得
    /// 用户按错的口径填。
    fn trigger_draft_card(
        &self,
        index: usize,
        item: &StructureTriggerDraft,
        cx: &App,
    ) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let control_h = theme.density().control;
        let view = self.view.clone();
        let timing_options = TRIGGER_TIMINGS
            .iter()
            .map(|timing| (SharedString::from(*timing), SharedString::from(*timing)))
            .collect::<Vec<_>>();
        let event_options = TRIGGER_EVENTS
            .iter()
            .map(|event| (SharedString::from(*event), SharedString::from(*event)))
            .collect::<Vec<_>>();
        let timing_view = view.clone();
        let event_view = view.clone();
        card(cx)
            .w_full()
            .flex_none()
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .child(
                v_flex()
                    .w_full()
                    .gap(tokens.spacing.xs)
                    .child(
                        h_flex()
                            .w_full()
                            .items_center()
                            .gap(tokens.spacing.sm)
                            .child(div().flex_none().w(px(200.0)).child(grid_input(
                                &item.name,
                                control_h,
                                tokens.typography.sm.size,
                                false,
                            )))
                            .child(dropdown_select(
                                SharedString::from(format!("db-structure-trigger-timing-{index}")),
                                timing_options,
                                SharedString::from(item.timing),
                                move |timing, _, cx| {
                                    let _ = timing_view.update(cx, |this, cx| {
                                        this.set_trigger_timing(index, &timing, cx);
                                    });
                                },
                                cx,
                            ))
                            .child(dropdown_select(
                                SharedString::from(format!("db-structure-trigger-event-{index}")),
                                event_options,
                                SharedString::from(item.event),
                                move |event, _, cx| {
                                    let _ = event_view.update(cx, |this, cx| {
                                        this.set_trigger_event(index, &event, cx);
                                    });
                                },
                                cx,
                            ))
                            .child(div().flex_1().min_w_0())
                            .child(
                                icon_button(
                                    SharedString::from(format!(
                                        "db-structure-trigger-drop-{index}"
                                    )),
                                    tr(cx, "删除触发器"),
                                    Icon::new(DuckIcon::Trash2).size(extended.icon.sm),
                                    ButtonVariant::Ghost,
                                    cx,
                                )
                                .on_click({
                                    let click_view = view.clone();
                                    move |_, _, cx| {
                                        let _ = click_view.update(cx, |this, cx| {
                                            this.remove_trigger_draft(index, cx);
                                        });
                                    }
                                }),
                            ),
                    )
                    .child(
                        div().w_full().child(
                            // 占位提示按引擎在建 InputState 时定（`Input` 元素没有
                            // `placeholder`），见 `add_structure_trigger`。
                            Input::new(&item.statement)
                                .h(control_h)
                                .text_size(tokens.typography.mono_md.size)
                                .w_full(),
                        ),
                    ),
            )
            .into_any_element()
    }

    /// DDL 标签：可编辑的 SQL 编辑器（高亮 + 行号），改完即进预览 / 应用流程。
    /// 读不到 DDL 的引擎（或语句为空）给空态，不摆一个空编辑器。
    fn render_structure_ddl(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let Some(draft) = self.structure.as_ref() else {
            return div().into_any_element();
        };
        match &draft.ddl_editor {
            Some(editor) => div()
                .id("db-structure-ddl")
                .flex_1()
                .min_h_0()
                .min_w_0()
                .child(
                    // 与查询编辑器同款（`shell.rs`）：无边框、满高满宽、等宽字号。
                    Editor::new(editor)
                        .bordered(false)
                        .p_0()
                        .h_full()
                        .w_full()
                        .text_size(tokens.typography.mono_md.size),
                )
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
                                                    .child(selectable_text(SharedString::from(
                                                        name.clone(),
                                                    ))),
                                            )
                                            .child(
                                                div()
                                                    .flex_none()
                                                    .text_size(tokens.typography.mono_md.size)
                                                    .text_color(tokens.colors.muted_foreground)
                                                    .child(selectable_text(SharedString::from(
                                                        type_name.clone(),
                                                    ))),
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
    pub(super) fn render_structure_page(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let target_bar = self.render_structure_target_bar(cx);
        // 表级属性行（只有 MySQL 拿得到值时才出现）。
        let props_bar = self.render_structure_table_props(cx);
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
                StructureTab::Constraints => self.render_structure_constraints(cx),
                StructureTab::Partitions => self.render_structure_partitions(cx),
                StructureTab::Triggers => self.render_structure_triggers(cx),
            }
        };
        let preview = self.structure_apply_sql(cx);
        let apply_view = self.view.clone();
        let preview_editor = self
            .structure
            .as_ref()
            .and_then(|draft| draft.preview_editor.clone());
        let preview_editing = self
            .structure
            .as_ref()
            .is_some_and(|draft| draft.preview_editing);
        let preview_manual = self
            .structure
            .as_ref()
            .is_some_and(|draft| draft.preview_manual);
        // 预览是草稿的**投影**（非编辑态、未手改）：生成文本变了才写回编辑器。
        // 值相同就不写 —— 每帧 set_value 会把滚动位置与选区重置掉。
        if !preview_editing
            && !preview_manual
            && let (Some(editor), Some(want)) = (preview_editor.clone(), preview.clone())
        {
            let shown = editor.read(cx).value();
            if shown.trim_end() != want.trim_end() {
                let view = self.view.clone();
                window.defer(cx, move |window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        // defer 期间用户可能已进入编辑态 / 手改：那时不再覆盖。
                        let still_auto = this
                            .structure
                            .as_ref()
                            .is_some_and(|draft| !draft.preview_editing && !draft.preview_manual);
                        let Some(editor) = this
                            .structure
                            .as_ref()
                            .and_then(|draft| draft.preview_editor.clone())
                        else {
                            return;
                        };
                        if !still_auto {
                            return;
                        }
                        let want = this.structure_generated_sql(cx).unwrap_or_default();
                        if editor.read(cx).value().trim_end() != want.trim_end() {
                            editor.update(cx, |state, cx| state.set_value(want, window, cx));
                        }
                    });
                });
            }
        }
        // 预览文本在预览卡里被 move 掉，底部按钮的禁用态先用布尔存下。
        let has_preview = preview.is_some();
        let collapsed = self
            .structure
            .as_ref()
            .is_some_and(|draft| draft.preview_collapsed);
        // 有删列就挂警示（DROP COLUMN 不可逆）。
        let drops_present = self
            .structure
            .as_ref()
            .is_some_and(|draft| draft.columns.iter().any(|column| column.dropped));
        let theme = active_theme(cx);
        let tokens = theme.tokens();
        let extended = theme.extended();
        let statement_count = preview
            .as_ref()
            .map(|sql| {
                sql.lines()
                    .filter(|line| {
                        let line = line.trim_start();
                        line.starts_with("ALTER")
                            || line.starts_with("CREATE")
                            || line.starts_with("DROP")
                            || line.starts_with("EXEC")
                            || line.starts_with("COMMENT")
                    })
                    .count()
            })
            .unwrap_or(0);
        // DDL 页被手改过：预览里除了生成的 ALTER 还会有整段手写 DDL，挂个徽标点明。
        let ddl_edited = self.structure_ddl_edited(cx);
        // 预览卡头部：做成闭包是因为**收起时它要单独当一条**（见下面的两档布局），
        // 两处各调一次，免得同一个头部在两条路径上各抄一份。
        let preview_head = |cx: &App| -> AnyElement {
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
                        // 就绪徽标：改动是否已能生成可执行语句（demo 同款）。
                        .child(neutral_tag(
                            SharedString::from(if has_preview {
                                tr(cx, "就绪")
                            } else {
                                tr(cx, "无变更")
                            }),
                            cx,
                        ))
                        .children((statement_count > 0).then(|| {
                            neutral_tag(
                                SharedString::from(format!(
                                    "{} {}",
                                    statement_count,
                                    tr(cx, "条语句")
                                )),
                                cx,
                            )
                        }))
                        .children(ddl_edited.then(|| neutral_tag(tr(cx, "DDL 已编辑"), cx)))
                        // 预览的手改态：应用 / 复制的就是编辑器里那段，不再随草稿走。
                        .children(preview_manual.then(|| neutral_tag(tr(cx, "已编辑"), cx)))
                        .children(preview_editing.then(|| neutral_tag(tr(cx, "编辑中"), cx))),
                )
                .child(
                    h_flex()
                        .flex_none()
                        .items_center()
                        .gap(tokens.spacing.xxs)
                        // 编辑 SQL：预览正文切到可编辑态（同一处高亮代码框，不是弹窗）。
                        .child(
                            button(
                                "db-structure-preview-edit",
                                if preview_editing {
                                    tr(cx, "完成")
                                } else {
                                    tr(cx, "编辑 SQL")
                                },
                                ButtonVariant::Ghost,
                                cx,
                            )
                            .on_click({
                                let view = apply_view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.set_structure_preview_editing(
                                            !preview_editing,
                                            window,
                                            cx,
                                        );
                                    });
                                }
                            }),
                        )
                        // 手改后给一条回到生成态的出口（否则网格改动永远追不上预览）。
                        .children((preview_manual && !preview_editing).then(|| {
                            button(
                                "db-structure-preview-regen",
                                tr(cx, "重新生成"),
                                ButtonVariant::Ghost,
                                cx,
                            )
                            .on_click({
                                let view = apply_view.clone();
                                move |_, window, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.regenerate_structure_preview(window, cx);
                                    });
                                }
                            })
                        }))
                        .child(
                            icon_button(
                                "db-structure-preview-toggle",
                                if collapsed {
                                    tr(cx, "展开")
                                } else {
                                    tr(cx, "折叠")
                                },
                                if collapsed {
                                    Icon::new(DuckIcon::ArrowDown).size(extended.icon.sm)
                                } else {
                                    Icon::new(DuckIcon::ArrowUp).size(extended.icon.sm)
                                },
                                ButtonVariant::Ghost,
                                cx,
                            )
                            .on_click({
                                let view = apply_view.clone();
                                move |_, _, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        this.toggle_structure_preview(cx);
                                    });
                                }
                            }),
                        )
                        .child(
                            button(
                                "db-structure-copy",
                                tr(cx, "复制 SQL"),
                                ButtonVariant::Ghost,
                                cx,
                            )
                            .disabled(!has_preview)
                            .on_click({
                                let view = apply_view.clone();
                                move |_, _, cx| {
                                    let _ = view.update(cx, |this, cx| {
                                        // 复制的是**预览里那一段**（DDL 页改过的语句 +
                                        // 生成的 ALTER），不是只有 ALTER —— 否则复制出来
                                        // 的东西和眼睛看到的对不上。
                                        if let Some(sql) = this.structure_apply_sql(cx) {
                                            this.put_on_clipboard(sql, tr(cx, "已复制 SQL"), cx);
                                        }
                                    });
                                }
                            }),
                        ),
                )
                .into_any_element()
        };
        // 预览正文：有语句就滚着放整段，没有就给一句「暂无更改」。
        let preview_body = v_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            // 删列警示：DROP COLUMN 会不可逆地丢掉该列数据。
            .when(drops_present, |body| {
                body.child(
                    h_flex()
                        .w_full()
                        .flex_none()
                        .items_center()
                        .gap(tokens.spacing.xs)
                        .px(tokens.spacing.sm)
                        .py(tokens.spacing.xxs)
                        .bg(extended.colors.warning.opacity(extended.opacity.tint))
                        .child(
                            Icon::new(DuckIcon::TriangleAlert)
                                .size(extended.icon.sm)
                                .text_color(extended.colors.warning),
                        )
                        .child(div().min_w_0().truncate().child(meta_text(
                            tr(cx, "包含 DROP COLUMN：删列会不可逆地丢掉该列数据，请先备份"),
                            cx,
                        ))),
                )
            })
            .child(match preview {
                Some(sql) => match preview_editor.clone() {
                    // 预览正文就是 SQL 编辑器（高亮 + 可选中复制；编辑态下可改）。
                    Some(editor) => div()
                        .id("db-structure-preview")
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .w_full()
                        .child(
                            Editor::new(&editor)
                                .bordered(false)
                                .p_0()
                                .h_full()
                                .w_full()
                                .text_size(tokens.typography.mono_md.size)
                                .readonly(!preview_editing),
                        )
                        .into_any_element(),
                    // 老状态没有编辑器实体（不会发生）：回落只读文本，别把预览画没。
                    None => div()
                        .id("db-structure-preview")
                        .flex_1()
                        .min_h_0()
                        .w_full()
                        .overflow_y_scroll()
                        .p(tokens.spacing.sm)
                        .text_size(tokens.typography.mono_md.size)
                        .text_color(tokens.colors.foreground)
                        .child(selectable_text(SharedString::from(sql)))
                        .into_any_element(),
                },
                None => div()
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(meta_text(tr(cx, "暂无更改"), cx))
                    .into_any_element(),
            });
        // 编辑面卡片（标签栏 + 内容面）。
        //
        // ⚠️ 卡片本体与卡片之间的包裹层**必须是 flex 容器**：`flex_1` / `h_full`
        // 在 `div()`（block）下不生效，高度链一断，里面所有 `overflow_*_scroll`
        // 都拿不到确定高度 —— 「字段表格不滚动、内容顶破卡片」就是这么来的
        // （与结果面板同一条纪律）。
        let editor_card = v_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .rounded(tokens.radius.md)
            .border_1()
            .border_color(tokens.colors.border)
            .bg(tokens.colors.background)
            .overflow_hidden()
            .child(tabs_bar)
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .p(tokens.spacing.xs)
                    .child(tab_body),
            )
            .into_any_element();
        let preview_card = v_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .rounded(tokens.radius.md)
            .border_1()
            .border_color(tokens.colors.border)
            .overflow_hidden()
            .child(preview_head(cx))
            .child(preview_body)
            .into_any_element();
        // 收起态的预览：只剩一条头（高度不占地方，编辑面独占整屏）。
        let preview_bar = v_flex()
            .w_full()
            .flex_none()
            .min_w_0()
            .rounded(tokens.radius.md)
            .border_1()
            .border_color(tokens.colors.border)
            .overflow_hidden()
            .child(preview_head(cx))
            .into_any_element();
        v_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .gap(tokens.spacing.sm)
            .bg(tokens.colors.background)
            .p(tokens.spacing.sm)
            // 目标条。
            .child(target_bar)
            // 表级属性（注释 / 存储引擎；非 MySQL 不占位）。
            .children(props_bar)
            // 编辑面与 SQL 预览之间是**可拖拽的上下分栏**（同「SQL 编辑器 / 结果区」
            // 那条机制）：两块都拿得到确定高度，网格才能在自己的框里滚，预览才有
            // 地方放整段 DDL —— 原来网格吃满整屏、预览只剩 `max_h(200)` 一条缝。
            .child(if collapsed {
                // 预览收起：编辑面独占整屏，预览只留一条头（分栏尺寸留在
                // `structure_split` 里，重新展开即原样恢复 —— 与 SQL 编辑器同款）。
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .gap(tokens.spacing.sm)
                    .child(v_flex().flex_1().min_h_0().min_w_0().child(editor_card))
                    .child(preview_bar)
                    .into_any_element()
            } else {
                v_resizable("db-structure-split")
                    .with_state(&self.structure_split)
                    .child(
                        resizable_panel()
                            .size(px(STRUCTURE_EDITOR_INITIAL_HEIGHT))
                            .size_range(
                                px(*STRUCTURE_EDITOR_HEIGHT_RANGE.start())
                                    ..px(*STRUCTURE_EDITOR_HEIGHT_RANGE.end()),
                            )
                            .child(editor_card),
                    )
                    .child(
                        resizable_panel()
                            .size_range(
                                px(*STRUCTURE_PREVIEW_HEIGHT_RANGE.start())
                                    ..px(*STRUCTURE_PREVIEW_HEIGHT_RANGE.end()),
                            )
                            .child(preview_card),
                    )
                    .into_any_element()
            })
            // 底部应用条：左侧口径提示（编辑是草稿，应用才落库），右侧主按钮。
            .child(
                h_flex()
                    .w_full()
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .gap(tokens.spacing.sm)
                    .child(
                        h_flex()
                            .min_w_0()
                            .items_center()
                            .gap(tokens.spacing.sm)
                            .child(div().min_w_0().truncate().child(meta_text(
                                tr(cx, "编辑仅保存在本地草稿，应用前不写入数据库"),
                                cx,
                            )))
                            .child(
                                h_flex()
                                    .flex_none()
                                    .items_center()
                                    .gap(tokens.spacing.sm)
                                    .children(
                                        ["Shift+Enter 新增字段", "⌘D 复制行", "⌘⌫ 删除行"].map(
                                            |hint| {
                                                div()
                                                    .flex_none()
                                                    .text_size(tokens.typography.xs.size)
                                                    .text_color(extended.colors.text_tertiary)
                                                    .child(tr(cx, hint))
                                            },
                                        ),
                                    ),
                            ),
                    )
                    .child(
                        icon_text_button(
                            "db-structure-apply",
                            tr(cx, "应用变更"),
                            Icon::new(DuckIcon::Save).size(extended.icon.sm),
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
            .on_key_down({
                let key_view = self.view.clone();
                move |event: &gpui::KeyDownEvent, window, cx| {
                    let shift = event.keystroke.modifiers.shift;
                    let platform = event.keystroke.modifiers.platform;
                    let key = event.keystroke.key.as_str();
                    // Shift+Enter（⌘⇧Enter 等价）：追加一行空字段。
                    if shift && key == "enter" {
                        let _ = key_view.update(cx, |this, cx| {
                            if this.structure_active_columns() {
                                this.add_structure_column(window, cx);
                            }
                        });
                        return;
                    }
                    if !platform {
                        return;
                    }
                    match key {
                        // ⌘D：复制焦点行（库里已有的行只是「再插一行同结构的新列」）。
                        "d" => {
                            let _ = key_view.update(cx, |this, cx| {
                                if !this.structure_active_columns() {
                                    return;
                                }
                                let Some(row) = this.focused_structure_row(window, cx) else {
                                    return;
                                };
                                // 已标记删除的行不再复制（demo 同款）。
                                let dropped = this
                                    .structure
                                    .as_ref()
                                    .and_then(|draft| draft.columns.get(row))
                                    .is_some_and(|column| column.dropped);
                                if !dropped {
                                    this.copy_structure_column(row, window, cx);
                                }
                            });
                        }
                        // ⌘⌫：删除焦点行——新增行直接移除，库里已有的行标记待删（可恢复）。
                        "backspace" | "delete" => {
                            let _ = key_view.update(cx, |this, cx| {
                                if !this.structure_active_columns() {
                                    return;
                                }
                                let Some(row) = this.focused_structure_row(window, cx) else {
                                    return;
                                };
                                let is_new = this
                                    .structure
                                    .as_ref()
                                    .and_then(|draft| draft.columns.get(row))
                                    .is_some_and(|column| column.original.is_none());
                                this.toggle_drop_column(row, cx);
                                if !is_new {
                                    notify::info(tr(cx, "已标记删除（应用前可恢复）"), window, cx);
                                }
                            });
                        }
                        _ => {}
                    }
                }
            })
            .into_any_element()
    }

    /// 追加一行空字段（库里还没有；「应用变更」时走 `ADD COLUMN`）。
    pub(super) fn add_structure_column(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.structure.as_mut() else {
            return;
        };
        let name = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
        let kind = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
        let length = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
        let default = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
        let comment = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
        let charset = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
        let collation = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
        draft.columns.push(StructureColumnDraft {
            original: None,
            original_type: String::new(),
            original_length: String::new(),
            original_nullable: true,
            name,
            kind,
            length,
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
        let Some(sql) = self.structure_apply_sql(cx) else {
            return;
        };
        // 建表模式：确认文案与落库回执都走自己的口径（成功后标签转正）。
        if self.structure.as_ref().is_some_and(|draft| draft.is_create) {
            self.apply_new_table(sql, window, cx);
            return;
        }
        let view = self.view.clone();
        // 确认文案跟改动来源走：DDL 页手改过 → 先跑手写 DDL 再跑生成的 ALTER；
        // 只动了网格草稿 → 原口径。
        let ddl_edited = self.structure_ddl_edited(cx);
        let has_alter = self.structure_alter_sql(cx).is_some();
        let message = match (ddl_edited, has_alter) {
            (true, true) => tr(
                cx,
                "将先执行 DDL 页编辑后的语句，再执行生成的 ALTER 语句。表结构变更不会自动回滚，确定继续吗？",
            ),
            (true, false) => tr(
                cx,
                "将执行 DDL 页编辑后的语句。表结构变更不会自动回滚，确定继续吗？",
            ),
            (false, _) => tr(
                cx,
                "将执行生成的 ALTER 语句。表结构变更不会自动回滚，确定继续吗？",
            ),
        };
        open_confirm(
            tr(cx, "应用表结构变更"),
            message,
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

    /// 建表模式的「应用变更」：确认后走**与编辑器同一条**执行路径（历史 / 审计 /
    /// 超时 / 危险语句分类都不另起一套），只是先挂上落库回执旗标
    ///（[`DatabasePage::create_applied`]）—— 执行成功由 `install_query_result`
    /// 消费，把标签原地转正成这张表的结构编辑器并刷新左栏（dbx
    /// `onStructureEditorSaved` 的 `createdTable` 分支同口径）。
    fn apply_new_table(&mut self, sql: String, window: &mut Window, cx: &mut Context<Self>) {
        // 表名此刻就要定死：它是回执旗标与转正目标（执行期间用户再改输入框
        // 影响的是下一次预览，不是这一批语句）。
        let Some(draft) = self.structure.as_ref() else {
            return;
        };
        let name = draft
            .new_table_name
            .as_ref()
            .map(|state| state.read(cx).value().trim().to_owned())
            .unwrap_or_default();
        if name.is_empty() {
            notify::warning(tr(cx, "请先填写表名。"), window, cx);
            return;
        }
        let schema = draft.table.schema.clone();
        let created = TableRef {
            schema: schema.clone(),
            name: name.clone(),
        };
        let view = self.view.clone();
        open_confirm(
            tr(cx, "应用表结构变更"),
            tr(
                cx,
                "将执行生成的 CREATE TABLE 语句。建表语句不会自动回滚，确定继续吗？",
            ),
            tr(cx, "执行"),
            Rc::new(move |window, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.create_applied = Some(created.clone());
                    this.execute_sql(sql.clone(), None, window, cx);
                });
            }),
            window,
            cx,
        );
    }

    /// 建表执行成功的收尾：标签转正 + 刷新左栏目录 + 就地重开结构编辑器
    /// （对新表做一次内省，草稿换成「编辑既有表」模式）。
    pub(super) fn finish_create_table(&mut self, created: TableRef, cx: &mut Context<Self>) {
        // 标签转正：`TabKind::NewTable` → `TabKind::Structure`，标题跟着变成
        // `结构 表名@库`（dbx 同款：建完不关标签，就地变成这张表的编辑器）。
        if let Some(tab) = self.active_tab.clone() {
            self.tab_kind.insert(
                tab,
                TabKind::Structure {
                    object: created.clone(),
                },
            );
        }
        self.refresh_catalog(cx);
        self.open_structure(created, cx);
    }

    /// 重取当前连接的库内对象目录（建表 / 删表这类**改变对象清单**的 DDL 之后调）。
    ///
    /// 树的表清单来自建连时的内省快照（[`Session::catalog`]），不刷新的话新表
    /// 要等重连才露面。取失败只当没发生过——目录是缓存，不是承诺。
    fn refresh_catalog(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        self.spawn_db(
            cx,
            move || {
                conn.lock()
                    .expect("connection lock poisoned")
                    .catalog(None)
                    .map_err(|error| error.to_string())
            },
            move |this, result, _window, cx| {
                if let Ok(catalog) = result
                    && let Some(session) = this.active_session_mut()
                {
                    session.catalog = catalog;
                    cx.notify();
                }
            },
        );
    }

    /// 当前草稿里可用的列名（未删、非空）——索引组成列 / 外键本表列的候选。
    fn structure_column_options(&self, cx: &App) -> Vec<String> {
        self.structure
            .as_ref()
            .map(|draft| {
                draft
                    .columns
                    .iter()
                    .filter(|column| !column.dropped)
                    .map(|column| column.name.read(cx).value().trim().to_owned())
                    .filter(|name| !name.is_empty())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 拖拽调宽落点（`StructureColResize` 的 `on_drag_move`）：改一列的宽度。
    fn resize_structure_column(&mut self, index: usize, width: Pixels, cx: &mut Context<Self>) {
        let Some(draft) = self.structure.as_mut() else {
            return;
        };
        let Some(slot) = draft.column_widths.get_mut(index) else {
            return;
        };
        if *slot == width {
            return;
        }
        *slot = width;
        cx.notify();
    }

    /// 单元格右端的候选值下拉（类型 / 字符集 / 排序规则共用）。
    fn cell_menu_button(
        &self,
        index: usize,
        menu: CellMenu,
        tint: gpui::Hsla,
        options: &[&str],
        empty_label: &'static str,
        cx: &App,
    ) -> AnyElement {
        let view = self.view.clone();
        let extended = active_theme(cx).extended();
        let label = match menu {
            CellMenu::Type => tr(cx, "类型候选"),
            CellMenu::Charset => tr(cx, "字符集候选"),
            CellMenu::Collation => tr(cx, "排序规则候选"),
        };
        // 候选清单：首项是「表默认」这类清空档（`empty_label` 为空表示没有这一档）。
        let mut specs: Vec<MenuItemSpec> = Vec::with_capacity(options.len() + 1);
        if !empty_label.is_empty() {
            specs.push(MenuItemSpec::new("", empty_label));
        }
        specs.extend(
            options
                .iter()
                .map(|option| MenuItemSpec::new(*option, *option)),
        );
        let kind = menu;
        let on_pick: Rc<dyn Fn(SharedString, &mut Window, &mut App)> =
            Rc::new(move |value, window, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.set_cell_menu_value(index, kind, value.to_string(), window, cx);
                });
            });
        // 类型候选 26 项：走可搜索下拉（搜索框钉顶 + 清单限高滚动），不再是一根长条。
        toolbar_searchable_dropdown(
            SharedString::from(format!("db-structure-cell-{}-{index}", menu.id())),
            label,
            Icon::new(DuckIcon::ChevronDown)
                .size(extended.icon.sm)
                .text_color(tint),
            Anchor::BottomRight,
            specs,
            Some(SharedString::from(tr(cx, "搜索…"))),
            on_pick,
            cx,
        )
        .into_any_element()
    }

    /// 候选值写回单元格：类型 / 字符集直接写输入框；换字符集时顺带把排序规则改成
    /// 该字符集的默认（demo 同款联动）。
    fn set_cell_menu_value(
        &mut self,
        index: usize,
        menu: CellMenu,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.structure.as_mut() else {
            return;
        };
        let Some(column) = draft.columns.get_mut(index) else {
            return;
        };
        let target = match menu {
            CellMenu::Type => &column.kind,
            CellMenu::Charset => &column.charset,
            CellMenu::Collation => &column.collation,
        };
        target.update(cx, |state, cx| state.set_value(value.clone(), window, cx));
        if matches!(menu, CellMenu::Charset) {
            let collation = collation_for_charset(&value);
            column
                .collation
                .update(cx, |state, cx| state.set_value(collation, window, cx));
        }
        cx.notify();
    }

    /// 追加一条空索引（库里还没有；「应用变更」时走 `CREATE INDEX`）。
    fn add_structure_index(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.structure.as_mut() else {
            return;
        };
        let name = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
        draft.indexes.push(StructureIndexDraft {
            original: None,
            original_columns: Vec::new(),
            original_unique: false,
            system: false,
            name,
            unique: false,
            columns: Vec::new(),
            rebuild: false,
            dropped: false,
        });
        cx.notify();
    }

    /// 标记 / 撤销删除一条索引。
    fn toggle_index_drop(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(draft) = self.structure.as_mut()
            && let Some(item) = draft.indexes.get_mut(index)
        {
            item.dropped = !item.dropped;
            cx.notify();
        }
    }

    /// 勾选 / 取消「唯一索引」。
    fn set_index_unique(&mut self, index: usize, unique: bool, cx: &mut Context<Self>) {
        if let Some(draft) = self.structure.as_mut()
            && let Some(item) = draft.indexes.get_mut(index)
        {
            item.unique = unique;
            cx.notify();
        }
    }

    /// 把一条索引排进「重建」（即使没改也要 DROP + CREATE）。
    fn set_index_rebuild(&mut self, index: usize, rebuild: bool, cx: &mut Context<Self>) {
        if let Some(draft) = self.structure.as_mut()
            && let Some(item) = draft.indexes.get_mut(index)
        {
            item.rebuild = rebuild;
            cx.notify();
        }
    }

    /// 勾选 / 取消索引的一列（勾选顺序即索引内列序）。
    fn toggle_index_column(&mut self, index: usize, column: String, cx: &mut Context<Self>) {
        if let Some(draft) = self.structure.as_mut()
            && let Some(item) = draft.indexes.get_mut(index)
        {
            if let Some(position) = item.columns.iter().position(|name| *name == column) {
                item.columns.remove(position);
            } else {
                item.columns.push(column);
            }
            cx.notify();
        }
    }

    /// 追加一条空外键。
    fn add_structure_foreign_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.structure.as_mut() else {
            return;
        };
        let name = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
        draft.foreign_keys.push(StructureForeignKeyDraft {
            original: None,
            original_columns: Vec::new(),
            original_ref_table: String::new(),
            original_ref_columns: Vec::new(),
            original_on_update: String::new(),
            original_on_delete: String::new(),
            name,
            columns: Vec::new(),
            ref_table: String::new(),
            ref_columns: Vec::new(),
            on_update: "NO ACTION".to_owned(),
            on_delete: "NO ACTION".to_owned(),
            dropped: false,
        });
        cx.notify();
    }

    /// 标记 / 撤销删除一条外键。
    fn toggle_foreign_key_drop(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(draft) = self.structure.as_mut()
            && let Some(item) = draft.foreign_keys.get_mut(index)
        {
            item.dropped = !item.dropped;
            cx.notify();
        }
    }

    /// 追加一条**新增触发器**草稿（默认 `BEFORE INSERT`，语句留空待填）。
    fn add_structure_trigger(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.structure.as_mut() else {
            return;
        };
        let name = cx.new(|cell_cx| InputState::new(&mut *window, cell_cx));
        // 触发体的占位提示按引擎分档：PG 收的是**函数名**（生成时补 `EXECUTE FUNCTION
        // …()`）；SQLite 只认 `BEGIN … END;` 块（单条语句会被它直接拒掉）；MySQL 收
        // `FOR EACH ROW` 后跟的触发体。占位只能在建 `InputState` 时定（`Input` 元素
        // 没有 `placeholder`），所以在这里按引擎选一次。
        let statement_placeholder: SharedString = match draft.kind {
            DatabaseKind::Postgres => tr(cx, "函数名（生成 EXECUTE FUNCTION …()）").into(),
            DatabaseKind::Sqlite => tr(cx, "触发体（BEGIN … END;）").into(),
            _ => tr(cx, "触发体 SQL 语句").into(),
        };
        let statement = cx.new(|cell_cx| {
            InputState::new(&mut *window, cell_cx).placeholder(statement_placeholder)
        });
        // 触发体 / 名称的改动要实时进 SQL 预览：订阅挂在页面生命周期上
        //（字段网格的输入框同一套口径，见 `build_structure_draft`）。
        for state in [&name, &statement] {
            self._subscriptions.push(cx.subscribe(
                state,
                |_this: &mut Self, _source: Entity<InputState>, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        cx.notify();
                    }
                },
            ));
        }
        draft.new_triggers.push(StructureTriggerDraft {
            name,
            timing: "BEFORE",
            event: "INSERT",
            statement,
        });
        cx.notify();
    }

    /// 删掉一条还没落库的新增触发器草稿（已有触发器只读，不在删除之列）。
    fn remove_trigger_draft(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(draft) = self.structure.as_mut() {
            draft.new_triggers.remove(index);
            cx.notify();
        }
    }

    /// 改新增触发器的时机（`BEFORE` / `AFTER`）。
    fn set_trigger_timing(&mut self, index: usize, timing: &str, cx: &mut Context<Self>) {
        if let Some(draft) = self.structure.as_mut()
            && let Some(item) = draft.new_triggers.get_mut(index)
            && let Some(value) = TRIGGER_TIMINGS.iter().find(|slot| **slot == timing)
        {
            item.timing = value;
            cx.notify();
        }
    }

    /// 改新增触发器的事件（`INSERT` / `UPDATE` / `DELETE`）。
    fn set_trigger_event(&mut self, index: usize, event: &str, cx: &mut Context<Self>) {
        if let Some(draft) = self.structure.as_mut()
            && let Some(item) = draft.new_triggers.get_mut(index)
            && let Some(value) = TRIGGER_EVENTS.iter().find(|slot| **slot == event)
        {
            item.event = value;
            cx.notify();
        }
    }

    /// 勾选 / 取消外键的本表列。
    fn toggle_foreign_key_column(&mut self, index: usize, column: String, cx: &mut Context<Self>) {
        if let Some(draft) = self.structure.as_mut()
            && let Some(item) = draft.foreign_keys.get_mut(index)
        {
            if let Some(position) = item.columns.iter().position(|name| *name == column) {
                item.columns.remove(position);
            } else {
                item.columns.push(column);
            }
            cx.notify();
        }
    }

    /// 勾选 / 取消外键的引用列（与 [`Self::toggle_foreign_key_column`] 按序对齐——
    /// 复合外键要求两边列数一致，生成前校验）。
    fn toggle_foreign_key_ref_column(
        &mut self,
        index: usize,
        column: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(draft) = self.structure.as_mut()
            && let Some(item) = draft.foreign_keys.get_mut(index)
        {
            if let Some(position) = item.ref_columns.iter().position(|name| *name == column) {
                item.ref_columns.remove(position);
            } else {
                item.ref_columns.push(column);
            }
            cx.notify();
        }
    }

    /// 改外键的更新 / 删除动作。
    fn set_foreign_key_action(
        &mut self,
        index: usize,
        on_update: bool,
        action: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(draft) = self.structure.as_mut()
            && let Some(item) = draft.foreign_keys.get_mut(index)
        {
            if on_update {
                item.on_update = action;
            } else {
                item.on_delete = action;
            }
            cx.notify();
        }
    }

    /// 改外键的引用表：换表就清掉已选引用列，并按需后台取该表列清单当候选。
    fn set_foreign_key_ref_table(&mut self, index: usize, table: String, cx: &mut Context<Self>) {
        let Some(draft) = self.structure.as_mut() else {
            return;
        };
        let Some(item) = draft.foreign_keys.get_mut(index) else {
            return;
        };
        if item.ref_table == table {
            return;
        }
        item.ref_table = table.clone();
        item.ref_columns.clear();
        let cached = draft.ref_columns.contains_key(&table);
        cx.notify();
        if !cached {
            self.load_ref_columns(table, cx);
        }
    }

    /// 后台取「引用表」的列清单（外键引用列的候选；一个表取一次，缓存进草稿）。
    fn load_ref_columns(&mut self, table: String, cx: &mut Context<Self>) {
        let Some(session) = self.active_session() else {
            return;
        };
        let conn = session.conn.clone();
        let schema = self
            .structure
            .as_ref()
            .and_then(|draft| draft.table.schema.clone());
        let target = TableRef {
            name: table.clone(),
            schema,
        };
        self.spawn_db(
            cx,
            move || -> Result<(String, Vec<String>), String> {
                let mut guard = conn.lock().expect("connection lock poisoned");
                let columns = guard
                    .table_column_meta(&target)
                    .map_err(|error| error.to_string())?;
                Ok((
                    table,
                    columns.into_iter().map(|column| column.name).collect(),
                ))
            },
            move |this, result, window, cx| match result {
                Ok((table, columns)) => {
                    if let Some(draft) = this.structure.as_mut() {
                        draft.ref_columns.insert(table, columns);
                    }
                    cx.notify();
                }
                Err(error) => {
                    notify::warning(trf(cx, "读取引用表字段失败：{}", &[&error]), window, cx);
                }
            },
        );
    }

    /// 改表存储引擎（只有 MySQL 有这一项）。
    fn set_structure_table_engine(&mut self, engine: String, cx: &mut Context<Self>) {
        if let Some(draft) = self.structure.as_mut()
            && draft.table_engine.as_deref() != Some(engine.as_str())
        {
            draft.table_engine = Some(engine);
            cx.notify();
        }
    }

    /// 折叠 / 展开 SQL 预览卡。
    fn toggle_structure_preview(&mut self, cx: &mut Context<Self>) {
        if let Some(draft) = self.structure.as_mut() {
            draft.preview_collapsed = !draft.preview_collapsed;
            cx.notify();
        }
    }

    /// 打开「字段注释放大编辑」弹窗（预填当前注释）。
    fn open_structure_comment(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.structure.as_ref() else {
            return;
        };
        let Some(column) = draft.columns.get(row) else {
            return;
        };
        let field = column.name.read(cx).value().trim().to_owned();
        let value = column.comment.read(cx).value().to_owned();
        // 多行注释用 Textarea（单行 `InputState` 没有多行档）。
        let text = cx.new(|cell_cx| TextareaState::new(&mut *window, cell_cx).rows(5));
        text.update(cx, |state, cx| state.set_value(value, window, cx));
        self.structure_comment = Some(StructureCommentDraft { row, field, text });
        self.show_structure_comment_dialog(window, cx);
    }

    /// 注释放大编辑弹窗（多行输入；确定后写回字段草稿的注释格）。
    fn show_structure_comment_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.view.clone();
        let drag = modal_drag(cx);
        let confirm_view = self.view.clone();
        let confirm: ConfirmHandler = Rc::new(move |window: &mut Window, cx: &mut App| {
            let mut accepted = true;
            let _ = confirm_view.update(cx, |this, cx| {
                let Some(draft) = this.structure_comment.take() else {
                    accepted = false;
                    return;
                };
                let text = draft.text.read(cx).value().to_owned();
                if let Some(structure) = this.structure.as_mut()
                    && let Some(column) = structure.columns.get_mut(draft.row)
                {
                    column
                        .comment
                        .update(cx, |state, cx| state.set_value(text, window, cx));
                }
                cx.notify();
            });
            accepted
        });
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let confirm = Rc::clone(&confirm);
            dialog
                .w(px(460.0))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .content({
                    let drag_for_content = drag.clone();
                    let content_view = view.clone();
                    move |content, _, cx| {
                        let theme = active_theme(cx);
                        let tokens = theme.tokens();
                        let draft = content_view.upgrade().and_then(|page| {
                            page.read(cx)
                                .structure_comment
                                .as_ref()
                                .map(|draft| (draft.field.clone(), draft.text.clone()))
                        });
                        let body = match draft {
                            Some((field, text)) => v_flex()
                                .w_full()
                                .gap(form_gap(cx))
                                .child(modal_title_row(
                                    "db-structure-comment-title",
                                    &drag_for_content,
                                    tr(cx, "编辑注释"),
                                    cx,
                                ))
                                .child(meta_text(field, cx))
                                .child(
                                    div()
                                        .w_full()
                                        .h(px(140.0))
                                        .rounded(tokens.radius.md)
                                        .border_1()
                                        .border_color(tokens.colors.border)
                                        .child(
                                            Textarea::new(&text)
                                                .appearance(false)
                                                .h_full()
                                                .w_full(),
                                        ),
                                ),
                            None => v_flex()
                                .w_full()
                                .child(meta_text(tr(cx, "注释编辑器已关闭"), cx)),
                        };
                        content.child(modal_drag_area(&drag_for_content, body))
                    }
                })
                .footer(dialog_footer(
                    "db-structure-comment",
                    tr(cx, "确定"),
                    ButtonVariant::Primary,
                    confirm,
                    cx,
                ))
        });
    }

    /// 打开 / 关闭「复制全部列名」弹窗。
    fn toggle_structure_copy_names(&mut self, cx: &mut Context<Self>) {
        self.structure_copy_names = !self.structure_copy_names;
        cx.notify();
    }

    /// 「复制全部列名」弹窗（清单只展示，复制入口在弹窗页脚）。
    fn show_structure_copy_names_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.view.clone();
        let drag = modal_drag(cx);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            dialog
                .w(px(460.0))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .content({
                    let drag_for_content = drag.clone();
                    let content_view = view.clone();
                    move |content, _, cx| {
                        let theme = active_theme(cx);
                        let tokens = theme.tokens();
                        let muted = theme.extended().colors.text_tertiary;
                        let rows: Vec<(String, String)> = content_view
                            .upgrade()
                            .and_then(|page| {
                                page.read(cx).structure.as_ref().map(|draft| {
                                    draft
                                        .columns
                                        .iter()
                                        .filter(|column| !column.dropped)
                                        .map(|column| {
                                            (
                                                column.name.read(cx).value().trim().to_owned(),
                                                column.comment.read(cx).value().trim().to_owned(),
                                            )
                                        })
                                        .filter(|(name, _)| !name.is_empty())
                                        .collect()
                                })
                            })
                            .unwrap_or_default();
                        let mut list = v_flex().w_full().gap(tokens.spacing.xxs);
                        for (name, comment) in rows {
                            list = list.child(
                                h_flex()
                                    .w_full()
                                    .items_center()
                                    .gap(tokens.spacing.sm)
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_size(tokens.typography.mono_md.size)
                                            .child(selectable_text(SharedString::from(name))),
                                    )
                                    .child(
                                        div()
                                            .min_w_0()
                                            .flex_1()
                                            .text_size(tokens.typography.xs.size)
                                            .text_color(muted)
                                            .truncate()
                                            .child(selectable_text(SharedString::from(comment))),
                                    ),
                            );
                        }
                        let body = v_flex()
                            .w_full()
                            .gap(form_gap(cx))
                            .child(modal_title_row(
                                "db-structure-copy-names-title",
                                &drag_for_content,
                                tr(cx, "复制全部列名"),
                                cx,
                            ))
                            .child(
                                div()
                                    .id("db-structure-copy-names")
                                    .max_h(px(280.0))
                                    .overflow_y_scroll()
                                    .child(list),
                            );
                        content.child(modal_drag_area(&drag_for_content, body))
                    }
                })
                .footer(dialog_footer(
                    "db-structure-copy-names",
                    tr(cx, "复制"),
                    ButtonVariant::Primary,
                    Rc::new({
                        let copy_view = view.clone();
                        move |_window: &mut Window, cx: &mut App| {
                            let _ = copy_view.update(cx, |this, cx| {
                                let Some(draft) = this.structure.as_ref() else {
                                    return;
                                };
                                let names: Vec<String> = draft
                                    .columns
                                    .iter()
                                    .filter(|column| !column.dropped)
                                    .map(|column| column.name.read(cx).value().trim().to_owned())
                                    .filter(|name| !name.is_empty())
                                    .collect();
                                let count = names.len().to_string();
                                this.structure_copy_names = false;
                                this.put_on_clipboard(
                                    names.join("\n"),
                                    trf(cx, "已复制 {} 个列名", &[&count]),
                                    cx,
                                );
                            });
                            true
                        }
                    }),
                    cx,
                ))
        });
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

/// 存储引擎下拉的常用档（demo 同款；库里是别的引擎时把真值插到最前）。
const ENGINE_OPTIONS: [&str; 3] = ["InnoDB", "MyISAM", "MEMORY"];

/// 拖拽调宽的最小列宽（再窄就没法看内容了）。
const MIN_STRUCTURE_COLUMN_WIDTH: f32 = 56.0;

/// 排序规则候选（MySQL；与字符集共用一张小表，不引入完整的 charset→collation
/// 映射表——那不是这个界面要解决的问题）。
const COLLATION_OPTIONS: [&str; 5] = [
    "utf8mb4_general_ci",
    "utf8mb4_unicode_ci",
    "utf8mb4_bin",
    "utf8_general_ci",
    "gbk_chinese_ci",
];

/// 字符集 → 默认排序规则（换字符集时联动；认不出来的字符集清空，交给用户自己填）。
fn collation_for_charset(charset: &str) -> String {
    match charset.trim() {
        "utf8mb4" => "utf8mb4_general_ci".to_owned(),
        "utf8" => "utf8_general_ci".to_owned(),
        "gbk" => "gbk_chinese_ci".to_owned(),
        _ => String::new(),
    }
}

/// 网格单元格右端的候选值菜单（类型 / 字符集 / 排序规则三种菜单共用一套渲染）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellMenu {
    /// 类型候选（按引擎给清单，写入类型框）。
    Type,
    /// 字符集候选（MySQL）。
    Charset,
    /// 排序规则候选。
    Collation,
}

impl CellMenu {
    /// 元素 id 里的稳定短名（同一行三个菜单不能重名）。
    const fn id(self) -> &'static str {
        match self {
            Self::Type => "type",
            Self::Charset => "charset",
            Self::Collation => "collation",
        }
    }
}

/// 多选下拉：`toolbar_text_dropdown_button` + 带勾选态的菜单，点一项切换一项。
///
/// 值就是显示文案（列名 / 动作名本身就是标识符），`selected` 的顺序即选择顺序
/// ——索引的组成列顺序与复合外键的列对齐都靠它。
/// 多选下拉（勾选态，可勾多项）：用于「索引组成列」「本表列」「引用列」这类
/// 列清单——候选动辄上百，菜单**限高 + 顶部搜索框**。
fn multi_select_button(
    id: SharedString,
    options: Vec<String>,
    selected: Vec<String>,
    placeholder: &'static str,
    on_toggle: impl Fn(String, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let theme = active_theme(cx);
    let summary = if selected.is_empty() {
        SharedString::from(placeholder)
    } else {
        SharedString::from(selected.join(", "))
    };
    let specs: Vec<MenuItemSpec> = options
        .iter()
        .map(|option| {
            MenuItemSpec::new(option.clone(), option.clone()).checked(selected.contains(option))
        })
        .collect();
    let picked: Rc<dyn Fn(SharedString, &mut Window, &mut App)> =
        Rc::new(move |value, window, cx| on_toggle(value.to_string(), window, cx));
    toolbar_text_searchable_dropdown(
        id,
        summary,
        Icon::new(DuckIcon::ListOrdered).size(theme.extended().icon.sm),
        Anchor::BottomLeft,
        specs,
        Some(SharedString::from(tr(cx, "搜索…"))),
        picked,
        cx,
    )
    .into_any_element()
}

/// 新增触发器的时机档（`CREATE TRIGGER` 的 `{BEFORE|AFTER}`）。
const TRIGGER_TIMINGS: [&str; 2] = ["BEFORE", "AFTER"];

/// 新增触发器的事件档。
const TRIGGER_EVENTS: [&str; 3] = ["INSERT", "UPDATE", "DELETE"];

/// 外键动作的合法档（SQL 标准 + MySQL / PG / T-SQL 的交集）。
const FK_ACTIONS: [&str; 5] = [
    "NO ACTION",
    "CASCADE",
    "SET NULL",
    "SET DEFAULT",
    "RESTRICT",
];

/// 把内核报的类型名拆成「基础类型 + 长度修饰」。
///
/// `varchar(50)` → `("varchar", "50")`；`decimal(14,3)` → `("decimal", "14,3")`；
/// 不带括号的类型长度落空串。**只按第一层括号拆**：`Nullable(Decimal(9,2))`
/// 这类嵌套（ClickHouse）整体当基础类型——长度列对它没有意义，硬拆反而生成坏
/// SQL。括号不闭合（脏元数据）时原样当基础类型，宁可原样回写。
fn split_type_name(raw: &str) -> (String, String) {
    let raw = raw.trim();
    let Some(open) = raw.find('(') else {
        return (raw.to_owned(), String::new());
    };
    if !raw.ends_with(')') || raw[..open].contains('(') {
        return (raw.to_owned(), String::new());
    }
    let base = raw[..open].trim();
    let length = raw[open + 1..raw.len() - 1].trim();
    // `Nullable(Decimal(9,2))`：内层还有括号 ⇒ 长度里带括号，说明是嵌套类型。
    if base.is_empty() || length.contains('(') {
        return (raw.to_owned(), String::new());
    }
    (base.to_owned(), length.to_owned())
}

/// 把「基础类型 + 长度」拼回内核口径的类型名（长度空则不加括号）。
fn full_type(base: &str, length: &str) -> String {
    let base = base.trim();
    let length = length.trim();
    if length.is_empty() {
        base.to_owned()
    } else {
        format!("{base}({length})")
    }
}

/// 这个基础类型带不带长度修饰（只用来决定长度输入框可不可编辑）。
///
/// 正名单而不是反名单：认不出来的类型一律当「不带长度」（用户可以把完整类型名
/// 直接写进类型框），不会出现「输入框禁用但值还在」的半吊子状态。**长度值非空
/// 时永远照原样回写**，所以认错也不会丢长度。
fn type_takes_length(base: &str) -> bool {
    const WITH_LENGTH: [&str; 26] = [
        "char",
        "varchar",
        "nvarchar",
        "nchar",
        "character",
        "character varying",
        "decimal",
        "numeric",
        "dec",
        "float",
        "double",
        "real",
        "binary",
        "varbinary",
        "bit",
        "enum",
        "set",
        "time",
        "timestamp",
        "datetime",
        "year",
        "blob",
        "tinyblob",
        "mediumblob",
        "longblob",
        "fixedstring",
    ];
    let base = base.trim().to_ascii_lowercase();
    WITH_LENGTH.contains(&base.as_str())
        // 带参数的类型（`decimal32(4)` / `datetime64(3)`）长度对它们同样有意义。
        || base.ends_with("64")
        || base.starts_with("decimal")
}

/// 引擎自建的索引：删不得（MySQL 的 `PRIMARY`、SQLite 的 `sqlite_autoindex_*`）。
fn is_system_index(name: &str, primary: bool) -> bool {
    primary || name.to_ascii_lowercase().starts_with("sqlite_autoindex_")
}

/// 外键动作规范化（内核原样给 `NO ACTION` / `cascade`；下拉要能对上档位）。
fn normalize_action(action: &str) -> String {
    let action = action.trim().to_ascii_uppercase();
    if action.is_empty() {
        "NO ACTION".to_owned()
    } else {
        action
    }
}

/// 表级属性（注释 / 存储引擎）对当前引擎可不可读，以及索引 / 外键可不可生成 DDL。
/// 返回 `(索引可编辑, 外键可编辑)`。
///
/// 索引：四家文本引擎都能 `CREATE / DROP INDEX`（ClickHouse 的「索引」是数据跳过
/// 索引、语义完全不同；Mongo / ES 没有这个概念）。
/// 外键：SQLite **加不了也删不了**外键（`ALTER TABLE` 不支持，只能重建表）；
/// ClickHouse / Mongo / ES 同样没有——按能力开放，不摆改不动的编辑器。
fn structure_capabilities(kind: DatabaseKind) -> (bool, bool, bool) {
    let index = matches!(
        kind,
        DatabaseKind::Mysql
            | DatabaseKind::Postgres
            | DatabaseKind::SqlServer
            | DatabaseKind::Sqlite
    );
    let foreign_key = matches!(
        kind,
        DatabaseKind::Mysql | DatabaseKind::Postgres | DatabaseKind::SqlServer
    );
    // 触发器：MySQL / SQLite 的触发体是内联语句、PG 是函数名，都生成得了
    // `CREATE TRIGGER`；T-SQL 等方言差异大，不给入口。
    let trigger = matches!(
        kind,
        DatabaseKind::Mysql | DatabaseKind::Postgres | DatabaseKind::Sqlite
    );
    (index, foreign_key, trigger)
}

/// 类型下拉的候选（**按引擎**；演示稿的 26 项是 MySQL 专用，PG 的 `timestamptz`
/// 与 T-SQL 的 `nvarchar` 塞不进去）。候选只是快捷入口——类型框仍可自由输入。
fn structure_type_options(kind: DatabaseKind) -> &'static [&'static str] {
    match kind {
        DatabaseKind::Mysql => &[
            "bigint",
            "int",
            "smallint",
            "tinyint",
            "decimal",
            "float",
            "double",
            "varchar",
            "char",
            "text",
            "tinytext",
            "mediumtext",
            "longtext",
            "datetime",
            "date",
            "timestamp",
            "time",
            "year",
            "json",
            "enum",
            "set",
            "blob",
            "varbinary",
            "binary",
            "boolean",
        ],
        DatabaseKind::Postgres => &[
            "bigint",
            "integer",
            "smallint",
            "numeric",
            "real",
            "double precision",
            "serial",
            "bigserial",
            "boolean",
            "varchar",
            "char",
            "text",
            "uuid",
            "json",
            "jsonb",
            "date",
            "timestamp",
            "timestamptz",
            "time",
            "timetz",
            "interval",
            "bytea",
            "inet",
            "cidr",
            "macaddr",
            "xml",
        ],
        DatabaseKind::Sqlite => &[
            "INTEGER", "TEXT", "REAL", "BLOB", "NUMERIC", "BOOLEAN", "DATE", "DATETIME", "VARCHAR",
            "DECIMAL",
        ],
        DatabaseKind::SqlServer => &[
            "bigint",
            "int",
            "smallint",
            "tinyint",
            "bit",
            "decimal",
            "numeric",
            "money",
            "float",
            "real",
            "date",
            "datetime",
            "datetime2",
            "smalldatetime",
            "time",
            "char",
            "varchar",
            "nchar",
            "nvarchar",
            "text",
            "ntext",
            "binary",
            "varbinary",
            "uniqueidentifier",
        ],
        DatabaseKind::ClickHouse => &[
            "UInt8",
            "UInt16",
            "UInt32",
            "UInt64",
            "Int8",
            "Int16",
            "Int32",
            "Int64",
            "Float32",
            "Float64",
            "Decimal",
            "String",
            "FixedString",
            "Date",
            "DateTime",
            "DateTime64",
            "UUID",
            "Enum8",
            "Enum16",
            "Array",
            "Map",
            "Nullable",
        ],
        // 没有列类型概念的引擎（「列」只是采样出来的字段名）：不给候选。
        DatabaseKind::Mongo | DatabaseKind::Elasticsearch => &[],
        // agent 档：类型名逐引擎不同（Oracle `NUMBER(19)` / Hive `STRING` / DB2 `BIGINT`…），
        // 列一份必然错的多数的候选不如不列 —— **类型框仍可自由输入**，这一项只是快捷入口。
        DatabaseKind::Agent => &[],
    }
}

/// 字符集下拉的候选（MySQL 专属属性；表默认 = 空串）。
const CHARSET_OPTIONS: [&str; 3] = ["utf8mb4", "utf8", "gbk"];

/// 类型名 → 着色（demo 的类型着色：整数橙 / 字符串绿 / 时间紫 / 小数蓝 / JSON 青）。
/// 只影响**显示**，不影响生成的 SQL。
fn type_color(base: &str, theme: &duck_theme::DuckThemeState) -> gpui::Hsla {
    let tokens = theme.tokens();
    let extended = theme.extended();
    let base = base.trim().to_ascii_lowercase();
    let is_int = matches!(
        base.as_str(),
        "bigint"
            | "int"
            | "integer"
            | "smallint"
            | "tinyint"
            | "mediumint"
            | "boolean"
            | "bool"
            | "bit"
            | "serial"
            | "bigserial"
    ) || base.starts_with("uint");
    let is_str = matches!(
        base.as_str(),
        "varchar"
            | "char"
            | "text"
            | "tinytext"
            | "mediumtext"
            | "longtext"
            | "enum"
            | "set"
            | "nvarchar"
            | "nchar"
            | "ntext"
            | "string"
            | "fixedstring"
            | "uuid"
            | "uniqueidentifier"
    );
    let is_time = matches!(
        base.as_str(),
        "datetime"
            | "date"
            | "timestamp"
            | "time"
            | "year"
            | "datetime2"
            | "smalldatetime"
            | "timestamptz"
            | "timetz"
            | "datetime64"
    );
    let is_decimal = matches!(
        base.as_str(),
        "decimal" | "numeric" | "float" | "double" | "real" | "money" | "dec" | "double precision"
    ) || base.starts_with("decimal");
    let is_json = matches!(base.as_str(), "json" | "jsonb" | "map" | "array");
    if is_int {
        extended.colors.warning
    } else if is_str {
        extended.colors.success
    } else if is_time {
        tokens.colors.primary
    } else if is_decimal {
        tokens.colors.accent
    } else if is_json {
        extended.colors.info
    } else {
        tokens.colors.muted_foreground
    }
}

/// [`DatabasePage::build_structure_draft`] 从 DDL 里解析出来的表级属性。
#[derive(Default)]
struct TableProps {
    /// 表注释（拿不到为 `None`）。
    comment: Option<String>,
    /// 存储引擎（拿不到为 `None`）。
    engine: Option<String>,
}

/// 从建表 DDL 里解析表注释与存储引擎。
///
/// **只有 MySQL 走这条**：`SHOW CREATE TABLE` 的收尾行是
/// `) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_general_ci COMMENT='…'`，
/// 结构化地拿这两个值内核没有 API（会顺带改 `duck-db`），所以在这里解析。
/// 解析不到（视图、非 MySQL、DDL 读失败）→ 两个槽位都是 `None`，表级属性行整行
/// 不显示——**不摆一个空框**。
///
/// 解析范围限定在**最后一行以 `)` 开头的那一行**：列定义里同样有 `COMMENT '…'`
/// （列注释语法还少一个 `=`），只看收尾行才不会把列注释当表注释。
fn parse_table_props(kind: DatabaseKind, ddl: Option<&str>) -> TableProps {
    if kind != DatabaseKind::Mysql {
        return TableProps::default();
    }
    let Some(tail) = ddl.and_then(|ddl| {
        ddl.lines()
            .rev()
            .find(|line| line.trim_start().starts_with(')'))
    }) else {
        return TableProps::default();
    };
    TableProps {
        comment: quoted_attr(tail, "COMMENT='"),
        engine: token_attr(tail, "ENGINE="),
    }
}

/// 取 `KEY=<标识符>` 形式的值（到第一个非标识符字符为止）。
fn token_attr(text: &str, key: &str) -> Option<String> {
    let rest = text.split_once(key)?.1;
    let value: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    (!value.is_empty()).then_some(value)
}

/// 取 `KEY='…'` 形式的值（`''` 是转义的单引号；未闭合返回 `None`）。
fn quoted_attr(text: &str, key: &str) -> Option<String> {
    let rest = text.split_once(key)?.1;
    let mut value = String::new();
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        if c == '\'' {
            if chars.clone().next() == Some('\'') {
                chars.next();
                value.push('\'');
                continue;
            }
            return Some(value);
        }
        value.push(c);
    }
    None
}

/// 索引 / 外键语句里的列名要按**当前草稿**解析：列可能在同一批语句里刚被改名。
/// 找不到对应原列（新列 / 未知名字）就原样返回。
fn resolved_column_name(draft: &StructureDraft, name: &str, cx: &App) -> String {
    draft
        .columns
        .iter()
        .find(|column| column.original.as_deref() == Some(name))
        .map(|column| column.name.read(cx).value().trim().to_owned())
        .filter(|current| !current.is_empty())
        .unwrap_or_else(|| name.to_owned())
}

/// `"schema.table"` / `"table"` 文本 → 逐段加引号的限定名。
fn quote_table_text(text: &str, quote: char) -> String {
    let text = text.trim();
    let (schema, name) = match text.split_once('.') {
        Some((schema, name)) => (Some(schema.trim().to_owned()), name.trim()),
        None => (None, text),
    };
    let table = TableRef {
        name: name.to_owned(),
        schema,
    };
    quote_qualified(&table, quote).unwrap_or_else(|_| text.to_owned())
}

/// 四家文本引擎的 `CREATE [UNIQUE] INDEX name ON t (cols)` —— 语法一致。
fn create_index_sql(
    table: &str,
    name: &str,
    columns: &[String],
    unique: bool,
    quote: char,
) -> String {
    let name = quote_identifier(name, quote).unwrap_or_else(|_| name.to_owned());
    let columns: Vec<String> = columns
        .iter()
        .map(|column| quote_identifier(column, quote).unwrap_or_else(|_| column.to_owned()))
        .collect();
    format!(
        "CREATE {}INDEX {name} ON {table} ({});",
        if unique { "UNIQUE " } else { "" },
        columns.join(", ")
    )
}

/// `DROP INDEX`：MySQL / T-SQL 要带表名（同一库内索引名可以重名），PG / SQLite 的
/// 索引是 schema 级对象，只给名字。
fn drop_index_sql(kind: DatabaseKind, table: &str, name: &str, quote: char) -> Option<String> {
    let name = quote_identifier(name, quote).unwrap_or_else(|_| name.to_owned());
    Some(match kind {
        DatabaseKind::Mysql | DatabaseKind::SqlServer => {
            format!("DROP INDEX {name} ON {table};")
        }
        DatabaseKind::Postgres | DatabaseKind::Sqlite => format!("DROP INDEX {name};"),
        DatabaseKind::ClickHouse | DatabaseKind::Mongo | DatabaseKind::Elasticsearch => {
            return None;
        }
        // agent 档：两种写法都有（Oracle / DM 不带表名，DB2 / Hive / Snowflake 各不一样），
        // 猜错就是一条只报语法错的语句。索引管理条在 agent 档本来也不画（见
        // `TreeCategory::concept_supported`），这里给 `None` 与界面一致。
        DatabaseKind::Agent => return None,
    })
}

/// `ADD CONSTRAINT … FOREIGN KEY …`（MySQL / PG / T-SQL 三家的公共语法）。
///
/// 约束名留空时由引擎自动命名（MySQL / PG 都支持省略 `CONSTRAINT`）；列数不等
/// 由调用点先拦。
#[allow(clippy::too_many_arguments)]
fn add_foreign_key_sql(
    table: &str,
    name: &str,
    columns: &[String],
    ref_table: &str,
    ref_columns: &[String],
    on_update: &str,
    on_delete: &str,
    quote: char,
) -> String {
    let quoted = |names: &[String]| -> String {
        names
            .iter()
            .map(|name| quote_identifier(name, quote).unwrap_or_else(|_| name.to_owned()))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let constraint = if name.trim().is_empty() {
        String::new()
    } else {
        format!(
            "CONSTRAINT {} ",
            quote_identifier(name, quote).unwrap_or_else(|_| name.to_owned())
        )
    };
    let reference = quote_table_text(ref_table, quote);
    format!(
        "ALTER TABLE {table} ADD {constraint}FOREIGN KEY ({}) REFERENCES {reference} ({}) ON UPDATE {} ON DELETE {};",
        quoted(columns),
        quoted(ref_columns),
        normalize_action(on_update),
        normalize_action(on_delete)
    )
}

/// `DROP CONSTRAINT`（PG / T-SQL 通用名）与 MySQL 的 `DROP FOREIGN KEY`。
fn drop_foreign_key_sql(
    kind: DatabaseKind,
    table: &str,
    name: &str,
    quote: char,
) -> Option<String> {
    if name.trim().is_empty() {
        return None;
    }
    let name = quote_identifier(name, quote).unwrap_or_else(|_| name.to_owned());
    Some(match kind {
        DatabaseKind::Mysql => format!("ALTER TABLE {table} DROP FOREIGN KEY {name};"),
        DatabaseKind::Postgres | DatabaseKind::SqlServer => {
            format!("ALTER TABLE {table} DROP CONSTRAINT {name};")
        }
        // agent 档按 `DROP CONSTRAINT` 出（Oracle / 达梦 / PG / DB2 / T-SQL 都是这个名）。
        // 这一条敢给、[`drop_index_sql`] 那条不敢，区别在**语法分歧面**：外键约束删除
        // 在 agent 那一族里几乎只有一种写法，索引删除则要分「带不带表名」两派。
        DatabaseKind::Agent => format!("ALTER TABLE {table} DROP CONSTRAINT {name};"),
        DatabaseKind::Sqlite
        | DatabaseKind::ClickHouse
        | DatabaseKind::Mongo
        | DatabaseKind::Elasticsearch => return None,
    })
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

/// 字段行拖拽的载荷：带**行身份**（该行「字段名」输入实体的 id）。
///
/// 身份而不是下标是关键：`on_drag_move` 会在同一行上**反复**触发，只有按身份重新
/// 定位「源行此刻在哪」才能让重排幂等（demo 在 `drop` 时才按下标搬一次，我们这版
/// 是活重排，用下标搬就会越拖越乱）。
#[derive(Clone)]
struct StructureRowDrag {
    row: gpui::EntityId,
}

impl Render for StructureRowDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

/// 列宽拖拽的载荷：起始鼠标 x 与起始列宽（`on_drag_move` 用位移算新宽度）。
#[derive(Clone)]
struct StructureColResize {
    index: usize,
    start_x: Pixels,
    start_w: Pixels,
}

impl Render for StructureColResize {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

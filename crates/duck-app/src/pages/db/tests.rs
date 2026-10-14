//! 数据库页的模块单测：全部是**纯函数**口径（列宽 / 可见列 / 命中行 / 分页排序 /
//! 仪表板排布 / 档案展开 / 结果集格式化），不需要窗口与真实数据库。
//!
//! 由 `mod.rs` 的 `#[cfg(test)] mod tests;` 引入。守卫扫描会跳过 `tests.rs`
//! （见 `duck-theme/src/i18n.rs` 的 `collect`），所以这里的夹具串不进 EN 表。

use super::*;
use duck_db::Value;
use gpui::TestAppContext;

/// 「字段筛选」的可见列计算：跳过被藏起来的列，但**保留下标**。
///
/// 下标是这里唯一的陷阱：行数据（`Vec<Value>`）与列宽都按**完整**列序对齐，
/// 返回的下标一旦被当成「可见列里的第几个」，藏一列就会让后面所有单元格的
/// 值与列名错位半格。顺带钉两个边界：一列不藏时长度不变、全藏光不 panic
/// （「至少留一列」是 [`DatabasePage::toggle_column_hidden`] 的职责，纯函数只管算）。
#[test]
fn visible_columns_keeps_original_indexes() {
    let columns = vec![
        Column {
            name: "id".to_owned(),
            type_name: "INTEGER".to_owned(),
        },
        Column {
            name: "secret".to_owned(),
            type_name: "TEXT".to_owned(),
        },
        Column {
            name: "name".to_owned(),
            type_name: "TEXT".to_owned(),
        },
    ];
    let hidden: BTreeSet<String> = ["secret".to_owned()].into_iter().collect();
    assert_eq!(
        visible_columns(&columns, &hidden)
            .iter()
            .map(|(index, column)| (*index, column.name.as_str()))
            .collect::<Vec<_>>(),
        vec![(0, "id"), (2, "name")],
    );
    assert_eq!(visible_columns(&columns, &BTreeSet::new()).len(), 3);
    let all: BTreeSet<String> = columns.iter().map(|column| column.name.clone()).collect();
    assert!(visible_columns(&columns, &all).is_empty());
}

/// 「在结果中搜索」的命中集：升序原始下标、大小写不敏感、**只搜可见列**、
/// 空词（含纯空白）= 不过滤。
///
/// 「只搜可见列」是刻意的：命中来源在被藏掉的列里、网格上却看不出这行为什么
/// 中选，比搜不到更让人糊涂。空词全量是 [`uniform_list`] 还要拿它当行数，
/// 返回空集会让「没搜也列表消失」。
#[test]
fn matched_row_indexes_searches_visible_columns_only() {
    let columns = vec![
        Column {
            name: "id".to_owned(),
            type_name: "INTEGER".to_owned(),
        },
        Column {
            name: "name".to_owned(),
            type_name: "TEXT".to_owned(),
        },
        Column {
            name: "secret".to_owned(),
            type_name: "TEXT".to_owned(),
        },
    ];
    let hidden: BTreeSet<String> = ["secret".to_owned()].into_iter().collect();
    let visible = visible_columns(&columns, &hidden);
    let rows = vec![
        vec![
            Value::Int(1),
            Value::Text("Alpha".to_owned()),
            Value::Text("alpha-secret".to_owned()),
        ],
        vec![
            Value::Int(2),
            Value::Text("beta".to_owned()),
            Value::Text("beta-secret".to_owned()),
        ],
        vec![
            Value::Int(3),
            Value::Text("Gamma".to_owned()),
            Value::Text("gamma-secret".to_owned()),
        ],
    ];
    // 命中 + 大小写不敏感，返回原始下标。
    assert_eq!(matched_row_indexes(&rows, &visible, "beta"), vec![1]);
    assert_eq!(matched_row_indexes(&rows, &visible, "BETA"), vec![1]);
    // 命中来源在隐藏列里：不搜（搜到了网格上也看不出这行为什么中选）。
    assert!(matched_row_indexes(&rows, &visible, "alpha-secret").is_empty());
    // 空词 / 纯空白 = 不过滤（顺序 = 原始下标升序）。
    assert_eq!(matched_row_indexes(&rows, &visible, ""), vec![0, 1, 2]);
    assert_eq!(matched_row_indexes(&rows, &visible, "   "), vec![0, 1, 2]);
}

/// 「来源语句 chip」的单行化：换行 / 缩进 / 连续空白都并成一个空格，首尾不留；
/// 单行原样、空串给空串。
///
/// 钉住的是「仅显示用」这一档 —— 复制走的是**原文**，别顺手拿这里的结果当清洗后的 SQL。
#[test]
fn one_line_sql_folds_whitespace_but_keeps_single_lines() {
    assert_eq!(
        one_line_sql("SELECT a,\n    b\nFROM t\nWHERE x = 1"),
        "SELECT a, b FROM t WHERE x = 1"
    );
    assert_eq!(one_line_sql("  SELECT 1  "), "SELECT 1");
    assert_eq!(one_line_sql("SELECT 1"), "SELECT 1");
    assert_eq!(one_line_sql("   \n\t "), "");
    assert_eq!(one_line_sql(""), "");
}

/// 「同对象复用」判定：同连接同对象才复用；跨连接、跨对象、身份缺失都不算。
///
/// 这是 [`find_reusable_tab`] 的纯函数验收——复用策略写歪的最常见形态是
/// 「忘了比对连接」（两个连接的同名表互相抢标签）。
#[test]
fn data_tab_reuse_requires_same_connection_and_object() {
    let tabs = vec!["t1".to_owned(), "t2".to_owned(), "t3".to_owned()];
    let kinds: HashMap<String, TabKind> = [
        ("t1".to_owned(), TabKind::Query),
        (
            "t2".to_owned(),
            TabKind::Data {
                object: TableRef::in_schema("wordpress", "ah_posts"),
            },
        ),
        (
            "t3".to_owned(),
            TabKind::Data {
                object: TableRef::bare("ah_posts"),
            },
        ),
    ]
    .into_iter()
    .collect();
    let owners: HashMap<String, String> = [
        ("t1".to_owned(), "c1".to_owned()),
        ("t2".to_owned(), "c1".to_owned()),
        ("t3".to_owned(), "c2".to_owned()),
    ]
    .into_iter()
    .collect();

    // 同连接 + 同库同表 → 复用 t2。
    assert_eq!(
        find_reusable_tab(
            &tabs,
            &kinds,
            &owners,
            "c1",
            &TabKind::Data {
                object: TableRef::in_schema("wordpress", "ah_posts"),
            },
        )
        .as_deref(),
        Some("t2"),
    );
    // 同表名但不同 schema → 不复用（裸名与限定名是两个对象）。
    assert_eq!(
        find_reusable_tab(
            &tabs,
            &kinds,
            &owners,
            "c1",
            &TabKind::Data {
                object: TableRef::bare("ah_posts"),
            },
        ),
        None,
    );
    // 同对象但归属另一条连接 → 不复用。
    assert_eq!(
        find_reusable_tab(
            &tabs,
            &kinds,
            &owners,
            "c2",
            &TabKind::Data {
                object: TableRef::in_schema("wordpress", "ah_posts"),
            },
        ),
        None,
    );
    // 查询标签不参与对象复用。
    assert_eq!(
        find_reusable_tab(&tabs, &kinds, &owners, "c1", &TabKind::Query).as_deref(),
        Some("t1"),
    );
}

/// 实例 / 仪表板标签的「同身份复用」：同连接同档（板）复用；档位不同、
/// 板不同、跨连接都不复用。
///
/// 实例四档与仪表板搬进标签层后，`TabKind` 的相等性就是复用的全部判据——
/// 这里钉住「`Instance(Overview)` ≠ `Instance(Metrics)`」这类身份区分，
/// 防止日后把枚举比对改成按变体名（`matches!`）时悄悄串档。
#[test]
fn instance_and_dashboard_tabs_reuse_by_exact_identity() {
    let tabs = vec!["t1".to_owned(), "t2".to_owned(), "t3".to_owned()];
    let kinds: HashMap<String, TabKind> = [
        ("t1".to_owned(), TabKind::Instance(InstanceTab::Overview)),
        ("t2".to_owned(), TabKind::Instance(InstanceTab::Metrics)),
        ("t3".to_owned(), TabKind::Dashboard(7)),
    ]
    .into_iter()
    .collect();
    let owners: HashMap<String, String> = [
        ("t1".to_owned(), "c1".to_owned()),
        ("t2".to_owned(), "c1".to_owned()),
        ("t3".to_owned(), "c1".to_owned()),
    ]
    .into_iter()
    .collect();

    // 同连接同档 → 复用。
    assert_eq!(
        find_reusable_tab(
            &tabs,
            &kinds,
            &owners,
            "c1",
            &TabKind::Instance(InstanceTab::Overview)
        )
        .as_deref(),
        Some("t1"),
    );
    // 同连接**不同档** → 不复用（实例概览和实例指标是两张标签）。
    assert_eq!(
        find_reusable_tab(
            &tabs,
            &kinds,
            &owners,
            "c1",
            &TabKind::Instance(InstanceTab::Inspector)
        ),
        None,
    );
    // 同连接同板 → 复用；别的板 → 不复用。
    assert_eq!(
        find_reusable_tab(&tabs, &kinds, &owners, "c1", &TabKind::Dashboard(7)).as_deref(),
        Some("t3"),
    );
    assert_eq!(
        find_reusable_tab(&tabs, &kinds, &owners, "c1", &TabKind::Dashboard(9)),
        None,
    );
    // 跨连接 → 不复用。
    assert_eq!(
        find_reusable_tab(
            &tabs,
            &kinds,
            &owners,
            "c2",
            &TabKind::Instance(InstanceTab::Overview)
        ),
        None,
    );
}

/// 标签重名去重：首个保持原名，之后按**出现顺序**补 ` n` 后缀；不同名的互不影响。
///
/// 去重写歪的常见形态有两种：给**首次**出现也补后缀（`users 2` 排在最前），
/// 以及给不同名也共享计数器（`posts` 被连累成 `posts 2`）。两条都在这里钉住。
#[test]
fn duplicate_tab_titles_get_ordered_suffix() {
    let base = vec![
        "users".to_owned(),
        "posts".to_owned(),
        "users".to_owned(),
        "users".to_owned(),
        "posts".to_owned(),
    ];
    assert_eq!(
        dedupe_titles(&base),
        vec!["users", "posts", "users 2", "users 3", "posts 2"],
    );
    // 空输入不 panic，全是唯一名时原样返回。
    assert!(dedupe_titles(&[]).is_empty());
    assert_eq!(
        dedupe_titles(&["main".to_owned(), "logs".to_owned()]),
        vec!["main", "logs"],
    );
}

/// `add_window` 要求构建函数返回一个 `Render` 类型；这里只用它的空壳承接断言。
struct SqlEditorProbe;
impl Render for SqlEditorProbe {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// SQL 编辑器必须真的挂上「已注册的」高亮语言（B8 的接线验收）。
///
/// 上游对认不出的语言名是**静默不报**（取不到 `LanguageRegistry` 条目只在渲染时记一条
/// warn，界面照旧出字），所以「把 `"sql"` 敲成 `"sq"`」这类错误编译期与运行期都不会有人
/// 喊 —— 只有把注册表与编辑器状态两头对一次才拦得住。这一条同时守住
/// `LanguageRegistry` 里确实有这个名字（即根 `Cargo.toml` 的
/// `features = ["tree-sitter-sql"]` 没被人顺手删掉）。
#[gpui::test]
fn the_sql_editor_is_wired_to_a_registered_language(cx: &mut TestAppContext) {
    let registry = gpui_component::highlighter::LanguageRegistry::singleton();
    assert!(
        registry
            .languages()
            .iter()
            .any(|name| name.as_ref() == SQL_LANGUAGE),
        "`{SQL_LANGUAGE}` 没进 LanguageRegistry：`tree-sitter-sql` feature 可能被关了"
    );

    // `EditorState::new` 需要 `&mut Window`，所以只能在窗口构建里建、也就地读回。
    cx.add_window(|window, cx| {
        let editor = cx.new(|cx| EditorState::new(window, cx).language(SQL_LANGUAGE));
        assert_eq!(
            editor.read(cx).language_name().as_ref(),
            SQL_LANGUAGE,
            "编辑器记下的语言名与常量不一致"
        );
        SqlEditorProbe
    });
}

/// 本页的页面级键位必须**真的**绑在 `"Database"` 上下文上（B10 的接线验收）。
///
/// 盯的是最容易各写各的三处：动作由 `actions!` 声明、上下文常量 `KEY_CONTEXT` 在
/// 同一模块、绑定由 [`bind_keys`] 做，而 `app::install` 只负责调一次。任何一处漂了，
/// 界面照样编译、键位却悄悄失效 —— 只有把键位表**读回来**才能发现，所以这里不测行为、
/// 测绑定本身。
///
/// 不断言「本页键位不与别处相撞」：那是 `duck_command::bind_all` 的职责（会打
/// `keybinding conflict` 警告）；这几条选键的依据是 `gpui_base::Root` 的默认表里
/// 没有它们，属「选型」而非「接线」。
#[gpui::test]
fn page_keys_are_bound_in_the_database_context(cx: &mut TestAppContext) {
    cx.update(bind_keys);
    cx.update(|cx| {
        // 声明顺序有讲究：`bindings_for_action` 要求 `&self` 与 `&dyn Action` 同寿，
        // 而 `Ref` 必须比它借的 `Rc` 先析构 —— 动作实例在最前、`Rc` 次之、`Ref` 最后。
        let run = RunOrStopQuery;
        let commit = CommitPendingEdits;
        let discard = DiscardPendingEdits;
        let undo = UndoPendingEdits;
        let redo = RedoPendingEdits;
        let copy_row = CopySelectedRow;
        let select_all = SelectAllRows;
        let cut = CutSelectedCell;
        let paste = PasteIntoSelectedCell;
        let stop = StopQuery;
        let bindings = cx.key_bindings();
        let keymap = bindings.borrow();

        let expected: [(&dyn gpui::Action, &str, bool); 10] = [
            (&run, "enter", false),
            (&commit, "s", false),
            (&discard, "backspace", true),
            (&undo, "z", false),
            (&redo, "z", true),
            (&copy_row, "c", false),
            (&select_all, "a", false),
            (&cut, "x", false),
            (&paste, "v", false),
            (&stop, ".", false),
        ];
        let mut strokes_seen = std::collections::BTreeSet::new();
        for (action, key, shift) in expected {
            let binding = keymap
                .bindings_for_action(action)
                .next()
                .unwrap_or_else(|| panic!("`{}` 没有绑定键位", action.name()));
            assert_eq!(
                binding
                    .predicate()
                    .map(|predicate| predicate.to_string())
                    .as_deref(),
                Some(KEY_CONTEXT),
                "`{}` 必须绑在页面上下文里，否则焦点稍深一点就失效",
                action.name()
            );
            let strokes = binding.keystrokes();
            assert_eq!(strokes.len(), 1, "`{}` 应当是单笔画键位", action.name());
            let stroke = &strokes[0];
            assert_eq!(stroke.key(), key, "`{}` 的键名不对", action.name());
            assert!(
                stroke.modifiers().secondary(),
                "`{}` 必须带平台主修饰键（⌘ / Ctrl）",
                action.name()
            );
            assert_eq!(
                stroke.modifiers().shift,
                shift,
                "`{}` 的 Shift 档位不对",
                action.name()
            );
            assert!(
                strokes_seen.insert(stroke.to_string()),
                "`{}` 与另一个动作挤在同一个键上（重复键不会编译报错，只能在这里拦）",
                action.name()
            );
        }
    });
}

/// 复制出来的 TSV 必须是「能直接粘进表格」的：分隔符、换行、引号都要处理好，
/// 否则一行有换行的备注就会把整张表错开一行（错位比少复制更难发现）。
///
/// 单格转义本身在内核（`duck-db`）测；这里盯的是**接线**：整表那条路必须走
/// [`ResultSet::to_tsv`]、「复制整行」那条路必须走同一个 [`tsv_field`]，
/// 两条路的口径不能各走各的。
#[test]
fn tsv_copy_escapes_separators_newlines_and_quotes() {
    assert_eq!(tsv_field("plain"), "plain");
    assert_eq!(tsv_field("a\tb"), "\"a\tb\"");
    assert_eq!(tsv_field("line\nbreak"), "\"line\nbreak\"");
    assert_eq!(tsv_field("say \"hi\""), "\"say \"\"hi\"\"\"");

    let result = ResultSet {
        columns: vec![
            Column {
                name: "id".to_owned(),
                type_name: "INTEGER".to_owned(),
            },
            Column {
                name: "note".to_owned(),
                type_name: "TEXT".to_owned(),
            },
        ],
        rows: vec![
            vec![Value::Int(1), Value::Null],
            vec![Value::Int(2), Value::Text("a\tb".to_owned())],
        ],
        truncated: false,
    };
    // 表头 + 每行一条，行式文本以换行收尾（与内核 `to_csv` 同口径）；
    // NULL 走展示文本 `NULL`（与网格里看到的一致）。
    assert_eq!(result.to_tsv(), "id\tnote\n1\tNULL\n2\t\"a\tb\"\n");
    // 整行拆分出来的每一段，和整表渲染出来的对应段逐字相同。
    assert_eq!(row_as_tsv(&result.rows[0]), "1\tNULL");
    assert_eq!(row_as_tsv(&result.rows[1]), "2\t\"a\tb\"");
    for (index, row) in result.rows.iter().enumerate() {
        let line = result
            .to_tsv()
            .lines()
            .nth(index + 1)
            .unwrap_or("")
            .to_owned();
        assert_eq!(line, row_as_tsv(row), "第 {index} 行两条路必须同源");
    }
}

/// `ResultFormat` 的正文渲染：每一档都要出得来，且 `InsertSql` 缺表引用时给 `None`
/// 而不是硬拼一条坏 SQL；`Binary`（原始字节）**永远不给文本**，它的载荷走
/// [`ResultFormat::render_bytes`]。
#[test]
fn every_result_format_renders_or_declines() {
    let result = ResultSet {
        columns: vec![Column {
            name: "id".to_owned(),
            type_name: "INTEGER".to_owned(),
        }],
        rows: vec![vec![Value::Int(1)]],
        truncated: false,
    };

    assert_eq!(
        ResultFormat::Tsv.render(&result, None),
        Some("id\n1\n".to_owned())
    );
    assert_eq!(
        ResultFormat::Csv.render(&result, None),
        Some("id\n1\n".to_owned())
    );
    assert_eq!(
        ResultFormat::Text.render(&result, None),
        Some("id\n1\n".to_owned())
    );
    assert_eq!(
        ResultFormat::Markdown.render(&result, None),
        Some("| id |\n| --- |\n| 1 |\n".to_owned())
    );
    assert_eq!(
        ResultFormat::Json.render(&result, None),
        Some("[\n  {\"id\": 1}\n]\n".to_owned())
    );
    assert_eq!(
        ResultFormat::JsonPretty.render(&result, None),
        Some("[\n  {\n    \"id\": 1\n  }\n]\n".to_owned())
    );
    assert_eq!(
        ResultFormat::Hex.render(&result, None),
        Some("1\n".to_owned())
    );
    assert_eq!(
        ResultFormat::Base64.render(&result, None),
        Some("1\n".to_owned())
    );
    // 没有表引用 + 引号（MongoDB / Elasticsearch）：不渲染。
    assert_eq!(ResultFormat::InsertSql.render(&result, None), None);
    // 给了就渲染成可执行草稿。
    assert_eq!(
        ResultFormat::InsertSql.render(&result, Some(("t", '"'))),
        Some("INSERT INTO t (\"id\") VALUES (1);\n".to_owned())
    );
    // 原始字节档不是文本档：文本渲染一律 `None`，单列整数结果也给不出字节。
    assert_eq!(ResultFormat::Binary.render(&result, None), None);
    assert_eq!(ResultFormat::Binary.render_bytes(&result), None);

    // 每档的落盘文件名各不相同（同一次导出不会互相覆盖）。
    let names = ResultFormat::EXPORT_ALL.map(ResultFormat::file_name);
    let unique: std::collections::BTreeSet<_> = names.iter().collect();
    assert_eq!(unique.len(), names.len(), "文件名必须两两不同");
    // 复制菜单是导出菜单的子集（原始字节不进剪贴板），且顺序一致。
    let export: Vec<_> = ResultFormat::EXPORT_ALL.to_vec();
    let copy: Vec<_> = ResultFormat::ALL.to_vec();
    assert_eq!(copy, export[..copy.len()].to_vec(), "两个清单必须同源同序");
    assert_eq!(
        export.last().copied(),
        Some(ResultFormat::Binary),
        "原始字节排在最末（它是导出专属档）"
    );
}

/// 原始字节档的可用性判据与渲染同源：单列二进制 → 可用且给字节；其余 → 不可用。
#[test]
fn binary_format_enables_only_for_raw_payloads() {
    let single_binary = ResultSet {
        columns: vec![Column {
            name: "blob".to_owned(),
            type_name: "BLOB".to_owned(),
        }],
        rows: vec![vec![Value::Bytes(vec![1, 2, 3])]],
        truncated: false,
    };
    assert!(ResultFormat::Binary.exportable(true, false, true));
    assert_eq!(
        ResultFormat::Binary.render_bytes(&single_binary),
        Some(vec![1, 2, 3])
    );
    // 引擎给不出引号规则也不影响原始字节档（它不拼 SQL）。
    assert!(ResultFormat::Binary.exportable(true, false, true));
    // 没有结果集 / 不是单列二进制：置灰。
    assert!(!ResultFormat::Binary.exportable(false, true, true));
    assert!(!ResultFormat::Binary.exportable(true, true, false));
    // 文本档只要「有结果」，不受 INSERT 与字节判据影响。
    assert!(ResultFormat::Csv.exportable(true, false, false));
    assert!(!ResultFormat::Csv.exportable(false, true, true));
}

/// 结构管理的三条骨架：有 DDL 的引擎给语句，MongoDB / Elasticsearch 一律 `None`
/// （界面据此**不给入口**，而不是给了再报错）。
#[test]
fn ddl_templates_cover_sql_engines_only() {
    for engine in DatabaseKind::ALL {
        let create = create_table_template(engine, "t");
        let alter = alter_table_template(engine, "t");
        let drop = drop_table_template(engine, "t");
        if supports_ddl(engine) {
            let create = create.expect("有 DDL 的引擎必须给建表骨架");
            assert!(
                create.starts_with("CREATE TABLE t ("),
                "{engine:?}: {create}"
            );
            // 骨架要能直接跑起来：主键列自带 `NOT NULL`，不留待填的空壳。
            assert!(create.contains("PRIMARY KEY (id)"), "{engine:?}: {create}");
            let alter = alter.expect("有 DDL 的引擎必须给改表骨架");
            assert!(
                alter.starts_with("ALTER TABLE t ADD COLUMN"),
                "{engine:?}: {alter}"
            );
            assert_eq!(
                drop.expect("有 DDL 的引擎必须给删表骨架"),
                "DROP TABLE IF EXISTS t;"
            );
        } else {
            assert!(
                create.is_none() && alter.is_none() && drop.is_none(),
                "{engine:?}"
            );
        }
    }
}

/// 「支持 DDL」的两个判据（引号有没有 / 类型占位表有没有）必须同源。
#[test]
fn ddl_support_judgements_agree() {
    for engine in DatabaseKind::ALL {
        assert_eq!(
            supports_ddl(engine),
            column_type_placeholders(engine).is_some(),
            "{engine:?}"
        );
    }
}

/// 对象树筛选：大小写不敏感的子串匹配；空筛选词**通过全部**（不是筛掉全部）。
///
/// `needle` 的约定是**调用点先小写化**，所以这里只断言「名字侧大小写无关」。
#[test]
fn object_filter_matches_case_insensitively() {
    assert!(object_matches("Users", ""));
    assert!(object_matches("Users", "user"));
    assert!(object_matches("USERS", "user"));
    assert!(object_matches("用户表", "用户"));
    assert!(!object_matches("users", "orders"));
}

/// 已保存查询的新 id：不与现有条目撞车（删过几条之后 `len()+1` 会撞上旧号）。
#[test]
fn saved_query_ids_never_collide() {
    let mk = |id: &str| SavedQuery {
        id: id.to_owned(),
        name: id.to_owned(),
        sql: "SELECT 1".to_owned(),
    };
    assert_eq!(SavedQuery::next_id(&[]), "saved-1");
    assert_eq!(
        SavedQuery::next_id(&[mk("saved-1"), mk("saved-2")]),
        "saved-3"
    );
    // 删掉中间那条后再新建：`len()+1` 会算出 `saved-3`（已存在），必须顺延。
    assert_eq!(
        SavedQuery::next_id(&[mk("saved-1"), mk("saved-3")]),
        "saved-4"
    );
}

/// 实例四档：`ALL` 是左栏渲染顺序，且只有仪表盘不取服务器数据。
#[test]
fn instance_tabs_split_by_server_fetch() {
    assert_eq!(InstanceTab::ALL.len(), 4);
    assert_eq!(InstanceTab::ALL[0], InstanceTab::Dashboard);
    assert!(!InstanceTab::Dashboard.fetches_server());
    for tab in InstanceTab::ALL
        .into_iter()
        .filter(|tab| tab.fetches_server())
    {
        assert_ne!(tab, InstanceTab::Dashboard);
    }
    // 元素 id 用的英文键两两不同（同一连接下四个入口不能共用一个 id）。
    let keys: std::collections::BTreeSet<_> =
        InstanceTab::ALL.iter().map(|tab| tab.key()).collect();
    assert_eq!(keys.len(), InstanceTab::ALL.len());
}

/// 指标时序只保留最近 [`METRIC_SAMPLES`] 次采样（内存有界）。
#[test]
fn metric_history_keeps_a_bounded_window() {
    let mut history = MetricHistory::default();
    for at in 0..(METRIC_SAMPLES as i64 + 10) {
        history.push(at, vec![metric("cpu", at as f64)]);
    }
    assert_eq!(history.len(), METRIC_SAMPLES);
    // 丢的是最旧的：留下的第一条是第 10 个采样（at == 10）。
    let series = history.series("cpu");
    assert_eq!(series.first().map(|(at, _)| *at), Some(10));
    assert_eq!(series.len(), METRIC_SAMPLES);
}

/// 某一轮没报某指标时，序列**跳过**那一轮而不是补 0（补 0 会画出不存在的「跌到底」）。
#[test]
fn metric_history_series_skips_missing_points() {
    let mut history = MetricHistory::default();
    history.push(0, vec![metric("cpu", 1.0), metric("mem", 2.0)]);
    history.push(1, vec![metric("mem", 3.0)]); // 这轮没报 cpu
    history.push(2, vec![metric("cpu", 5.0), metric("mem", 4.0)]);

    let cpu = history.series("cpu");
    assert_eq!(cpu, vec![(0, 1.0), (2, 5.0)], "cpu 序列应跳过第 1 轮");
    let mem = history.series("mem");
    assert_eq!(mem.len(), 3);
    assert!(history.series("nope").is_empty());
}

/// 可画的指标名以**最近一轮**为准（指标清单随引擎版本变，不能用并集）。
#[test]
fn metric_history_keys_follow_the_latest_sample() {
    let mut history = MetricHistory::default();
    history.push(
        0,
        vec![metric("a", 1.0), metric("b", 1.0), metric("a", 9.0)],
    );
    assert_eq!(history.keys(), vec!["a".to_owned(), "b".to_owned()]);
    history.push(1, vec![metric("b", 1.0), metric("c", 1.0)]);
    assert_eq!(history.keys(), vec!["b".to_owned(), "c".to_owned()]);
}

/// 覆盖时长：不足两次采样为 0（界面据此显示「只有一次采样」而不是「覆盖 0s」）。
#[test]
fn metric_history_span_needs_two_samples() {
    let mut history = MetricHistory::default();
    assert_eq!(history.span_secs(), 0);
    history.push(100, vec![metric("cpu", 1.0)]);
    assert_eq!(history.span_secs(), 0);
    history.push(160, vec![metric("cpu", 1.0)]);
    assert_eq!(history.span_secs(), 60);
}

/// 紧凑时长逐档进位（横轴刻度与「覆盖 {}」共用）。
#[test]
fn compact_duration_steps_through_units() {
    assert_eq!(compact_duration(0), "0s");
    assert_eq!(compact_duration(59), "59s");
    assert_eq!(compact_duration(60), "1m");
    assert_eq!(compact_duration(3_599), "59m");
    assert_eq!(compact_duration(3_600), "1h");
    assert_eq!(compact_duration(86_399), "23h");
    assert_eq!(compact_duration(86_400), "1d");
    // 负数（时钟漂移）按 0 处理，不出现「-3s」。
    assert_eq!(compact_duration(-5), "0s");
}

/// 紧凑数值：整数原样、小数两位、千/百万/十亿缩写（纵轴刻度用）。
#[test]
fn compact_number_abbreviates_large_magnitudes() {
    assert_eq!(compact_number(0.0), "0");
    assert_eq!(compact_number(42.0), "42");
    assert_eq!(compact_number(1.5), "1.50");
    assert_eq!(compact_number(1_500.0), "1.5k");
    assert_eq!(compact_number(2_000_000.0), "2.0M");
    assert_eq!(compact_number(1_000_000_000.0), "1.0G");
    // 绝对值判档，负计数（速率差）同样缩写。
    assert_eq!(compact_number(-1_500.0), "-1.5k");
}

/// 造一个指标项。
fn metric(key: &str, value: f64) -> InstanceMetric {
    InstanceMetric {
        key: key.to_owned(),
        value,
    }
}

/// 造一个结果集（列名 + 行）。
fn result_set(columns: &[&str], rows: Vec<Vec<Value>>) -> ResultSet {
    ResultSet {
        columns: columns
            .iter()
            .map(|name| Column {
                name: (*name).to_owned(),
                type_name: String::new(),
            })
            .collect(),
        rows,
        ..ResultSet::default()
    }
}

/// 数值列判据看**值**而不是声明类型（SQLite 动态类型 / CH 的 `Nullable(...)` 包装都
/// 会让类型名不可靠）。
#[test]
fn numeric_columns_are_judged_by_value_not_declared_type() {
    let result = result_set(
        &["name", "amount", "note", "flag", "mixed", "empty"],
        vec![
            vec![
                Value::Text("a".into()),
                Value::Int(1),
                Value::Decimal("2.50".into()),
                Value::Bool(true),
                Value::Int(5),
                Value::Null,
            ],
            vec![
                Value::Text("b".into()),
                Value::Float(2.5),
                Value::Decimal("-3".into()),
                Value::Bool(false),
                Value::Text("boom".into()),
                Value::Null,
            ],
        ],
    );
    let numeric = |name: &str| {
        let index = result
            .columns
            .iter()
            .position(|column| column.name == name)
            .expect("列存在");
        column_is_numeric(&result, index)
    };
    assert!(!numeric("name"), "文本列不是数值");
    assert!(numeric("amount"), "整数 / 浮点算数值");
    assert!(numeric("note"), "能 parse 的 Decimal 算数值");
    assert!(!numeric("flag"), "布尔画成数是误导，不算数值");
    assert!(!numeric("mixed"), "只要有一格非数就整列不算");
    assert!(!numeric("empty"), "整列 NULL 不当数值（没东西可画）");
}

/// 默认映射：X 取第一个非数值列，Y 取数值列（封顶）。
#[test]
fn default_chart_columns_pick_a_label_axis_and_numeric_series() {
    let result = result_set(
        &["created_at", "amount", "qty"],
        vec![vec![
            Value::Text("2026-01-01".into()),
            Value::Int(10),
            Value::Int(2),
        ]],
    );
    let (x, y) = default_chart_columns(&result).expect("有数值列就该给出映射");
    assert_eq!(x, "created_at");
    assert_eq!(y, vec!["amount".to_owned(), "qty".to_owned()]);

    // 全是数值 → 拿第一列当轴，其余当序列。
    let all_numeric = result_set(&["a", "b"], vec![vec![Value::Int(1), Value::Int(2)]]);
    let (x, y) = default_chart_columns(&all_numeric).expect("有数值列");
    assert_eq!(x, "a");
    assert_eq!(y, vec!["b".to_owned()]);

    // 一个数值列都没有 → 不硬编一份空映射。
    let no_numeric = result_set(
        &["a", "b"],
        vec![vec![Value::Text("x".into()), Value::Null]],
    );
    assert!(default_chart_columns(&no_numeric).is_none());

    // 数值列多于封顶时只取前 CHART_SERIES_LIMIT 条。
    let wide: Vec<String> = (0..(CHART_SERIES_LIMIT + 3))
        .map(|i| format!("c{i}"))
        .collect();
    let names: Vec<&str> = wide.iter().map(String::as_str).collect();
    let row: Vec<Value> = (0..wide.len()).map(|i| Value::Int(i as i64)).collect();
    let crowded = result_set(&names, vec![row]);
    let (_, y) = default_chart_columns(&crowded).expect("有数值列");
    assert_eq!(
        y.len(),
        CHART_SERIES_LIMIT,
        "序列数按 CHART_SERIES_LIMIT 封顶"
    );
}

/// 序列抽取：分类缺失 / 数值非数的行跳过（不补 0）。
#[test]
fn chart_series_skips_rows_that_cannot_be_plotted() {
    let result = result_set(
        &["label", "value"],
        vec![
            vec![Value::Text("a".into()), Value::Int(1)],
            vec![Value::Null, Value::Int(2)],
            vec![Value::Text("c".into()), Value::Text("nope".into())],
            vec![Value::Text("d".into()), Value::Decimal("4.5".into())],
        ],
    );
    let series = chart_series(&result, "label", "value");
    assert_eq!(series, vec![("a".to_owned(), 1.0), ("d".to_owned(), 4.5)]);

    // 列名不在结果里 → 空表（调用方据此提示「配置失效」）。
    assert!(chart_series(&result, "nope", "value").is_empty());
    assert!(chart_series(&result, "label", "nope").is_empty());

    // 非有限的浮点进不了坐标系。
    assert_eq!(chart_number(&Value::Float(f64::NAN)), None);
    assert_eq!(chart_number(&Value::Float(f64::INFINITY)), None);
    assert_eq!(chart_number(&Value::Bool(true)), None);
}

/// 配置失效判据：列名对不上就判失效（重跑后列变了不能按老配置画）。
#[test]
fn chart_config_validity_follows_column_names() {
    let result = result_set(
        &["label", "value"],
        vec![vec![Value::Text("a".into()), Value::Int(1)]],
    );
    let good = ChartConfig {
        kind: ChartKind::Line,
        x: "label".to_owned(),
        y: vec!["value".to_owned()],
    };
    assert!(good.is_valid_for(&result));

    let gone_axis = ChartConfig {
        x: "renamed".to_owned(),
        ..good.clone()
    };
    assert!(!gone_axis.is_valid_for(&result), "轴列没了即失效");

    let gone_series = ChartConfig {
        y: vec!["value".to_owned(), "dropped".to_owned()],
        ..good.clone()
    };
    assert!(!gone_series.is_valid_for(&result), "任一条序列没了即失效");

    let empty_series = ChartConfig {
        y: Vec::new(),
        ..good
    };
    assert!(!empty_series.is_valid_for(&result), "没有序列就画不出图");
}

/// 图表类型：稳定键可往返；饼图只吃一条序列，其余封顶。
#[test]
fn chart_kinds_round_trip_and_bound_their_series() {
    for kind in ChartKind::ALL {
        assert_eq!(ChartKind::from_key(kind.key()), Some(kind));
    }
    assert_eq!(ChartKind::from_key("nope"), None);
    assert_eq!(ChartKind::Pie.series_limit(), 1);
    for kind in [ChartKind::Line, ChartKind::Bar, ChartKind::Area] {
        assert_eq!(kind.series_limit(), CHART_SERIES_LIMIT);
    }
    // 键两两不同（同一连接下几张图的元素 id 不能撞）。
    let keys: BTreeSet<&str> = ChartKind::ALL.iter().map(|kind| kind.key()).collect();
    assert_eq!(keys.len(), ChartKind::ALL.len());

    // `effective_y` 按类型截断。
    let config = ChartConfig {
        kind: ChartKind::Pie,
        x: "label".to_owned(),
        y: vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
    };
    assert_eq!(config.effective_y().to_vec(), vec!["a".to_owned()]);
    let line = ChartConfig {
        kind: ChartKind::Line,
        ..config
    };
    assert_eq!(line.effective_y().len(), 3);
}

/// 饼图配色按黄金角旋转色相：条数够多也不撞色，且第一条就是基色。
#[test]
fn pie_palette_rotates_hue_from_the_base_color() {
    let base = Hsla {
        h: 0.5,
        s: 0.8,
        l: 0.5,
        a: 1.0,
    };
    let palette = chart_palette(base, 6);
    assert_eq!(palette.len(), 6);
    assert_eq!(palette[0], base, "第一条就是基色");
    for pair in palette.windows(2) {
        assert_ne!(pair[0].h, pair[1].h, "相邻切片色相不同");
    }
    // 色相恒落在 [0, 1)（旋转后不会被 rem_euclid 甩到区间外）。
    for color in &palette {
        assert!((0.0..1.0).contains(&color.h), "色相越界: {}", color.h);
    }
}

/// 已保存图表的 id 序列不与现有条目撞车。
#[test]
fn saved_chart_ids_never_collide() {
    let mk = |id: &str| SavedChart {
        id: id.to_owned(),
        name: id.to_owned(),
        sql: String::new(),
        kind: ChartKind::Line,
        x: String::new(),
        y: Vec::new(),
    };
    assert_eq!(SavedChart::next_id(&[]), "chart-1");
    assert_eq!(
        SavedChart::next_id(&[mk("chart-1"), mk("chart-2")]),
        "chart-3"
    );
    // 中间被删过（只有 1 与 3）→ 补到 4，不重用已存在过的号。
    assert_eq!(
        SavedChart::next_id(&[mk("chart-1"), mk("chart-3")]),
        "chart-4"
    );
    // 落盘往返：类型 / 列映射都得原样回来。
    let saved = SavedChart {
        id: "chart-9".to_owned(),
        name: "按天下单量".to_owned(),
        sql: "SELECT day, amount FROM orders".to_owned(),
        kind: ChartKind::Bar,
        x: "day".to_owned(),
        y: vec!["amount".to_owned()],
    };
    let json = serde_json::to_string(&saved).expect("可序列化");
    let back: SavedChart = serde_json::from_str(&json).expect("可反序列化");
    assert_eq!(back.kind, ChartKind::Bar);
    assert_eq!(back.x, "day");
    assert_eq!(back.config().y, ["amount".to_owned()]);
}

/// 12 栏流式打包：一行最多 12 栏，装不下就换行；跨度越界收口。
#[test]
fn dashboard_rows_pack_into_twelve_columns() {
    // 两个半行 = 一行。
    assert_eq!(pack_dashboard_rows(&[6, 6]), vec![vec![0, 1]]);
    // 半行 + 半行 + 半行 → 第三块换行（6+6 已满 12）。
    assert_eq!(pack_dashboard_rows(&[6, 6, 6]), vec![vec![0, 1], vec![2]]);
    // 整行独占一行，后面的从新行开始（哪怕前一行还剩 1 栏）。
    assert_eq!(
        pack_dashboard_rows(&[5, 12, 7]),
        vec![vec![0], vec![1], vec![2]]
    );
    // 5 + 7 = 12 正好一行；再来的 1 换行。
    assert_eq!(pack_dashboard_rows(&[5, 7, 1]), vec![vec![0, 1], vec![2]]);
    // 越界收口：0 当 1 栏、99 当 12 栏（kv 是可以被外部改的）。
    assert_eq!(pack_dashboard_rows(&[0, 99]), vec![vec![0], vec![1]]);
    assert_eq!(clamp_span(0), 1);
    assert_eq!(clamp_span(99), DASHBOARD_COLUMNS);
    // 空表 → 没有行（调用方据此画空态，而不是画一条空行）。
    assert!(pack_dashboard_rows(&[]).is_empty());
}

/// 一栏的宽度占比 = 栏数 / 12。
#[test]
fn column_fraction_is_span_over_twelve() {
    assert!((column_fraction(6) - 0.5).abs() < f32::EPSILON);
    assert!((column_fraction(12) - 1.0).abs() < f32::EPSILON);
    // 越界同样收口（否则会算出宽度 > 1 的越界面板）。
    assert!((column_fraction(0) - 1.0 / 12.0).abs() < f32::EPSILON);
    assert!((column_fraction(99) - 1.0).abs() < f32::EPSILON);
}

/// 折叠的分隔条只挡它**下面那一段**（直到下一个分隔条），分隔条自己始终可见。
#[test]
fn collapsed_divider_hides_only_its_own_section() {
    let board = Dashboard {
        id: 1,
        name: "生产监控".to_owned(),
        panels: vec![
            panel(
                1,
                6,
                DashboardPanelKind::Chart {
                    saved_chart_id: "chart-1".into(),
                },
            ),
            panel(
                2,
                12,
                DashboardPanelKind::Divider {
                    title: "上半场".into(),
                },
            ),
            panel(
                3,
                6,
                DashboardPanelKind::Chart {
                    saved_chart_id: "chart-2".into(),
                },
            ),
            panel(
                4,
                12,
                DashboardPanelKind::Divider {
                    title: "下半场".into(),
                },
            ),
            panel(
                5,
                6,
                DashboardPanelKind::Inspector {
                    view: InspectorView::Metrics,
                },
            ),
        ],
    };
    // 都不折：五块全在，顺序不变。
    assert_eq!(
        visible_dashboard_panels(&board.panels, &BTreeSet::new()),
        vec![0, 1, 2, 3, 4]
    );
    // 折第一条：它统领的第 3 块藏起来，第二条分隔条与它后面的照旧。
    let collapsed: BTreeSet<u32> = [2].into_iter().collect();
    assert_eq!(
        visible_dashboard_panels(&board.panels, &collapsed),
        vec![0, 1, 3, 4]
    );
    // 折第二条：只影响第 5 块（它自己的那一段）。
    let collapsed: BTreeSet<u32> = [4].into_iter().collect();
    assert_eq!(
        visible_dashboard_panels(&board.panels, &collapsed),
        vec![0, 1, 2, 3]
    );
    // 两条都折：被折的两块都藏起来，两条分隔条自己还在。
    let collapsed: BTreeSet<u32> = [2, 4].into_iter().collect();
    let visible = visible_dashboard_panels(&board.panels, &collapsed);
    assert!(visible.contains(&1) && visible.contains(&3));
    assert_eq!(visible, vec![0, 1, 3]);
}

/// 分隔条一律整行：它的 `span` 存满栏，布局也按满栏算。
#[test]
fn divider_panels_always_span_the_full_row() {
    let divider = panel(
        1,
        6,
        DashboardPanelKind::Divider {
            title: "分组".into(),
        },
    );
    assert_eq!(dashboard_panel_span(&divider), DASHBOARD_COLUMNS);
    let chart = panel(
        2,
        6,
        DashboardPanelKind::Chart {
            saved_chart_id: "chart-1".into(),
        },
    );
    assert_eq!(dashboard_panel_span(&chart), 6);
    // 越界的非分隔面板同样收口。
    let odd = panel(
        3,
        40,
        DashboardPanelKind::Inspector {
            view: InspectorView::Processes,
        },
    );
    assert_eq!(dashboard_panel_span(&odd), DASHBOARD_COLUMNS);
}

/// 面板 id 不回收：删掉中间那块之后，新加的不会撞上还活着的 id。
#[test]
fn dashboard_panel_ids_are_never_reused() {
    let mut board = Dashboard {
        id: 1,
        name: "看板".to_owned(),
        panels: vec![
            panel(
                1,
                6,
                DashboardPanelKind::Divider {
                    title: "一".into()
                },
            ),
            panel(
                3,
                6,
                DashboardPanelKind::Divider {
                    title: "二".into()
                },
            ),
        ],
    };
    assert_eq!(board.next_panel_id(), 4, "取最大值 +1，不复用 2");
    board.panels.push(panel(
        board.next_panel_id(),
        6,
        DashboardPanelKind::Divider {
            title: "三".into()
        },
    ));
    assert_eq!(board.next_panel_id(), 5);
    assert_eq!(Dashboard::next_id(&[]), 1);
    assert_eq!(Dashboard::next_id(&[board]), 2);
}

/// 前挪 / 后挪：与相邻面板换位，越界不动（不 panic、不绕回）。
#[test]
fn moving_a_panel_swaps_it_with_its_neighbour() {
    let mut board = Dashboard {
        id: 1,
        name: "看板".to_owned(),
        panels: vec![
            panel(
                1,
                6,
                DashboardPanelKind::Divider {
                    title: "一".into()
                },
            ),
            panel(
                2,
                6,
                DashboardPanelKind::Divider {
                    title: "二".into()
                },
            ),
            panel(
                3,
                6,
                DashboardPanelKind::Divider {
                    title: "三".into()
                },
            ),
        ],
    };
    let ids = |board: &Dashboard| board.panels.iter().map(|p| p.id).collect::<Vec<_>>();
    board.move_panel(0, 1);
    assert_eq!(ids(&board), vec![2, 1, 3]);
    board.move_panel(2, -1);
    assert_eq!(ids(&board), vec![2, 3, 1]);
    // 两个边界：第一块往前、最后一块往后都不动。
    board.move_panel(0, -1);
    assert_eq!(ids(&board), vec![2, 3, 1]);
    board.move_panel(2, 1);
    assert_eq!(ids(&board), vec![2, 3, 1]);
}

/// 落盘往返：面板的「类型 + 跨度」两件事都得原样回来（面板是展平进外层结构的）。
#[test]
fn dashboards_round_trip_through_json() {
    let board = Dashboard {
        id: 7,
        name: "生产监控".to_owned(),
        panels: vec![
            panel(
                1,
                12,
                DashboardPanelKind::Divider {
                    title: "上半场".into(),
                },
            ),
            panel(
                2,
                6,
                DashboardPanelKind::Chart {
                    saved_chart_id: "chart-2".into(),
                },
            ),
            panel(
                3,
                6,
                DashboardPanelKind::Inspector {
                    view: InspectorView::Processes,
                },
            ),
        ],
    };
    let json = serde_json::to_string(&board).expect("可序列化");
    // 展平：面板里直接读得到 `kind`，不是 `kind: { kind: … }` 两层。
    assert!(json.contains(r#""kind":"divider""#), "{json}");
    assert!(json.contains(r#""kind":"chart""#), "{json}");
    assert!(json.contains(r#""kind":"inspector""#), "{json}");
    let back: Dashboard = serde_json::from_str(&json).expect("可反序列化");
    assert_eq!(back.id, 7);
    assert_eq!(back.panels.len(), 3);
    assert_eq!(back.panels[0].span, 12);
    assert_eq!(
        back.panels[1].kind,
        DashboardPanelKind::Chart {
            saved_chart_id: "chart-2".into()
        }
    );
    assert_eq!(
        back.panels[2].kind,
        DashboardPanelKind::Inspector {
            view: InspectorView::Processes
        }
    );
    // 老 kv（没有 `span` / 没有 `panels`）也得读得进来 —— 缺字段走 serde 默认。
    let legacy: Dashboard =
        serde_json::from_str(r#"{"id":1,"name":"旧板"}"#).expect("缺字段要能回落");
    assert!(legacy.panels.is_empty());
    let no_span: Dashboard = serde_json::from_str(
        r#"{"id":1,"name":"旧板","panels":[{"id":1,"kind":"divider","title":"段"}]}"#,
    )
    .expect("缺 span 要能回落");
    assert_eq!(no_span.panels[0].span, DASHBOARD_DEFAULT_SPAN);
}

/// 检查器两档的稳定键往返（下拉回投靠它）。
#[test]
fn inspector_view_keys_round_trip() {
    for view in InspectorView::ALL {
        assert_eq!(InspectorView::from_key(view.key()), Some(view));
    }
    assert_eq!(InspectorView::from_key("bogus"), None);
    // 两档的键两两不同（同一个下拉里不能有两个同值项）。
    let keys: std::collections::BTreeSet<&str> =
        InspectorView::ALL.iter().map(|view| view.key()).collect();
    assert_eq!(keys.len(), InspectorView::ALL.len());
}

/// 复制一块仪表板：面板原样搬过去（id 另起，免得两块板的面板 id 系列对不上）。
#[test]
fn duplicating_a_dashboard_copies_its_panels() {
    let board = Dashboard {
        id: 1,
        name: "生产监控".to_owned(),
        panels: vec![panel(
            1,
            6,
            DashboardPanelKind::Chart {
                saved_chart_id: "chart-1".into(),
            },
        )],
    };
    let copy = board.duplicate(9, "生产监控 副本".to_owned());
    assert_eq!(copy.id, 9);
    assert_eq!(copy.name, "生产监控 副本");
    assert_eq!(copy.panels.len(), 1);
    assert_eq!(copy.panels[0].id, 1);
    assert_eq!(copy.panels[0].kind, board.panels[0].kind);
}

/// 造一块面板（测试排版用；`kind` 直接给，省得写一串字段名）。
fn panel(id: u32, span: u32, kind: DashboardPanelKind) -> DashboardPanel {
    DashboardPanel { id, span, kind }
}

// ── 连接钩子（#19） ──────────────────────────────────────────────

/// 视图类测试的前置：按 `main.rs` 的顺序装组件与主题全局。
///
/// `InputState::new` 会读主题（占位符配色 / 字阶），没装全局就直接 panic
/// （报的是「no state of type DuckThemeState / ThemeRegistry exists」，
/// 与钩子本身无关）。
fn init_theme(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        duck_theme::init(cx);
    });
}

/// 载体两档的稳定键两两不同（元素 id 靠它，撞了两个按钮就成了同一个）。
#[test]
fn hook_mode_keys_are_distinct() {
    let keys: std::collections::BTreeSet<&str> = [HookMode::Command, HookMode::Script]
        .iter()
        .map(|mode| mode.key())
        .collect();
    assert_eq!(keys.len(), 2);
    assert_eq!(HookMode::Command.key(), "command");
    assert_eq!(HookMode::Script.key(), "script");
}

/// 新建一行钩子的默认值是**明确**的：命令档 / 建连前 / 启用 / 不阻断 / 不单独设超时。
///
/// 这四条合起来决定了「用户刚加一条钩子、还没填命令」时的语义，任何一条漂了都会
/// 变成「刚加就挡住建连」这类意外。
#[gpui::test]
fn a_new_hook_row_defaults_to_an_enabled_pre_connect_command(cx: &mut TestAppContext) {
    init_theme(cx);
    cx.add_window(|window, cx| {
        let row = cx.new(|cx| HookRow::new("hook-1".to_owned(), None, window, cx));
        let hook = row.read(cx).to_hook(cx);
        assert_eq!(hook.id, "hook-1");
        assert_eq!(hook.phase, HookPhase::PreConnect);
        assert!(hook.enabled);
        assert_eq!(
            hook.failure,
            HookFailure::Warn,
            "新建的钩子不该默认阻断建连"
        );
        assert_eq!(hook.timeout_secs, None);
        assert!(matches!(hook.action, HookAction::Command { .. }));
        // 空命令 = 没填完；只有**启用**的才拦（停用那行留空不该挡住保存）。
        assert!(row.read(cx).is_incomplete(cx));
        let row_entity = row.clone();
        row_entity.update(cx, |row, cx| {
            row.enabled = false;
            cx.notify();
        });
        assert!(!row.read(cx).is_incomplete(cx));
        SqlEditorProbe
    });
}

/// 脚本档读回的是「解释器 + 文件」，并且**两端空白都去掉**、空的工作目录变 `None`。
#[gpui::test]
fn script_hook_rows_read_back_into_core_scripts(cx: &mut TestAppContext) {
    init_theme(cx);
    cx.add_window(|window, cx| {
        let row = cx.new(|cx| HookRow::new("hook-2".to_owned(), None, window, cx));
        let (interpreter, path, cwd, timeout) = {
            let row = row.read(cx);
            (
                row.interpreter.clone(),
                row.path.clone(),
                row.cwd.clone(),
                row.timeout.clone(),
            )
        };
        for (state, value) in [
            (&interpreter, "  bash  "),
            (&path, " /tmp/hook.sh "),
            // 只填空格的工作目录等于没填（`current_dir("")` 会直接失败）。
            (&cwd, "   "),
            (&timeout, "5"),
        ] {
            state.update(cx, |state, cx| state.set_value(value, window, cx));
        }
        row.update(cx, |row, _| row.mode = HookMode::Script);
        let hook = row.read(cx).to_hook(cx);
        match &hook.action {
            HookAction::Script {
                interpreter,
                path,
                cwd,
            } => {
                assert_eq!(interpreter, "bash");
                assert_eq!(path, "/tmp/hook.sh");
                assert_eq!(*cwd, None);
            }
            other => panic!("脚本档读回的不是脚本：{other:?}"),
        }
        assert_eq!(hook.timeout_secs, Some(5));
        assert!(!row.read(cx).is_incomplete(cx), "填全了就不该算没填完");

        // 解析不出数字的超时按「没填」处理，不卡住保存。
        timeout.update(cx, |state, cx| state.set_value("很久", window, cx));
        assert_eq!(row.read(cx).to_hook(cx).timeout_secs, None);
        // 解释器清空 → 又算没填完。
        interpreter.update(cx, |state, cx| state.set_value("  ", window, cx));
        assert!(row.read(cx).is_incomplete(cx));
        SqlEditorProbe
    });
}

/// 已有档案填充表单时，相位 / 载体 / 失败策略 / 超时都要原样回来（编辑不该悄悄改配置）。
#[gpui::test]
fn an_existing_hook_fills_the_row_verbatim(cx: &mut TestAppContext) {
    init_theme(cx);
    cx.add_window(|window, cx| {
        let existing = ConnectionHook {
            id: "hook-7".to_owned(),
            phase: HookPhase::PostDisconnect,
            enabled: false,
            action: HookAction::Script {
                interpreter: "python3".to_owned(),
                path: "/tmp/bye.py".to_owned(),
                cwd: Some("/tmp".to_owned()),
            },
            failure: HookFailure::Block,
            timeout_secs: Some(12),
        };
        let row = cx.new(|cx| HookRow::new("hook-7".to_owned(), Some(&existing), window, cx));
        assert_eq!(row.read(cx).to_hook(cx), existing);
        // 停用的行不被「没填完」检查拦下（它有内容，但语义上本就不跑）。
        assert!(!row.read(cx).is_incomplete(cx));
        SqlEditorProbe
    });
}

/// 造一份只填了主机的代理档。
fn test_proxy_at(host: &str) -> ProxyConfig {
    ProxyConfig {
        kind: ProxyKind::Socks5,
        host: host.into(),
        port: None,
        username: String::new(),
        password: None,
    }
}

/// 造一份只填了实例 id 的 SSM 档。
fn test_ssm_at(instance: &str) -> SsmConfig {
    SsmConfig {
        instance: instance.into(),
        remote_host: None,
        region: None,
        profile: None,
    }
}

/// 四档传输的中文名两两不同（下拉标签与状态卡都读它，撞名就分不清在走哪条路）。
#[gpui::test]
fn transport_labels_are_distinct(cx: &mut TestAppContext) {
    init_theme(cx);
    cx.update(|cx| {
        let labels: std::collections::BTreeSet<&str> = [
            TransportKind::Direct,
            TransportKind::Ssh,
            TransportKind::Proxy,
            TransportKind::Ssm,
        ]
        .iter()
        .map(|kind| transport_kind_label(*kind, cx))
        .collect();
        assert_eq!(labels.len(), 4, "四档标签不能有重名：{labels:?}");
    });
}

/// 档案「是哪一档」的判据与内核同源：**填不全的档位不算数**，且优先级一致。
#[test]
fn a_profile_reports_only_fully_filled_transports() {
    assert_eq!(TunnelProfile::default().kind(), None);

    let mut profile = TunnelProfile {
        id: "tunnel-1".into(),
        name: "档案".into(),
        ssh: Some(SshTunnelConfig {
            host: "  ".into(),
            user: "duck".into(),
            ..SshTunnelConfig::default()
        }),
        ..TunnelProfile::default()
    };
    assert_eq!(profile.kind(), None, "主机空白 = 没填完，不该说它是 SSH");

    profile.ssh = Some(SshTunnelConfig {
        host: "bastion".into(),
        user: "duck".into(),
        ..SshTunnelConfig::default()
    });
    assert_eq!(profile.kind(), Some(TransportKind::Ssh));

    profile.proxy = Some(test_proxy_at("proxy.local"));
    assert_eq!(
        profile.kind(),
        Some(TransportKind::Ssh),
        "SSH 优先于代理（与内核 transport() 同一顺序）"
    );

    profile.ssm = Some(test_ssm_at("i-0123456789abcdef0"));
    assert_eq!(profile.kind(), Some(TransportKind::Ssm));
}

/// 展开档案：**整体替换**三档；档案不在时回落连接自己的三档（绝不静默直连）。
#[test]
fn a_referenced_profile_replaces_all_three_transports() {
    let mut cfg = ConnectionConfig {
        kind: DatabaseKind::Postgres,
        host: "db.local".into(),
        ssh_tunnel: Some(SshTunnelConfig {
            host: "own-bastion".into(),
            user: "duck".into(),
            ..SshTunnelConfig::default()
        }),
        ..ConnectionConfig::default()
    };
    // 没引用档案：原样返回，`hit` 为空。
    let (out, hit) = expand_tunnel_profile(&cfg, &[]);
    assert!(hit.is_none());
    assert_eq!(out.transport(), TransportKind::Ssh);

    // 引用的档案不存在：**回落连接自己的三档**，而不是变成直连。
    cfg.tunnel_profile = Some("tunnel-missing".into());
    let (out, hit) = expand_tunnel_profile(&cfg, &[]);
    assert!(hit.is_none());
    assert_eq!(out.transport(), TransportKind::Ssh);
    assert!(out.ssh_tunnel.is_some());

    // 命中档案：三档整体换成档案的——连接自己的 SSH 必须让位。
    cfg.tunnel_profile = Some("tunnel-1".into());
    let profile = TunnelProfile {
        id: "tunnel-1".into(),
        name: "公司代理".into(),
        proxy: Some(test_proxy_at("proxy.corp")),
        ..TunnelProfile::default()
    };
    let (out, hit) = expand_tunnel_profile(&cfg, std::slice::from_ref(&profile));
    assert_eq!(hit.map(|hit| hit.id.as_str()), Some("tunnel-1"));
    assert!(
        out.ssh_tunnel.is_none(),
        "档案里没有 SSH，连接上那条不能留下（否则两档都成立、只有一档生效）"
    );
    assert_eq!(out.transport(), TransportKind::Proxy);
    // 引用本身留在展开结果上，保存时才知道该写回哪份档案。
    assert_eq!(out.tunnel_profile.as_deref(), Some("tunnel-1"));
}

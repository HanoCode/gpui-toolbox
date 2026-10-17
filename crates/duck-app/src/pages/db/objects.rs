//! 模式对象的展示与语句骨架：目录对象的图标 / 中文名、引用骨架、引擎是否支持 DDL、
//! 列类型占位与「建表 / 改表 / 删表」模板、对象名匹配。
//!
//! 结构管理走「生成 DDL → 填进编辑器 → 用户确认后执行」这一条统一路径，所以这里给的
//! 都是**骨架**（`CREATE` 给全、`ALTER` 只给一条 `ADD COLUMN`、`DROP` 就一行）；界面
//! 猜一种写法而已，语法正确比「猜得完整」重要。

use super::*;

/// 表 / 视图的小图标。
pub(super) fn table_kind_icon(kind: TableKind) -> DuckIcon {
    match kind {
        TableKind::Table => DuckIcon::Columns2,
        TableKind::View => DuckIcon::Layers,
        // 存储过程 / 函数 / 序列：这三类对象**不能当表浏览**（见 [`TableKind::is_relation`]），
        // 树里只给出图标与名字，点开走 «生成引用骨架» 那条路。
        TableKind::Procedure => DuckIcon::SquareTerminal,
        TableKind::Function => DuckIcon::SquareFunction,
        TableKind::Sequence => DuckIcon::ListOrdered,
    }
}

/// 目录对象种类的中文名（左栏 tooltip / 补全详情共用）。
pub(super) fn table_kind_label(kind: TableKind, cx: &App) -> String {
    match kind {
        TableKind::Table => tr(cx, "表"),
        TableKind::View => tr(cx, "视图"),
        TableKind::Procedure => tr(cx, "存储过程"),
        TableKind::Function => tr(cx, "函数"),
        TableKind::Sequence => tr(cx, "序列"),
    }
    .to_string()
}

/// 存储过程 / 函数 / 序列点开时放进编辑器的**引用骨架**。
///
/// 这几类对象不能像表那样翻页读，但也不能「点了没反应」——它们各自有唯一一种
/// 在 SQL 里被引用的写法，直接给出来最省事（名字已按引擎的限定名规则拼好）。
pub(super) fn object_snippet(kind: TableKind, engine: DatabaseKind, name: &str) -> String {
    match kind {
        TableKind::Table | TableKind::View => format!("SELECT * FROM {name} LIMIT 100;"),
        // 存储过程一律按过程调用（`CALL`）；函数按表函数（`SELECT * FROM fn()`），
        // SQL Server 的标量函数要 `SELECT dbo.fn()` 才跑得起来，这里按最通用的写法给。
        TableKind::Procedure => format!("CALL {name}();"),
        TableKind::Function => match engine {
            DatabaseKind::Mysql => format!("SELECT {name}();"),
            _ => format!("SELECT * FROM {name}();"),
        },
        // 序列：取号函数各引擎不同名 —— PG 是 `nextval('序列名')`，
        // SQL Server 是标准 SQL 的 `NEXT VALUE FOR 序列名`（没有 nextval）。
        TableKind::Sequence => match engine {
            DatabaseKind::SqlServer => format!("SELECT NEXT VALUE FOR {name};"),
            _ => format!("SELECT nextval('{name}');"),
        },
    }
}

/// 引擎有没有 SQL 的 DDL 这一层（建表 / 改表 / 删表），**按本仓写得出模板的引擎**回答。
///
/// 判据是 [`DatabaseKind::identifier_quote`] 有没有值 —— MongoDB 走 JSON 命令、
/// Elasticsearch 走 REST 映射，都没有「带引号的表名」，结构管理对它们**不给入口**
/// （菜单项隐藏），而不是给了再报错：「不支持」与「执行失败」在界面上必须分得开。
///
/// ⚠️ **agent 档返回 `false`，但那不是「它没有 DDL」** —— Oracle / 达梦 / DB2 当然有。
/// 返回 `false` 是因为**我们生成不出对的语句**：这一档的模板函数（[`column_type_placeholders`]）
/// 要逐引擎知道类型名（Oracle `NUMBER(19)`、Hive `STRING`、DB2 `BIGINT`、ClickHouse `Int64`…），
/// 而当前驱动清单里**没有方言字段**。与其给 40 多个引擎一条多数跑不通的 `CREATE TABLE`，
/// 不如不给模板 —— 用户照样能在编辑器里自己写（那里有补全）。
///
/// 要把它打开，正确做法是**往 `duck-drivers` 的生成清单里加方言字段**（跟着 dbx 走），
/// 而不是在这里按 kind 猜。判据是 kind 而不是会话引号，也是同一个理由：这里回答的是
/// 「我们**写得出**模板吗」，不是「这条连接接不接 SQL」（后者看 `Session::quote`）。
pub(super) fn supports_ddl(engine: DatabaseKind) -> bool {
    engine.identifier_quote().is_some()
}

/// 该引擎的列类型占位（`(主键列, 普通列)`）；`None` = 拿不到可用类型名。
///
/// ⚠️ agent 档返回 `None`（理由见 [`supports_ddl`]）。它与引号那张表**在 agent 档上
/// 故意分叉** —— 单测钉住的「两集合一致」只覆盖内建七档。
pub(super) fn column_type_placeholders(
    engine: DatabaseKind,
) -> Option<(&'static str, &'static str)> {
    Some(match engine {
        DatabaseKind::Sqlite => ("INTEGER", "TEXT"),
        DatabaseKind::Postgres => ("serial", "text"),
        DatabaseKind::Mysql => ("BIGINT", "VARCHAR(255)"),
        DatabaseKind::ClickHouse => ("Int64", "String"),
        DatabaseKind::SqlServer => ("BIGINT", "NVARCHAR(255)"),
        DatabaseKind::Mongo | DatabaseKind::Elasticsearch => return None,
        DatabaseKind::Agent => return None,
    })
}

/// 建表语句骨架（`qualified` 已按引擎规则拼好限定名）。
///
/// ⚠️ 已随「新建表」改道结构编辑器（dbx 同款的可视化建表标签）而删除——
/// 骨架只服务过那两个入口；类型占位表 [`column_type_placeholders`] 仍在
/// `supports_ddl` 判据与单测里服役。
///
/// 改表语句骨架：给一条能直接跑的 `ADD COLUMN` 起点。
///
/// 不生成「完整的」`ALTER TABLE`：改结构有类型 / 默认值 / 约束 / 重整表几十种分支，
/// 界面猜一种而已。一条**语法正确**的骨架 + 编辑器，比一条猜错的完整语句好。
pub(super) fn alter_table_template(engine: DatabaseKind, qualified: &str) -> Option<String> {
    let (_, name_type) = column_type_placeholders(engine)?;
    Some(format!(
        "ALTER TABLE {qualified} ADD COLUMN new_column {name_type};"
    ))
}

/// 删表语句骨架。**破坏性**：只生成语句，跑不跑由用户在编辑器里点「执行」决定。
pub(super) fn drop_table_template(engine: DatabaseKind, qualified: &str) -> Option<String> {
    // 判据与建表同源（`column_type_placeholders`），两者一致由单测钉住 ——
    // 「引号有没有」与「类型表有没有」这两个集合将来不许分叉。
    let _ = column_type_placeholders(engine)?;
    Some(format!("DROP TABLE IF EXISTS {qualified};"))
}

/// 对象树筛选：`needle` 由调用点**先小写化**，名字按子串匹配、忽略大小写。
///
/// 空 `needle` 一律通过 —— 「没填筛选词」不等于「筛掉全部」。
pub(super) fn object_matches(name: &str, needle: &str) -> bool {
    needle.is_empty() || name.to_lowercase().contains(needle)
}

/// 目录对象（视图 / 过程 / 函数）的**定义原文查询**：返回一条可以立刻执行的 SQL，
/// 结果第一行里就带着定义文本。各引擎结果列名 / 列序不同，取值不按列位猜 ——
/// 取该行**最长的单元格**（定义体总是那一行里最长的文本）。序列没有定义出口
/// （PG 也没有一条干净的取回语句），返回 `None`；不支持 SQL 的引擎同样 `None`。
pub(super) fn definition_query(
    kind: TableKind,
    engine: DatabaseKind,
    schema: &str,
    name: &str,
) -> Option<String> {
    // 名字进字面量前先转义单引号（与 CSV 导入同一口径），不给注入留缝。
    let name = name.replace('\'', "''");
    let schema = schema.replace('\'', "''");
    Some(match (kind, engine) {
        // 视图：MySQL / ClickHouse 直接 SHOW CREATE；PG 走 pg_get_viewdef；
        // SQLite 的定义原文就躺在 sqlite_master；SQL Server 用 OBJECT_DEFINITION。
        (TableKind::View, DatabaseKind::Mysql) => format!("SHOW CREATE VIEW `{name}`"),
        (TableKind::View, DatabaseKind::ClickHouse) => format!("SHOW CREATE TABLE `{name}`"),
        (TableKind::View, DatabaseKind::Postgres) => {
            format!(
                "SELECT pg_get_viewdef(c.oid, true) FROM pg_class c \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE c.relname = '{name}' AND n.nspname = '{schema}'"
            )
        }
        (TableKind::View, DatabaseKind::Sqlite) => {
            format!("SELECT sql FROM sqlite_master WHERE name = '{name}'")
        }
        (TableKind::View, DatabaseKind::SqlServer) => {
            format!("SELECT OBJECT_DEFINITION(OBJECT_ID('{schema}.{name}'))")
        }
        // 过程 / 函数：MySQL 的 SHOW CREATE 按种类分两条；PG 有统一的
        // pg_get_functiondef；SQL Server 的 OBJECT_DEFINITION 两者通吃。
        (TableKind::Procedure, DatabaseKind::Mysql) => format!("SHOW CREATE PROCEDURE `{name}`"),
        (TableKind::Function, DatabaseKind::Mysql) => format!("SHOW CREATE FUNCTION `{name}`"),
        (TableKind::Procedure | TableKind::Function, DatabaseKind::Postgres) => {
            format!(
                "SELECT pg_get_functiondef(p.oid) FROM pg_proc p \
                 JOIN pg_namespace n ON n.oid = p.pronamespace \
                 WHERE p.proname = '{name}' AND n.nspname = '{schema}'"
            )
        }
        (TableKind::Procedure | TableKind::Function, DatabaseKind::SqlServer) => {
            format!("SELECT OBJECT_DEFINITION(OBJECT_ID('{schema}.{name}'))")
        }
        _ => return None,
    })
}

/// 库级对象（触发器 / 事件 / 索引）的定义原文查询，口径同 [`definition_query`]。
pub(super) fn db_object_definition_query(
    category: TreeCategory,
    engine: DatabaseKind,
    schema: &str,
    name: &str,
) -> Option<String> {
    let name = name.replace('\'', "''");
    let schema = schema.replace('\'', "''");
    Some(match (category, engine) {
        (TreeCategory::Trigger, DatabaseKind::Mysql) => format!("SHOW CREATE TRIGGER `{name}`"),
        (TreeCategory::Trigger, DatabaseKind::Postgres) => {
            format!(
                "SELECT pg_get_triggerdef(t.oid) FROM pg_trigger t \
                 JOIN pg_class c ON c.oid = t.tgrelid \
                 JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE t.tgname = '{name}' AND n.nspname = '{schema}'"
            )
        }
        (TreeCategory::Trigger, DatabaseKind::Sqlite) => {
            format!("SELECT sql FROM sqlite_master WHERE name = '{name}'")
        }
        (TreeCategory::Trigger, DatabaseKind::SqlServer) => {
            format!("SELECT OBJECT_DEFINITION(OBJECT_ID('{schema}.{name}'))")
        }
        (TreeCategory::Event, DatabaseKind::Mysql) => format!("SHOW CREATE EVENT `{name}`"),
        (TreeCategory::Index, DatabaseKind::Postgres) => {
            format!(
                "SELECT indexdef FROM pg_indexes \
                 WHERE indexname = '{name}' AND schemaname = '{schema}'"
            )
        }
        (TreeCategory::Index, DatabaseKind::Sqlite) => {
            format!("SELECT sql FROM sqlite_master WHERE name = '{name}'")
        }
        _ => return None,
    })
}

/// 目录对象（视图 / 过程 / 函数 / 序列）的删除语句骨架。**破坏性**：只生成语句，
/// 跑不跑由用户在编辑器里决定。表走 [`drop_table_template`]（那里有引擎差异），
/// 这里对表返回 `None`。
///
/// 「不给无 SQL 引擎」这道闸**不在这里**：调用点要先拿引号去拼限定名
/// （`quote_qualified`），拿不到引号就到不了这一句。少一次 `supports_ddl` 判断，
/// 也顺带让 agent 档（`supports_ddl` 为 `false`）保住了这条**通用**的 `DROP`——
/// `DROP VIEW "v";` 对 Oracle / 达梦 / DB2 / Hive 都是对的，没必要跟着模板一起关掉。
pub(super) fn drop_catalog_sql(kind: TableKind, qualified: &str) -> Option<String> {
    let keyword = match kind {
        TableKind::View => "VIEW",
        TableKind::Procedure => "PROCEDURE",
        TableKind::Function => "FUNCTION",
        TableKind::Sequence => "SEQUENCE",
        TableKind::Table => return None,
    };
    Some(format!("DROP {keyword} IF EXISTS {qualified};"))
}

/// 库级对象（触发器 / 事件 / 索引）的删除语句骨架，口径同 [`drop_catalog_sql`]。
///
/// `table` 是索引挂在哪张表：MySQL 的 `DROP INDEX` 必须带 `ON 表名`，给不出表名
/// （detail 缺失）就不给这项，而不是生成一条必错的语句。
pub(super) fn drop_db_object_sql(
    category: TreeCategory,
    engine: DatabaseKind,
    name: &str,
    table: Option<&str>,
) -> Option<String> {
    let quote = engine.identifier_quote()?;
    let quoted = duck_db::drivers::quote_identifier(name, quote).ok()?;
    match category {
        TreeCategory::Trigger => Some(format!("DROP TRIGGER IF EXISTS {quoted};")),
        TreeCategory::Event => Some(format!("DROP EVENT IF EXISTS {quoted};")),
        TreeCategory::Index => {
            let table = table?;
            let quoted_table = duck_db::drivers::quote_identifier(table, quote).ok()?;
            match engine {
                DatabaseKind::Mysql => Some(format!("DROP INDEX {quoted} ON {quoted_table};")),
                DatabaseKind::SqlServer => Some(format!("DROP INDEX {quoted} ON {quoted_table};")),
                _ => Some(format!("DROP INDEX IF EXISTS {quoted};")),
            }
        }
        _ => None,
    }
}

/// 分类级「新建 …」骨架：视图在 browse.rs 里内联拼（表已改道结构编辑器的
/// 建表标签），这里补其余七类程序 / 取号 / 权限对象的**语法正确**起点。引擎给不出
///（SQLite / ClickHouse 没有用户体系、ClickHouse 没有触发器、事件是 MySQL 独有、
/// 序列只有 PG / SQL Server 有）返回 `None`，菜单不给项。
pub(super) fn create_category_skeleton(
    category: TreeCategory,
    engine: DatabaseKind,
) -> Option<String> {
    if !supports_ddl(engine) {
        return None;
    }
    Some(match category {
        TreeCategory::Index => "CREATE INDEX idx_new_index ON 表名 (列名);".to_owned(),
        TreeCategory::Procedure => match engine {
            DatabaseKind::Postgres => {
                "CREATE PROCEDURE new_procedure()\nLANGUAGE plpgsql\nAS $$\nBEGIN\n    -- 过程体\nEND\n$$;"
                    .to_owned()
            }
            DatabaseKind::SqlServer => {
                "CREATE PROCEDURE new_procedure\nAS\nBEGIN\n    -- 过程体\nEND;".to_owned()
            }
            _ => "CREATE PROCEDURE new_procedure()\nBEGIN\n    -- 过程体\nEND;".to_owned(),
        },
        TreeCategory::Function => match engine {
            DatabaseKind::Postgres => {
                "CREATE FUNCTION new_function() RETURNS integer\nLANGUAGE plpgsql\nAS $$\nBEGIN\n    RETURN 0;\nEND\n$$;"
                    .to_owned()
            }
            DatabaseKind::SqlServer => {
                "CREATE FUNCTION new_function()\nRETURNS INT\nAS\nBEGIN\n    RETURN 0;\nEND;"
                    .to_owned()
            }
            _ => "CREATE FUNCTION new_function() RETURNS INT\nBEGIN\n    RETURN 0;\nEND;"
                .to_owned(),
        },
        TreeCategory::Trigger => {
            "CREATE TRIGGER new_trigger\nAFTER INSERT ON 表名\nFOR EACH ROW\nBEGIN\n    -- 触发体\nEND;"
                .to_owned()
        }
        // 事件是 MySQL 独有的调度对象。
        TreeCategory::Event if engine == DatabaseKind::Mysql => {
            "CREATE EVENT new_event\nON SCHEDULE EVERY 1 DAY\nDO\n    -- 事件体".to_owned()
        }
        // 序列：PG 与 SQL Server 的取号器（`START WITH` / `INCREMENT BY` 两边同语法）。
        TreeCategory::Sequence => {
            "CREATE SEQUENCE new_sequence\nSTART WITH 1\nINCREMENT BY 1;".to_owned()
        }
        // 用户体系：MySQL 账号 = 'user'@'host'；PG 一条 CREATE USER；
        // SQL Server 登录名与库用户要分开建。
        TreeCategory::User => match engine {
            DatabaseKind::Mysql => "CREATE USER 'new_user'@'localhost' IDENTIFIED BY '密码';"
                .to_owned(),
            DatabaseKind::Postgres => "CREATE USER new_user WITH PASSWORD '密码';".to_owned(),
            DatabaseKind::SqlServer => {
                "CREATE LOGIN new_user WITH PASSWORD = '密码';\nCREATE USER new_user FOR LOGIN new_user;"
                    .to_owned()
            }
            _ => return None,
        },
        _ => return None,
    })
}

/// 序列当前值设置（PG 独有，序列本来就只有 PG 有）：`setval` 第三参 `true`
/// 表示下一个 `nextval` 返回 `next + 1`（与「设置后的下一个值」语义一致）。
pub(super) fn sequence_setval_sql(schema: &str, name: &str, next: u64) -> String {
    let qualified = if schema.is_empty() {
        name.to_owned()
    } else {
        format!("{schema}.{name}")
    };
    format!("SELECT setval('{qualified}', {next}, true);")
}

/// 用户权限查询：执行即得该用户的授权清单。MySQL 用 `SHOW GRANTS`（账号由
/// 'user'@'host' 共同决定，`host` 来自用户行的说明列）；PG 查 `role_table_grants`。
/// 其余引擎没有对应的元数据出口，返回 `None`。
pub(super) fn user_grants_sql(
    engine: DatabaseKind,
    name: &str,
    host: Option<&str>,
) -> Option<String> {
    let name = name.replace('\'', "''");
    Some(match engine {
        DatabaseKind::Mysql => {
            let host = host.unwrap_or("%").replace('\'', "''");
            format!("SHOW GRANTS FOR '{name}'@'{host}'")
        }
        DatabaseKind::Postgres => {
            format!(
                "SELECT grantee, privilege_type, table_schema, table_name \
                 FROM information_schema.role_table_grants \
                 WHERE grantee = '{name}' \
                 ORDER BY table_schema, table_name, privilege_type"
            )
        }
        _ => return None,
    })
}

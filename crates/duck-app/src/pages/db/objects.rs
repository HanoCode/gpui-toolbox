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
        // 序列只有 PostgreSQL 有（`nextval` 是它的取号函数）。
        TableKind::Sequence => format!("SELECT nextval('{name}');"),
    }
}

/// 引擎有没有 SQL 的 DDL 这一层（建表 / 改表 / 删表）。
///
/// 判据只有一条：引擎有没有 SQL 标识符概念（[`DatabaseKind::identifier_quote`]）。
/// MongoDB 走 JSON 命令、Elasticsearch 走 REST 映射，都没有「带引号的表名」，
/// 结构管理对它们**不给入口**（菜单项禁用），而不是给了再报错：
/// 「不支持」与「执行失败」在界面上必须分得开。
pub(super) fn supports_ddl(engine: DatabaseKind) -> bool {
    engine.identifier_quote().is_some()
}

/// 该引擎的列类型占位（`(主键列, 普通列)`）；`None` = 没有 SQL 类型系统。
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
    })
}

/// 建表语句骨架（`qualified` 已按引擎规则拼好限定名）。
///
/// 结构管理走「生成 DDL → 填进编辑器 → 用户确认后执行」这一条统一路径，**不做**
/// 可视化建表向导：列类型 / 约束在真实项目里千变万化，表单化编辑器只会把「能表达
/// 的形状」做成上限（比要建的表更简单都做不到）；而在编辑器里改一行比在表单里
/// 找那个字段快得多。界面负责给一个**语法正确**的起点。
pub(super) fn create_table_template(engine: DatabaseKind, qualified: &str) -> Option<String> {
    let (id_type, name_type) = column_type_placeholders(engine)?;
    Some(format!(
        "CREATE TABLE {qualified} (\n    id {id_type} NOT NULL,\n    name {name_type},\n    PRIMARY KEY (id)\n);"
    ))
}

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

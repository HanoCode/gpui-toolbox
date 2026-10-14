//! SQL 编辑器的补全：关键字 / 函数 / 类型来自 `duck_db::sqlwords`，表名与本连接内省到的
//! 列名来自调用方喂进来的 `catalog`（本模块**不碰会话**，所以可单测）。
//!
//! 类型定义仍在 `mod.rs`；这里是它的两个 `impl` 块（固有方法 + `CompletionProvider`）。

use super::*;

impl SqlCompletionProvider {
    pub(super) fn new(view: WeakEntity<DatabasePage>) -> Self {
        Self {
            view,
            columns: Rc::new(std::cell::RefCell::new(HashMap::new())),
        }
    }

    /// 表名补全候选（含视图）。
    ///
    /// 收「目录切片」而非 `&DatabasePage`：调用点在 `completions` 里只能对页面持
    /// 临时不可变借用，而本函数还要用 `cx` 取译文——持借用会让可变 / 不可变借用
    /// 撞车。目录先按值取一份（≤200 项、两个字段），借用随即释放。
    pub(super) fn table_items(
        &self,
        catalog: &[CatalogTable],
        query: &str,
        cx: &App,
    ) -> Vec<CompletionItem> {
        catalog
            .iter()
            .filter(|table| matches_query(&table.name, query))
            .take(COMPLETION_LIMIT)
            .map(|table| CompletionItem {
                label: table.name.clone(),
                kind: Some(CompletionItemKind::STRUCT),
                detail: Some(table_kind_label(table.kind, cx)),
                ..Default::default()
            })
            .collect()
    }

    /// 取（必要时后台查）点号前那张表的列。
    pub(super) fn column_completions(
        &self,
        table: &str,
        query: &str,
        cx: &App,
    ) -> Task<anyhow::Result<CompletionResponse>> {
        let table_key = table.to_lowercase();
        if let Some(columns) = self.columns.borrow().get(&table_key) {
            return Task::ready(Ok(CompletionResponse::Array(
                columns
                    .iter()
                    .filter(|column| matches_query(&column.name, query))
                    .take(COMPLETION_LIMIT)
                    .map(|column| CompletionItem {
                        label: column.name.clone(),
                        kind: Some(CompletionItemKind::FIELD),
                        detail: Some(column.type_name.clone()),
                        ..Default::default()
                    })
                    .collect(),
            )));
        }
        let Some(page) = self.view.upgrade() else {
            return Task::ready(Ok(CompletionResponse::Array(Vec::new())));
        };
        let Some(conn) = page.read(cx).active_conn() else {
            return Task::ready(Ok(CompletionResponse::Array(Vec::new())));
        };
        let table_owned = table.to_owned();
        let query_owned = query.to_owned();
        let cache = self.columns.clone();
        let fetch = cx.background_executor().spawn(async move {
            let mut guard = conn.lock().expect("连接锁不应中毒");
            // 补全拿到的只有「用户敲的那段名字」，没有 schema 上下文 → 裸名引用，
            // 让引擎按自己的 search_path / 当前库解析。
            guard.table_columns(&TableRef::bare(table_owned))
        });
        cx.spawn(async move |_| {
            let columns = fetch.await.map_err(|error| anyhow!(error.to_string()))?;
            let items: Vec<CompletionItem> = columns
                .iter()
                .filter(|column| matches_query(&column.name, &query_owned))
                .take(COMPLETION_LIMIT)
                .map(|column| CompletionItem {
                    label: column.name.clone(),
                    kind: Some(CompletionItemKind::FIELD),
                    detail: Some(column.type_name.clone()),
                    ..Default::default()
                })
                .collect();
            cache.borrow_mut().insert(table_key, columns);
            Ok(CompletionResponse::Array(items))
        })
    }
}

impl CompletionProvider for SqlCompletionProvider {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<CompletionResponse>> {
        let query = trigger.trigger_character.unwrap_or_default();
        let Some(page) = self.view.upgrade() else {
            return Task::ready(Ok(CompletionResponse::Array(Vec::new())));
        };

        // 光标前的词：标识符字符与点号。带点号说明在写 `table.`，转列补全。
        let prefix = text.to_string();
        let word_before = prefix
            .get(..offset.min(prefix.len()))
            .unwrap_or_default()
            .chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '.')
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();

        if let Some((table, column_query)) = word_before.rsplit_once('.') {
            if !table.is_empty() {
                return self.column_completions(table, column_query, cx);
            }
            return Task::ready(Ok(CompletionResponse::Array(Vec::new())));
        }

        // 目录与引擎都按值取一份再释放借用，避免与随后用 `cx` 取译文撞借用。
        let (catalog, kind): (Vec<CatalogTable>, Option<DatabaseKind>) = {
            let page = page.read(cx);
            let session = page.active_session();
            (
                session
                    .map(|session| session.catalog.clone())
                    .unwrap_or_default(),
                session.map(|session| session.kind),
            )
        };
        // 词表按**当前引擎**给：通用核心 + 该引擎的增量（`duck_db::sqlwords`）。
        // 还没连上（没有活跃会话）时只给通用核心 —— 那时补的是「SQL 长什么样」，
        // 不该替某个引擎说话：在 PostgreSQL 里补出 `AUTOINCREMENT`、在 SQLite 里
        // 补出 `JSONB`，都是把人往执行失败的方向推。
        let (core, extra) = match kind {
            Some(kind) => duck_db::sqlwords::words(kind),
            None => (
                duck_db::sqlwords::common_words(),
                SqlWords {
                    keywords: &[],
                    functions: &[],
                },
            ),
        };
        // 非 SQL 引擎的「函数」桶装的是**命令名**（MongoDB 的 `find` / `aggregate`），
        // 分类文案跟着换 —— 把命令叫成函数是错的。
        let function_label = match kind.map_or(QueryLanguage::Sql, duck_db::sqlwords::language) {
            QueryLanguage::Sql | QueryLanguage::Elasticsearch => tr(cx, "函数"),
            QueryLanguage::Mongo => tr(cx, "命令名"),
        };

        let mut items: Vec<CompletionItem> = self.table_items(&catalog, &query, cx);
        items.extend(
            core.keywords
                .iter()
                .chain(extra.keywords)
                .filter(|word| matches_query(word, &query))
                .take(COMPLETION_LIMIT)
                .map(|word| CompletionItem {
                    label: (*word).to_string(),
                    kind: Some(CompletionItemKind::KEYWORD),
                    detail: Some(tr(cx, "关键字").to_string()),
                    ..Default::default()
                }),
        );
        items.extend(
            core.functions
                .iter()
                .chain(extra.functions)
                .filter(|word| matches_query(word, &query))
                .take(COMPLETION_LIMIT)
                .map(|word| CompletionItem {
                    label: (*word).to_string(),
                    kind: Some(CompletionItemKind::FUNCTION),
                    detail: Some(function_label.to_string()),
                    ..Default::default()
                }),
        );
        items.truncate(COMPLETION_TOTAL_LIMIT);
        Task::ready(Ok(CompletionResponse::Array(items)))
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
        // 标识符字符与点号触发；整段粘贴（>2 字符）不打断输入。
        new_text.chars().count() <= 2
            && new_text
                .chars()
                .last()
                .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.')
    }
}

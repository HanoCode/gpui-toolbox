//! 批量执行：一批语句在**同一条连接锁**下一次跑完，逐条回执 + 批级事务 / 取消。
//!
//! ## 为什么单独一层（对照 dbflux / dbx 的结论）
//!
//! 界面层原先的「多条语句」是**逐条排队**：一条语句一个后台任务、一次连接加锁、一个
//! 取消句柄、一个超时预算。dbx / dbflux 的批量路径都反过来做 —— 一批语句共用一次
//! 加锁与一条事务，逐条回执在批内收集。三条实打实的收益：
//!
//! 1. **一次加锁**：批量跑 500 条 INSERT 不再抢 500 次 `Mutex<Connection>`；
//! 2. **一个事务**（[`BatchTransaction::Single`]）：SQLite / PG / MySQL 都是
//!    「事务里提交一次」而不是「每条一次 fsync」——这是导入脚本最大的提速点；
//! 3. **批级取消**：取消句柄与超时预算按**批**算（`docs/database-design.md` §4.4
//!    的读路径口径不变），而不是每条重新计一次。
//!
//! ## 取消为什么要一个谓词而不是 `CancelHandle`
//!
//! [`crate::cancel::CancelHandle`] 只有「送一枪」没有「查状态」（七引擎的带外取消
//! 原语都只有单向信号）。所以「下一条要不要跑」由调用方的谓词回答：界面在
//! 「停止」按钮上同时置自己的原子标志，两者一对就构成「当前这条被引擎打断、后面的
//! 不再跑」的完整语义。
//!
//! ⚠️ 本层**不做**危险闸门也不碰界面：它只按给的语句清单跑，是否要问用户一句由
//! 调用方在进来之前决定（界面走 `DatabasePage::guard_dangerous`）。

use std::time::Instant;

use crate::drivers::Connection;
use crate::error::DbError;
use crate::result::ExecOutcome;
use crate::sqlintel::TxControl;

/// 批量执行的事务口径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchTransaction {
    /// 不开显式事务：按服务器默认逐条提交（SQLite 是每条一个隐式事务）。
    AutoCommit,
    /// 整批包在**一个**显式事务里：要么全成、要么全回滚。
    Single,
}

/// 批量执行的选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SqlBatchOptions {
    /// 事务口径。
    pub transaction: BatchTransaction,
    /// 遇错是否继续。`false` 时第一条失败即停，剩下的标 [`StatementStatus::Skipped`]。
    pub continue_on_error: bool,
    /// 读语句的行上限（与 [`Connection::execute`] 同一个参数）。
    pub max_rows: usize,
}

impl SqlBatchOptions {
    /// 逐条提交、遇错即停 —— 与界面层原先的排队语义一致（前几条已生效）。
    #[must_use]
    pub const fn new(max_rows: usize) -> Self {
        Self {
            transaction: BatchTransaction::AutoCommit,
            continue_on_error: false,
            max_rows,
        }
    }

    /// 改成「整批一个事务」。
    #[must_use]
    pub const fn single_transaction(self) -> Self {
        Self {
            transaction: BatchTransaction::Single,
            ..self
        }
    }

    /// 改成「出错继续跑剩下的」。
    #[must_use]
    pub const fn continue_on_error(self) -> Self {
        Self {
            continue_on_error: true,
            ..self
        }
    }
}

/// 一条语句的结局。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatementStatus {
    /// 跑完了（读语句可能零行、写语句可能零行受影响，都不算失败）。
    Ok,
    /// 报错（引擎原文，本 crate 的错误一律英文）。
    Failed(String),
    /// 没跑：前面已经失败、上游叫停，或它在批尾被跳过。
    Skipped,
}

/// 一条语句的回执。
///
/// ⚠️ 只有 `PartialEq`：回执里装着 [`ExecOutcome`]（结果集里的浮点值不是 `Eq`）。
#[derive(Debug, Clone, PartialEq)]
pub struct StatementOutcome {
    /// 在批次里的序号（与调用方给的清单同序）。
    pub index: usize,
    /// 语句原文（回执里带给界面，用于「Messages」流水与审计摘要）。
    pub sql: String,
    /// 结局。
    pub status: StatementStatus,
    /// 引擎回执（`Ok` 才有；读语句的结果集、写语句的影响行数都在里面）。
    pub outcome: Option<ExecOutcome>,
    /// 这一条的耗时。
    pub elapsed_ms: u128,
}

impl StatementOutcome {
    /// 影响行数（没跑成 / 没有回执都是 0）。
    #[must_use]
    pub fn affected(&self) -> u64 {
        self.outcome.as_ref().map_or(0, |outcome| outcome.affected)
    }

    /// 是否成功。
    #[must_use]
    pub fn is_ok(&self) -> bool {
        matches!(self.status, StatementStatus::Ok)
    }
}

/// 批次的总体回执。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BatchReport {
    /// 逐条回执（与调用方给的清单**等长同序**：没跑的也在，标 `Skipped`）。
    pub statements: Vec<StatementOutcome>,
    /// 真跑了的条数。
    pub executed: usize,
    /// 报错的条数。
    pub failed: usize,
    /// 没跑的条数。
    pub skipped: usize,
    /// 影响行数合计。
    pub affected: u64,
    /// 上游叫停（谓词命中）——剩下的没跑。
    pub stopped: bool,
    /// 引擎报了 [`DbError::Cancelled`]（超时守护或带外取消打在这条语句上）。
    pub cancelled: bool,
    /// 整批耗时。
    pub elapsed_ms: u128,
    /// 事务收尾（`COMMIT` / `ROLLBACK`）报错的原文；只有 [`BatchTransaction::Single`] 会有。
    ///
    /// 单独留一格而不是挂进某条语句：收尾失败说明**事务状态未知**，不是某一条语句的事。
    pub transaction_error: Option<String>,
}

impl BatchReport {
    /// 全部成功（没有任何失败 / 跳过）。
    #[must_use]
    pub fn all_ok(&self) -> bool {
        self.failed == 0 && self.skipped == 0 && self.transaction_error.is_none()
    }

    /// 第一条错误原文（界面报错优先用它）。
    #[must_use]
    pub fn first_error(&self) -> Option<&str> {
        self.statements
            .iter()
            .find_map(|entry| match &entry.status {
                StatementStatus::Failed(message) => Some(message.as_str()),
                _ => None,
            })
    }
}

/// 按选项跑一批语句，逐条回执。
///
/// - `should_stop`：每一条**发起前**问一次；命中即把这条与剩下的全标
///   [`StatementStatus::Skipped`]（正在跑的那条由调用方的 [`crate::cancel::CancelHandle`]
///   负责打断，本层管不到在途语句）。
/// - `on_progress`：每有一条语句落定就回调一次（含 `Skipped`）——界面据此更新
///   「N / 总数」的进度，不必等整批回来。
///
/// 返回 `Err` 只有一种情况：整批事务的 `BEGIN` 就失败了（那时没有任何语句跑过，
/// 也就没有回执可给）。
pub fn execute_batch(
    conn: &mut Connection,
    statements: &[String],
    options: &SqlBatchOptions,
    should_stop: &dyn Fn() -> bool,
    on_progress: &mut dyn FnMut(&StatementOutcome),
) -> Result<BatchReport, DbError> {
    let started = Instant::now();
    let mut report = BatchReport {
        statements: Vec::with_capacity(statements.len()),
        ..BatchReport::default()
    };
    // 整批事务：开在第一条之前、收在最后（收尾见函数末尾）。
    let mut in_transaction = false;
    if options.transaction == BatchTransaction::Single && !statements.is_empty() {
        let begin = TxControl::Begin.statement(conn.kind());
        conn.execute(begin, 1)?;
        in_transaction = true;
    }
    // 「剩下的都不跑了」：遇错即停之后一直为真，后面每条都进 `Skipped` 分支。
    let mut halted = false;
    for (index, sql) in statements.iter().enumerate() {
        let at = Instant::now();
        if halted || should_stop() {
            halted = true;
            report.stopped = true;
            report.skipped += 1;
            let entry = StatementOutcome {
                index,
                sql: sql.clone(),
                status: StatementStatus::Skipped,
                outcome: None,
                elapsed_ms: 0,
            };
            on_progress(&entry);
            report.statements.push(entry);
            continue;
        }
        match conn.execute(sql, options.max_rows) {
            Ok(outcome) => {
                report.affected += outcome.affected;
                report.executed += 1;
                let entry = StatementOutcome {
                    index,
                    sql: sql.clone(),
                    status: StatementStatus::Ok,
                    outcome: Some(outcome),
                    elapsed_ms: at.elapsed().as_millis(),
                };
                on_progress(&entry);
                report.statements.push(entry);
            }
            Err(error) => {
                report.failed += 1;
                // 「取消」与「出错」必须分得开：前者在审计里不是失败，而是「这条语句
                // 被中断过」（排查半截数据的第一手线索）。
                report.cancelled |= matches!(error, DbError::Cancelled(_));
                let entry = StatementOutcome {
                    index,
                    sql: sql.clone(),
                    status: StatementStatus::Failed(error.to_string()),
                    outcome: None,
                    elapsed_ms: at.elapsed().as_millis(),
                };
                on_progress(&entry);
                report.statements.push(entry);
                if !options.continue_on_error {
                    halted = true;
                }
            }
        }
    }
    // 事务收尾：有失败 / 被取消就回滚，全成则提交。
    if in_transaction {
        let control = if report.failed > 0 || report.cancelled || report.stopped {
            TxControl::Rollback
        } else {
            TxControl::Commit
        };
        let statement = control.statement(conn.kind());
        if let Err(error) = conn.execute(statement, 1) {
            report.transaction_error = Some(error.to_string());
        }
    }
    report.elapsed_ms = started.elapsed().as_millis();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConnectionConfig, DatabaseKind};
    use crate::result::Value;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 每个测试一个独立临时库文件（进程 pid + 自增序号，避免并行测试互踩）。
    fn temp_sqlite() -> (Connection, PathBuf) {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "duck-batch-{}-{}.sqlite",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let config = ConnectionConfig {
            id: "batch-test".to_owned(),
            kind: DatabaseKind::Sqlite,
            database: path.display().to_string(),
            ..ConnectionConfig::default()
        };
        let conn = Connection::connect(&config, None).expect("临时 SQLite 建连");
        (conn, path)
    }

    fn list(statements: &[&str]) -> Vec<String> {
        statements.iter().map(|sql| (*sql).to_owned()).collect()
    }

    /// 表里的行数（结果只取第一格）。
    fn count(conn: &mut Connection, table: &str) -> i64 {
        let outcome = conn
            .execute(&format!("SELECT count(*) FROM {table}"), 10)
            .expect("count 查询");
        let cell = &outcome.result.expect("count 有结果集").rows[0][0];
        match cell {
            Value::Int(value) => *value,
            other => other.render().to_string().parse().expect("count 是整数"),
        }
    }

    /// 跑一批（「不停」的谓词）。
    fn run(conn: &mut Connection, statements: &[String], options: &SqlBatchOptions) -> BatchReport {
        execute_batch(conn, statements, options, &|| false, &mut |_| {})
            .expect("批量执行不该在 BEGIN 上失败")
    }

    /// 逐条提交：遇错继续时后面的照跑，遇错即停时后面的标 `Skipped`。
    #[test]
    fn auto_commit_respects_continue_on_error() {
        let (mut conn, _path) = temp_sqlite();
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)", 1)
            .expect("建表");
        let batch = list(&[
            "INSERT INTO t VALUES (1)",
            "INSERT INTO nope VALUES (1)",
            "INSERT INTO t VALUES (2)",
        ]);

        // 继续：三条都走完，中间那条失败。
        let report = run(
            &mut conn,
            &batch,
            &SqlBatchOptions::new(100).continue_on_error(),
        );
        assert_eq!(report.executed, 2);
        assert_eq!(report.failed, 1, "{:?}", report.statements);
        assert_eq!(report.skipped, 0);
        assert_eq!(report.affected, 2);
        assert!(!report.all_ok());
        assert!(report.first_error().is_some());
        assert_eq!(count(&mut conn, "t"), 2, "继续档下前后两条都落了库");

        // 即停：清空后重跑同一批，第三条明确未跑（回执里在，状态是 Skipped）。
        conn.execute("DELETE FROM t", 1).expect("清空");
        let report = run(&mut conn, &batch, &SqlBatchOptions::new(100));
        assert_eq!(report.statements.len(), 3);
        assert!(report.statements[2].status == StatementStatus::Skipped);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.failed, 1);
        assert_eq!(report.executed, 1);
        assert_eq!(count(&mut conn, "t"), 1, "即停档下第二条未跑");
    }

    /// 整批一个事务：中途失败 → 整批回滚（前面几条也不留）。
    #[test]
    fn single_transaction_rolls_back_the_whole_batch() {
        let (mut conn, _path) = temp_sqlite();
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)", 1)
            .expect("建表");
        // 第三条故意打不存在的表：SQLite 的 `1/0` 是 NULL 不是错误，不能拿来当失败样本。
        let batch = list(&[
            "INSERT INTO t VALUES (1)",
            "INSERT INTO t VALUES (2)",
            "INSERT INTO nope VALUES (1)",
        ]);
        let report = run(
            &mut conn,
            &batch,
            &SqlBatchOptions::new(100).single_transaction(),
        );

        assert_eq!(report.executed, 2);
        assert_eq!(report.failed, 1, "{:?}", report.statements);
        assert!(report.transaction_error.is_none(), "回滚本身不该报错");
        assert_eq!(count(&mut conn, "t"), 0, "整批事务失败要回滚前面两条");
    }

    /// 整批一个事务：全成则提交，回执与影响行数照给。
    #[test]
    fn single_transaction_commits_when_everything_succeeds() {
        let (mut conn, _path) = temp_sqlite();
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)", 1)
            .expect("建表");
        let batch = list(&["INSERT INTO t VALUES (1)", "INSERT INTO t VALUES (2)"]);
        let report = run(
            &mut conn,
            &batch,
            &SqlBatchOptions::new(100).single_transaction(),
        );

        assert!(report.all_ok(), "{}", report.failed);
        assert_eq!(report.affected, 2);
        assert_eq!(report.statements[0].affected(), 1);
        assert_eq!(count(&mut conn, "t"), 2);
    }

    /// 上游叫停：谓词命中后剩下的全标 `Skipped`，且进度回调**逐条**都送到。
    #[test]
    fn stop_predicate_skips_the_rest() {
        let (mut conn, _path) = temp_sqlite();
        conn.execute("CREATE TABLE t(id INTEGER PRIMARY KEY)", 1)
            .expect("建表");
        let batch = list(&[
            "INSERT INTO t VALUES (1)",
            "INSERT INTO t VALUES (2)",
            "INSERT INTO t VALUES (3)",
        ]);
        // 跑到第二条之前叫停：进度回调数到 1（第一条刚落定）时，谓词就变真。
        let seen = AtomicUsize::new(0);
        let stop = || seen.load(Ordering::Relaxed) >= 1;
        let report = execute_batch(
            &mut conn,
            &batch,
            &SqlBatchOptions::new(100).continue_on_error(),
            &stop,
            &mut |_| {
                seen.fetch_add(1, Ordering::Relaxed);
            },
        )
        .expect("批量执行");

        assert!(report.stopped);
        assert_eq!(report.executed, 1);
        assert_eq!(report.skipped, 2);
        assert_eq!(
            seen.load(Ordering::Relaxed),
            3,
            "进度回调对每条（含跳过的）都要送一次"
        );
        assert_eq!(count(&mut conn, "t"), 1);
    }

    /// 空批次没有任何副作用（也不开事务）。
    #[test]
    fn empty_batch_is_a_no_op() {
        let (mut conn, _path) = temp_sqlite();
        let report = run(
            &mut conn,
            &[],
            &SqlBatchOptions::new(100).single_transaction(),
        );
        assert!(report.statements.is_empty());
        assert!(report.all_ok());
    }
}

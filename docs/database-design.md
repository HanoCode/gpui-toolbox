# Duck Toolbox 数据库模块架构设计（duck-db）

> 状态：已确认开工（2026-10-12）。参考物：temp/dbx、dbui、dbdelve、dbflux。
>
> **实施修订（相对最初稿）**：M1 采用「同步内核」方案——`duck-db` 对外只暴露同步 API，
> 阻塞驱动（rusqlite / postgres / mysql 28）的调用由 UI 层丢进 `background_executor`；
> 异步-only 驱动（M2 的 mongodb 等）在驱动内部自管 current-thread runtime `block_on`
> （dbdelve 模式），tokio future 永不进 GPUI executor。理由：M1 三引擎均有成熟阻塞驱动，
> postgres_rustls 0.1.6 的 MSRV 1.97 被本仓 1.85 硬门否决，引入内嵌 tokio runtime 的
> 收益要等 M2 才出现。§4.1 的运行时抽象缝保留：`duck-db` 公共 API 无 Future、无 tokio 类型。
>
> **实施修订二（2026-10-12 晚，M1 收尾 + 提前项）**：M1 全量落地（含 §4.4/§4.5 的
> 取消/超时之外全部判据——「浏览-查询-改一格-保存」链路已通：表浏览选中行即入
> 暂存编辑器，保存逐列 `update_cell`，无主键表只读）。三项按推荐口径提前/裁剪：
> ① **SSH 隧道**（原 M2）提前落地——系统 `ssh -N -L` + `SSH_ASKPASS_REQUIRE=force`
>   （口令经环境变量进子进程、不落盘），`Drop` 杀进程防孤儿，句柄随会话释放；
> ② **AI text-to-SQL**（M3 提前的基础版）——`AiProvider` trait + OpenAI 兼容
>   `/chat/completions`（ureq，本仓未开 `json` 特性故手工序列化），只读单语句经
>   `risk_of_statements` 判定后自动执行，写/多条语句填回编辑器人工审批；
> ③ **取消/超时与执行表（§4.4）暂缓**——阻塞驱动下逐语句安全取消需 per-engine
>   cancel 通道（SQLite interrupt / PG cancel_query / MySQL KILL QUERY 各一套），
>   价值/成本比低于编辑链路，推迟到与 M2 并发执行模型一起做。
> PG TLS 维持 MSRV 结论缓行；Redis/SQLServer/Mongo 等新引擎待验收后按矩阵分批准入。
>
> **实施修订三（2026-10-12 晨，M1.6 补齐数据出入口）**：在 M1 判据之外补三项「自带
> 数据进出」能力，全部沿用既有分层（内核零 gpui、UI 走 `background_executor`）：
> ① **DDL 查看**——`Connection::table_ddl`：SQLite 读 `sqlite_master.sql` 原文、
>   MySQL `SHOW CREATE TABLE`、PG 由 `information_schema.columns` **重构近似 DDL**
>   （含列/类型/可空/缺省与主键，不含索引与约束细节，文档与 UI 均如实标注）；
>   表页加载时顺带取，右栏「对象」下新增 DDL 卡（定高滚动 + 等宽）。
> ② **CSV 导入**——新增 `duck-db/src/csv.rs::parse_csv`（导出 `to_csv` 的读侧对偶：
>   引号内逗号/换行/`""` 转义、CRLF、空白行跳过；**未加引号空单元格 = `NULL`，
>   带引号空串保留空字符串**）；`Connection::insert_rows` 三引擎**全参数化**批量插入
>   （SQLite 预编译 + 逐行绑定，PG/MySQL 每行一条；`None → NULL`），分发表层做等宽校验；
>   UI 工具条「导入 CSV」按首行表头对位列名。→ §4.4 的「DDL 查看 / CSV 导入」两项完成。
> ③ **连接档案导出/导入**——左栏底部一对按钮；导出 `serde_json` 档案数组（**口令只以
>   密文形式存在程序自己的凭据文件里、明文天然不经文件**），导入按 `id` 去重并跳过
>   `validate()` 失败项，结果落 kv。
>
> **实施修订四（2026-10-12 午后，§4.4 取消/超时 + 事务化写）**：把「仍未做」里的两项补齐，
> 均为**零新增依赖**（三引擎的原生取消原语 + std 线程）：
> ① **查询取消与超时**——新增 `duck-db/src/cancel.rs`：`CancelHandle` 是**带外**句柄
>   （与连接锁无关，后台线程正握着 `Mutex<Connection>` 也能送「停」），三引擎分别走
>   SQLite `sqlite3_interrupt` / PostgreSQL `CancelToken::cancel_query`（M1 恒 `NoTls`）/
>   MySQL 另开短连接发 `KILL QUERY <id>`（协议要求；句柄随身带建连 `Opts` 与连接 id）；
>   `CancelTimer` 是超时守护线程（`disarm()`/drop 立刻撤销——查询正常跑完必须撤销，
>   否则晚到的一枪会打在下一条语句上）。UI 侧：`spawn_db_guarded` 只服务**读路径**
>   （SQL 执行、表页加载），工具条在跑时把「执行」换成「停止」；超时 30s
>   （`QUERY_TIMEOUT`，单次覆盖留给后续查询选项面板）。错误归一：`DbError::Cancelled`
>   （rusqlite `SQLITE_INTERRUPT` / PG `57014` / MySQL `1317`・`3024`），文案由 UI 按
>   「点过停止没有」区分「已停止」与「查询超时」。⚠️ **写路径不给取消入口**——
>   中途取消会留下半截写入。
> ② **行内多行编辑事务化**——`Connection::update_cells(table, &[CellEdit])`：暂存式编辑的
>   多列改动折成**一个事务**提交（三驱动分别用 rusqlite `Transaction` / PG
>   `Client::transaction` / MySQL `start_transaction`），要么全成要么全回滚；
>   命中行数少于计划数时 UI 提示「该行可能已被他人修改」。`insert_rows`（CSV 导入）
>   同样补上单事务——原先是逐行隐式事务（慢且可能留下半截导入）。
> ③ **事务状态显示**——`Session.in_tx` 由本次执行的语句文本判定（[`tx_effect`]：
>   `BEGIN`/`START TRANSACTION` 开、`COMMIT`/`ROLLBACK`/`END` 关），状态栏显示「事务中」
>   档位并提供「提交 / 回滚」两个出口；`execute_sql` 是「执行」与这两个按钮的公共路径
>   （谁在跑、怎么停、事务还在不在只有一份真相）。
> ④ 顺带：SQL 结果行上限对齐本节 §4.5 的口径（1 000 → **10 000** 行，网格本就虚拟化）。
> **仍未接取消**：AI 助手那条路径（自带 60s 网络超时；模型产出只读 SQL 后自动执行，
> 用户本就在等），后续与「查询选项面板」一起做。
> **仍未做**：新引擎矩阵（SQL Server / MongoDB / ClickHouse，需依赖准入评审）、
> 每连接的默认超时与单次覆盖（查询选项面板）、结果网格的多行选取（跨行批量改）。
>
> **实施修订五（2026-10-12 午后二，§4.4 查询选项 + §4.5 多行选取 + 内省补全）**：
> 把修订四列出的「仍未做」除引擎矩阵外全部清空（**依然零新增依赖**）：
> ① **查询超时三级回落**——生效值 = 单次覆盖 → 连接默认 → 全局 30 秒。
>   「连接默认」是新字段 `ConnectionConfig::default_timeout_secs`（`Option<u64>`，
>   `#[serde(default)]`，旧存档缺字段即 `None`；`0` 与 `None` 同义，写回时归
>   `None`），在连接详情弹窗新增的 **QUERY** 分节里填；「单次覆盖」是页面内存态
>   `query_timeout_override`（不落盘、不进档案），由工具条「查询选项」弹窗设，
>   生效时工具条挂「超时 N 秒」标签（否则用户会以为连接默认在管）。
>   上限 `MAX_TIMEOUT_SECS = 3600`，挡住手滑多打一位。
>   ⚠️ 该弹窗正文**没有**挂视图实体（与连接详情不同）：它只有静态字段 + 一个输入框，
>   输入框的正文由 `EditorState` 自己的视图重绘，弹窗不参与打字时的重排——
>   「弹窗正文必须视图化」这条约束只对「点了要变色 / 边打边过滤」成立。
> ② **AI 只读执行接入取消/超时**——`AiOutcome` 拆成 `ReadOnly { sql, tokens }` /
>   `NeedsApproval`：模型调用（网络等待，只能靠它自带的 HTTP 超时）与 SQL 执行
>   （真正在库上跑、需要「停」）**分成两步**，前者留在 `spawn_db`，后者回主线程
>   再走一次 `execute_ai_sql` → `spawn_db_guarded`，与手敲执行同一条路径。
> ③ **结果网格多行选取（跨行批量改 / 批量删）**——`selected_row`（在看哪一行）与
>   `checked_rows`（要一起处理哪一批）**是两个概念**，互不干扰：行首新增
>   `SELECT_COLUMN_WIDTH` 宽的复选框列，表头是三态全选（`CheckState::Indeterminate`），
>   勾选框自己 `stop_propagation()`，免得点勾选顺带打开右侧行编辑器。
>   勾选非空时网格上方浮出「批量工具条」：已选行数 + 列选择
>   （`dropdown_select`，选项来自当前结果集的列）+ 新值输入 + 应用到选中行 /
>   删除选中行 / 清空选择。两个写动作都走**单事务**内核接口：批量改 →
>   `update_cells`，批量删 → 新增的 `Connection::delete_row_set`。
>   ⚠️ **结果集一换，行号就失效**——`install_query_result` / 换表 / 翻页 / 刷新
>   四处都要 `clear_checked_rows()`，否则旧的勾选会落到别的行上。
> ④ **右栏补索引与行数**——新增两个内核内省：`Connection::table_indexes`
>   （SQLite `pragma_index_list` + `pragma_index_info`、MySQL
>   `information_schema.STATISTICS`、PG `pg_index` + `unnest(indkey) WITH ORDINALITY`）
>   与 `Connection::table_row_count`。行数口径**写进类型**（[`RowCount::Exact`] /
>   [`RowCount::Estimate`]）：SQLite 没有系统统计，只能 `COUNT(*)`（精确）；
>   PG 读 `pg_class.reltuples`、MySQL 读 `information_schema.TABLES.TABLE_ROWS`
>   （都是估算，文案「约 N 行」）。UI 上「行数 / 索引」两张卡在右栏的 DDL 卡之后。
>   ⚠️ 表级元信息**只在换表 / 刷新时取一次**（`table_meta_for` 记账）：翻页重复
>   `COUNT(*)` 在大表上是纯浪费。
> **实施修订六（2026-10-12 傍晚，新引擎矩阵首批：ClickHouse + SQL Server）**：用户拍板后
> 按 §5 矩阵分批准入，本轮落地两条引擎，依赖增量 12 个包（lock 925 → 937）。
> ① **ClickHouse（零新增依赖）**——按 §5 原选型走 **`ureq` 直连 HTTP 接口**（8123），
> 不开原生 TCP 协议。几处与 SQL 引擎**根本不同**的地方：
>   - **无会话**：HTTP 每请求都是新会话，所以「建连时 `SET` 一次」不成立。只读档
>     **每请求带 URL 参数 `readonly=1`**；参数化走官方 `param_<名>=<值>` 配 SQL 里的
>     `{名:String}` 占位（**没有字符串拼接，也就没有注入面**）；
>     `database` / `query_id` / 参数值全部 percent 编码（一个含 `&` 的库名足够往
>     查询串里塞参数）。
>   - **结果集取 `default_format=JSONCompact`**：一次拿到 `meta`（列名 + 类型名）与 `data`，
>     不必自解 TSV 转义。**行数截断在 Rust 侧做**——URL 的 `max_result_rows` 是**块级**
>     `overflow_mode=break`，返回行数落在块边界而非恰好 N 行，拿它探测「还有更多」会时灵时不灵。
>   - **取消**：HTTP 无连接级中断，走 `query_id` + `KILL QUERY WHERE query_id = '…'`
>     （`CancelKind::ClickHouse` 自带 `inflight` 槽；没在跑的查询就空操作，与 SQLite 的
>     interrupt 同语义）。
>   - **诚实性**：`table_primary_key` 恒返回空——ClickHouse 的 `PRIMARY KEY` 是 `ORDER BY`
>     排序表达式、`UPDATE` 是异步 mutation，**没有可寻址的单行**，所以表浏览判只读、
>     四个写方法返回 `DbError::Unsupported`，而不是给一个点了会失败的按钮。
>     `list_schemas` **保留 `system`**（`system.query_log` / `system.parts` / `system.replicas`
>     是运维一等公民），只剔与 `INFORMATION_SCHEMA` 重复的那份。
> ② **SQL Server（`tiberius 0.12`，闭包 +12 包）**——§4.1 方案 A 的第一个真实消费者：
>   - **async-only 塞进同步内核**：每条连接自管一个 `tokio` current-thread runtime，
>     所有 await 在它上面 `block_on`；`tokio` / `futures` 类型**一个都不进公共 API**。
>     `tiberius::Client::connect` 要的是 `futures::io::AsyncRead + AsyncWrite`，而
>     `tokio::net::TcpStream` 实现的是 tokio 自己的 trait → 靠
>     `tokio_util::compat::TokioAsyncWriteCompatExt::compat_write()` 搭桥。
>   - **只读档是客户端拦截**：SQLite `query_only` / PG `default_transaction_read_only` /
>     MySQL `SESSION TRANSACTION READ ONLY` / ClickHouse `readonly=1` 都是**服务端**开关，
>     而 SQL Server **没有会话级开关**（只能靠只读账号或把库设成 `READ_ONLY`），所以由驱动
>     在写语句进引擎之前拦下——可信，但**不是服务器兜底**，文档与 UI 均如实标注。
>   - **标识符引号用 `"` 而不是 `[`**：本仓 `quote_identifier` 只支持**同字符成对**的引号，
>     `[`/`]` 是一对不同字符（要改就得动所有引擎共用的那个函数）；`"` 在
>     `QUOTED_IDENTIFIER ON`（默认）下完全合法。
>   - **分页**：`OFFSET … FETCH` **强制要求** `ORDER BY`；无稳定序时补
>     `ORDER BY (SELECT NULL)`。
>   - **`QueryStream` 超出 `max_rows` 之后必须继续消费**（不能 `break`）：TDS 是流水线协议，
>     丢下半条响应流会让下一条语句读到上一个查询的残留。
>   - **TDS 时间族自研换算**（避开 `tiberius` 的 `time` feature——那会拉 `time 0.3.55`，
>     其 `rust-version` 是 **1.88**，顶破本仓 1.85 硬门）：`datetime` 的原点是 **1900-01-01**、
>     片段是 **1/300 秒**；`smalldatetime` 同原点但第二个字段是**分钟**；`date` / `datetime2` /
>     `datetimeoffset` 的原点是 **0001-01-01**；`datetimeoffset` 存的是 **UTC** + 偏移分钟数。
>     二十来行整数运算（Howard Hinnant 的 `civil_from_days`）换掉一条越线依赖。
>   - **`KillTarget`**：取消另开短连接发 `KILL <spid>`；`session_id <= 0` 时**宁可不发**——
>     `KILL 0` 是杀所有会话。
>   - **依赖准入实证**：`tiberius 0.12.3` **未声明 `rust-version`**，MIT/Apache-2.0，
>     `edition 2021`；必须 `default-features = false`（默认带 `winauth` 非目标平台 +
>     `native-tls` 会引 openssl），只开 `tds73` + `rustls`。闭包 132 个包中**只 3 个超 1.85**
>     （`encoding_rs 0.8.42` / `uuid 1.28.0` / `wasip2 1.0.4`），且**都已存在于 HEAD 的
>     `Cargo.lock`**（gpui 侧既有传递依赖）→ 未新增超限包；`tokio 1.53.2` 同样早已在图内
>     （`postgres` 阻塞版内部就是 `tokio-postgres`）→ **零新增 tokio**。
> ③ **UI 接线**：两条引擎在驱动选择器里从占位变成真卡片（`kind: Some(...)`），
> `kind_logo` / `kind_label` / `default_user` 三处 match 补齐（ClickHouse 默认账号
> `default`、SQL Server 用内建管理员 `sa`）；`DatabaseKind::ALL` 扩到 5 档。
> ④ **许可证**：`THIRD_PARTY_LICENSES.md` 重生成，906 → **917** 个组件（tiberius 闭包 +12；
> 2026-10-12 全仓凭据与系统钥匙串解绑后 `keyring 3.6.3` 已从图内**彻底删除**，
> 见 `architecture-gap-analysis.md` 的 **P3-10**，该文档需再重生成一次）。
> ⑤ **MongoDB 依赖准入（已通过，驱动待写）**——§5 矩阵里的第三个 M2 引擎，先把「引不引得进」
> 用实测钉死（**这一步是本批最费劲的一步**）：
>   - **版本必须 `=3.6.0`**：`mongodb 3.6` 声明 `rust-version = "1.85"`（恰好压线），而
>     `3.9` 已是 **1.88**。与其它「钉版本」不同，这条不是 API 兼容问题而是**硬门**问题——
>     写 `"3.6"` 会解析到 3.9+，必须 `=`。
>   - **`default-features = false` 是必须的**：默认还带 `dns-resolver`（`hickory` 系，只服务
>     `mongodb+srv://` 的 SRV 查询）。关掉之后 `cargo tree` 里**没有** `mongocrypt` /
>     `aws-config` / `reqwest` / `hickory` / `openssl`——`in-use-encryption` 那条路（要
>     `mongocrypt-sys` 的 C 编译）根本没被启用。
>   - **零新增 TLS 栈**：3.6 走 `rustls 0.23` + `tokio-rustls 0.26` + `ring`，与本仓
>     `mysql` / `postgres_rustls` 是**同一套**（不会出现两代 rustls 并存）。
>   - ⚠️ **`time` 族必须降到 0.3.44**：`bson 2.15` 显式开了 `time` 的 `macros` feature，
>     于是 `time` / `time-core` / `time-macros` 一起**进入编译树**（引入前它们并不在树里——
>     `mac-notification-sys` 那个引用方在 host 上没启用，`cargo tree -i time` 无输出）。
>     而 crates.io 当前的 `time 0.3.55` 声明 **1.88**，顶破硬门；降到 **`0.3.44` = 1.81**
>     后四个包全部达标，且 `cargo check -p duck-platform` 通过（两侧约束都只是 `^0.3`）。
>     **pin 由 `Cargo.lock` 承载**（本仓构建一律 `--locked`）。
>   - **净结果**：lock +28 包（**零新增超限包**）、`mongodb 3.6.0` 实测编译通过
>     （1m11s，无 C 依赖）。依赖已接入 `Cargo.toml` / `duck-db`，**驱动代码是下一步**。
> **实施修订七（2026-10-12 晚，MongoDB 驱动落地 + 库名解禁）**：
> ① **MongoDB 驱动**（`drivers/mongo.rs`，§4.2 那条 `DocumentExt` 抽象的**第一个真实实现**）
> 落到与其余五条引擎同一套网格契约上，不需要另立 UI：
>   - **映射**：库 / schema ↔ database（`listDatabases`）、表 ↔ collection（`listCollections`，
>     `type=view` 归视图）、行 ↔ 文档、**主键恒 `_id`**（每个集合都有唯一索引，是唯一可靠的
>     定位方式）、分页 `find + sort(_id) + skip + limit`（多取一条探测下一页）。
>   - **列只能靠采样**：文档库没有声明式 schema，列清单 = **样本 `SCHEMA_SAMPLE=200` 条文档的
>     字段并集**；空集合推不出列 → 返回空 `Vec`，界面老实显示「无样本」而不是编造列。
>   - **查询语言是 JSON 命令**：编辑器里输入的是 `db.runCommand(<doc>)` 的参数
>     （`{"find": "users", "filter": {…}}`）。走这条而不是自造 `db.coll.find({…})` 壳语法：
>     命令文档是**服务端稳定接口**，壳语法只是 shell 的语法糖，解析它等于自己维护一门方言。
>   - **只读档是客户端拦截**（与 SQL Server 同理，MongoDB 没有会话级只读开关）：按命令
>     **首关键字**白名单判定，**未知命令一律当写**——安全方向必须是默认拒绝。
>   - **无多文档事务**（standalone 上根本不可用）：批处理退化成**一条**覆盖多行的命令
>     （`delete_many({$or: […]})` / `update_many`）或顺序多条按命中数累计，边界如实写进模块文档。
>   - **取消**：`killOp` 要**点名 `opid`**，而 `opid` 只能从 `$currentOp` 里按 `appName` 捞——
>     所以每条连接有进程内唯一的 `appName`（`duck-<pid>-<序号>-<id>`），取消时另开短客户端
>     去 `$match` 它。没有在跑的操作是**空操作**（取消本就该幂等）。
>   - **技术坑（都改过一遍才对）**：`mongodb 3.x` 的 `Find` / `Update` / `RunCommand` 等
>     **只实现 `IntoFuture`**（`Future` 落在 `into_future()` 产出的类型上）→ 统一入口必须先
>     转一次，直接传 action 会报「`Find` is not a future」；`ClientOptions::parse` 同理；
>     `Cursor::deserialize_current` 反而是**同步**的（只有 `advance` 要 await）；
>     `Document::get_str` 返回 `Result` 而不是 `Option`；`bson::DbPointer` 两个字段都是
>     `pub(crate)`（取不到，只能走 `Debug`）。
> ② **库名解禁——「连库不必先选库」**（用户提出的问题：MySQL 这类引擎不该强制填库名）：
>   校验层曾经写的是「网络引擎库名一律必填」，**比驱动更严**——五个引擎的驱动本来都允许不填。
>   现在 `needs_database()` **只有 SQLite 为真**（那边装的是文件路径），其余全部放行，
>   各自落点写在各驱动里：
>   - **PostgreSQL**：**省略 `dbname=`**（⚠️ 不能传空串——那会真的把空库名塞进 startup 包，
>     服务端回 `database "" does not exist`），由服务端按 libpq 规则取**用户名同名库**。
>   - **MySQL**：`db_name(None)` → 不发 `COM_INIT_DB`，`SELECT DATABASE()` 回 NULL，正是
>     其它客户端「不选库连上」的行为；表名可用 `db.table` 全限定。
>   - **ClickHouse**：回落惯例库 `default`（与请求不带 `database` 时的 `currentDatabase()` 对齐；
>     否则 `WHERE database = ''` 一条都查不出来）。
>   - **SQL Server**：不设 `database` → 落入**登录名的默认库**（通常 `master`）。
>   - **MongoDB**：三级回落 **命令点名 → 档案记住的命名空间 → 档案里的默认库**，三级全空即
>     **未选定库**（合法状态：连得上、列得出所有库，只是命令没落点）→ 目录返回空、执行给
>     一句可操作的错误，用户在上方命名空间下拉里一键选。
>   落点**文案住在界面层**（`db.rs::database_hint` 逐引擎一句 + EN 表），内核只给
>   `needs_database()` 这类事实——零依赖的 `duck-db` 不参与 i18n，字面量不该出现在那边。
>   空目录提示也一并改准（「当前库没有表或视图」在「根本没选库」时是误报）。
> ③ **测试**：`duck-db` 新增 `config::database_is_optional_for_every_network_engine`
>   （含口径哨兵：必填库名的引擎**只能有一个**）+ 各驱动的落点回落单测；
>   回落逻辑抽成**纯函数**（`pick_database`）以便脱开连接断言。
> **仍未做**：MongoDB 的 `aggregate` 管道编辑器（当前统一走 JSON 命令输入，没有管道专用 UI）。
>
> **实施修订八（2026-10-12 深夜，Elasticsearch 驱动落地 —— 引擎矩阵清零）**：§5 里最后一条
> `⏳` 落地，`drivers/elasticsearch.rs` 走 `ureq` 直连 REST（与 ClickHouse 同一条路子），
> **依然零新增依赖**：`ureq` 早在图内；`base64 0.22.1` 本来就在 `duck-db` 的依赖图里
> （`bson 2.15 → mongodb 3.6` 的传递依赖，`cargo tree -i base64@0.22.1` 可证），提成直接依赖
> **不新增任何包**，`THIRD_PARTY_LICENSES.md` 第 122 行早已登记。至此 6 引擎（SQLite + 5 网络引擎）
> 全部落在同一套网格契约上，界面无一处引擎特判。
>   - **编辑器里的「查询语言」= Kibana Dev Tools 控制台格式**：`<METHOD> <PATH>` + 换行 + JSON 体，
>     **空行分隔多条**，`#` 开头当注释。理由与 MongoDB 那条同构——**不自造壳语法**；
>     Kibana 控制台格式已是事实标准（官方文档到处是可直接粘贴的片段），复用它等于复用
>     别人的用户教育成本。
>   - **网格契约映射**：
>     - **库 / schema ↔ 没有这一层**（索引是平铺的）→ `list_schemas()` **恒返回空**；命名空间
>       下拉只在条目 > 1 时才渲染，空清单正好让它自然消失，**不给界面加引擎特判**。
>     - **表 ↔ index**，**视图 ↔ alias**（别名 = 指向一个或多个索引的虚拟名，与视图语义最近）——
>       两者一次 `GET /<pattern>/_alias?expand_wildcards=all` 同时拿到。**不用 `_cat/indices`**：
>       那条要 `monitor` 权限，而 `_alias` 只要索引元数据权限。
>     - **行 ↔ 文档**，**主键恒 `_id`**；**列 ↔ mapping 字段路径**（`properties` 递归展平出
>       `user.name`，**多字段 `fields` 跳过**——`title.keyword` 不是 `_source` 里独立的一格，
>       列出来会让用户以为能单独编辑；runtime 字段同理跳过，它永远空白）。
>   - ⚠️ **`_id` 是虚拟列，不在 `_source` 里** → 必须由驱动**注入结果集首位**。不注入的话编辑层
>     按列名找不到主键，整张表会被判成**不可编辑**。
>   - ⚠️ **`_id` 不能排序**（服务端报 `Fielddata access on the _id field is disallowed`），而
>     调用方给的 `order_by` 恰好就是主键 → 排序档先按**列类型**过滤（keyword 族 / 数值 / 日期 /
>     布尔 / ip 可排；`text` 默认没开 fielddata，一并挡在构造子句之前），过滤后为空退回 **`_doc`**
>     （段内物理序，是「没有排序」时的确定性落点）。用户因此拿不到一条服务端报错。
>   - **分页 `from` + `size`**：受 `index.max_result_window`（缺省 10000）约束，翻过头服务端会明确
>     报 `Result window is too large` —— 本条驱动**原样透出**，不吞；`track_total_hits: false`，
>     多取一条（`size = limit + 1`）探测下一页。
>   - **只读档 = 客户端拦截**（ES 没有「会话只读」开关，与 SQL Server / MongoDB 同理）：`GET`/`HEAD`
>     一律放行；`POST` 只在路径段命中**只读命令白名单**时放行（`_search` / `_count` / `_msearch` /
>     `_mget` / `_sql` / `scroll` …——搜、数、聚合本来就是 POST）；**未知命令一律当写**，
>     安全方向必须是默认拒绝。
>   - **没有跨文档事务**：`_bulk` 里每条 action 各自原子提交 → 批量改 / 删用**一条 `_bulk`**，
>     **部分失败要「吵」**并把已生效条数写进错误（`3 updated, 2 failed — first: …`），不假装原子。
>   - **取消链路**：每个请求带 **`X-Opaque-Id`**（进程内唯一 `duck-<pid>-<序号>`）→ 取消时
>     `GET /_tasks` 按 opaque id 认领（`find_task_by_opaque` 返回 `nodeId:taskId`，**头名大小写
>     不敏感**）→ `POST /_tasks/<id>/_cancel`；没有在跑的请求时取消是**空操作**（取消本就该幂等）。
>   - **纯函数化的可测面**（绕开「要连库才能验」）：`parse_requests`（空行分段 / 注释跳过）·
>     `is_read_only_request`（默认拒绝）· `encode_pattern` · `flatten_mappings` · `sort_clause` ·
>     `bind_value` · `find_task_by_opaque` · `map_json` · `ddl_of`。`table_ddl` 首行是英文注释
>     （**零依赖内核不吐成品中文**）。
>   - ⚠️ **`encode_pattern` 必须保留模式元字符**：索引模式里的 `*` / `?` / `,`（多索引列表）是
>     Elasticsearch 的**模式语法**而非数据，编码成 `%2A` 等于把语义押在中间层的解码行为上；
>     所以按模式语法切开、**字面部分**仍走 `url_encode`（应对空格 / 非 ASCII）。
>   - ⚠️ **`map_json` 未知类型按 JSON 形状给值**（数值给数值、对象给 JSON 文本）——原本一律降成
>     文本会让控制台查询 / `_cat` 响应失去数值语义；`scaled_float` 仍然走文本
>     （定点数**绝不进 f64**，本仓硬纪律）。
>   - ⚠️ **测试不能押列序**：本仓 `serde_json` 的 `preserve_order` feature 是别的依赖开的，
>     不归 `duck-db` 管 → 断言改按**下标找列名**。
> ② **UI**：选择器新增**「搜索」分区**与 Elasticsearch 卡片（`:9200`）；`DatabaseLogo::Elasticsearch`
>   补品牌 logo（`assets/dblogos/elasticsearch.svg`，全彩：`#343741` 条 / `#FEC514` / `#00BFB3`）；
>   `kind_logo` / `kind_label` / `default_user` / `database_hint` 四处 match 臂补齐。
>   **库名栏在 ES 上就是「默认索引 / 别名」**（支持 `logs-*` 通配，留空即 `_all`）；
>   用户名预填内建超级用户 `elastic`——8.x 起安全模块默认开启，而**未开安全的集群不会因此报错**：
>   `xpack.security.enabled: false` 时 Security 模块整个不加载、凭证字段被直接忽略。要空账号清掉即可。
> ③ **测试与门禁**：`duck-db` **119 passed / 0 failed**（其中 ES 侧 20 余条纯函数 / 契约单测）；
>   `duck-components` logo 资源守卫（路径唯一 + 逐个能加载 + 清单条数一致）覆盖新 svg；
>   `duck-theme` 的 `table_is_sorted` 通过（EN 表按 UTF-8 字节序整段重排，**纯新增 0 删除**）。
> **仍未做**：MongoDB 的 `aggregate` 管道编辑器；DuckDB（M3+，视体积取舍）。

> **实施修订九（2026-10-13 晚，dbx 式界面改版：多查询标签 + 图标工具条 + 编辑器执行列）**：
> 对齐 `temp/dbx` 的主区设计，四处改动（**零新增依赖**，全部在 `duck-app` / `duck-theme` 内）：
> ① **标签模型从「连接」改为「查询」**——`tabs` 存**标签 id**（自增 `t<n>`），新增
>   `tab_conn: HashMap<标签, 连接>` 与 `active_tab`；`sessions_views` 改按标签 id 存快照。
>   **同一连接可以并排多个查询标签**（dbx 的「+」语义），每个标签各拿各的 SQL 与结果区；
>   会话（`sessions`）仍按连接共享。`active` 保持**连接语义**不变（页面其余逻辑照旧读它），
>   由 `active_tab` 派生；连接成功回调从「`tabs.contains` 去重」改为 `activate_conn`
>   （已有标签就切过去，没有就新开）。关标签：该连接还有别的标签时只关这一个
>   （连接保持，`remove_tab`），最后一个才走原「断开连接」钩子路径（`disconnect_conn`）。
> ② **标签栏**：tab 加引擎品牌 logo（`kind_logo(kind).element()`，全彩，中性底色靠它辨识
>   引擎——不学 dbx 的彩色 tab 底，主题纪律优先）；行尾新增「+ 新建查询标签」
>   （无活跃会话时禁用）；✕ 的提示从「断开连接」改为「关闭标签」。
> ③ **编辑器工具条改纯图标行（dbx 同款）**：执行▶/停止■（Primary/Secondary）、执行计划
>   （ListOrdered）、格式化（Code）、AI（Sparkles）、查询选项（SlidersHorizontal）、
>   历史（Clock）、保存查询（FileText）、刷新（RotateCw）——语义与 handler 全部照旧，
>   文字收进无障碍标签；右侧新增**连接下拉**（`connection_picker`，列出打开中的标签所属
>   连接，同连接多标签去重，选中切标签）与**库/Schema 下拉**（`namespace_picker`，
>   构造从右栏提出来共用，右栏那份只是换了调用点）。原状态文案挪到中间弹性带。
> ④ **编辑器左侧执行列**：贴着行号 gutter 外侧一颗 ▶（跑着换 ■），执行**编辑器全文**
>   （与 ⌘↵ / 工具条「执行」同一条 `run_query` 路径，含危险 SQL 二次确认）——
>   行号是 `code_editor` 布局模式自带的（`LayoutMode::code_editor` 默认 `line_number: true`），
>   无需自绘。
> ⑤ **i18n**：新增「关闭标签」「新建查询标签」两条（EN 表按 UTF-8 字节序插入；
>   ⚠️ 「标」= E6A087 排在「时」= E697B6 **之后**——先按次序比对再落位，守卫会抓）。
>   `tests/db_grid_horizontal_scroll.rs` 是副本型测试，与 `render_result` 无编译期耦合，不受影响。

> **实施修订十（2026-10-13 深夜，dbx 对齐第二轮：逐条执行 + 结果区 header/footer）**：
> ① **逐条语句执行**——内核 `sqlintel` 新增纯函数 `split_statement_spans`（`StatementSpan{text,start}`，
>   扫描逻辑与 `split_statements` 逐字对应、互证测试钉死），页面按 `起始字节偏移 → 行号`
>   把 ▶ 钉在**每条完整语句的首行外侧**（dbx 同款），只执行那一条
>   （`run_statement`，与全文执行同一条危险确认 / 执行路径）。行高读编辑器实际布局
>   （`InputBaseState::line_height()`，未布局完退回 mono_md 档），滚动偏移读
>   `scroll_offset()`——**跟随滚动靠 `cx.observe(&sql)`**：InputEvent 里没有滚动事件，
>   但编辑器滚动会 notify，observe 到页面重渲即可重新定位（⚠️ `h_flex()` 是 cross-axis
>   居中，满高并排行必须 `div().flex().flex_row()`，见 10-13 修订九附注）。
> ② **结果区头条**（`render_result_header`）：对象 pill（表浏览=当前表；SQL 结果=可编辑
>   目标表，否则「SQL 结果」）+「N 列 · M 行」+「在编辑器中修改 DDL」（仅表浏览且已取到
>   DDL；与右栏 DDL 卡共用 `stage_sql`——右栏可能被折叠，头条是常驻入口）。
> ③ **结果区脚注**（`render_result_footer`）：「共 N 行」（截断挂「已达上限」tag）·
>   耗时（新字段 `query_elapsed_ms`，`execute_sql` 完成时与 status 同源记录、
>   随 `SessionView` 快照走）· 产出结果的 SQL（`query_sql`）· **导出下拉迁到这里**
>   （`export_dropdown` 抽成方法；tab 行只留 看图表 / 导入 CSV / 「+」）。工具条中部
>   的 status 是易逝操作反馈，脚注跟结果共存亡——两层职责不再混。
> ④ **i18n**：新增「执行第 {} 条语句」「在编辑器中修改 DDL」「已达上限」「{} 列 · {} 行」
>   四条（「共 {} 行」「SQL 结果」「在编辑器中修改」已有，直接复用）。
> ⑤ 本轮与另一个会话**并发**：`duck-app/Cargo.toml` / `Cargo.lock` 归对方改（duck-ai 扩展），
>   本轮零新增依赖、未触碰 lock；编译门禁以 lock 收口后的 `--locked` 结果为准。

---

## 1. 目标与范围

- 在已存在的占位路由 `database`（`RouteId::new("database")`）下实装一个数据库客户端模块。
- **支持连接所有主流数据库**：关系型（PostgreSQL / MySQL / MariaDB / SQLite / SQL Server / ClickHouse / DuckDB…）、KV（Redis）、文档（MongoDB）、搜索（Elasticsearch）等，且架构上允许后续逐个增加。
- **预留 AI 能力**：自然语言生成 SQL（text-to-SQL）、查询解释、按 schema 上下文回答问题；所有 AI 触发的写操作必须有风险分级 + 人工确认闸门。
- 不做的事：不做云同步、不做跨库 2PC、不做 ER 图自动布局（后续可选）、**第一版不引 tokio 进 workspace 顶层**。

## 2. 参考项目评估结论（各取什么、弃什么）

| 项目 | 形态 | 取 | 弃 |
| --- | --- | --- | --- |
| **dbx** | Tauri + tokio，100+ 库 | ① `dbx-sql-*` 系列思想：SQL 风险分级（`SqlRisk`）、只读证明、方言能力模型——纯函数零 I/O，可直接借鉴甚至搬运；② `ConnectionSecretStore` 抽象 + OS keychain 存主密钥 + AES-GCM 加密文件；③ "每库一个具体客户端 + 枚举分发"的轻量模式；④ AI Agent 工具循环（`agent_tools.rs` 按库类型裁剪工具集 + `classify_sql_risk` 审批闸门）；⑤ russh SSH 隧道实现 | Tauri 命令壳、760 个 invoke 命令、Java Agent 桥（Oracle/达梦，后期再说）、云同步 |
| **dbui** | GPUI + sqlx | ① **异步桥接黄金范式**：独立小 tokio runtime（2 worker）封在 App 层，`spawn → oneshot → Task<T>`，UI 在 `cx.spawn` 里 await + `cx.notify()`——数据库代码完全不知道 GPUI 存在；② 暂存式编辑模型（staging → `RowBatch` 单事务提交）；③ `(connection, tab, seq, run_id)` 四重校验防过期结果覆盖；④ 系统 `ssh -L` 隧道 + askpass + 防孤儿 leash；⑤ 凭据进 keychain、JSON 原子写 | sqlx（0.9 偏重且 MSRV 风险）；它的"单一巨型 trait"可用但按 dbx/delve 经验枚举分发更务实 |
| **dbdelve** | GPUI，7 引擎 | ① **不引 tokio 的替代桥接**：每连接 current-thread tokio runtime + `block_on`，从 `background_executor` 调用，"no tokio future reaches GPUI's executor"；② `on_own_thread` 防占满后台池；③ 行上限中止 + `Feed` 流式拉取；④ enum dispatch 纪律（"跨界全是渲染字符串"） | vendor 的 GPUI 渲染补丁（ ours 是 gpui-fast，不同） |
| **dbflux** | GPUI + RPC 外置驱动 | ① 驱动中立契约 + 能力位元数据（`DatabaseCategory × QueryLanguage × DriverCapabilities`）；② `ConnectionExt` 按范式向下转型（`as_relational / as_document / as_keyvalue`）——**这是"支持所有数据库"又不开天窗的关键抽象**；③ RPC 外置驱动协议（UDS + postcard 长度帧）作为远期扩展点；④ MCP 治理层（只读/审批/策略三分档） | 13 个 driver crate 的前期体量；AWS SDK 系 |

**综合裁决**：核心抽象采用 dbflux 的「契约 + 能力位 + 范式细分 trait」，分发用 dbx/delve 的「enum + 每库具体客户端」，桥接采用 dbui/delve 双方案择一（见 §4.1），SQL 安全层搬 dbx-sql-core 思想，AI 层搬 dbx 的 agent 循环 + 审批闸门。

## 3. 总体架构与 crate 布局

```
duck-app（bin）
  └─ pages/db.rs            DbPage 实体（照抄 TerminalPage 样板）
       │  Entity / render / 左栏连接树 / 右栏对象详情
       ▼
duck-db-ui（可选，若页面长大再拆；M1 先留 duck-app）
       ▼
duck-db（内核 crate，★新建，不依赖 gpui）
  ├─ config      ConnectionConfig / DatabaseKind / 能力位
  ├─ secret      凭据存取（→ duck-platform::secret，第一位真实消费者）
  ├─ store       连接档案持久化（→ duck-store kv，struct→serde_json→单键）
  ├─ runtime     查询执行器：任务派发 / 取消令牌 / 超时 / 并发上限
  ├─ conn        DbConnection trait + ConnectionExt + DriverRegistry
  ├─ drivers/    postgres / mysql / sqlite / redis / mongodb / …（feature 门控）
  ├─ sqlintel    sqlparser：语句分类、风险分级、只读证明、方言差异
  ├─ result      Value / ResultSet / RowBatch（统一值模型，NUMERIC 走 Decimal(String)）
  └─ ai          （M3）Provider trait + Agent 循环 + 工具集 + 审批闸门
        ▼
duck-platform（paths / secret / dialog）   duck-store（kv 持久化）
```

依赖方向严格向下；`duck-db` 内核**零 `gpui::` 引用**（对齐 terminal-ssh-sftp-design §依赖纪律），可用普通 `#[test]` 覆盖。`duck-db` 依赖 `duck-platform`（secret/paths）与 `duck-store`（连接档案），均为既有底层 crate。

> 备选：把 sqlintel / ai 拆成 `duck-db-sqlintel`、`duck-db-ai` 子 crate。M1 不拆，等 ai 真正落地且 sqlintel 被外部（如插件）消费时再拆——避免过早碎片化。

## 4. 核心设计决策

### 4.1 异步桥接（本项目最关键的一个决定）

本仓现状：**零 tokio**，异步统一走 gpui `background_executor`（smol 底座）。但"支持所有数据库"意味着必然要用异步驱动（`mongodb` 只有 async；`tokio-postgres`/`mysql_async` 是事实标准）。

三个候选方案：

| 方案 | 描述 | 采用者 | 评价 |
| --- | --- | --- | --- |
| A. 纯阻塞驱动 | postgres(blocking) / mysql 28 / rusqlite / redis(blocking)，异步-only 的用 per-connection current-thread runtime `block_on` | dbdelve、dbflux | 零 tokio 泄漏，最贴本仓纪律；但 blocking postgres 生态弱（如 COPY、通知监听），每连接一个 runtime 有线程成本 |
| B. 内嵌小 tokio runtime | `duck-db` 内持一个 2-worker `tokio::runtime::Runtime`，`spawn → oneshot` 回传，UI 侧 `cx.spawn` await | dbui | async 驱动原生跑、池化成熟；tokio 被完全封在 `duck-db` 内部，不进 gpui executor，不传染其他 crate |
| C. 全仓 tokio | — | dbx | 违背本仓依赖纪律，否决 |

**裁决：方案 B 为主，阻塞驱动直跑 `background_executor` 为辅。**

- `duck-db::runtime::QueryRuntime` 封装 tokio runtime（`worker_threads=2`，惰性创建），对外只暴露 `spawn(future) -> Task<T>`（oneshot 包装）与 `spawn_blocking`。
- 纯阻塞驱动（rusqlite、mysql 28 这类）不需要过 tokio：直接 `cx.background_executor().spawn(...)`（既有范式 `plugins.rs:330-349`）。
- 对外 API 全部同步/Task 化，**tokio 类型（`Future` 除外）不出现在 `duck-db` 公共 API**，用 `pub use` 白名单 + CI grep 守卫（借鉴 dbx 的架构测试脚本）。
- tokio 进依赖图前按依赖准入流程核对 MSRV：tokio 1.x MSRV 1.70（近期版本 1.81+，需钉在 MSRV≤1.85 的版本线）；`tokio-postgres 0.7`、`deadpool-postgres 0.14`、`mysql_async 0.37`、`redis 0.27~0.32`、`tiberius 0.12`、`mongodb 3.x` 逐个核 MSRV 并写版本理由注释；超线的（如 mongodb 未来版本）按本仓先例降版本钉住。

### 4.2 驱动抽象（契约 + 枚举分发）

不做单一巨型 trait，也不做裸 enum：**trait 定义最小公约面 + 能力位声明细分能力 + enum 做运行时分发**。

```rust
// duck-db/src/conn/mod.rs
pub trait DbConnection: Send + Sync {
    fn kind(&self) -> DatabaseKind;
    fn capabilities(&self) -> Capabilities;          // 位标志：SCHEMA_INTROSPECT | EDITABLE_GRID
    fn language(&self) -> QueryLanguage;             // Sql | Mongo | Redis | EsDsl | …
    fn ping(&self) -> Result<(), DbError>;
    fn execute(&self, req: QueryRequest) -> Result<QueryResult, DbError>;  // 同步面，内部自管 async
    fn cancel(&self, token: CancelToken) -> Result<bool, DbError>;
    fn close(self: Box<Self>);
}
// 范式细分（dbflux ConnectionExt 思想）
pub trait RelationalExt { fn introspect_schema(&self, ...) -> ...; fn apply_changes(&self, batch: &RowBatch) -> ...; }
pub trait DocumentExt  { fn list_collections(&self, ...) -> ...; }
pub trait KeyValueExt  { fn scan_keys(&self, ...) -> ...; }
```

- 内部每引擎一个模块（`drivers/postgres/` 等：`mod.rs` + `catalog.rs` + `decode.rs`，抄 dbui 的文件切法），`DriverRegistry` 返回 `Box<dyn DbConnection>` 或 enum 均可——**M1 用 enum 分发**（`Connection::Postgres(...) | Mysql(...) | ...`），类型安全、零动态分发开销，且与 dbdelve "跨界全是具体值"的纪律一致；驱动数量超过 ~8 个再评估转 trait object。
- 统一值模型：所有引擎类型拓宽为 `Value`（`Value::Decimal(String)` 承载 NUMERIC，绝不走 f64——dbui 纪律）；`QueryResult { columns, column_types, rows, affected_rows, truncated, execution_time_ms }` 对齐 dbx-types DTO。
- 驱动 feature 门控：`duck-db = { features = ["postgres", "mysql", "sqlite", ...] }`，SQLite 默认开（`rusqlite 0.32 bundled` 已在 workspace 图内，零新增）。

### 4.3 连接配置与凭据

- **连接档案**：`kv: db_connections`（单键 JSON 数组，沿用 `appearance` 范式），元素含 `id / name / kind / host / port / user / database / tls / ssh_tunnel / group / env_tag(Local|Testing|Staging|Prod) / secret_ref`；**口令不入 kv**，只留 `secret_ref`。
- **库名只有 SQLite 必填**（那里装的是文件路径）：**五个网络引擎留空一律放行**，各自落点在驱动里
  ——PG 省略 `dbname`（服务端取用户名同名库）、MySQL 不选库、ClickHouse 用 `default`、
  SQL Server 用登录名默认库、MongoDB 三级回落（见修订七）。界面按引擎逐条给提示
  （`db.rs::database_hint`）；提示文案住在界面层，内核只给 `needs_database()` 这类事实。
- **凭据**：`duck-platform::secret`（**2026-10-12 起为应用自有文件后端**：值 = vault 密文，
  落 `<应用数据目录>/credentials.json`，键 `dbconn:{id}:password`、`dbconn:{id}:ssh`；
  加密密钥另落 `vault.key`（0600））——这是 secret.rs 的第一位真实消费者，终端 SSH 模块后续同源。
  零日志、无明文回落（环境故障直接报错，**绝不**折叠成「用户还没配」）。
  ⚠️ keyring 阶段设计的 `SERVICE = "com.duck.toolbox"` 已随依赖一并删除。
- **SSH 隧道**：M1 用系统 `ssh -N -L` + `SSH_ASKPASS` + 防孤儿 leash（dbui 方案，零新增依赖、尊重用户 `~/.ssh/config`）；与本项目 SSH 模块（russh 0.63）汇合后可切换为 russh 本地端口转发（dbx `ssh_tunnel.rs` 方案）。M1 不做代理/HTTP 隧道多跳。
- 配置写盘原子化：kv 写线程已有去抖 + 事务，无需额外处理；导出/导入连接走 `kv_all`/`kv_replace_all` + 脱敏。

### 4.4 查询执行、取消、超时

- 每次执行分配 `ExecutionId` + `CancelToken`（tokio `CancellationToken` 或自制 AtomicBool），注册进 `QueryRuntime` 的执行表 → UI 可寻址取消（⌘.）。
- 取消语义按引擎落实（dbui 经验）：PG `pg_cancel_backend`、MySQL `KILL QUERY`、SQLite `set_progress_handler`、Redis/Mongo 客户端侧放弃。
- 超时：每语句独立超时（连接级默认 + 单次可覆盖），`tokio::select!` 三路竞争（结果/停止/超时——直接抄 dbui `until_stopped`）。
- 并发上限：全局 `Semaphore`（建议 4），防大结果集拖垮后台线程。

### 4.5 结果网格与编辑

- **虚拟化**：`uniform_list` + `cx.processor(|_, range, _, _| …)` 只渲染窗口内行（dbui `grid.rs` 模式，gpui-component 有同款 API，以当前源码为准，不发明 API）。
- 行上限：SQL 编辑器结果默认截断 10 000 行（`CAP+1` 探测"还有更多"），表浏览走真分页（主键稳定排序，`limit+1` 探测）。
- **暂存式编辑**（dbui 模型）：单元格编辑先落 `pending_edits/inserts/deletes`（按主键），网格画删除线；⌘S 折叠成 `RowBatch` 单事务提交，⌘Z 丢弃。写路径统一经 `RelationalExt::apply_changes`。
- 结果可编辑性判定：先问 `sqlintel`（语句是否可定位单表 + 主键，dbx `sql_editability` 思想），不可判定则只读。
- **就地编辑的编辑态视觉**（2026-10-14 新增，dbx / dbflux 同款）：双击进入编辑后，编辑态必须**一眼可辨**——① 编辑格整体换 2px 强调描边（`extended.stroke.strong` + `tokens.colors.ring`）并提亮底色（`surface`），输入框本身 `appearance(false)`，视觉全靠这层框（后写的 `.border` 覆盖按边的 `border_r`）；② 进入编辑即聚焦 + **全选**（`select_all`），高亮选区本身就是「输入框已接管这一格」的信号，直接打字即可整格覆盖；③ 正在编辑的行，行号格用「文本选区底色 + 强调色文字」跟着点亮。
- **复制永远作用于选区**（2026-10-14 新增，dbx 同款）：右键「复制 ▸」与 `⌘C` 的目标 = 勾选行（多选）> 单击选中行 > 整份结果；子菜单父项文案跟着目标走（「复制所选 N 行 / 复制选中行 / 复制全部」），toast 回执同口径。行子集复制 = 列原样共享 + 按下标取行重建 `ResultSet`（选中行通常个位数，克隆可忽略；整份结果仍走零拷贝原路）。要复制全部先全选（表头勾选框 / `⌘A`）。
- **网格拥有剪贴板键**（2026-10-14 新增，dbx 同款）：结果区容器 `.track_focus` + 点击即聚焦（`grid_focus`）——按键派发沿焦点链走，不聚焦的话 `⌘C / ⌘A / ⌘X / ⌘V` 全被先前聚焦的 SQL 编辑器更深一层抢走（用户看到的就是「结果区不支持系统快捷键」）。四个键都绑在页面 `"Database"` 上下文：`⌘C` 复制所选行；`⌘A` 全选 / 清空勾选；`⌘X` 剪切选中格（复制文本 + 原格暂存 `NULL`）；`⌘V` 粘贴剪贴板文本到选中格（暂存新值）。剪切 / 粘贴都走 `pending_edits` 暂存路径（`⌘Z` 可退、提交前可整批丢弃）；就地编辑打开时这些键归输入框（更深的编辑器上下文先派发，原生语义）。

### 4.6 SQL 智能层（sqlintel）

`sqlparser 0.5x (visitor)`（MSRV 友好）：
- 语句分类（SELECT/DML/DDL/多语句拆分）；
- **风险分级** `SqlRisk` + **只读证明** `prove_read_only()`（dbx-sql-core 思想，纯函数、可单测）；
- 方言差异表（初版仅 mysql/postgres/sqlite 三档，声明式内嵌，不上 YAML 热加载）；
- 结果集可编辑性判定（供 4.5 用）。

### 4.7 UI 形态（duck-app 内）

照抄 `TerminalPage` 范式（`app.rs:610`/`:665-673` 换实体即可，路由常量与 `KNOWN_ROUTES` 都不动）：

- **左栏**（`with_sidebar`）：连接分组树（环境 tag 着色）→ 库 → schema → 表/视图/函数（延迟内省）。
- **主区**：连接页签 + 双模式（表浏览 / SQL 编辑器），SQL 编辑器用现有输入原语起步，M2 评估语法高亮（tree-sitter 或先正则着色）。
- **右栏**（`with_right_sidebar`）：对象详情（列/索引/DDL 预览/行数估算）。
- 状态栏：执行耗时、行数、截断提示、事务状态（`BEGIN` 开着要显示，dbui `TransactionState`）。
- i18n：全部中文源串 + EN 表按字节序补齐；新图标进 `duck-components/assets/icons/`（Lucide 1.75）。
- 后续把「数据库」登记为 `ToolMeta` 进命令面板（duck-tools），导航动作 `ROUTE_DATABASE`（⌘4）已存在。

### 4.8 AI 集成（M3，架构先行、实现靠后）

- **Provider trait**（`duck-db::ai`）：`complete(prompt, ctx) / stream(...)`，首版走 OpenAI 兼容 HTTP（`ureq` + SSE，本仓唯一 HTTP 客户端，零新增）；模型配置存 kv `ai_settings`，API key 走 secret。
- **Agent 循环**（dbx-core/src/ai 模式）：工具集按当前连接能力裁剪——`list_tables / describe_table / sample_rows / run_readonly_sql / run_sql(需审批)`；schema 上下文来自内省缓存（避免每次全量 DDL）。
- **安全闸门**：模型产出的 SQL 必过 `sqlintel::prove_read_only`；只读直接执行，写操作弹确认框（展示将影响的语句 + 风险分级）；连接可设 `ReadOnly` 档（池级 `SET default_transaction_read_only`）。
- 远期：MCP server（`rmcp`，把 duck-db 暴露给外部 AI 客户端）与 RPC 外置驱动（dbflux 协议）都留了口子，不在前三期。

### 4.9 表浏览的筛选条件：顺序比较必须按列的声明类型分叉（2026-10-13 修订）

**问题**：ClickHouse 驱动原本把**所有**筛选值都先 `toString(...)` 再比，于是
`WHERE amount > '100'` 实际比的是**字典序**——`'9' > '100'` 为真，`amount > 100` 会漏掉
所有三位以上的数。这类错误不报错、只是结果少几行，属于最难发现的一档。

**依据**（不是推测，是上游明文）：

- ClickHouse 源码 `FunctionsComparison.h` 的文档注释——*"You can compare the following types: numbers and decimals; strings and fixed strings; dates; datetimes; **within each group, but not from different groups**."*
- 官方文档多篇关于 `TYPE_MISMATCH` / `ILLEGAL_TYPE_OF_ARGUMENT` 的说明：**不做 String → Number 隐式转换**。

**修法**：判定收成一个纯函数 `numeric_ordering(types, column, op)`，**只在**
「顺序算子（`>` / `>=` / `<` / `<=`）**且**该列声明类型属数值族」时返回真：

| 情形 | 生成的比较 |
| --- | --- |
| 数值列 + 顺序算子 | 两侧都包 `toFloat64OrNull(...)`（数值序） |
| 其余（`=` / `!=` / `LIKE` / 文本列 / 日期列 / 枚举…） | 保持 `toString(...)`（字典序） |

**为什么日期列不一起改**：`Date` / `DateTime` / `Enum` **不能**过 `toFloat64OrNull`
（会抛 `ILLEGAL_TYPE_OF_ARGUMENT`），而它们的 ISO 文本形态**字典序就是时间序**——
改成数值反而要额外解析，且会引入新错误。

**为什么相等比较不一起改**：`=` 在字典序与数值序下结果一致（同一个数只有一种文本写法
时），改了只增加风险面。`LIKE` 更不能改：它本来就是文本运算。

**已知代价**（写进代码注释，不藏）：整数列走 `toFloat64OrNull` 后，**绝对值超过 2^53**
的取值会在比较前丢精度。这是拿「绝大多数场景正确」换「极端值正确」，因为不修的
字典序错误是**必然**发生的，而 2^53 以上的整数主键极罕见。

判定列类型靠 `column_types(table)`（复用 `table_columns` 的元数据查询）；**元数据查询
失败或列名对不上时保守走字典序**——一次元数据抖动不该让整页筛选报错。加了两条纯函数
测试（数值族识别、双条件判定），`duck-db` 147 条全绿。

### 4.10 连接钩子（2026-10-13 新增，对齐 DBFlux 四相位）

**定位**：在建连 / 断开的**前后**跑一条**本机命令**。典型用途是开 VPN、切 DNS、
把这次连接写进**你自己组织的**审计系统、断开时收尾。（本应用自己的审计流水见 §4.12，
那是另一回事：它记的是「应用里发生过什么」，钩子看到的是「本机发生了什么」。）

**内核**（`duck-db/src/hooks.rs`，零 gpui、零新依赖）：

| 项 | 取值 | 理由 |
| --- | --- | --- |
| 相位 | `PreConnect` / `PostConnect` / `PreDisconnect` / `PostDisconnect` | 与 DBFlux 同形；`key()` 是稳定英文键（持久化 / 元素 id / 环境变量共用） |
| 载体 | `Command`（`sh -c '<行>'`）/ `Script`（`<解释器> <文件>`） | **不**在进程内跑 Lua/Python：那需要再引一个运行时，留给「脚本文档」那件独立的事 |
| 失败策略 | `Warn`（默认）/ `Block` | 钩子是用户自己写的，打错一个字不该让生产库连不上；要阻断得显式选 |
| 超时 | 默认 30s，可逐条覆盖 | 钩子挂住 = 连接挂住，必须先于连接本身了断 |
| 输出 | 重定向到**临时文件**，截 8 KiB | **不用管道**：钩子若把写端交给后台孙进程，管道读端永远等不到 EOF（join 卡死） |

**三条纪律**（写进模块文档，也在测试里钉住）：

1. **口令绝不进子进程环境**。环境变量只给「这条连接是谁」——`DUCK_HOOK_PHASE` /
   `DUCK_CONN_ID` / `DUCK_CONN_NAME` / `DUCK_CONN_KIND` / `DUCK_CONN_HOST` /
   `DUCK_CONN_PORT` / `DUCK_CONN_USER` / `DUCK_CONN_DATABASE`。需要凭据的钩子自己去
   凭据文件取（`vault`），不由我们把口令递出去。
2. **环境按「继承 + 覆盖」**：钩子该看得见用户的 `PATH`，只有上面这几个键我们说了算。
3. **被信号杀死 ≠ 退出码 0**：`status.code()` 为 `None` 时如实报 `Failed{code: None}`。

**应用层的时序**（`duck-app/src/pages/db.rs`）：

- **建连**：`PreConnect` → 连（隧道 / 连接 / 只读档 / 目录内省）→ `PostConnect`。
  三段在**同一个后台闭包**里跑（`spawn_db` 的 work 侧），界面不卡。
  产物是 `ConnectAttempt{pre, post, session, error}` 而不是 `Result<Session, DbError>`——
  「钩子没放行」与「引擎连不上」是**两种处置**（前者去改钩子，后者去查网络 / 凭据），
  塞进一个 `Result` 会让提示分不开。
- **建连前被 `Block`**：**连都不连**（不是「连上了又关掉」——那会在服务端留一条认证记录）。
- **建连后被 `Block`**：会话丢掉（socket 随之关闭），这次「建连」整体不算数。
- **断开**：`PreDisconnect` → **拆会话** → `PostDisconnect`。
  `PreDisconnect` 的语义是「连接还活着时跑」，所以钩子必须在会话被移除**之前**发生：
  `close_tab` 因此把真正的拆除让给 `teardown_session`，钩子跑完在回调里再调它。
  没有钩子（绝大多数情况）走同一条同步路径，行为与从前一字不差。
- **没有活跃会话就不跑断开钩子**：钩子不该为一个从未建立（或早掉了）的连接跑。
- **测试连接不跑钩子**：它只是一次探活，不改任何状态、也不该触发副作用。
- **删档案时先拆会话再删档**：否则档案已不在，`close_tab` 查不到钩子，等于静默跳过
  用户配好的收尾动作。

**可观测性**：每次执行逐条 `tracing::info!`（字段：connection / hook / phase / command /
status / duration_ms），同时在 `DatabasePage::hook_log` 留一份**会话内**环形缓冲（上限
200）。缓冲渲染在**实例页的「仪表盘」**上——那一页的定位正是「这条连接的构成与最近做过的
事」，而钩子是静默跑的，平时没有任何界面会提到它。

⚠️ **钩子的成功执行不进审计流水**（§4.12）：一条连接每次建立 / 拆除都会把**每个**钩子跑
一遍，全记「成功」只会把该看见的淹掉；而钩子唯一能改变连接结局的那一次（挡住 / 失败）
已经由连接路径记成 `Blocked` / `Failed` 了。钩子执行历史留在上面那两处。

**编辑界面**：连接详情弹窗新增 `HOOKS` 分区（在 `TRANSPORT` 之后）。一行 = 启用开关 /
相位下拉 / 载体两档 / 内容（命令一行，或「解释器 + 文件」两行）/ 工作目录 / 超时 /
失败策略下拉 / **试跑** / 删除。「试跑」在后台**单跑一次**（不建连、不落状态），结果就地
显示状态 + 耗时 + 输出首行。表单里持的是 `HookRow`（工作副本，含 `Entity<InputState>`），
点保存才经 `to_hook` 写回档案；**启用的钩子没填完会被拦下**（存一条永远跑不起来的命令，
错误会推迟到建连那一刻才暴露）。

### 4.11 传输四档：直连 / SSH 隧道 / 代理 / AWS SSM（2026-10-13 新增，对齐 DBFlux）

**定位**：把「连接怎么出去」这件事从「只有一条 SSH 隧道」扩成**四档**，其中代理与 SSM
是这次新增的。四档对内都只暴露**一个本机 `127.0.0.1` 端口**，所以驱动层完全无感。

| 档 | 载体 | 新增依赖 | 备注 |
| --- | --- | --- | --- |
| 直连 | — | 0 | 默认 |
| SSH 隧道 | 用户本机 `ssh -N -L` | 0 | §4.3 已有 |
| 代理 | **进程内手写** SOCKS5 / HTTP CONNECT | 0 | `duck-db/src/proxy.rs` |
| AWS SSM | 用户本机 `aws ssm start-session` | 0 | `duck-db/src/ssm.rs` |

**四档互斥，优先级 `SSM > SSH > 代理 > 直连`**，规则**只写在**
`ConnectionConfig::transport()` 一处（界面标签、状态卡、建连路径都问它）。
「配了但填不全」的档位**不算生效**（SSM 没填实例 id、代理没填主机…），并且
`has_incomplete_transport()` 能把这种情况问出来给界面提示 —— 否则表现是
「用户以为在走代理，实际直连」，那是所有失败里最难查的一种。

为什么不做「叠加」：真叠加要「让 ssh 子进程先经代理出网」这类链路改造（`ProxyCommand`
依赖系统有 `nc`/`socat`，两者都不是 macOS 标配），收益远小于复杂度。取舍写在这里，
界面上也直说「同时启用多档时只有优先级最高的一档生效」。

**代理内核的取舍**（`proxy.rs`，零依赖）：

- **CONNECT 一律用 `ATYP=0x03`（域名）**：代理侧本来就要按名字解析，交给它更符合预期，
  也避免我们这一侧先解析一次、拿到一个不代表代理视角的 IP。
- **不代理 UDP**：数据库协议都是 TCP；做 UDP 会引入一套并行的会话管理，没有用户。
- **要认证却没配口令 → 如实报错，绝不退回匿名**：静默退匿名会连着成功、却走了一条
  用户没预期的路径（出口 IP 不对），比连不上更难发现。
- **握手完成后撤掉读写超时**：隧道是长连接，留着握手期那 15 秒的期限会周期性掐断正常查询。
- **每条入站连接单起线程**：转发本身是两次 `io::copy`，没有共享状态；`is_alive()` 只看
  一个原子标志，`Drop` 时置位并唤醒 accept 循环。

**AWS SSM 的取舍**（`ssm.rs`）：**走用户本机已装的 `aws` CLI，不引 AWS SDK**。
与本项目「尊重用户本机工具」的既有口径一致（SSH 走 `ssh`、编辑器走系统默认），
也意味着**本项目不存任何 AWS 凭据** —— `aws sso login` / `aws configure` 由用户自己管。
文档名按有没有 `remote_host` 分叉：有 → `AWS-StartPortForwardingSessionToRemoteHost`
（`host` 参数指远端主机），没有 → `AWS-StartPortForwardingSession`（实例自身）。
轮询按 **100 × 200ms（约 20 秒）**：SSM 冷启动明显比 SSH 慢，用 SSH 那套 5 秒会误判。

**可复用传输档案**（对齐 DBFlux 的 reusable tunnel/proxy profile）：

- 存 kv 单键 `db_tunnel_profiles`（`Vec<TunnelProfile>`，范式同 `db_connections`）；
  连接档案里只留一个 `tunnel_profile` 引用 id。一份公司 SOCKS5 出口被五条连接引用时，
  换主机 / 轮换口令只改一处。
- **档案是独立凭据主体**：口令存 `dbprofile:{profile_id}:ssh` / `:proxy`，**不是**
  每条引用它的连接的 `dbconn:{id}:…` 上。从连接的键上读会得到一份别的口令（或 `None`），
  表现成「档案里的代理明明能通、测试却认证失败」。
- **展开语义是「整体替换」而不是逐档合并**：档案里存在哪几档就用哪几档，连接自己那三档
  本次不参与。合并会得到「两份都成立」而优先级只挑一个，结果不可预期。
- **引用的档案不存在时回落连接自己的三档**，**绝不静默直连**。
- **删档案连清引用**：把引用它的连接的 `tunnel_profile` 一并置空并重存，界面回到
  「自定义」这个诚实的状态（而不是指着不存在的 id，让用户以为还在走档案）。

**应用层的落点**（`duck-app/src/pages/db.rs`）：

| 作用 | 函数 | 说明 |
| --- | --- | --- |
| 展开三档（纯） | `expand_tunnel_profile(cfg, profiles)` | 返回展开结果 + 命中的档案；界面判据用它 |
| 展开 + 换凭据 | `resolve_transport(cfg, profiles, creds)` | 建连路径用它（多说一句凭据的事） |
| 建通道 | `open_transport(cfg, creds)` | 按 `transport()` 落地成进程 / 监听端口，返回「指向通道入口的副本」 |
| 建连 | `open_session(cfg, creds)` | 传输通道 → 连接 → 只读档 → 目录内省 |
| 探活 | `probe_connection(cfg, creds)` | 同一套通道逻辑；**不跑钩子**（见 §4.10） |

`Credentials { db, ssh, proxy }` 取代了原来一路传的元组：三份 `Option<String>` 并排之后
光看调用点已经分不清谁是谁，而**送错口令**的后果是「认证失败」这种最难查的错。

`Transport` 枚举（`Ssh` / `Proxy` / `Ssm`）替代了原来的 `Option<Arc<SshTunnel>>`：
三者对内只暴露 `local_port()` / `is_alive()` / `label()`，界面因此不需要分支就知道
「在走哪条路、还通不通」。

**可观测性**：连接信息卡多一行「接入方式」，通道死了再补一行
「通道状态：已断开（请重新连接）」—— 通道进程退出而会话还挂着时，表现全是「查询超时」，
用户会去查库、查网络，最后才想到「ssh 早就退出了」；仪表盘另有「传输通道」卡
（接入方式 / 本机端口 / 存活状态 / 配置没填完的提示）。

**编辑界面**：`TRANSPORT` 分区顶部新增「传输档案」下拉（`自定义` = 用下面的配置，
其余是各档案），选中档案时**下面三档就是这份档案的内容**（保存连接时一并存回档案），
另有「存为新档案」/「删除这类档案」。三档各自一个开关 + 展开字段；
代理口令与 SSH 口令同一条纪律：**只进凭据文件**，表单留空 = 保持既有。

### 4.12 审计日志与错误关联 id（2026-10-13 新增，对齐 DBFlux）

**定位**：一张**只追加、可裁剪**的事件流水，回答「这条库被谁、什么时候、做了什么，
哪些没做成」。与 `tracing` 日志的分工：日志给**开发者**看（会滚掉、可能被关），
审计给**用户**看（落 SQLite、能在设置页按类别 / 结果 / 关联 id 筛）。

**表**（`duck-store`，与 `kv` 同库；形状见 `crates/duck-store/src/audit.rs`）：

| 列 | 说明 |
| --- | --- |
| `id` | 自增；裁剪按它倒序留最新 |
| `at` | Unix 秒，**由写线程在落盘那一刻统一取**（免得各调用方各算一遍「现在」） |
| `kind` | 七类之一：`connection` / `query` / `write` / `transport` / `hook` / `plugin` / `mcp` |
| `action` | **稳定英文键**（`connect` / `probe` / `open_transport` / `execute` / `commit` / `edit_row` / `delete_rows` / `browse` / `explain` / `run_hook` / `invoke` / `clear` …） |
| `outcome` | `ok` / `failed` / `blocked` / `cancelled` |
| `scope` | 事件挂在谁身上：连接级 = 连接档案 id，语句 / 写 / 浏览级 = **会话** id |
| `detail` | 明细，界面侧已 `tr`（库里存的是**写入那一刻那种语言**的成品串） |
| `fingerprint` | 语句形状指纹（字面量已抹掉），用来归类「同一类语句」 |
| `duration_ms` | 耗时；**失败也带**——「立刻返回的错」与「跑满 30 秒才超时」是两件事，而错误文本里都没有这个信息 |
| `trace_id` | 只有**问题**（非 `ok`）才有的关联 id，形如 `t-1a2b3c4d5e6f` |

**四条口径**：

1. **类别封闭、动作开放**。`AuditKind` 是枚举（多一类要动代码），`action` 是「常量 +
   文案表」（`crates/duck-app/src/audit.rs::action` + `ACTIONS`）——加一个功能就多一两个
   动作，不该为了一个动作去改枚举。守卫测试钉住「每个常量都有文案」：少一条，那个动作
   在界面上就永远显示英文键。
2. **口令绝不进 `detail`**。审计是明文落盘、而且用途就是被人读 / 复制出去的表；只记
   「哪条连接 / 哪张表 / 什么结果」。口令只经凭据文件（`vault`）进内存。
3. **记「用户动作」，不记「每一次函数调用」**。打开一张表记一条，同一次浏览的翻页 /
   排序 / 过滤不各记一条，写之后的自动重载也不算一次「浏览」（`browse_open_pending`
   这个一次性标记就是干这个的，与 `plan_next` 同一种手法）。反过来：**失败一律记**
   （罕见，且是排查的第一手线索），**取消只在 SQL 执行上记**（翻页点快了产生的取消是
   导航噪声，不是事件；而「一条跑了一半的写语句被中断」是真事件）。
4. **写路径全部落流水**：暂存区提交（`commit`）、批量改列（`edit_row`）、批量删行与
   单行删除（`delete_rows`）、SQL 执行（`execute` / `commit` 按语句风险分）、查看执行
   计划（`explain`）、打开表（`browse`）。失败的那一次也记——否则流水里只看得到成功的，
   「这个时刻有过一次没写成的提交」就查不出来。

**裁剪**：`Op::PushAudit { entry, keep }` 写入时**顺带**把表裁到 ≤ `keep`（所以表永远长不
起来）；另有 `Op::TrimAudit { keep }` **只裁不写**，给「上限被调小」用。分开是必要的而不是
对称强迫症：只有它存在，才可能出现「裁剪没有任何写入陪着」的情况；没有它，用户把 2000
改成 200 之后列表里还是那 2000 行，得等下一次写入才缩——看起来就是「改了没反应」。
同批里两者取**更宽松**的那个（`audit_keep.max(audit_trim)`）：上限调小是用户意图，但不该
把同一批里刚记下的记录砍掉。

**关联 id 与状态栏徽标**：`record_entry` 发现结果不是 `ok` 就生成一个 `t-…` id 并返回，
调用方把它拼进错误提示（`failure_message`：`<什么失败>（<关联 id>）：<原因>`）；同时把
「最近一次失败 + 自上次查看审计页起累计条数」记进内存里的 `PendingFailure`，主窗状态栏
出现一个失败色（`status_failed`）徽标（点它 = flush → 清徽标 → 跳到审计页并**按这个 trace
过滤**）。存计数而不是
只留最近一条：只留一条的话，看完一条之后剩下的几条就凭空消失了，而那正是最该被提醒的漏。
⚠️ 徽标状态用 `update_default_global` 而不是 `update_global`——后者在全局**还没被创建**时
会 panic，而「还没失败过」正是最常见的初始状态。

**⚠️ 读之前必须 flush**：审计的写是去抖的（400ms 窗口），进审计页 / 点徽标都要先
`duck_store::flush(cx)`，否则会看到一张缺了「刚刚那次」的列表。

**界面**（设置 › 系统 › 审计日志）：筛选四件（类别 / 只看问题 / 关联 id / 保留条数）+ 清空；
下面一行一条流水，四档结果色（`success` / `status_failed` / `warning` / `text_tertiary`），
列宽**固定**——流水是要**竖着扫**的，列宽随内容浮动会让同名字段落在不同的水平位置上。
清空**本身也记一笔**（同一批里「清空」先于写入执行，所以这条不会被自己擦掉），否则
「谁把痕迹擦了」就查不出来。列表是**快照**（`audit_rows`）：查一次要走读连接 + `LIMIT 200`
排序，快照把开销压到「筛选变了才查」，代价是必须显式 `reload_audit`（进页 / 改筛选 /
再点一次当前导航项——在流水页上「再点一下当前项」就是很自然的刷新）。

**⚠️ 已知缺口**：整包导出（`backup.rs::write_archive`）带的是 `kv` + 键位（+ 插件 /
日志 / `ai.db`），**不含 `audit` 表**——「留证」这条链还差一步（审计目前既没有独立导出
入口，也不跟随备份）。所以「保留条数」那条提示语说的是「要长期留证就把上限调大」，而不是
「去导出」：界面文案不许承诺一个不存在的功能。

### 4.13 右栏的自动展开 / 收起（2026-10-14 新增）

**问题**：右栏（`with_right_sidebar`）是 **shell 级**的 —— 一条路由一个 `Entity<SidebarState>`，
页面既拿不到它的展开态、也改不动它。于是「点按钮 → 内容落在右栏」这一类入口在右栏恰好收着时，
用户什么都看不见。此前 db 页唯一的兜底是往状态栏写一句「查询历史已切到右侧栏。」（代码注释
原话：*状态栏那句话是唯一能让「右栏恰好收着」这种情形下也看得见反馈的地方*）—— 文案看得见，
内容仍然看不见；单元格详情（网格行尾 ⓘ / 右键「单元格详情」）连文案都没有。

**修法**（三层，零新增依赖）：

| 层 | 位置 | 内容 |
| --- | --- | --- |
| shell | `duck-shell/src/view.rs` | `active_right_sidebar_open(&self, cx) -> bool`（只读；**不**复用 `active_right_sidebar()` 的 `.expect` —— 它在点击处理器里被调用，无活跃路由时按「收起」回答优于 panic）；`set_active_right_sidebar_open(open, cx)`（正向极性别名，内转既有的 `set_active_right_sidebar_collapsed(!open)`，避免调用点写双否定） |
| app | `duck-app/src/app.rs` | `right_sidebar_open(cx) -> Option<bool>` / `set_right_sidebar_open(cx, open) -> bool`，与 `navigate_to` / `open_tool` 同层，走 `Desktop::main_views` → shell，**不新增全局状态** |
| 页面 | `duck-app/src/pages/db.rs` | `reveal_right_dock(dock, cx)` / `dismiss_cell_dock_if_auto(cx)` + 页面级 `right_dock_auto: bool` |

**交互口径**（核心是「不跟用户对抗」—— 只有右栏是**页面替用户展开**的，页面才会自动收起它）：

1. 工具条「历史」按钮在**右栏已展开且正显示历史**时再点 = **收起右栏**（对齐 IDE 工具窗开关
   习惯）；右栏收着、或当前停在别的分段时 = 展开并切到历史。想换右栏内容走顶部的分段 tab，
   不再让这一个按钮兼任「切回详情」与「收起」两种相反语义。
2. 单元格详情失效（执行新查询 / 切换结果标签 / 关掉最后一条结果）→ **自动收起右栏**，但**仅当**
   「右栏正显示单元格详情、且它是页面替用户展开的」。用户自己展开的、或右栏正显示别的分段
   （历史 / 详情）的，原地留下 —— 页面绝不关用户手动开的右栏，也不该因为一次查询清空了格子
   就把正在看的历史一起收走。
3. 用户在右栏里点分段 tab = 右栏归用户，`right_dock_auto` 清零，此后页面不再自动收它。
4. **状态联动**：右栏的开合必须**在按钮 / 图标上看得见**，否则收起之后用户以为它还开着 ——
   - 工具条「历史」按钮的高亮 = 「右栏开着 **且** 停在这一栏」，右栏一收起就灭。
   - 顶栏的右栏开关在展开态给一块选中底（`nav_selected` 底 + `nav_selected_icon` 图标色，
     这一对对比度 ≥ 3:1，由无障碍守卫 `selected_nav_icon_meets_non_text_contrast` 钉住），
     收起态回常态。只改 tooltip 文案不够 —— 指针不悬停时用户看不出开合。
   - ⚠️ **页面实体是 `AnyView`，shell 重绘不连带重绘它**（子视图有自己的渲染缓存；这也正是
     右栏内容走 `Fn(&App) -> AnyElement` 闭包、而主内容走 `AnyView` 的原因）。所以「按钮
     高亮跟着右栏走」只能由页面自己 `cx.observe` 右栏实体
     （`app::right_sidebar_state` → `ShellView::active_right_sidebar_state`）、在回调里
     `cx.notify()`，且**只订阅一次**（路由与右栏实体都常驻，重复订阅只会堆回调）。

**⚠️ 已知耦合**：右栏展开态是**全局单键** `window_right_sidebar_open`（`settings.rs:32`），经
`on_right_sidebar_changed` → `save_right_sidebar_open` 落盘。所以页面自动展开会被持久化 ——
下次启动在任何路由右栏都是展开的，与现在手动点开关的行为一致。改成按路由存是另一个决定。

**✅ 组件层保障（宽度与不透明平面）**：右栏「展开后背景透出主内容」**不是页面层的问题** ——
侧栏宽度钳制（不超过容器 60%）、内容面板留 240px 下限、以及**不透明 chrome 平面**三件事，
统一由 `duck-components::SidebarPanel`（`crates/duck-components/src/sidebar.rs`）保证，
配套 `tests/sidebar_layout.rs` 钉住 900 / 700 / 600 / 500 四个宽度下的分配。
页面（db 页含 `dock_body`）**不重复实现，也不该假设自己能拿到右栏实际宽度**。

### 4.14 结果区「来源语句」chip：可点击复制（2026-10-14 新增）

**对位**：dbx 结果底栏那个「SQL 结果」tag 实际是**可点击的 SQL 单行文本**（左键复制该 SQL、
悬停看全）。本仓的对应点是结果区**读数条**（`resbar`）里的「来源语句」段 —— 而**不是**
编辑器工具条右组那个 `neutral_tag("SQL 结果")`（那是模式标签：查询 / 表浏览 / 实例档 /
仪表板，语义不同，保持不可点）。

**实现**（`db.rs::result_sql_chip`）：

- **显示**用单行化文本（`split_whitespace()` 合一，换行与缩进压成一个空格）；**复制**拿**原文**
  （含换行与缩进）—— 粘回编辑器能直接用。截断只影响显示，完整语句在悬停 `Tooltip` 里。
- **取数零成本**：`query_sql` 由 `install_query_result` 写入，**恒等于当前结果标签的语句**，
  于是多语句批次切标签时「显示与复制的内容」自动跟着换（与 `query_result` 同一套设计）。
- 没有结果时不渲染（`.children()` 摊平 `None`）—— 空态不摆一个点了没反应的壳。

**「已复制」这类动作回执的归宿**：dbx 是底部居中 toast（2s 消失）。本仓当时定为「一次独立
重构」并推迟；**2026-10-14 已落地**，口径与实现见 **§4.15**。关键是先划清范围：只有
**动作回执**走 toast（复制 / 格式化 / 压缩 / 注释），**查询生命周期读数**留在状态栏。

### 4.15 工具条对位 dbx 三改：运行内联选区 / Tx 两态按钮 / 底部 toast（2026-10-14 新增）

本轮把 dbx 差异清单里用户拍板的四项一次收口（第 1、2、3、4 项；第 4 项见本节末）。

#### 4.15.1 「运行」内联选区判断（删掉独立的「选中执行」）

dbx 的工具条**没有**独立的「选中执行」按钮：主按钮自己判断 —— 有选区跑选区、没选区跑全部。
本仓第 9 轮曾按设计稿加了那枚钮，本轮按 dbx 口径**删除**。

- `run_query`（快捷键 ⌘↵ 与主按钮共用的唯一入口）现在**选区优先**：`editor_selection()`
  非空就只跑那一段（`stmt_index = None`，选段可以是半条语句、不对应语句槽），否则跑全文。
- 原 `run_editor_selection` 整函数删除（逻辑并入 `run_query`）；危险闸门在两条路上共用
  ——「只是选了一段」不是跳过确认的理由。
- **提示即口径**：删掉按钮后，「这一下跑哪一段」只由 tooltip 说明，所以 `run_tip` 随选区
  动态取值：有选区 `只执行选中的语句`，无选区 `执行全部语句（⌘↵）`。

#### 4.15.2 「执行计划」支持选区（不再只能看编辑器里的那一条）

原先 `explain_editor_sql` 恒取编辑器**全文**，而 `explain_sql_for` 只接受**单条**语句 ——
于是编辑器里写了两条语句就永远只能看计划失败（`执行计划一次只看一条语句。`）。
现在同 `run_query` 口径：**选区优先**，选中一条就只看这一条。

#### 4.15.3 Tx 两态按钮（内核新增 `TxControl`）

**内核**（`duck-db::sqlintel::TxControl`，纯函数 + 单测）：

| 动作 | SQLite / PG / CH 等 | MySQL |
| --- | --- | --- |
| `Begin` | `BEGIN` | `START TRANSACTION` |
| `Commit` | `COMMIT` | `COMMIT` |
| `Rollback` | `ROLLBACK` | `ROLLBACK` |

配 `TxControl::effect()` → `TxEffect`，与既有的 `tx_effect()`（**读**语句判断状态）构成闭环：

- `tx_control_statements_round_trip_through_tx_effect`：7 引擎 × 3 动作，造出来的语句必须被
  `tx_effect` 认出来。**这是这条路径上唯一的致命失败** —— 认不出来，点一下 `in_tx` 不变，
  界面就卡在旧态。
- `only_begin_is_dialect_specific`：只有「开启」按引擎分叉。

**界面**（`db.rs`）：工具条右组的 Tx **徽标改成两态按钮** `db-tx-toggle`：

- 自动提交态（Ghost，`Tx · 自动提交`）→ 点击发 `Begin`；
- 事务中态（Secondary，`Tx · 事务中`）→ 点击发 `Commit`；
- **回滚不在这枚钮上**：它更少用、也更该刻意决定，留在状态栏（那里另有「事务中」读数与
  提交 / 回滚两个出口）。

语句走**既有的** `execute_sql`，因此取消 / 超时 / 审计 / `in_tx` 状态更新全部复用同一条路径。

#### 4.15.4 底部居中 toast（动作回执）

**分工**（这是本节唯一需要记住的口径）：

| | 状态栏 `self.status` | toast |
| --- | --- | --- |
| 内容 | **常驻读数**：正在执行… / 查询完成 / 连接状态 / 就绪 | **动作回执**：已复制该行。/ 已格式化。/ 已注释 N 行。 |
| 去向 | 一直挂着，被下一条读数替换 | 约 **2 秒**自动消失 |

为什么不把 48 处 `self.status = …` 全改掉：其中多数是**查询生命周期读数**，本来就该常驻；
本批只把**动作回执**那一族改走 toast（`put_on_clipboard` / `format_editor_sql` /
`minify_editor_sql` / `toggle_comment_editor_sql`，共 4 个入口、覆盖全部复制与编辑器动作）。

**实现**（`db.rs`）：

- `Toast { message, tone, seq }` + `DatabasePage::toast()`；`seq` 是自增序号，定时器到点时
  **只有 `seq` 还是自己那条才清** —— 否则两秒内连点两次，「已复制」的定时器会把第二条
  （如「已格式化」）一起抹掉，第二次等于没弹。后来的 toast 直接**替换**前一条，页面上永远
  只有一枚（不堆叠）。
- 浮层 `render_toast()`：`.absolute().bottom(status_bar + lg).left(0).right(0)` + 内层
  `justify_center`，锚在**状态栏上沿之上**居中。外层容器不接鼠标事件（无 hitbox），
  所以底边这条浮层**不挡下面网格的点击**。
- 配色用既有的 `extended.colors.chrome` 平面 + `hairline` 描边 + `shadow_md()`，与状态栏 /
  工具条同一套「浮起的 chrome 平面」，不引入深色药丸（那是 dbx 的皮肤，不是本仓的语言）。
- 语义档 `notify::Tone`：复制 / 格式化 / 压缩 / 注释用 `Success`；唯一例外是「已格式化；N 段
  解析不了」用 `Warning` —— 那是要用户**留意**的。

### 4.16 结果区对位 dbx：tab 栏 / 工具条 / 列宽拖拽 / 分页跳页 / 格式化选项（2026-10-14 新增）

用户反馈的五点（结果表格不能调宽、表头与 dbx 不一致、「SQL 结果」按钮点不动、格式化没有
换行缩进、缺「小写关键字 / 自动换行」）本轮一次收口。

#### 4.16.1 结果区 tab 栏 + 常驻动作组

`db.rs::ResultAreaTab`（`Data / Summary / Chart / Messages / Plan`）+ `render_result_tabbar`：
左侧五档 tab 换掉网格本体，右侧动作组把原「编辑条」的写操作**提到常驻位置**（刷新 / 保存结果 /
新增行 / 删除行 / 提交 / 回滚）。**不可用时置灰而不是消失** —— 按钮忽隐忽现会让工具条自身的位置
也跟着跳。

- 「数据表」= 网格本体（原行 / 列 / JSON 三档 `ResultViewMode` 在这里生效）；「执行计划」的
  **原始输出**档也复用网格（计划树档另画缩进树）。
- 摘要 tab：总量读数（行数 / 列数 / 耗时）+ 逐列清单（列名 / 类型 / **空值计数**）。空值计数
  **一次性预计算**（1 万行 × 20 列 ≈ 20 万次分支，微秒级），不放进逐帧渲染路径。
- 消息 tab：`ExecLogEntry` 执行流水（新在前，`EXEC_LOG_CAP = 200` 裁头）。成功 / 失败由
  `extended.colors.success` / `status_failed` 分色。
- **去掉了「SQL 结果 / 表浏览」两个 `neutral_tag`**：它们本来就是不可交互的徽标（用户报告
  「点了没反应」），当前模式已由 tab 栏与界面形态自明。

⚠️ `uniform_list` 的渲染闭包是 `'static`，跨不进去 `tokens` / `extended` / `cx`：摘要与消息两个
视图都按网格既有口径**先把 `Copy` 值拷成局部变量**再进闭包（否则报 `borrowed data escapes`）。

#### 4.16.2 列宽拖拽（表头右缘 4px 把手）

- `COLUMN_RESIZE_HANDLE = 4.`；把手**绝对定位挂在列头行**上（不是塞进列头格 —— 列头格自带
  `overflow_x_hidden` 负责列名截断，嵌进去的把手会被裁掉一半而够不着），并在 `.children()`
  里**最后追加**：绘制顺序靠后 = 优先接到拖拽，否则列头自己的「点一下排序」会吃掉按下的那一下。
- 拖拽载荷 `ColResizeDrag { col }`（照 gpui-component Table 的做法，`on_drag` + `on_drag_move`，
  只认这个类型，别的拖拽不会误触）；页面侧状态 `col_resize: Option<(usize, f32, f32)>` =
  `(列, 起拖指针 x, 起拖列宽)`。**只记位移**：`新宽 = 起宽 + (当前 x − 起拖 x)`，与网格在窗口里的
  位置、横向滚动偏移都无关，滚动条拖到一半也不会跳。
- 手动拖拽上限是 `COLUMN_RESIZE_MAX = 1200`（**不是**估宽用的 `COLUMN_MAX_WIDTH = 260`）：
  估宽夹在 260 是为了「一屏塞得下」，手动拖的人明确要放宽看长文本。
- 双击把手 = 按内容自适应该列（与初始估宽同一套 `compute_column_widths` 口径）。
- 拖拽结果写进 `SessionView.column_widths` → `result_widths()` → 「字段筛选 / 跳转列 / 网格」
  三处共用同一份宽度（不搞第二处真源）。

#### 4.16.3 分页支持跳页

`page_jump: Entity<InputState>` 由查询（客户端分页）与表浏览（服务器侧分页）**两套翻页组共用**；
回车触发 `jump_to_page`（1 起页码，空 / 非数字 / 0 一律当没输入 —— 静默跳第 1 页会让人以为
「按了回车页面乱跳」）。查询模式改序号即刻生效；表浏览改 `table_page` 后走 `load_table_page`
重新按新 `OFFSET` 取一页。用完即清（一次性输入）。**不订阅 `Change`**：边打边跳会在输入「12」
的中途先跳到第 1 页。

#### 4.16.4 SQL 格式化：接入 `sqlformat` + 四个可配档

- **依赖**：`sqlformat = "0.5.0"`（`rust-version = 1.84` ≤ MSRV 1.85，MIT OR Apache-2.0，
  默认无 feature；传递依赖 `winnow` 已在图内，新增 `unicode_categories` 一个小包）。
  过「依赖准入三关」，`THIRD_PARTY_LICENSES.md` 已重生成。
- **内核**（`duck-db::sqlintel`）：`SqlFormatOptions { keyword_case, indent_tabs, indent_width }`
  + `format_sql_with`；`format_sql` = 历史默认（**关键字大写**，空格缩进 2）。**解析门仍保留**：
  解析得了的才交给 `sqlformat` 排版，解析不了的**原样保留**并记进 `FormattedSql::kept_verbatim`
  —— 宁可不动，也不要把用户手上跑得通的半截语句改成跑不了的。
- **真源**：设置页 `DbSettings`（`sql_keyword_case` / `sql_indent_tabs` / `sql_indent_width` /
  `editor_word_wrap`），全部 `Option`/`serde(default)` + 默认值回落既有常量。
- 格式化完成后按「有没有解析不了的段落」分 `Success` / `Warning` 档 toast（`kept_verbatim`
  非空 = 要用户留意）。

#### 4.16.5 工具条新增两个开关（纯图标 + 悬停提示）

`opt()` 辅助（紧挨 `tb()`）：**开 = `Primary`、关 = `Ghost`**，状态由按钮底色体现，文字只进
无障碍名与 tooltip，不占横向宽度（这一行已经有十来个按钮）。

- **小写关键字**（`DuckIcon::CaseLower`）：在大写 ⇄ 小写之间切换，写的是设置页同一个键；
  **只改「格式化时用哪种大小写」，不动编辑器里已有的文本** —— 想看到效果再点一次「格式化」。
- **自动换行**（`DuckIcon::WrapText`）：`EditorState::set_soft_wrap`。⚠️ 这个入口来自
  `impl<M: MultiLineMode> InputBaseState<M>`（`EditorMode: MultiLineMode`，见
  `gpui-base/src/input/base/kind.rs:67`），**不需要 vendor 补丁** —— 早前误判为只有
  `InputMode` / `TextareaMode` 有，写了重复定义才发现。
- 两个图标（`case-lower` / `wrap-text`，Lucide ISC 子集，线宽 1.75）已并入 `DuckIcon`。

#### 4.16.6 回归修复：提示只能挂一次（2026-10-14 当晚）

上面这两处开关（以及结果区动作组）上线的当口，**打开数据库页直接 panic 秒退**：

```
calling tooltip more than once on the same element is not supported
```

根因：`duck_components::icon_button` **内部已经**用 `with_icon_tooltip` 挂了提示
（`crates/duck-components/src/lib.rs:52`），而调用点又补了一次 `.tooltip()`。
gpui 对同一元素二次挂载是**硬 panic**（abort 级），编译期不报，只有真渲染到那个元素才炸。

改法（组件层收口，让调用方没有「再挂一次」的机会）：

- `icon_button_frame` → **只做外观与无障碍名，不含提示**；
  `icon_button` = frame + 提示（用 label）。
- 新增 `icon_button_tip(id, label, icon, tip, variant, cx)`：无障碍名用 `label`（简短），
  提示用 `tip`（可带状态 / 动态读数）。
- `db.rs` 的 `opt()` 与 `act()` 改走 `icon_button_tip` / `icon_button`，**不再挂第二次**；
  提交 / 回滚的动态提示（`提交 N 条暂存改动`）改由 `tip` 参数传入（新增 `act_tip` 辅助）。
- 约定已写进 `AGENTS.md` §3.6：**图标按钮的提示由组件内部挂**，调用点不要再挂。

静态核对口径：`db.rs` 里凡出现 `icon_button(` / `icon_button_tip(` / `toolbar_action_button(`
的语句，其后 18 行内不得再出现 `.tooltip(`（改用脚本扫，当前 0 命中）。

#### 4.16.7 结果区二轮调整（2026-10-14 晚，用户截图反馈）

1. **残留编辑条整条移除**：上批把写操作提到 tab 栏动作组后，旧「编辑条」（新增行按钮 +
   排序读数 + 待提交组）成了重复入口。`render_edit_bar` 整函数删除；其中**唯一不重复**的
   排序读数（`按 {} 降序/升序`）迁往读数条（隐藏列读数之后）。
2. **「在结果中搜索」上移到 tab 栏行**：找行是「看着结果」时的动作，跟档位与写动作同排；
   读数条只留事实读数（结果 · N 行 · 耗时 · 已隐藏 N 列 · 排序 · 来源语句）。搜索词仍不进
   快照（`result_search` 页面级单例）。
3. **摘要 tab 重构为两段**：
   - **执行过程**（dbx 摘要同款语义）：状态（成功 ✓；失败不产出结果，走报错条）、耗时、
     行数、列数、已达上限 tag + 来源语句 chip（可复制、悬停看全文）。
   - **列统计**：原逐列清单（列名 / 类型 / 空值计数）原样保留。
4. **执行报错进结果区**（`render_result_error_banner`）：`last_error` 存在时在结果区顶部
   渲染报错条（alert 图标 + `执行失败：{原文}` 全文 + 失败语句单行），三处接入：
   tab 面板骨架（摘要 / 消息 / 图表 / 计划都看得见）、网格路径、**无结果空态** —— 失败后
   结果区不再只是空着，tab 栏照常可切「消息」看流水。编辑器上方那条带 AI 动作的
   报错条保持不变（两条并存：一条给动作、一条给现场）。
5. **对象头条整行撤除**（`render_result_header` 删除，四个调用点收拢）：
   - **对象名**（表名 / 查询目标）迁到 **tab 栏行首**当「标题」（图标 + 名，>320px 截断，
     悬停看全名）；⚠️ **本节已被 §4.16.8 取代**：对象名最终落在**结果标签**上
     （标签名 = `表名-结果`），tab 栏行首不再有标题块 —— 同一信息只留一处。
   - **字段筛选 / 跳转列**两个图标下放到**读数条**右端 —— 顺带在 `toolbar_dropdown_button`
     组件内部补上悬停提示（gpui-component 的 Button 自带 `tooltip(SharedString)` 字段，
     必须在 `dropdown_menu_with_anchor` **之前**给，之后只剩 `DropdownMenuPopover` 包装层；
     它不走 gpui 的 div 挂载，无「二次挂载」panic 之说）。语言切换 / 邮件工具条等既有
     调用点一并受益；
   - **DDL 入口**（表浏览 + 已取到 DDL）迁到读数条右端，仍是文字按钮，`stage_sql` 路径不变。
6. **编辑器工具条整排纯图标**（用户反馈「要用图标就都用图标」）：`tb()` 辅助从
   `icon_text_button`（图标 + 文字）改为 `icon_button` / `icon_button_tip`（纯图标）——
   图标/文字混排让这排忽宽忽窄，也不成体系。运行/停止、格式化、注释、压缩、执行计划、
   保存查询、历史全部转纯图标，`label` 只进无障碍名，原有的 `tip`（动作 + 快捷键）继续
   由 `icon_button_tip` 内部挂；与右侧两个开关（小写关键字 / 自动换行）、查询选项同一
   规格（`density.control` 方形 + 14px 图标）。主按钮「运行」也随图标化（▶），悬停提示
   保留「执行全部语句（⌘↵）」。
7. **去重：结果读数只说一遍**（用户反馈「放在摘要/消息里就不要再显示了」）：
   - **顶部工具栏**撤掉中间的 status 弹性带（`208 行 · 1 条语句 · N ms`）与
     「SQL 结果 / 表浏览」模式标签 —— 摘要「执行过程」与底部状态栏都在说；
     `status` 的操作反馈仍在底部状态栏 + 右上角通知，实例信息页的档位标签保留。
   - **读数条**改为纯「结果标签行」：撤掉 `结果 · N 行`、耗时与来源语句 chip
     （语句在摘要「执行过程」里可复制），保留结果标签、已隐藏 N 列、排序读数、
     字段筛选 / 跳转列 / DDL 入口。⚠️ **后两项已被 §4.16.8 调整**：字段筛选 / 跳转列
     移到档位行的动作组（回滚之后），这一条整条**上移到结果区第一行**。

#### 4.16.8 结果区三轮调整：结果标签置顶 + 标签带表名（2026-10-14 晚，用户截图反馈）

1. **结果标签行上移到结果区第一行**（用户反馈「tab 这行要上移一行，放在顶上」）。
   行序由「档位行 → 结果标签行 → …」改为「**结果标签行 → 档位行 → …**」，四个渲染
   路径**同一口径**，避免各自漂移：
   - 网格路径（`Render` 里的 grid 分支，`panel.child` 两行）；
   - 列模式 / JSON 模式分支；
   - `result_tab_panel`（摘要 / 消息 / 图表三档的公共骨架）；
   - 执行计划树分支（`ResultAreaTab::Plan` 且非「原始输出」）。
   理由与 dbx 一致：`ResultSetNavigator` 在结果工具条的最左侧，视图切换器在其后 ——
   标签说的是「现在看的是哪条语句的结果」，是这块区域的标题，压在档位之前。
   ⚠️ `result_tab_panel` 同时补上了结果标签行（此前只有档位行）：多结果批次切进
   摘要之后既看不见也切不了结果标签，等于「进去容易出来难」。
2. **单结果也显示标签**（用户反馈「点击顶部的执行，不会出现查询结果 tab」）。原条件
   `mode == Query && result_tabs.len() > 1` 里的 `len() > 1` 是错的：一条语句执行完，
   `result_tabs` 恰好 1 条，标签被整排藏掉 —— 用户点「执行」后**看不到结果落在哪条
   语句上**。改为 `mode == Query && !result_tabs.is_empty()`。单标签不是噪音，
   它是这份结果的**身份**（dbx 同样恒摆）。
3. **标签名带目标表：`表名-结果`**（用户反馈「选中的表名不要显示在这个地方，显示在
   结果 tab 上，类似：表名-结果」）。新增 `result_tab_label(index, multiple, cx)`：
   - 表名取 `ResultTab::target`（执行时由 SQL 解析出的可编辑目标表，与结果集来源同一
     份解析），用**裸表名**（`schema` 不进标签，标签格宽有限）；
   - `multiple`（标签数 > 1）时补序号：`{表名}-结果{n}`。同一条 SQL 跑两遍会有两个同名
     标签，光看名字分不出谁是谁；
   - 解析不出目标表（`SELECT 1`、多表连接取不到唯一源、DDL 等）回落 `结果 {n}` ——
     名字退化了，标签本身不消失。
   i18n 新增两条格式键：`{}-结果`（单结果）与 `{}-结果{}`（多结果）。
   口径对齐 dbx 的 `resultTabNamingMode = source`（默认档，取 `resultNamingLabel` 解析出
   的来源表），不是它的 `ordinal` 档。
4. **字段筛选 / 跳转列改挂档位行的动作组**（用户反馈「放在丢弃全部暂存改动图标后面」）：
   从读数条右端移到 `render_result_tabbar` 的动作组末尾（「回滚」之后）。这两件事都是
   「看着结果」时用的网格工具，跟刷新 / 删除 / 提交排在同一条；悬停提示仍由
   `toolbar_dropdown_button` 组件内部挂（§4.16.6 的约定不变）。
5. **空条让位**：`render_result_resbar` 现在会顶在结果区最上面，没内容时不能留一条
   空行 + 一条悬空分隔线。加前置判据 —— 标签（查询模式且已有结果）与读数（藏列 /
   排序 / DDL）两边都没有时，整条返回空 div。表浏览下「没排过序、没藏列、没取到 DDL」
   是最常见的情形，正是这条要挡的。

### 4.17 SQL 补全词表：按引擎分层（2026-10-14 新增）

**问题**：补全的关键字与函数原本是 `db.rs` 里两张**写死的通用表**（`SQL_KEYWORDS` 63 条 /
`SQL_FUNCTIONS` 21 条，偏 SQLite 口味）。在 PostgreSQL / MySQL / ClickHouse / SQL Server
上，用户敲不出 `RETURNING` / `AUTO_INCREMENT` / `PREWHERE` / `TOP`，也敲不出自己引擎的
内置函数 —— 补全等于「只认识通用 SQL」。

**改动**：词表下沉到内核新模块 `crates/duck-db/src/sqlwords.rs`，按引擎给。

选词口径照抄两个参考实现的分层（dbx 的 `DATABASE_SQL_KEYWORDS`、
`apps/desktop/src/lib/sql/sqlCompletion.ts`；dbflux 的 `sql_keyword_candidates`,
`dbflux_ui_document/src/code/completion.rs`）：**通用核心 + 引擎增量**。

| 层 | 内容 | 归属 |
| --- | --- | --- |
| 通用核心 | 五个 SQL 引擎都认得、名字语义都一致的词（129 关键字 + 43 函数） | `COMMON_KEYWORDS` / `COMMON_FUNCTIONS` |
| 引擎增量 | 只有该引擎有、或名字/语义不同的词 | `SQLITE_*` / `POSTGRES_*` / `MYSQL_*` / `CLICKHOUSE_*` / `SQLSERVER_*` |
| 非 SQL 引擎 | 各自查询语言的词 | `MONGO_OPERATORS` + `MONGO_COMMANDS` / `ELASTICSEARCH_KEYWORDS` |

几条**刻意**的取舍：

1. **判不准的宁可下沉**。`GREATEST` / `LEAST` 在 SQLite 与服务端 SQL Server 上不存在 →
   只进 PostgreSQL / MySQL 增量。放通用表里的代价是教人写错：在 SQLite 里补出 `JSONB`、
   在 PostgreSQL 里补出 `AUTO_INCREMENT`，写出来一执行就报错。
2. **ClickHouse 的函数名大小写规则分两档**（已核官方文档与实际行为）：常见**聚合与
   类型转换**函数（`sum` / `count` / `toDate` 那类）注册为大小写不敏感，写 `SUM` 也认；
   **专有**函数（`arrayJoin` / `multiIf` / `toStartOfDay`）大小写敏感，必须照官方拼写。
   所以 `CLICKHOUSE_FUNCTIONS` 只收专有函数 / 官方拼写就是小写或 camelCase 的那些，
   通用聚合交给通用桶 —— 免得菜单里同时冒出 `SUM` 与 `sum` 两个候选（同一件事两份
   写法，用户不知道该挑哪个）。
3. **MongoDB 收命令名，不收 shell 外壳语法**。`drivers/mongo.rs` 的编辑器吃的是**一个
   JSON 命令文档**（`{"aggregate": "users", "pipeline": [...]}`，即 `db.runCommand(<doc>)`
   的参数），不是 `db.coll.find({})`。所以收「服务端命令名 + 聚合阶段 + 查询/更新操作符」，
   而 dbflux 的 `MONGO_METHODS`（shell 方法）对我们**不适用** —— 补出来就是让人白写。
   命令名在补全菜单里按「**命令名**」分类（新 i18n 键），不叫「函数」。
4. **Elasticsearch 收控制台语法的词**：`drivers/elasticsearch.rs` 走 REST，编辑器吃的是
   **Kibana 控制台**（`GET /logs-*/_search` + JSON 体），所以收请求动词、端点与检索 DSL 键。
   它没有函数调用，函数桶是空的。
5. 非 SQL 引擎**连通用 SQL 核心都不给**：Mongo 里补出 `SELECT` 毫无意义。

**API**：`sqlwords::words(kind) -> (SqlWords /*核心*/, SqlWords /*增量*/)`（两段切片，
调用方依次遍历，不分配、不去重）；`sqlwords::common_words()` 给「还没连上、没有活跃
会话」的兜底（那时补的是「SQL 长什么样」，不该替某个引擎说话）；
`sqlwords::language(kind) -> QueryLanguage` 给界面选分类文案的唯一判据（界面不再自己
按 `DatabaseKind` 分叉一遍）。当前引擎从 `Session::kind` 取。

**候选限额改为分类限额**：`COMPLETION_LIMIT = 80` 现在是**表 / 关键字 / 函数各一份**，
另有 `COMPLETION_TOTAL_LIMIT = 240` 兜底。原实现只有总量截断 —— 词表按引擎展开后光
关键字就有一百多条，会把「函数」整桶挤掉，用户敲 `count` 反而什么都补不出来。

**测试**（`cargo test -p duck-db --lib sqlwords`，5 条）：表内无重复；
增量不与通用核心撞车（这条当场抓到 `CHAR_LENGTH` 同时进了通用桶与 MySQL 桶）；
每个引擎都有词可补且 SQL 引擎带通用核心；SQL 词一律大写（ClickHouse 函数名是已知
例外）；**不串台** —— 每个引擎「该有该引擎的专有词、不该有别家的专有词」逐条断言
（在 SQLite 上补出 `AUTO_INCREMENT`、在 MySQL 上补出 `RETURNING` 都会被它挡下）。

⚠️ 与 `sqlintel::WRITE_KEYWORDS` **不是一回事**，也不是第二真源：那张表只收「会写库的
语句动词」（语义判定，用来证明只读），这张收**全部语法词**（给用户补全）。两张表之间
没有「改一处另一处要跟着改」的耦合。

### 4.18 SQL 语义诊断 + dbx 版式 footer + 滚动时显示的纵向滚动条（2026-10-14 深夜，用户截图反馈）

三项对齐 dbx：语义诊断开关、grid-foot 版式、滚动条现身时机。

**4.18.1 SQL 语义诊断（内核 `duck-db::sqldiag`）**

dbx 的 `useQueryEditorDiagnostics` 做解析报错 + 未知对象高亮。本仓落地为纯函数内核：

- 输入 `(DatabaseKind, sql, &[CatalogTable])`，输出 `Vec<SqlDiagnostic>`
  （`line` / `column` 1 起或 0 = 未知、`Severity::{Error, Warning}`、英文 `message`）。
- **解析报错**：`sqlparser` 按引擎方言（SQLite / PG / MySQL / ClickHouse / MSSQL 各自
  的 Dialect）整段解析；报错消息自带的 ` at Line: N, Column: M` 后缀被解出成位置、
  从消息里剥掉。MongoDB / Elasticsearch 的编辑器内容不是 SQL → 恒空表。
- **未知表**（`Warning`）：`visitor` feature 遍历 AST 收表引用。目录（`CatalogTable`
  裸名 + `schema.name` 限定名，不分大小写）与 **CTE 别名**（每层 `WITH`，嵌套子查询
  也算）里都没有的引用才报；表值函数（`TableFactor::Table { args: Some(..) }`）不是
  目录对象，跳过。匹配口径按引用段数走：裸名对裸名、≥2 段取末两段对限定名
  （`other.users` 不能因为目录里有裸 `users` 就放行）。
- **目录为空时不做表校验**：元数据没加载就报「全是未知表」是误报洪水。解析报错照报。

UI 层（db 页）：工具条「SQL 语义诊断」开关（`spell-check` 图标，提示随状态翻转
「已开启（点击关闭）」dbx 同款文案），偏好落 `DbSettings::semantic_diagnostics`
（默认开，dbx `SQL_SEMANTIC_DIAGNOSTICS_AUTO_ENABLED` 同款）；编辑器文本变化即重算
（observe 里同步跑，微秒级）。**问题条**挂编辑器面板底部：gpui-base 编辑器没有
装饰通道（画不了下划线 / 波浪线，这是与 dbx 的实质差距），折衷为列表条 —— 至多
3 行 + 「共 N 个问题」，位置 + 内核英文消息原样给出（消息不翻壳，`Unknown table
"x"` 是让用户照着搜的）；干净时不摆任何东西。

**测试**（`cargo test -p duck-db --lib sqldiag`，7 条）：非 SQL 引擎恒空；解析报错
带行号且消息剥离位置后缀；未知表带行号；CTE 与表函数不误报；空目录跳过表校验但
保留解析错误；schema 限定匹配（`public.users` 放行、`other.users` 报）；上限 50 条。

**4.18.2 footer 对齐 dbx grid-foot**

右端三件套：「N 行/页」下拉（查询 / 表浏览各自既有下拉，触发文案从「每页 N 行」改
「N 行/页」）→ **紧凑翻页组** `« ‹ [页码] › »` → 导出菜单。要点：

- 翻页组**常驻**：只有一页时按钮置灰但整条还在（dbx 同款；旧实现「一页整组消失」
  连页码输入框都找不到）。查询模式总页数已知、五钮全判；表浏览总页数未知，
  「»」恒置灰（装作知道再点空是骗人）。
- 翻页动作**只走既有路径**：查询改 `query_page`；表浏览前进走 `table_next_page`
  （带「满页才前进」守卫）、回退 / 跳转直接设页码重取（与「跳至某页」同口径）。
- 旧的文字版翻页组（`« 上一页 第 x/y 页 下一页 »`）与表浏览旧翻页条（含重复的
  「刷新」—— tab 栏已有）整体删除。
- 导出菜单落 footer（用户截图指定位置）；tab 栏「保存结果」保留（dbx 两处都有）。
- dbx footer 的 ChevronsDown「载入全部行」不搬：那是服务端大表 `loadAllRows` 的
  交互，本仓表浏览是逐页取、查询整批在手，没有这层语义，搬来是个点了没用的钮。

**4.18.3 纵向滚动条：滚动时现身**

结果网格（`uniform_list`）此前**没有纵向滚动条**（只有横向常驻那条）。补
`result_v_scroll: Arc<UniformListScrollHandle>`（`uniform_list` 挂 `track_scroll`）
+ `Scrollbar::vertical` overlay，档位 [`ScrollbarMode::Scrolling`] —— 滚动时现身、
停下淡出（gpui-base scrollbar.rs 默认档）。横向维持 `Always`（4.16.2 的结论不变）。

## 5. 数据库支持矩阵（M1→M2 引擎与依赖）

| 引擎 | 底层 crate | 期 | 状态与备注 |
| --- | --- | --- | --- |
| SQLite | `rusqlite`（bundled，已在图内） | M1 | ✅ **已落地**；零新增 |
| PostgreSQL | 阻塞 `postgres`（内部即 `tokio-postgres`） | M1 | ✅ **已落地**；TLS 按 MSRV 结论缓行 |
| MySQL | 阻塞 `mysql` | M1 | ✅ **已落地** |
| MariaDB | 复用 `mysql` 驱动 | M1/M2 | ⏳ 驱动已能连（同一 crate）；选择器卡片仍是**占位** |
| Redis | `redis = "=1.2.2"`（独立 crate `duck-redis`） | M2 | ✅ **已落地**（见 `redis-design.md`） |
| ClickHouse | `ureq` 直连 HTTP 接口（8123） | M2 | ✅ **已落地**（修订六）；**零新协议依赖** |
| SQL Server | `tiberius 0.12` + `tokio-util/compat` | M2 | ✅ **已落地**（修订六）；闭包 +12 包 |
| MongoDB | `mongodb = "=3.6.0"`（`rust-version` 恰为 1.85，**必须精确钉**） | M2 | ✅ **已落地**（修订七）；网格契约复用，列靠采样 |
| Elasticsearch | `ureq` 直连 REST（9200） | M2 | ✅ **已落地**（修订八）；`ureq` + `base64` 均在图内，**零新增包** |
| DuckDB | `duckdb crate`（bundled，编译重） | M3+ | ⏳ 视体积取舍 |

每个新依赖过「依赖准入三关」：MSRV ≤ 1.85、许可证登记（licenses.py 重生成）、版本理由注释；能复用图内 `rustls/ring` 就不引 openssl。

## 6. 分阶段实施计划

| 阶段 | 内容 | 交付判据 |
| --- | --- | --- |
| **M1 内核 + 三库** | `duck-db` crate（config/secret/store/runtime/conn/drivers{sqlite,pg,mysql}/sqlintel/result）+ `DbPage`（连接管理、表浏览、SQL 编辑、结果网格、暂存编辑、取消/超时）+ i18n + 单测 | 连上三个库完成"浏览-查询-改一格-保存"全链路；`cargo test -p duck-db` 覆盖 sqlintel 与值模型 |
| **M2 扩引擎 + 隧道** | SSH 隧道、Redis/SQLServer/Mongo、右栏对象详情、DDL 查看、CSV 导入导出、连接导入（TablePlus/DBeaver 可选） | 五引擎矩阵过冒烟；隧道经 bastion 全链路通 |
| **M3 AI** | Provider trait + text-to-SQL + 只读证明闸门 + 审批确认 + token 统计 | 自然语言→SQL→结果闭环，写操作必有确认 |
| **M4 生态** | MCP server、RPC 外置驱动、DuckDB、ER 图 | 按需排期 |

## 7. 与项目约定的符合性自查

- [x] 分层：内核不依赖 gpui；依赖方向单向向下；新 crate 经用户确认后建
- [x] 依赖准入：MSRV 硬门、许可证重生成、版本理由注释、`--locked` 并发纪律
- [x] i18n：中文源串 + EN 字节序 + 覆盖率守卫
- [x] 存储：kv 单键 JSON + secret 零明文；duck-store 的**表结构**只在审计这一处扩过
      （新增 `audit` 表，见 §4.12——它是「事件流」，塞进 kv 会只剩现值、塞进 recent 会
      被同 id 覆盖），其余一律走 kv 单键
- [x] UI：FluxDown 规格、hover_tint、accent_tag、字阶 token、无裸 px(字号)
- [x] a11y：Button 族自带的直接用；自绘 on_click 补 role + aria_label
- [x] 不发明 API：所有 gpui-component 调用以 temp/gpui-kit 与 0.7.1 源码为准

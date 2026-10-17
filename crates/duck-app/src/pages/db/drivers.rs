//! 驱动管理（驱动商店）：分类侧栏 + 驱动列表 + 安装 / 更新 / 卸载 / 下载源切换。
//!
//! # 列表里有什么
//!
//! **只有 [`duck_drivers::agent_sources`] 那一条判据选出来的驱动** —— 也就是
//! 「数据源 + 有工件可装 + **本仓真会驱动它**」三者同时成立的那些（45 条）。这不是随手筛的：
//!
//! - 连接选择器里的卡片，数据源与它**同源**（`EngineRef` 的展示名也查这份清单），
//!   于是「能装什么」与「能连什么」天然一一对应，不会出现「装得下但连不上」的行；
//! - dbx 注册表里另外那些（etcd / ZooKeeper / Kafka / SQLite SSH Worker…）本应用
//!   **没有**对应的用法（没有键值控制台、没有消息队列页、SQLite 走原生 rusqlite），
//!   把它们照抄进商店只会得到一排装了也没用的行 —— 那是**演示项**，不是功能；
//!   DuckDB 曾经也在这串里，现在有了 sidecar 引擎（见 `duck-db` 的 `drivers::duckdb`），
//!   于是它**留下**了：装了能连、能查、能翻页。
//!
//! # 为什么注册表可以「还没到」
//!
//! 清单（`duck_drivers::catalog`）是编进二进制的，**恒可用**；远端注册表要联网，
//! 可能慢、可能失败。两者分开对待：注册表没到时照常列出全部驱动、照常能装
//! （`DriverStore::install` 内部自己会拉），只是版本栏与「有没有新版」显示不出来。
//!
//! # 事件与节流
//!
//! 下载进度走 `mpsc`（内核那边一条条 `send`），宿主这边**定时批量 drain**：
//! 一个 20 MB 的工件会产生几百条进度，逐条 `cx.notify()` 就是几百次重绘。
//! 收工（`Finished`）时把接收端置回 `None`，轮询任务据此自然退出。

use super::*;

/// 轮询内核事件的间隔。120ms ≈ 每秒八帧进度，肉眼连续，又不至于把重绘打满。
const PUMP_INTERVAL: Duration = Duration::from_millis(120);

/// 驱动列表的最大高度（弹窗内滚动区的上限；布局约束，不在 spacing 阶梯上）。
const DRIVER_LIST_MAX_HEIGHT: f32 = 380.;
/// 分类侧栏宽度（同上）。
const DRIVER_RAIL_WIDTH: f32 = 132.;

/// 本机驱动目录（一个进程里只有一处）。
///
/// ⚠️ **必须缓存**，不能每次 `DriverStore::new()`：那个构造里就 `DriverState::load`
/// （读 `state.json`），而渲染路径每画一行（商店列表 44 行、连接表单的头像与提示）
/// 都要问一次「装了没」「哪个版本」—— 一帧就是几十次文件读，列表一滚就掉帧。
///
/// 缓存不会让状态变旧：写路径（`install` / `uninstall` / 切源）全部落在**这个**
/// 实例上（进程内唯一），而真正要用驱动的是 `duck-db` 建连时自己新读一份
/// （`drivers/agent.rs`），那边读到的正是这里刚写下的盘上内容。
pub(super) fn driver_store() -> &'static DriverStore {
    static STORE: OnceLock<DriverStore> = OnceLock::new();
    STORE.get_or_init(DriverStore::new)
}
/// 驱动管理弹窗宽度。
///
/// 比选择器（[`PICKER_WIDTH`] 720）宽一档：那里是一张张等宽卡片并排，这里多了
/// 一条分类侧栏（132pt），剩下的宽度才是「图标 + 名称/版本 + 按钮」一行 ——
/// 按 720 算下来一行只剩 540 上下，中文驱动名（「阿里云 MaxCompute」）会被截。
const DRIVER_MANAGER_WIDTH: f32 = 780.;

impl DriverManagerView {
    /// 新建视图并**立刻**开始拉注册表（弹窗一开就干活，不等用户点刷新）。
    pub(super) fn new(
        page: WeakEntity<DatabasePage>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let search = text_input(window, cx, tr(cx, "按名称或键筛选"));
        let subscription = cx.subscribe(
            &search,
            |this: &mut Self,
             _source: Entity<InputState>,
             event: &InputEvent,
             cx: &mut Context<Self>| {
                if matches!(event, InputEvent::Change) {
                    this.query = this.search.read(cx).value().trim().to_lowercase();
                    cx.notify();
                }
            },
        );
        let mut view = Self {
            page,
            search,
            query: String::new(),
            category: None,
            only_installed: false,
            registry: None,
            registry_state: RegistryState::Idle,
            running: HashMap::new(),
            batch: None,
            notice: None,
            events: None,
            cancel: None,
            pumping: false,
            bytes_installed: 0,
            _subscription: subscription,
        };
        view.refresh_registry(false, cx);
        view
    }

    /// 本机驱动目录（见 [`driver_store`]）。
    fn store() -> &'static DriverStore {
        driver_store()
    }

    /// 拉注册表。`force` = 忽略本地缓存（切了下载源、或用户点了刷新）。
    fn refresh_registry(&mut self, force: bool, cx: &mut Context<Self>) {
        if matches!(self.registry_state, RegistryState::Loading) {
            return;
        }
        self.registry_state = RegistryState::Loading;
        self.notice = None;
        let source = Self::store().download_source();
        self.spawn_task(
            cx,
            move || duck_drivers::registry::registry(source, force),
            |this, result, _cx| match result {
                Ok(registry) => {
                    this.registry = Some(Arc::new(registry));
                    this.registry_state = RegistryState::Ready;
                }
                Err(error) => this.registry_state = RegistryState::Failed(error.to_string()),
            },
        );
    }

    /// 装一条（已装则升级）。
    fn install(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some(registry) = self.registry.clone() else {
            // 注册表还没到：`install` 自己会去拉，但那样就没有「正在拉注册表」的进度了，
            // 而且用户点得太早会得到两条并行的网络请求。直接告诉他还差一步。
            self.notice = Some(DriverNotice {
                failed: true,
                text: tr(cx, "注册表还没拉下来，稍等一下再试（或点刷新）。").to_string(),
            });
            return;
        };
        if self.running.contains_key(key) {
            return;
        }
        self.begin_batch(1);
        self.running.insert(key.to_owned(), ProgressStep::Registry);
        let key = key.to_owned();
        let store = Self::store();
        let task_key = key.clone();
        // 取消标志在**主线程**上建、往后台搬一个克隆：界面要拿着它去点「取消」，
        // 而标志本身是 `Arc<AtomicBool>`（高频读、不阻塞）—— 正是为这种分工设计的。
        let cancel = CancelFlag::new();
        self.cancel = Some(cancel.clone());
        self.spawn_task(
            cx,
            move || {
                let (sink, receiver) = Self::channel();
                // 事件接收端要先交回视图 —— 但视图更新得等任务返回。
                // 做法：把 receiver 一并当成返回值带回来（见下面的 `done`）。
                let outcome = store.install(&registry, &task_key, &sink, &cancel);
                (receiver, outcome)
            },
            // `move`：收工闭包是 `'static`（进 `background_executor`），`key` 得搬进去。
            move |this, (receiver, outcome), cx| {
                this.attach_events(receiver, cx);
                this.finish_one(&key, outcome, cx);
            },
        );
    }

    /// 装一批（「全部更新」与单条走同一条路：内核的 `install` 本身就是幂等的升级）。
    fn install_many(&mut self, keys: Vec<String>, cx: &mut Context<Self>) {
        let Some(registry) = self.registry.clone() else {
            self.notice = Some(DriverNotice {
                failed: true,
                text: tr(cx, "注册表还没拉下来，先点一下刷新。").to_string(),
            });
            return;
        };
        if keys.is_empty() {
            self.notice = Some(DriverNotice {
                failed: false,
                text: tr(cx, "所有已安装的驱动都是最新版。").to_string(),
            });
            return;
        }
        self.begin_batch(keys.len() as u32);
        for key in &keys {
            self.running.insert(key.clone(), ProgressStep::Registry);
        }
        let store = Self::store();
        let all_keys = keys.clone();
        let cancel = CancelFlag::new();
        self.cancel = Some(cancel.clone());
        self.spawn_task(
            cx,
            move || {
                let (sink, receiver) = Self::channel();
                let mut outcomes = Vec::new();
                let count = keys.len() as u32;
                for (index, key) in keys.iter().enumerate() {
                    if cancel.is_cancelled() {
                        break;
                    }
                    // 位置标注走 `decorate` 而不是「每条事件各标一次」：内核在
                    // `install` 内部自己产生下载 / 解包那几十条事件，调用方拿不到 ——
                    // 包一层之后，它们**每一条**都带上「第几个 / 共几个」。
                    let positioned = sink.decorate(move |event| event.at(index as u32 + 1, count));
                    let result = store.install(&registry, key, &positioned, &cancel);
                    outcomes.push((key.clone(), result));
                }
                (receiver, outcomes)
            },
            // `move`：收工闭包是 `'static`，`all_keys` 得搬进去（理由同单条那条）。
            move |this, (receiver, outcomes), cx| {
                this.attach_events(receiver, cx);
                let batch = this.batch;
                let mut ran: Vec<String> = Vec::new();
                for (key, outcome) in outcomes {
                    ran.push(key.clone());
                    this.finish_one(&key, outcome, cx);
                }
                // 被取消而**根本没轮到**的那些：`outcomes` 里没有它们的条目，于是
                // `running` 里的「正在拉注册表」会永远挂着 —— 在这里一并清掉。
                // 只清这一批自己的键（可能有别的单条安装正在并行跑）。
                for key in &all_keys {
                    if !ran.iter().any(|done| done == key) {
                        this.running.remove(key);
                    }
                }
                this.batch = batch;
                this.report_batch(cx);
            },
        );
    }

    /// 卸载一条（连同安装目录；**不动托管 JRE** —— 别的 jar 轨驱动可能还在用）。
    fn uninstall(&mut self, key: &str, cx: &mut Context<Self>) {
        if self.running.contains_key(key) {
            return;
        }
        self.running.insert(key.to_owned(), ProgressStep::Remove);
        self.notice = None;
        let store = Self::store();
        let task_key = key.to_owned();
        let label = key.to_owned();
        self.spawn_task(
            cx,
            move || {
                let (sink, receiver) = Self::channel();
                let outcome = store.uninstall(&task_key, &sink);
                (receiver, outcome)
            },
            move |this, (receiver, outcome), cx| {
                this.attach_events(receiver, cx);
                this.running.remove(&label);
                this.bytes_installed = 0;
                this.notice = Some(match outcome {
                    Ok(()) => DriverNotice {
                        failed: false,
                        text: trf(cx, "已卸载 {}。", &[&label]),
                    },
                    Err(error) => DriverNotice {
                        failed: true,
                        text: trf(cx, "卸载失败：{}", &[&error.to_string()]),
                    },
                });
                // 卸掉之后本机状态变了：宿主页面上那些「已就绪」的提示要跟着走。
                this.notify_page(cx);
            },
        );
    }

    /// 请求取消正在跑的安装。**只是置位**：内核在每个可中断点（下载回调每 64 KB
    /// 一次、以及各步骤之间）查它，取消后的收尾由那条任务自己走完（落一条提示）。
    ///
    /// 不去 `running.remove(..)`：按钮立刻变回「安装」而流程还在写盘，用户会以为它停了
    /// 又点一次 —— 那时内核的 `busy` 锁会把第二次挡下，界面表现是「点了没反应」。
    fn cancel_running(&mut self, cx: &mut Context<Self>) {
        let Some(cancel) = &self.cancel else {
            return;
        };
        cancel.cancel();
        self.notice = Some(DriverNotice {
            failed: false,
            text: tr(cx, "正在取消…").to_string(),
        });
        cx.notify();
    }

    /// 装 / 卸之后叫宿主页面刷一下。
    ///
    /// 连接表单里的「去装驱动」提示、选择器卡片上的可用状态都按本机驱动情况画，
    /// 不通知的话得关掉弹窗重开才更新 —— 用户刚装完就看到「还没装」是最容易挨骂的。
    fn notify_page(&self, cx: &mut Context<Self>) {
        let Some(page) = self.page.upgrade() else {
            return;
        };
        page.update(cx, |_page, cx| cx.notify());
    }

    /// 清下载缓存（断点续传留下的分块临时文件）。
    fn clear_cache(&mut self, cx: &mut Context<Self>) {
        let freed = Self::store().clear_download_cache();
        self.notice = Some(DriverNotice {
            failed: false,
            text: if freed == 0 {
                tr(cx, "下载缓存本来就是空的。").to_string()
            } else {
                // 字节格式化走主题层那一个（`duck_theme::bytes`）—— 全仓只此一份口径。
                trf(
                    cx,
                    "已清理下载缓存，释放 {}。",
                    &[&duck_theme::bytes(freed)],
                )
            },
        });
    }

    /// 切下载源并重新拉注册表 —— **必须 `force`**：缓存是按源存的，不强制就会拿到
    /// 上一份源的结果，用户会以为「切了没用」。
    fn set_source(&mut self, source: DownloadSource, cx: &mut Context<Self>) {
        if Self::store().download_source() == source {
            return;
        }
        if let Err(error) = Self::store().set_download_source(source) {
            self.notice = Some(DriverNotice {
                failed: true,
                text: trf(cx, "保存下载源失败：{}", &[&error.to_string()]),
            });
            return;
        }
        self.registry = None;
        self.refresh_registry(true, cx);
    }

    /// 内核事件通道（宿主侧只收）。
    fn channel() -> (EventSink, std::sync::mpsc::Receiver<DriverEvent>) {
        let (sender, receiver) = std::sync::mpsc::channel();
        (EventSink::new(sender), receiver)
    }

    /// 把接收端交给视图并（若还没起）拉起轮询任务。
    ///
    /// ⚠️ **只有一条轮询任务**（`pumping` 旗标）：每装一条就起一个新任务的话，
    /// N 条并行会起 N 个轮询者去抢同一个接收端，事件会被谁先拿到就谁处理 ——
    /// 表现为「进度文案随机跳」。
    fn attach_events(
        &mut self,
        receiver: std::sync::mpsc::Receiver<DriverEvent>,
        cx: &mut Context<Self>,
    ) {
        self.events = Some(receiver);
        if self.pumping {
            return;
        }
        self.pumping = true;
        let view = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor().timer(PUMP_INTERVAL).await;
                let mut stop = false;
                let alive = view.update(cx, |this, cx| {
                    let drained = this.drain_events();
                    if drained {
                        cx.notify();
                    }
                    // 收工条件：没有接收端了（每条流程的 `Finished` 都会把它清掉）。
                    stop = this.events.is_none();
                });
                if alive.is_err() || stop {
                    break;
                }
            }
            // 轮询停了，旗标也要落地 —— 否则下一条驱动开工时没人收事件。
            let _ = view.update(cx, |this, _cx| this.pumping = false);
        })
        .detach();
    }

    /// 批量开始：清掉上一轮的提示与进度，记下总数。
    fn begin_batch(&mut self, count: u32) {
        self.notice = None;
        self.batch = Some((0, count));
    }

    /// 批量收尾：把「第几个」清掉，写一条汇总提示。
    fn report_batch(&mut self, cx: &mut Context<Self>) {
        let Some((done, total)) = self.batch else {
            return;
        };
        self.batch = None;
        self.notice = Some(DriverNotice {
            failed: done < total,
            text: if done == total {
                trf(cx, "已完成 {} 条驱动的更新。", &[&total.to_string()])
            } else {
                trf(
                    cx,
                    "{} 条里只完成了 {} 条，其余请看各行的提示。",
                    &[&total.to_string(), &done.to_string()],
                )
            },
        });
    }

    /// 单条收尾：从「进行中」移走、落一条结果提示、重算占用。
    fn finish_one(
        &mut self,
        key: &str,
        outcome: duck_drivers::Result<InstalledDriver>,
        cx: &mut Context<Self>,
    ) {
        self.running.remove(key);
        self.bytes_installed = 0;
        // 还有别的在跑就留着取消标志（它属于那一条），跑完才收掉。
        if self.running.is_empty() {
            self.cancel = None;
        }
        match outcome {
            Ok(installed) => {
                if let Some((done, _)) = self.batch.as_mut() {
                    *done += 1;
                }
                self.notice = Some(DriverNotice {
                    failed: false,
                    text: trf(cx, "{} 已就绪（版本 {}）。", &[key, &installed.version]),
                });
                // 装成了才通知页面：失败时本机状态没变，白重绘一次。
                self.notify_page(cx);
            }
            Err(error) => {
                self.notice = Some(DriverNotice {
                    failed: true,
                    text: trf(cx, "{} 失败：{}", &[key, &error.to_string()]),
                });
            }
        }
    }

    /// drain 接收端里攒下的事件；返回「有没有拿到东西」（决定要不要重绘）。
    fn drain_events(&mut self) -> bool {
        let mut got = false;
        let mut finished: Vec<String> = Vec::new();
        if let Some(receiver) = &self.events {
            while let Ok(event) = receiver.try_recv() {
                got = true;
                if event.step == ProgressStep::Finished {
                    finished.push(event.key.clone());
                } else {
                    self.running.insert(event.key.clone(), event.step);
                }
            }
        }
        for key in finished {
            self.running.remove(&key);
        }
        // 收工：没有在跑的了就把接收端与取消标志一起放掉（轮询任务据此退出）。
        if self.running.is_empty() && self.batch.is_none() {
            self.events = None;
            self.cancel = None;
        }
        got
    }

    /// 后台任务 → 主线程的桥。形状与 `DatabasePage::spawn_db` 一致（同一套
    /// 「后台算、主线程改状态」的分工），只是落点在这个视图上。
    fn spawn_task<T, F, D>(&mut self, cx: &mut Context<Self>, work: F, done: D)
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
        D: FnOnce(&mut Self, T, &mut Context<Self>) + 'static,
    {
        let task = cx.background_executor().spawn(async move { work() });
        let view = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let value = task.await;
            cx.update(move |cx| {
                crate::update::on_notify_window(cx, move |_window, cx| {
                    let _ = view.update(cx, |this, cx| {
                        done(this, value, cx);
                        cx.notify();
                    });
                });
            });
        })
        .detach();
    }

    /// 注册表里这条驱动的最新版（注册表未到时 `None`）。
    fn latest_version(&self, key: &str) -> Option<String> {
        self.registry
            .as_ref()
            .and_then(|registry| registry.driver(key))
            .map(|info| info.version.clone())
    }

    /// 这一行现在是个什么状态。
    fn row_state(&self, entry: &DriverEntry) -> RowState {
        let installed = Self::store().installed(entry.key);
        let latest = self.latest_version(entry.key);
        match (installed, latest) {
            (None, latest) => RowState::Missing { latest },
            (Some(installed), Some(latest)) if installed.version != latest => RowState::Updatable {
                installed: installed.version,
                latest,
            },
            (Some(installed), _) => RowState::Installed {
                version: installed.version,
            },
        }
    }

    /// 当前该显示哪些驱动（分类 + 只看已装 + 关键字三重筛选，顺序沿用清单）。
    ///
    /// 判据是 [`duck_drivers::agent_sources`] —— 与连接选择器同源（见模块文档）。
    fn visible(&self) -> Vec<&'static DriverEntry> {
        let installed_keys: Vec<String> = if self.only_installed {
            let store = Self::store();
            duck_drivers::agent_sources()
                .filter(|entry| store.installed(entry.key).is_some())
                .map(|entry| entry.key.to_owned())
                .collect()
        } else {
            Vec::new()
        };
        duck_drivers::agent_sources()
            .filter(|entry| self.category.is_none_or(|key| entry.category == key))
            .filter(|entry| {
                !self.only_installed || installed_keys.iter().any(|key| key == entry.key)
            })
            .filter(|entry| {
                self.query.is_empty()
                    || entry.label.to_lowercase().contains(&self.query)
                    || entry.key.to_lowercase().contains(&self.query)
                    || entry
                        .default_port
                        .is_some_and(|port| port.to_string().contains(&self.query))
            })
            .collect()
    }

    /// 有新版可更新的那些键（「全部更新」用）。
    fn updatable(&self) -> Vec<String> {
        duck_drivers::agent_sources()
            .filter(|entry| matches!(self.row_state(entry), RowState::Updatable { .. }))
            .map(|entry| entry.key.to_owned())
            .collect()
    }
}

/// 一条驱动在列表里的状态（决定版本栏与按钮）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowState {
    /// 没装；`latest` 是注册表里的最新版（注册表未到时为 `None`）。
    Missing { latest: Option<String> },
    /// 装了且是最新。
    Installed { version: String },
    /// 装了但有新版。
    Updatable { installed: String, latest: String },
}

/// 进度步骤 → 中文文案（内核给枚举，文案在界面层拼）。
fn step_label(step: ProgressStep) -> &'static str {
    match step {
        ProgressStep::Registry => "正在拉注册表",
        ProgressStep::JreDownload => "正在下载 Java 运行时",
        ProgressStep::Download => "正在下载",
        ProgressStep::Verify => "正在校验摘要",
        ProgressStep::Extract => "正在解包",
        ProgressStep::Install => "正在落位",
        ProgressStep::Remove => "正在卸载",
        ProgressStep::Finished => "已完成",
    }
}

/// 下载源 → 展示名。
fn source_label(source: DownloadSource) -> &'static str {
    match source {
        DownloadSource::Official => "官方源",
        DownloadSource::Mirror => "国内镜像",
    }
}

impl DatabasePage {
    /// 打开驱动管理弹窗。
    ///
    /// 入口在连接选择器的脚注（「驱动管理…」）：那正是用户第一次发现「这个引擎要装个
    /// 驱动」的地方，也是 dbx 的入口位置。
    ///
    /// 弹窗外壳与 [`Self::open_picker_dialog`] 同形（标题行 + 视图正文 + 脚注），
    /// 正文挂视图实体而不是把状态摊在页面上的理由见 [`DriverManagerView`] 的说明。
    pub(super) fn open_driver_manager(&self, window: &mut Window, cx: &mut Context<Self>) {
        let page = self.view.clone();
        let manager = cx.new(|cx| DriverManagerView::new(page, window, cx));
        let drag = modal_drag(cx);
        window.open_dialog(cx, move |dialog, _window, cx| {
            let offset = modal_offset(&drag, cx);
            let manager_for_content = manager.clone();
            let drag_for_content = drag.clone();
            dialog
                .w(px(DRIVER_MANAGER_WIDTH))
                .ml(offset.x)
                .margin_top(dialog_top_offset(cx) + offset.y)
                .opacity(offset.opacity)
                .close_button(false)
                .button_props(DialogButtonProps::default())
                .content(move |content, _, _cx| {
                    content.child(modal_drag_area(
                        &drag_for_content,
                        v_flex()
                            .w_full()
                            .min_h_0()
                            .child(modal_title_row(
                                "db-driver-manager-title",
                                &drag_for_content,
                                tr(_cx, "驱动管理"),
                                _cx,
                            ))
                            .child(manager_for_content.clone()),
                    ))
                })
                .footer(
                    DialogFooter::new().child(div().flex_1()).child(
                        button(
                            "db-driver-manager-close",
                            tr(cx, "关闭"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                    ),
                )
        });
    }
}

impl Render for DriverManagerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = active_theme(cx);
        let tokens = theme.tokens().clone();
        let extended = theme.extended();
        let hover_bg = tokens.colors.muted;
        let view = cx.entity().downgrade();
        let store = Self::store();
        let source = store.download_source();

        // ── 工具条：下载源 / 筛选 / 批量动作 ──────────────────────────────
        let mut source_row = h_flex().flex_none().items_center().gap(tokens.spacing.xxs);
        for option in DownloadSource::ALL {
            let active = option == source;
            let view = view.clone();
            source_row = source_row.child(
                button(
                    SharedString::from(format!("db-driver-source-{}", option.wire_name())),
                    tr(cx, source_label(option)),
                    if active {
                        ButtonVariant::Primary
                    } else {
                        ButtonVariant::Secondary
                    },
                    cx,
                )
                .on_click(move |_, _, cx| {
                    let _ = view.update(cx, |this, cx| this.set_source(option, cx));
                }),
            );
        }

        let only_installed = self.only_installed;
        let view_toggle = view.clone();
        let installed_row = check_row(
            "db-driver-only-installed",
            only_installed,
            div().child(selectable_text(tr(cx, "只显示已安装"))),
            move |_, _, cx| {
                let _ = view_toggle.update(cx, |this, cx| {
                    this.only_installed = !this.only_installed;
                    cx.notify();
                });
            },
            cx,
        );

        let view_update_all = view.clone();
        let update_all = button(
            "db-driver-update-all",
            tr(cx, "全部更新"),
            ButtonVariant::Secondary,
            cx,
        )
        .on_click(move |_, _, cx| {
            let _ = view_update_all.update(cx, |this, cx| {
                let keys = this.updatable();
                this.install_many(keys, cx);
            });
        });

        let view_refresh = view.clone();
        let refresh = icon_button(
            "db-driver-refresh",
            tr(cx, "刷新注册表"),
            Icon::new(DuckIcon::RotateCw).size(tokens.typography.md.size),
            ButtonVariant::Secondary,
            cx,
        )
        .on_click(move |_, _, cx| {
            let _ = view_refresh.update(cx, |this, cx| this.refresh_registry(true, cx));
        });

        let view_cache = view.clone();
        let clear_cache = icon_button(
            "db-driver-clear-cache",
            tr(cx, "清理下载缓存"),
            Icon::new(DuckIcon::Trash2).size(tokens.typography.md.size),
            ButtonVariant::Secondary,
            cx,
        )
        .on_click(move |_, _, cx| {
            let _ = view_cache.update(cx, |this, cx| this.clear_cache(cx));
        });

        // 「取消」只在真有活儿在跑时出现 —— 常在的工具条上多一个平时不能点的按钮，
        // 就是给界面添噪音（与「全部更新」的启用/禁用口径一致）。
        let cancel = (!self.running.is_empty()).then(|| {
            let view_cancel = view.clone();
            button(
                "db-driver-cancel",
                tr(cx, "取消"),
                ButtonVariant::Secondary,
                cx,
            )
            .on_click(move |_, _, cx| {
                let _ = view_cancel.update(cx, |this, cx| this.cancel_running(cx));
            })
        });

        let toolbar = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.lg)
            .py(tokens.spacing.sm)
            .border_b_1()
            .border_color(tokens.colors.border)
            .child(source_row)
            .child(div().flex_1())
            .child(installed_row)
            .child(update_all)
            .when_some(cancel, |bar, cancel| bar.child(cancel))
            .child(refresh)
            .child(clear_cache);

        // ── 搜索 ─────────────────────────────────────────────────────────
        let search = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .rounded(tokens.radius.md)
            .border_1()
            .border_color(tokens.colors.border)
            .bg(tokens.colors.background)
            .child(
                Icon::new(DuckIcon::Search)
                    .size(tokens.typography.md.size)
                    .text_color(tokens.colors.muted_foreground),
            )
            .child(
                div().flex_1().min_w_0().child(
                    Input::new(&self.search)
                        .appearance(false)
                        .with_size(Size::Small)
                        .text_size(tokens.typography.sm.size)
                        .w_full(),
                ),
            );

        // ── 分类侧栏 ─────────────────────────────────────────────────────
        let mut rail = v_flex()
            .w(px(DRIVER_RAIL_WIDTH))
            .flex_none()
            .gap(tokens.spacing.xxs);
        let total = duck_drivers::agent_sources().count();
        rail = rail.child(Self::rail_row(
            "all",
            SharedString::from(tr(cx, "全部")),
            total,
            self.category.is_none(),
            view.clone(),
            &tokens,
            extended.colors.nav_selected,
            hover_bg,
        ));
        for category in duck_drivers::categories() {
            let count = duck_drivers::agent_sources()
                .filter(|entry| entry.category == category.key)
                .count();
            // 这个分类下一条可管理的驱动都没有（`registry_config` 之类）就不画 ——
            // 点开必为空的分区是哑节点。
            if count == 0 {
                continue;
            }
            rail = rail.child(Self::rail_row(
                category.key,
                SharedString::from(tr(cx, category.label)),
                count,
                self.category == Some(category.key),
                view.clone(),
                &tokens,
                extended.colors.nav_selected,
                hover_bg,
            ));
        }

        // ── 驱动列表 ─────────────────────────────────────────────────────
        let rows = self.visible();
        let mut list = v_flex().w_full().min_h_0().gap(tokens.spacing.xxs);
        if rows.is_empty() {
            list = list.child(
                div()
                    .w_full()
                    .py(tokens.spacing.lg)
                    .flex()
                    .justify_center()
                    .child(meta_text(tr(cx, "没有匹配的驱动。"), cx)),
            );
        }
        let row_view = view.clone();
        for entry in rows {
            let state = self.row_state(entry);
            let running = self.running.get(entry.key).copied();
            list = list.child(Self::driver_row(
                row_view.clone(),
                entry,
                state,
                running,
                &tokens,
                hover_bg,
                cx,
            ));
        }

        let body = h_flex()
            .w_full()
            .min_h_0()
            .items_start()
            .gap(tokens.spacing.md)
            .child(rail)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(tokens.spacing.sm)
                    .child(search)
                    .child(
                        div()
                            .id("db-driver-list")
                            .w_full()
                            .min_h_0()
                            .max_h(px(DRIVER_LIST_MAX_HEIGHT))
                            .overflow_y_scroll()
                            .child(list),
                    ),
            );

        v_flex().w_full().min_h_0().child(toolbar).child(
            v_flex()
                .w_full()
                .min_h_0()
                .gap(tokens.spacing.sm)
                .px(tokens.spacing.lg)
                .py(tokens.spacing.sm)
                .child(self.status_line(&tokens, extended.colors.success, cx))
                .child(body),
        )
    }
}

impl DriverManagerView {
    /// 分类侧栏里的一行（不对外；`render` 里连排十来个）。
    #[allow(clippy::too_many_arguments)]
    fn rail_row(
        key: &'static str,
        label: SharedString,
        count: usize,
        active: bool,
        view: WeakEntity<Self>,
        tokens: &duck_theme::SemanticThemeTokens,
        selected_bg: Hsla,
        hover_bg: Hsla,
    ) -> AnyElement {
        let category = (key != "all").then_some(key);
        h_flex()
            .id(SharedString::from(format!("db-driver-category-{key}")))
            .w_full()
            .items_center()
            .justify_between()
            .gap(tokens.spacing.xs)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .rounded(tokens.radius.md)
            .cursor_pointer()
            .when(active, |el| el.bg(selected_bg))
            .hover(move |style| style.bg(hover_bg))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(tokens.typography.sm.size)
                    .child(label),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(tokens.typography.xs.size)
                    .text_color(tokens.colors.muted_foreground)
                    .font_features(tabular_numbers())
                    .child(SharedString::from(count.to_string())),
            )
            .on_click(move |_, _, cx| {
                let _ = view.update(cx, |this, cx| {
                    this.category = category;
                    cx.notify();
                });
            })
            .into_any_element()
    }

    /// 一条驱动一行：图标 + 名称/键 + 版本状态 + 动作按钮。
    ///
    /// ⚠️ 关联函数（不是 `&self` 方法）且只拿 `&App` 不是 `&mut Context<Self>`：
    /// `Render::render` 里的 `&mut self` 与 `&mut Context<Self>` 指向**同一块存储**
    /// （视图状态就住在 context 里），同时借一个可变、一个不可变会被借用检查拦下。
    /// 行本身只读状态、只往 `view` 上挂回调，本来也不需要可变上下文。
    #[allow(clippy::too_many_arguments)]
    fn driver_row(
        view: WeakEntity<Self>,
        entry: &'static DriverEntry,
        state: RowState,
        running: Option<ProgressStep>,
        tokens: &duck_theme::SemanticThemeTokens,
        hover_bg: Hsla,
        cx: &App,
    ) -> AnyElement {
        let engine = EngineRef::new(DatabaseKind::Agent, Some(entry.key));
        let icon = match engine.logo() {
            Some(logo) => logo.element(px(20.)).into_any_element(),
            None => Icon::new(DuckIcon::Database)
                .size(px(20.))
                .text_color(tokens.colors.muted_foreground)
                .into_any_element(),
        };

        // 版本栏：注册表还没到就说「版本待定」，而不是空着或编一个。
        let version = match &state {
            RowState::Missing { latest } => match latest {
                Some(latest) => trf(cx, "未安装 · 最新 {}", &[latest]),
                None => tr(cx, "未安装").to_string(),
            },
            RowState::Installed { version } => trf(cx, "已安装 {}", &[version]),
            RowState::Updatable { installed, latest } => trf(cx, "{} → {}", &[installed, latest]),
        };

        let (action, plain) = if let Some(step) = running {
            (None, tr(cx, step_label(step)).to_string())
        } else {
            match &state {
                RowState::Missing { .. } => (
                    Some((tr(cx, "安装"), entry.key)),
                    tr(cx, "未安装").to_string(),
                ),
                RowState::Updatable { .. } => (
                    Some((tr(cx, "更新"), entry.key)),
                    tr(cx, "有新版").to_string(),
                ),
                RowState::Installed { .. } => (None, tr(cx, "已是最新").to_string()),
            }
        };

        let mut row = h_flex()
            .id(SharedString::from(format!("db-driver-row-{}", entry.key)))
            .w_full()
            .items_center()
            .gap(tokens.spacing.sm)
            .px(tokens.spacing.sm)
            .py(tokens.spacing.xs)
            .rounded(tokens.radius.md)
            .border_1()
            .border_color(tokens.colors.border)
            .bg(tokens.colors.background)
            .hover(move |style| style.bg(hover_bg))
            .child(icon)
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .truncate()
                            .text_size(tokens.typography.sm.size)
                            .font_weight(FontWeight::MEDIUM)
                            .child(SharedString::from(tr(cx, entry.label))),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(tokens.typography.xs.size)
                            .text_color(tokens.colors.muted_foreground)
                            .child(SharedString::from(version)),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(tokens.typography.xs.size)
                    .text_color(tokens.colors.muted_foreground)
                    .child(SharedString::from(plain)),
            );

        match action {
            Some((label, key)) => {
                let view_install = view.clone();
                let key_owned = key.to_owned();
                row = row.child(
                    button(
                        SharedString::from(format!("db-driver-install-{key}")),
                        label,
                        ButtonVariant::Primary,
                        cx,
                    )
                    .on_click(move |_, _, cx| {
                        let key = key_owned.clone();
                        let _ = view_install.update(cx, |this, cx| this.install(&key, cx));
                    }),
                );
            }
            None => {
                // 已安装：给一个卸载入口（**只在没在跑的时候**，否则会撞上安装的目录替换）。
                if running.is_none()
                    && matches!(
                        state,
                        RowState::Installed { .. } | RowState::Updatable { .. }
                    )
                {
                    let view_remove = view.clone();
                    let key_owned = entry.key.to_owned();
                    row = row.child(
                        button(
                            SharedString::from(format!("db-driver-remove-{}", entry.key)),
                            tr(cx, "卸载"),
                            ButtonVariant::Secondary,
                            cx,
                        )
                        .on_click(move |_, _, cx| {
                            let key = key_owned.clone();
                            let _ = view_remove.update(cx, |this, cx| this.uninstall(&key, cx));
                        }),
                    );
                }
            }
        }
        row.into_any_element()
    }

    /// 顶部状态行：注册表进度 / 磁盘占用 / 批量进度 / 最近一条提示。
    ///
    /// `success` 由调用方从**扩展 token** 里取（`extended.colors.success`）：
    /// 语义色那一层只有 `destructive`，成功色住在扩展层，这里不该再自己查一次主题。
    fn status_line(
        &self,
        tokens: &duck_theme::SemanticThemeTokens,
        success: Hsla,
        cx: &App,
    ) -> AnyElement {
        let mut line = h_flex()
            .w_full()
            .flex_none()
            .items_center()
            .gap(tokens.spacing.sm)
            .text_size(tokens.typography.xs.size);
        match &self.registry_state {
            RegistryState::Idle => {}
            RegistryState::Loading => {
                line = line.child(meta_text(tr(cx, "正在拉取驱动注册表…"), cx));
            }
            RegistryState::Ready => {
                let installed = duck_drivers::agent_sources()
                    .filter(|entry| Self::store().installed(entry.key).is_some())
                    .count();
                let bytes = if self.bytes_installed == 0 {
                    let store = Self::store();
                    duck_drivers::agent_sources()
                        .filter_map(|entry| store.installed(entry.key))
                        .map(|driver| driver.bytes)
                        .sum::<u64>()
                } else {
                    self.bytes_installed
                };
                line = line.child(meta_text(
                    trf(
                        cx,
                        "已安装 {} / {} · 占用 {}",
                        &[
                            &installed.to_string(),
                            &duck_drivers::agent_sources().count().to_string(),
                            &duck_theme::bytes(bytes),
                        ],
                    ),
                    cx,
                ));
            }
            RegistryState::Failed(error) => {
                line = line.child(
                    div()
                        .text_color(tokens.colors.destructive)
                        .child(SharedString::from(trf(cx, "注册表拉取失败：{}", &[error]))),
                );
            }
        }
        if let Some((done, total)) = self.batch {
            line = line.child(meta_text(
                trf(
                    cx,
                    "批量进行中 {} / {}",
                    &[&done.to_string(), &total.to_string()],
                ),
                cx,
            ));
        }
        if let Some(notice) = &self.notice {
            line = line.child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(if notice.failed {
                        tokens.colors.destructive
                    } else {
                        success
                    })
                    .child(SharedString::from(notice.text.clone())),
            );
        }
        line.into_any_element()
    }
}

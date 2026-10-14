//! 传输队列：入队、上传拖入文件、`spawn_transfer` 后台执行、取消 / 重试 / 标记完成与进度回写。

use super::*;

impl TerminalPage {
    /// 把选中项加入传输队列，并**立刻开始传**。
    ///
    /// 上传取本地面板的选中项、目标是远端的当前目录；下载反过来。
    /// 传输跑在后台执行器上（`scp` 是阻塞进程），完成后刷新**对侧**目录 ——
    /// 新文件该出现在列表里，否则用户无从确认它到底传成功没有。
    pub(super) fn queue_transfer(
        &mut self,
        upload: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let source = if upload { Side::Local } else { Side::Remote };
        let Some(name) = self.pane(source).selected.clone() else {
            notify::warning(tr(cx, "请先在文件列表里选中一项。"), window, cx);
            return;
        };
        let Some(handle) = self.sftp_target() else {
            notify::warning(tr(cx, "远端连接尚未建立，无法传输。"), window, cx);
            return;
        };
        let local_dir = self.pane(Side::Local).path.clone();
        let remote_dir = self.pane(Side::Remote).path.clone();
        // 远端目录还没列举出来时不给传：`scp` 的远端路径是真要用的，
        // 拿一个空串过去只会得到一个语焉不详的报错。
        if remote_dir.trim().is_empty() {
            notify::warning(tr(cx, "远端目录尚未确定，请先浏览一次远端。"), window, cx);
            return;
        }

        let id = self.next_transfer_id;
        self.next_transfer_id += 1;
        let request = TransferRequest {
            id,
            upload,
            local_file: PathBuf::from(&local_dir).join(&name),
            local_dir: PathBuf::from(&local_dir),
            remote_dir: remote_dir.clone(),
            remote_file: format!("{}/{}", remote_dir.trim_end_matches('/'), name),
        };
        self.begin_transfer(request, name.clone(), upload, handle, cx);
        notify::success(trf(cx, "已开始传输 {}。", &[name.as_str()]), window, cx);
    }

    /// 入队一条传输并立刻开跑（队列行 + 后台 `scp`）。
    ///
    /// 「选中项」与「从访达拖入」两条入口共用它：入队的表格形态、状态流转、
    /// 完成后的对侧刷新都只有一份实现。
    pub(super) fn begin_transfer(
        &mut self,
        request: TransferRequest,
        name: String,
        upload: bool,
        handle: RemoteHandle,
        cx: &mut Context<Self>,
    ) {
        self.transfers.push(Transfer {
            id: request.id,
            name,
            progress: 0.,
            total: None,
            done: 0,
            state: data::TransferState::Queued,
            upload,
            reason: None,
        });
        cx.notify();
        self.spawn_transfer(request, handle, cx);
    }

    /// 从访达拖进来的文件 → 上传到远端当前目录。
    ///
    /// 一次可落多个文件，每个各占队列一行；目录也收（上传路径上的 `scp -r` 会递归）。
    /// 判断落在**哪一个窗格**不靠指针坐标 —— 落点处理器挂在远端面板本身上，
    /// 只有拖到远端面板才触发（拖到本地面板什么都不发生）。
    pub(super) fn upload_dropped_files(
        &mut self,
        paths: Vec<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if paths.is_empty() {
            return;
        }
        let Some(handle) = self.sftp_target() else {
            notify::warning(tr(cx, "远端连接尚未建立，无法传输。"), window, cx);
            return;
        };
        let remote_dir = self.pane(Side::Remote).path.clone();
        if remote_dir.trim().is_empty() {
            notify::warning(tr(cx, "远端目录尚未确定，请先浏览一次远端。"), window, cx);
            return;
        }

        let mut started = 0usize;
        for path in paths {
            let Some(name) = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                continue;
            };
            let id = self.next_transfer_id;
            self.next_transfer_id += 1;
            let request = TransferRequest {
                id,
                upload: true,
                local_dir: path
                    .parent()
                    .map_or_else(|| PathBuf::from("."), PathBuf::from),
                local_file: path,
                remote_dir: remote_dir.clone(),
                remote_file: format!("{}/{}", remote_dir.trim_end_matches('/'), name),
            };
            self.begin_transfer(request, name, true, handle.clone(), cx);
            started += 1;
        }
        if started > 0 {
            notify::success(
                trf(cx, "已开始传输 {} 个条目。", &[&started.to_string()]),
                window,
                cx,
            );
        }
    }

    /// 起一次传输，并在旁边挂一个**字节数轮询**。
    ///
    /// 与旧版的关键区别：`scp` 进程由 [`duck_ssh::upload_spawn`] / [`duck_ssh::download_spawn`]
    /// **在本函数（UI 线程）里同步起好**并交出一个 [`duck_ssh::TransferHandle`] ——
    /// `Command::spawn` 不阻塞（阻塞的只有「等结果」），所以句柄能立刻登记进
    /// [`Self::transfer_handles`]，界面上任何「传输中」的行都**马上可取消**。
    /// 「等结果」被挪到后台执行器的一条线程上（`TransferHandle::wait` 轮询式等待，
    /// 取消随时插得进来）。三件事各自待在合适的线程上：
    ///
    /// 1. 传输本体（阻塞等待）跑在后台执行器的一条线程上；
    /// 2. 轮询也跑在后台执行器上（远端 `wc -c` 要走 ssh，绝不能落在 UI 线程）；
    /// 3. 数值回填走 `page.update`，回到 UI 线程。
    ///
    /// 分母量不到时（远端没有 `du` / `wc`、路径还不存在）就退化成不确定进度，
    /// **不编数据**：进度条停在 0 并显示「—」，而不是假装有个百分比。
    pub(super) fn spawn_transfer(
        &mut self,
        request: TransferRequest,
        handle: RemoteHandle,
        cx: &mut Context<Self>,
    ) {
        // 起进程 + 登记句柄（失败在下面统一处理）。先起后记，中间没有 let 借出 `self`。
        let spawned = if request.upload {
            duck_ssh::upload_spawn(
                &handle.target,
                Some(&handle.socket_dir),
                &request.local_file,
                &request.remote_dir,
            )
        } else {
            duck_ssh::download_spawn(
                &handle.target,
                Some(&handle.socket_dir),
                &request.remote_file,
                &request.local_dir,
            )
        };
        let transfer_handle = match spawned {
            Ok(transfer_handle) => transfer_handle,
            Err(error) => {
                // 起不来（本地源没了、没装 scp）就在这一帧直接落成失败，并保留输入快照
                // —— 「重试」按钮因此对这些失败同样有效（用户修好本地文件再点）。
                self.transfer_requests
                    .insert(request.id, (request.clone(), handle));
                self.mark_transfer(
                    request.id,
                    data::TransferState::Failed,
                    Some(ssh_error_text(cx, &error)),
                    cx,
                );
                return;
            }
        };
        self.transfer_handles
            .insert(request.id, transfer_handle.clone());
        self.transfer_requests
            .insert(request.id, (request.clone(), handle.clone()));
        // 重试会复用同一个行 id：把上一轮的读数清干净，否则进度条会从旧位置起跳。
        if let Some(row) = self.transfers.iter_mut().find(|row| row.id == request.id) {
            row.progress = 0.;
            row.done = 0;
            row.total = None;
        }
        self.mark_transfer(request.id, data::TransferState::Active, None, cx);

        cx.spawn(async move |page, cx| {
            let executor = cx.background_executor().clone();

            // 分母：上传用本地源（同步，便宜），下载问远端。
            let total = executor
                .spawn({
                    let request = request.clone();
                    let handle = handle.clone();
                    async move { transfer_total_bytes(&request, &handle) }
                })
                .await;
            if total.is_some() {
                let _ = page.update(cx, |page, cx| {
                    page.set_transfer_progress(request.id, total, 0, cx);
                });
            }

            // 传输跑到一半要能知道「结束了没」：用一个 `AtomicBool` 当跨线程信号，
            // 免得再引一条 oneshot 通道（本仓不新增依赖）。
            let finished = Arc::new(AtomicBool::new(false));
            let transfer = executor.spawn({
                let transfer_handle = transfer_handle.clone();
                let finished = Arc::clone(&finished);
                async move {
                    let result = transfer_handle.wait();
                    finished.store(true, Ordering::SeqCst);
                    result
                }
            });

            // 轮询：只在有分母时才有意义（没有分母就没得算）。
            if total.is_some() {
                while !finished.load(Ordering::SeqCst) {
                    executor.timer(PROGRESS_POLL).await;
                    if finished.load(Ordering::SeqCst) {
                        break;
                    }
                    let done = executor
                        .spawn({
                            let request = request.clone();
                            let handle = handle.clone();
                            async move { transfer_done_bytes(&request, &handle) }
                        })
                        .await;
                    if let Some(done) = done {
                        let _ = page.update(cx, |page, cx| {
                            page.set_transfer_progress(request.id, total, done, cx);
                        });
                    }
                }
            }

            let result = transfer.await;
            let _ = page.update(cx, |page, cx| {
                // 是不是**我们主动取消的**：`TransferHandle::wait` 对被杀掉的进程同样
                // 返回 `Err`（非零退出），光看 `result` 分不出「取消」与「真失败」——
                // 判据必须是「用户点过取消」（行状态已进 `Cancelling`）。
                let cancelled = page
                    .transfers
                    .iter()
                    .find(|row| row.id == request.id)
                    .is_some_and(|row| row.state == data::TransferState::Cancelling);
                page.transfer_handles.remove(&request.id);
                match result {
                    _ if cancelled => {
                        // 取消不是失败：清掉输入快照（不再提供「重试」），标成已取消。
                        page.transfer_requests.remove(&request.id);
                        page.mark_transfer(request.id, data::TransferState::Cancelled, None, cx);
                    }
                    Ok(()) => {
                        page.transfer_requests.remove(&request.id);
                        page.mark_transfer(request.id, data::TransferState::Done, None, cx);
                    }
                    Err(error) => {
                        // 失败：**保留**输入快照，行上给「重试」。
                        page.mark_transfer(
                            request.id,
                            data::TransferState::Failed,
                            // `scp` 的 stderr 原文：它指得出下一步（权限？磁盘满？路径不对？），
                            // 而一句「传输失败」什么也没说。
                            Some(ssh_error_text(cx, &error)),
                            cx,
                        );
                    }
                }
                if request.upload {
                    page.reload_pane(Side::Remote, cx);
                } else {
                    page.reload_pane(Side::Local, cx);
                }
            });
        })
        .detach();
    }

    /// 取消一行正在进行的传输：**真的终止** `scp` 进程（`TransferHandle::kill`）。
    ///
    /// 状态先进 `Cancelling` 再 `kill`：`kill` 只是发信号，进程要几十毫秒才退出，
    /// 而「谁取消的」这一事实得先落到行上 —— 后台等待任务回来时靠它把结果判成
    /// `Cancelled` 而不是 `Failed`（见 [`Self::spawn_transfer`] 的收尾）。
    pub(super) fn cancel_transfer(&mut self, id: u64, cx: &mut Context<Self>) {
        let cancellable = self
            .transfers
            .iter()
            .any(|row| row.id == id && row.state == data::TransferState::Active);
        if !cancellable {
            return;
        }
        self.mark_transfer(id, data::TransferState::Cancelling, None, cx);
        if let Some(handle) = self.transfer_handles.get(&id) {
            // `kill` 幂等：进程若刚好已退出，这是一次无害的空操作。
            handle.kill();
        }
        cx.notify();
    }

    /// 重试一行**失败**的传输：拿冻结的输入快照再跑一遍（新起一条 `scp`）。
    ///
    /// 只对失败行有效 —— 已完成 / 已取消的行没有「再来一次」的语义。
    /// 状态按迁移表走 `Failed → Queued → Active`：先回队列（清掉上一轮读数），
    /// 再交给 [`Self::spawn_transfer`] 起进程。
    pub(super) fn retry_transfer(&mut self, id: u64, cx: &mut Context<Self>) {
        let failed = self
            .transfers
            .iter()
            .any(|row| row.id == id && row.state == data::TransferState::Failed);
        if !failed {
            return;
        }
        let Some((request, handle)) = self.transfer_requests.get(&id).cloned() else {
            return;
        };
        self.mark_transfer(id, data::TransferState::Queued, None, cx);
        self.spawn_transfer(request, handle, cx);
    }

    /// 更新一行传输的状态（**状态机唯一入口**）。
    ///
    /// 迁移合法性由 [`data::TransferState::can_transition_to`] 判定：非法迁移记一条
    /// `warn` 后**照常应用** —— 这是刻意的「软校验」。宁可状态照常前进，也不让界面
    /// 卡在一个不动的格子上（例如某条竞态路径走到未列出的边时，取消按钮会永远转圈）。
    /// 迁移表本身是纯函数，已被单测逐条钉住。
    pub(super) fn mark_transfer(
        &mut self,
        id: u64,
        state: data::TransferState,
        reason: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.transfers.iter_mut().find(|row| row.id == id) {
            if !row.state.can_transition_to(state) {
                tracing::warn!(
                    id,
                    from = ?row.state,
                    to = ?state,
                    "传输状态出现未登记的迁移，仍按新状态应用"
                );
            }
            row.state = state;
            row.reason = reason;
            if state == data::TransferState::Done {
                row.progress = 1.;
                // 收尾时把 `done` 对齐到 `total`：某些落点（目录 `du -sk` 的块粒度）
                // 量出来会略微小于真实大小，不对齐就会显示「99%」卡着不走。
                row.done = row.total.unwrap_or(row.done);
            }
        }
        cx.notify();
    }

    /// 轮询到**真实字节数**后回填进度。
    ///
    /// 只更新数值，不动状态机：状态流转仍由 `mark_transfer` 一家负责，
    /// 否则「轮询还在跑、传输已经失败」时会互相打架。
    pub(super) fn set_transfer_progress(
        &mut self,
        id: u64,
        total: Option<u64>,
        done: u64,
        cx: &mut Context<Self>,
    ) {
        if let Some(row) = self.transfers.iter_mut().find(|row| row.id == id) {
            // 行已进入终态（完成 / 取消 / 失败）：迟到的进度轮询不再改动读数 ——
            // `mark_transfer` 已经把定稿（例如 `Done` 把 progress 钉在 1.0），
            // 让一次晚到的 `wc -c` 把它拉回去就会看到「100% 又掉头」。
            if row.state.is_terminal() {
                return;
            }
            if total.is_some() {
                row.total = total;
            }
            row.done = done;
            row.progress = match row.total {
                Some(total) if total > 0 => (done as f32 / total as f32).clamp(0., 1.),
                // 分母未知 / 为 0：留在 0，界面按「不确定」渲染。
                _ => 0.,
            };
        }
        cx.notify();
    }

    // ------------------------------------------------------------------
    // 缓冲区搜索（⌘F）。
    //
    // 命中由内核算（`Session::search_matches`，走 alacritty 的 DFA），界面只做三件事：
    // 记住「搜的是哪一块画布」、把网格坐标换算成视口行画高亮、按 Enter 在命中之间走。
    // ------------------------------------------------------------------
}

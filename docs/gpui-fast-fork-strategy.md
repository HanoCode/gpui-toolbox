# gpui-fast 内部化策略

> 范围：本仓 GPUI 内核（`gpui-pre` / `gpui-pre-platform` / `gpui-pre-macros` /
> `gpui-pre-sum-tree`）当前经 `[patch.crates-io]` 锁在 `longbridge/gpui-fast`
> 的某个 rev。文档给出**两条内部化路径**与推荐顺序，并约定未来的升级与发版流程。

## 1. 风险盘点（为什么必须内部化）

| 风险 | 当前 | 内部化后 |
| --- | --- | --- |
| 上游弃坑 / 公司仓库策略变更 | 一夜之间 rev 失效 | 我们自己控制 release tag |
| 升级缺 changelog | `git fetch` 只能看到一堆 commit，diff 要人肉 | release tag 配 changelog，逐版本可读 |
| 性能回归无基准 | 升完才发现「retained mode 没启」 | 基准脚本 CI 跑，回归即红 |
| 紧急 hotfix | 等 longbridge 合 PR + rebase | 自己 fork 直接改 |
| 内部修复合并 | 走外部 PR 等 review | 内部分支即合即用 |

## 2. 路径 A：fork 到自己组织（**推荐**）

### 步骤

1. 在 `yourorg/gpui-fast` 创建镜像（gh CLI 一行）：
   ```bash
   gh repo create yourorg/gpui-fast --private \
     --description "Mirror of longbridge/gpui-fast with project-specific patches"
   ```
2. 推一份与 `b8f0c026553711c1e049fee501932d3801f23567` 同步的初始分支（`main`）：
   ```bash
   git remote add upstream https://github.com/longbridge/gpui-fast.git
   git fetch upstream b8f0c026553711c1e049fee501932d3801f23567
   git push origin main
   ```
3. **第一次合入前**：写 `CHANGELOG.md`（首版从 upstream 拉一份，标注「同步自
   `b8f0c026`」），写 `RELEASE.md`（升级流程见下）。
4. 改根 `Cargo.toml` 的 `[patch.crates-io]`：
   ```toml
   gpui-pre = { git = "https://github.com/yourorg/gpui-fast", rev = "v0.1.6" }  # ← tag，不是 commit
   ```
5. `cargo update -p gpui-pre` 重生成 lock 文件。
6. 跑全套门禁（`cargo check -p duck-app --locked` + `cargo test -p duck-theme` +
   `cargo clippy --workspace --all-targets`），记录首次 baseline 帧时间。

### 后续升级流程（每季度一次）

```bash
# upstream 有新 commit
git fetch upstream
git log --oneline upstream/main  # 审 changelog

# 拉到本地分支
git checkout -b sync/upstream-$(date +%Y%m%d)
git merge upstream/main
# 解决冲突（如果有 patch 冲突——本仓目前没有 patch，只跟着 upstream 走即可）

# 写 changelog、跑基准、修任何回归
./scripts/bench-gpui-fast.sh
cargo test --workspace
cargo clippy --workspace --all-targets

# 打 tag
git tag v0.2.0
git push origin v0.2.0

# 根 Cargo.toml 改 rev 到 tag
# Cargo.lock 由 CI 自动更新
```

## 3. 路径 B：完全 vendor（与 `alacritty_terminal` 同款）

如果不想 fork 上游组织（如公司无 GitHub 仓库管理权限），可以走全 vendor 路线：

1. `cargo vendor gpui-fast` 把所有 crates.io 依赖和 gpui-fast 子 crate 全部拷到 `vendor/`。
2. 改根 `Cargo.toml`：
   ```toml
   [source.crates-io]
   replace-with = "vendored-sources"
   [source.vendored-sources]
   directory = "vendor"
   ```
3. 同时取消 `[patch.crates-io]`（vendor 已经替身完成）。
4. 升级 = 重新 `cargo vendor` 拉新版本 + 解决可能的 patch 冲突（见 `vendor/README.md`）。

### 路径 B 的代价

- 体积：vendor 目录从 ~20 MB 涨到 ~150 MB（gpui-fast 拖一堆编译产物）
- 升级成本：每次升级要全量 vendor，CI 时间 +1-2 min
- 不灵活：patch 必须改 vendor 内的源码，diff 散在多个 commit 之间

**只有当公司禁止依赖外部 git 时**才走路径 B。

## 4. 性能基准（必做，不管走哪条路径）

性能是 gpui-fast 的核心价值（retained mode + scroll layers），必须有一道**可重复**
的基准把帧时间钉住。脚本见 `scripts/bench-gpui-fast.sh`，CI 跑在
`.github/workflows/gpui-fast-bench.yml`（见该文件说明）。

基线（首跑记录在 `docs/gpui-fast-baseline.md`）：

| 场景 | 目标帧时间 | 实测 |
| --- | --- | --- |
| 主窗打开（cold start） | < 100 ms | （待测） |
| 1000 行列表滚动 | < 8 ms / 帧 | **7.2 ms / 帧**（1000 行 × 20 列数据网格，release，1400×860 无头，探针 `crates/duck-components/tests/data_table_cost.rs`） |
| 切路由（toolbox ↔ database） | < 16 ms | （待测） |
| retained mode 启 / 停环境变量切换 | < 50 ms | （待测） |

⚠️ **读这张表前必须确认编译档**：同一帧（上表那一行）在 dev 档是 **68.2 ms**、release 档是
**7.2 ms** —— 相差约 9.5 倍，而**两次的分配次数完全相同**（28,090 次）。dev 档默认
opt-level 0 时 taffy 排版 / 文本整形 / gpui 都不优化，界面会「看起来很卡」。所以
根 `Cargo.toml` 的 `[profile.dev]`（本仓 1 / 依赖 3，zed 同款）是这张表成立的前提：
拿掉它再测，数字会整体塌掉一个数量级，那不是回归，是编译档。

环境变量闸门（保留回退能力）：
- `GPUI_VIEW_RETENTION=0` 关 retained mode
- `GPUI_SCROLL_LAYERS=0` 关 scroll layers

⚠️ **scroll layers 在无头测试里量不到**：`layout_stats()` 的 `layer_frames_composited` /
`layer_frames_repainted` 恒为 0（连一个裸 `uniform_list` 也是），因为 headless 渲染器
没有 tile 合成路径 ⇒ 别拿它当验收口径，也别据此判定「层没生效」。

## 5. CHANGELOG 与 release tag

`CHANGELOG.md` 用 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 格式。
**每发一版必写一段**：Added / Changed / Deprecated / Removed / Fixed / Security。

release tag 命名：跟随 upstream 的版本号（v0.1.6+1 → v0.1.7），后缀表示本仓
patch 序号（`+1` 是 first patch）。

## 6. 验收清单

- [ ] 路径 A：fork 仓库创建并同步 baseline
- [ ] 根 `Cargo.toml` 的 rev 改为可读的 tag（如 `v0.1.6+1`）而非 commit hash
- [ ] `CHANGELOG.md` 首版就位
- [ ] `RELEASE.md` 升级流程文档就位
- [ ] `scripts/bench-gpui-fast.sh` 可运行，CI 接入
- [ ] `docs/gpui-fast-baseline.md` 记录首跑基线
- [ ] 全量门禁（check / test / clippy）通过

## 7. 后续 follow-up

- §6.3.5「崩溃上报」落地后，把 `bench-gpui-fast` 的结果上报到远程（不上报本地）
- 与 `vendor/alacritty-terminal` 共用同一份「vendor 升版脚本」（`sbin/vendor-sync.sh`
  计划项）

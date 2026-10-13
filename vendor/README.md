# vendor/ — vendored 依赖（[patch.crates-io] 指向这里）

| 目录 | 上游 | 基线 | 来源 | 补丁 |
| --- | --- | --- | --- | --- |
| `alacritty-terminal/` | `alacritty_terminal` | **0.26.0**（MSRV 1.85.0，Apache-2.0） | 从 `temp/oxideterm`（oxideterm 的 vendored 副本）拷入 | 见目录内 `OXIDETERM_PATCHES.md`（ASCII 快路径 + 视口滚动单独上报 damage + 转发 `OSC 133` 标记与 `Grid::anchor` 锚点） |
| `vte/` | `vte` | **0.15.0**（MSRV 1.62.1，Apache-2.0 OR MIT） | 同上 | 见目录内 `OXIDETERM_PATCHES.md`（ASCII 批量跨度 + `OSC 133` 原样转发到 `Handler::shell_mark`） |

## 与原版的差异

1. 删除 `[lints] workspace = true`：vendored crate 不是本 workspace 成员，
   不应被本仓 lint 基线（`deny(unsafe_op_in_unsafe_fn)` 等）管到。
2. 删除 `alacritty-terminal/Cargo.lock`（registry 解包残留，path 依赖不需要）。

## 同步约定

- 升级基线 = 从上游（alacritty / oxideterm）重新拷贝并重放上面两条差异，同时更新本表。
- 依赖一变必须重生成并提交 `assets/licenses/THIRD_PARTY_LICENSES.md`（static 腿 + package.sh 双重门禁）。

# Duck Toolbox

> 基于 [GPUI](https://gpui-kit.com)（gpui-kit 快照）构建的 macOS 桌面工具箱外壳。

Duck Toolbox 是一个 Rust 桌面应用的起点工程：在 `gpui-component` / `gpui-base` 之上封装了
**主题系统、国际化、视觉组件库与窗口外壳**（自绘标题栏、活动栏、路由与内容槽位），
用于承载后续各类桌面工具页面。

## 技术栈

| 层 | 选型 |
| --- | --- |
| 语言 | Rust（edition 2024，MSRV 1.85） |
| UI 框架 | GPUI —— 经 gpui-kit 快照引入 |
| 组件层 | `gpui-component` 0.7.1 · `gpui-base` 0.7.1 |
| GPUI 内核 | `gpui-pre` 0.3.8（以 `gpui` 为 crate 别名） |
| 平台层 | `gpui-pre-platform` 0.3.8 |
| 图标资源 | `gpui-kit-assets` 0.7.1 + 自带 Lucide SVG |

> 依赖版本与 gpui-kit 发布时锁定的快照**严格对应**，升级任一 crate 时必须同步 `=` 约束，
> 否则 GPUI 内核与组件层 API 会错配。详见 `Cargo.toml` 注释与 `AGENTS.md`。

## 目录结构

分层是**单向**的：下面的层不知道上面的层存在，`duck-platform` 及插件三件套刻意不依赖 gpui，
因此可以脱离 UI 单独测试。

```
gpui-toolbox/
├── crates/
│   ├── duck-app/             # 二进制入口（bin = duck）：窗口装配、页面
│   ├── duck-shell/           # 窗口外壳：自绘标题栏、活动栏、路由与内容槽位
│   ├── duck-components/      # 视觉组件库：按钮 / 导航 / 卡片 / 表单原语 / 侧栏面板
│   ├── duck-theme/           # 主题层：token 快照、内置预设、国际化（i18n）
│   ├── duck-command/         # 交互骨架：动作注册表 / 键位 / 菜单 / 命令面板索引
│   ├── duck-tools/           # 工具注册表：元数据、分类、拼音索引、工具级设置
│   ├── duck-platform/        # 平台能力：路径 / 日志 / 原生对话框 / 剪贴板 / 系统外壳 / 单实例锁
│   ├── duck-store/           # 持久化：SQLite（内嵌）+ 版本化迁移 + 去抖写入
│   ├── duck-plugin/          # 插件框架：plugin.toml 清单解析、能力网关、贡献发现
│   ├── duck-plugin-js/       # 插件 JS 执行轨：QuickJS 沙箱 + 能力仲裁的宿主函数桥
│   ├── duck-plugin-registry/ # 插件市场：远端索引、拉取、SHA-256 校验、安装/卸载
│   └── duck-update/          # 更新器：JSON manifest + SHA-256 校验 + 就地替换 .app
├── sbin/                     # 构建脚本：run.sh / package.sh（菜单） / package-*.sh / licenses.py 等
├── plugins/                  # 随仓库分发的插件（text-stat 等）
├── docs/                     # 架构与设计文档（含架构差距分析）
├── .skills/                  # GPUI Kit 编码与设计指南（供 AI 助手与开发者查阅）
├── assets/                   # 应用图标等资源（AppIcon.icns）
├── .github/workflows/        # CI 门禁（ci.yml）与发版（release.yml）
├── dist/                     # 打包产物（已 gitignore）
└── temp/                     # 上游参考源码（已 gitignore）
```

## 快速开始

环境要求：macOS 11+，Rust stable（版本见 `rust-toolchain.toml`）。

```bash
# 运行（debug 构建 + 启动）
sbin/run.sh

# release 运行 / 指定日志级别
sbin/run.sh --release
sbin/run.sh --log-level debug

# 打包 macOS 应用包：dist/Duck.app，可附产 .dmg / .zip
sbin/package.sh --ci --mac
# 交互菜单（选平台）
sbin/package.sh
# 全平台打包（macOS + Windows + Linux + RPM）
sbin/package.sh --ci --all

# 直接使用 cargo
cargo run -p duck-app
```

打包默认进行 **ad-hoc 签名**；正式分发需指定 Developer ID 证书并完成公证。

## 主题与国际化

- **主题**：内置 **42 套预设**（明暗两个槽位各选一套，默认 `Default Dark` / `Default Light`）。
  外观统一经 `duck_theme::update_appearance(cx, |a| ...)` 修改，落在一个 `Appearance`
  结构体里（主题槽位、强调色、界面缩放、界面密度、字号/字体、语言、活动栏与右栏开关等），
  再整体序列化进单个持久化键。字段清单以 `duck-theme/src/manager.rs` 为准。
- **国际化**：层位于 `duck-theme::i18n`，支持 `System` / `ZhHans` / `En`。
  采用「**源串即键**」策略 —— 代码中直接写中文，取词用 `tr(cx, "外观")`，
  英译表为 `i18n.rs` 的 `EN`。新增中文文案必须同步补 `EN`，否则测试守卫失败。

## 开发规范

编码助手与协作者的**强制约束**集中在 [`AGENTS.md`](./AGENTS.md)，动手前请先阅读。
另有格式与静态检查基线：`rustfmt.toml`、`.editorconfig`、`Cargo.toml` 的 `[workspace.lints]`。

## 许可证

[MIT](./LICENSE) © 2026 HanoCode

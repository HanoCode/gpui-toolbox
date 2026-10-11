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

```
gpui-toolbox/
├── crates/
│   ├── duck-app/         # 二进制入口（bin = duck）：窗口装配、页面
│   ├── duck-shell/       # 窗口外壳：自绘标题栏、活动栏、路由与内容槽位
│   ├── duck-components/  # 视觉组件库：按钮 / 导航 / 卡片 / 表单原语 / 侧栏面板
│   └── duck-theme/       # 主题层：token 快照、内置预设、国际化（i18n）
├── sbin/                 # 构建脚本：run.sh / package.sh / _common.sh
├── .skills/              # GPUI Kit 编码与设计指南（供 AI 助手与开发者查阅）
├── .workbuddy/memory/    # 项目约定与工作日志
├── assets/               # 应用图标等资源（AppIcon.icns）
├── dist/                 # 打包产物（已 gitignore）
└── temp/                 # 上游参考源码（已 gitignore）
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
sbin/package.sh --dmg --zip
sbin/package.sh --universal --sign "Developer ID Application: ..."

# 直接使用 cargo
cargo run -p duck-app
```

打包默认进行 **ad-hoc 签名**；正式分发需指定 Developer ID 证书并完成公证。

## 主题与国际化

- **主题**：`Appearance { preference, accent, ui_scale, font_size, font_family, language, theme }`
  统一经 `duck_theme::update_appearance(cx, |a| ...)` 修改。内置 5 套预设：
  DefaultDark / DefaultLight / MidnightBlue / Nord / WarmLight。
- **国际化**：层位于 `duck-theme::i18n`，支持 `System` / `ZhHans` / `En`。
  采用「**源串即键**」策略 —— 代码中直接写中文，取词用 `tr(cx, "外观")`，
  英译表为 `i18n.rs` 的 `EN`。新增中文文案必须同步补 `EN`，否则测试守卫失败。

## 开发规范

编码助手与协作者的**强制约束**集中在 [`AGENTS.md`](./AGENTS.md)，动手前请先阅读。
另有格式与静态检查基线：`rustfmt.toml`、`.editorconfig`、`Cargo.toml` 的 `[workspace.lints]`。

## 许可证

[MIT](./LICENSE) © 2026 HanoCode

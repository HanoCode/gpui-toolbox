# 插件签名设计（§6.2.4）

> 复用 `duck-update` 的 minisign 验签基建——零新增依赖，与更新器**同一套格式 / 同一套私钥管理**。
> 本文档给出**最终形态**，代码落地见 §6.2.4 follow-up 工单。

## 1. 为什么必须签名

`crates/duck-plugin-registry` 当前从 `https://yourorg/plugin-index` 拉索引、按
`plugins/<id>/<version>.tar.gz` 拉包，**只用 SHA-256 校验完整性**。

- **完整 ≠ 真实**：索引本身可以被中间人替换为「同样哈希但是攻击者写的包」。
- **SHA-256 密钥在仓库里**：CDN / 索引仓被攻陷 = 攻击者写的新包也能通过校验。
- **插件 = 任意代码执行**（QuickJS 沙箱 + `host::fetch` + `host::readFile`）。
  即便沙箱在，被替换的恶意包也可以发请求、读文件。

签名是**身份**层的防护：包必须由你信任的私钥持有者签。

## 2. 复用 `duck-update` 的 minisign 基建

`crates/duck-update/src/signature.rs` 已经有：

- `verify_minisign(pub_key, message, signature) -> Result<(), SignatureError>`
- 公钥编译期内建在二进制里（`build.rs` 读取 `assets/keys/minisign.pub`）
- 同一套格式（Ed25519 + BLAKE2b-512 预哈希）

**结论**：插件签名**直接调用** `duck_update::signature::verify_minisign`，
**不引**新 crate、不写新算法。

## 3. 文件格式

每发一版插件，发布侧除了 `plugin.tar.gz` 与 `plugin.toml`，还要给一个
`plugin.minisig`：

```text
# plugin.minisig
untrusted comment: minisign encrypted signature
<base64 signature, 写到 plugin.tar.gz 的 SHA-256 上>
<base64 global signature, 写到 plugin.toml 的 SHA-256 上>
trusted comment: <algorithm> <key id> <hash>
<...>
```

`minisign -S -s <priv.key> -m plugin.tar.gz` 一行生成。

### 索引侧

`plugin-index` 仓的 `plugins/<id>.json` 添两个字段：

```json
{
  "id": "http-replay",
  "version": "0.2.0",
  "tarball": "plugins/http-replay/0.2.0.tar.gz",
  "tarball_sha256": "...",
  "tarball_minisig": "plugins/http-replay/0.2.0.tar.gz.minisig",
  "manifest_sha256": "...",
  "manifest_minisig": "plugins/http-replay/0.2.0.toml.minisig"
}
```

客户端下载 tarball / manifest 时**同步**下载对应的 `.minisig`。

## 4. 验签流程

`crates/duck-plugin-registry/src/install.rs::install_from_archive` 当前流程：

```
1. 拉索引 → 拉 tarball
2. 校验 tarball.sha256 == 计算的 SHA-256        ← 现在
3. 解 tarball / 读 plugin.toml
4. 写 installed 状态
```

加签名后：

```
1. 拉索引 → 拉 tarball + tarball.minisig + manifest.minisig
2. 验 tarball.minisig（公钥内建、消息是 tarball 字节）  ← 新增
3. 校验 tarball.sha256（防御签名算法 bug）
4. 解 tarball / 读 plugin.toml
5. 验 manifest.minisig（消息是 plugin.toml 字节）      ← 新增
6. 写 installed 状态
```

**任一失败 → 拒绝安装 + 走告警通道**（notify::error + 写 audit 行）。

## 5. 私钥管理

| 用途 | 私钥位置 | 持有者 |
| --- | --- | --- |
| 私仓 self-host 插件 | `~/.minisign/<filename>.key` + 密码 | 仓库 owner |
| CI 自动签名 release | GitHub Actions Secret `MINISIGN_SECRET_KEY_BASE64` + `MINISIGN_PASSPHRASE` | CI |
| 第三方插件作者 | 自有私钥 | 外部开发者 |

**客户端永远只验不签**——私钥永远不进客户端二进制。

### 第三方插件的信任模型

`crates/duck-plugin/src/capability.rs` 已经有 `CapabilityGranter`，但**默认信任
所有安装的插件**。新流程：

1. 第三方插件作者用**自己的私钥**签 tarball。
2. 私仓 owner 收到 tarball 后**手动 rebase 到自己的私钥**（重新签），推上索引。
3. 客户端只信**官方私钥**——这条链之外的包即便签名也是「不认识的私钥」。

`assets/keys/official-plugins.pub` 是唯一信任根。

## 6. 撤列表（revocation）

插件可被**撤**——比如发现 0.2.x 有漏洞：

```
plugins/<id>/revoked.json
  { "versions": ["0.2.0", "0.2.1"], "reason": "...", "revoked_at": "..." }
```

客户端 `update.rs::is_revoked(id, version)` 在 install 前查一次；已装但被撤
的插件下次启动弹**「应当更新」**提示。

撤列表本身用**官方私钥**签（与更新器 `manifest` 同款）。

## 7. 与 min_app_version 守门的关系

`duck-update` 已经有 `min_app_version`（应用版本低于门槛则更新被拒）。
插件没有这层（插件可以独立于应用发版），但**安装路径**仍走 `duck-store` 的
版本化迁移——装上一个**要求更新到新应用**的插件是允许的（`min_app_version`
是更新的门槛，不是插件的）。

## 8. 关键改动清单

| 文件 | 改动 |
| --- | --- |
| `crates/duck-update/src/signature.rs` | 提为 `pub`：让 `duck-plugin-registry` 直接调 `verify_minisign` |
| `crates/duck-plugin-registry/src/install.rs` | 加 minisig 验签两步（tarball / manifest） |
| `crates/duck-plugin-registry/src/index.rs` | 解析 `tarball_minisig` / `manifest_minisig` 字段 |
| `assets/keys/official-plugins.pub`（新增） | 官方插件公钥（与 `assets/keys/minisign.pub` 同源） |
| `build.rs`（根） | 把 `official-plugins.pub` 编进二进制 |
| `crates/duck-plugin-registry/src/revocation.rs`（新增） | 撤列表 + 校验 |
| `crates/duck-update/src/signature.rs::verify_minisign` | 已是 `pub`，确认签名接口稳定 |
| `docs/plugin-signing.md` | 用户向的「插件开发者如何签名」指南 |
| `sbin/sign_plugin.sh` | CI 脚本：拉 tarball + 调 `minisign -S` + 推索引 |

## 9. 验收清单

- [ ] `crates/duck-update::verify_minisign` 暴露为 `pub` API
- [ ] `crates/duck-plugin-registry` 在 install 路径上完成 tarball + manifest 双验
- [ ] 公钥编进二进制，**没有**明文私钥
- [ ] 撤列表同源签名
- [ ] `sbin/sign_plugin.sh` 走完一遍 release 演练
- [ ] `docs/plugin-signing.md` 第三方插件作者看得懂

## 10. 实施顺序（1 个 PR ≈ 6 人日）

1. **Day 1**：把 `verify_minisign` 提到 `pub`；建 `official-plugins.pub`。
2. **Day 2**：`install.rs` 加 minisig 校验；增加 `tarball_minisig` / `manifest_minisig` 字段。
3. **Day 3**：撤列表 + `revocation.rs`；UI 在设置里加「插件更新检查」。
4. **Day 4**：`sbin/sign_plugin.sh` 写好，CI 试跑一次。
5. **Day 5**：写 `docs/plugin-signing.md`（第三方开发指南）。
6. **Day 6**：测试 + 集成（含**未签名包拒绝**、**签名错误包拒绝**、**撤列表命中**）。

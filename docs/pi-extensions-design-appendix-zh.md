# Pi Extensions 一等支持设计附录

> 状态：仅设计，不含实现。主工程不实现 extensions；themes 明确不在范围内。
> authority：任何上游路径、加载顺序、启用规则或 tool 组合语义，在实施前都必须
> 由 pin `ab366ebe94cacd419d986be454f12b1b9913aaca` 的 oracle 或
> `scripts/pi-transport-capture.mjs` 实际执行确认，本文不以源码阅读代替证据。

## 目标

未来让用户在 cc-switch 中观察、导入、启用和停用 Pi extensions，同时满足：

1. 原生文件/目录是真相，`exists = active`，不制造与 Pi 分叉的 enabled 影子状态；
2. 只接管 cc-switch 明确拥有或用户显式、可验证采用的内容；
3. extension 提供的 tools 与 pinned core tools 分开展示，不伪装为 MCP server；
4. 所有写操作可并发检测、可补偿，portable import 不覆盖目标设备的未知资产；
5. 复用前置 C inspection 与当前 Skill/Prompt 的共享文件、fingerprint、ownership
   和协调器原语。

## 模块边界

```text
PiExtensionInspector (只读、oracle 驱动)
        │
        ├── NativeExtensionObservation
        │      path / fingerprint / manifest / contributed tools
        │      validity / reasons / ownership
        │
        └── PiExtensionCoordinator (唯一写入口)
               ├── exact-content adoption
               ├── CAS + atomic replace
               ├── ownership ledger transaction
               ├── catalog epoch / UI invalidation
               └── compensation on partial failure
```

- `PiExtensionInspector` 只负责原生观察和结构化诊断；不得写数据库或文件。
- `PiExtensionCoordinator` 是唯一写入口；命令、deeplink、portable reconcile 与
  UI mutation 都调用它，禁止各自复制目录。
- 通用 `shared_file`、Skill tree fingerprint 与 ownership ledger 可复用；
  extension-specific manifest/加载规则必须先新增捕获向量，不能借 Skill 规则猜测。
- gateway 只消费 coordinator 发布后的 immutable runtime snapshot；extension
  不能在请求中途直接改 candidate/header 计划。

## 状态模型

建议公开三个正交维度：

- `discovery`: `absent | active | invalid | unknown`，只来自 native observation；
- `ownership`: `external | adoptable_exact | managed | conflict`；
- `capability`: `inspectable | manageable | unsupported | unknown`。

不得增加独立 `enabled` 布尔值。用户点击“停用”时，语义是对受管原生资产执行可逆
移除；外部资产只能显式采用后再管理。内容变化导致 fingerprint 不匹配时进入
`conflict`，不得覆盖。

## Tools 与 MCP

- capture 已确认 pinned core tool inventory 为
  `bash/edit/find/grep/ls/read/write`；未来 capture 应分别记录每个 extension
  注入前后的 tool inventory 与来源。
- UI 将 tools 按 `core` / `extension:<id>` 分组，并展示冲突与覆盖次序的实测结果。
- MCP 页面仍不为 Pi 建虚假 registry。即使某个 extension 通过自身机制连接外部
  tool，也属于 extension capability，除非未来 pinned Pi 真正提供 MCP registry
  且由新证据和契约明确升级。

## Portable 与冲突策略

- 备份只携带 cc-switch 拥有的 extension 描述、内容 hash 和期望状态，不携带绝对
  目录、设备 token 或未知外部目录。
- 导入先观察目标设备；missing 可部署，exact 可采用，different 必须 conflict，
  绝不“最后写入者获胜”。
- 多 extension 贡献同名 tool、command 或资源时 fail-closed；只有 oracle/capture
  证明 Pi 的确定 precedence 且产品明确展示该覆盖时，才允许自动解析。

## 实施前验收

1. 扩展 transport capture：发现路径、空/损坏 manifest、启停、重复 ID、资源覆盖、
   tool inventory、相对路径和 symlink 负例。
2. 冻结 schema/oracle provenance，建立 lossless raw observation；未知形状为
   `unknown`，不得整目录连坐隐藏合法兄弟。
3. 服务级测试覆盖显式采用、并发 CAS loser、写后补偿、portable reconcile、
   外部冲突、目录越界与 UI 的 `exists = active`。
4. 中英文 UI 与可访问性完成后，再进入独立实现与盲审。

Themes 不与 extensions 共用该项目：其资源语义、预览与安全面另行立项。

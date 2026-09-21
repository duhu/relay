# Relay 项目概览（供 agent 参考）

> 让多台 Mac 共用的显示器与罗技键鼠"一键接力"：键盘按 Easy-Switch 离开本机 → 本机把画面（DDC）和鼠标（HID++ ChangeHost）一起交给目标主机。也支持菜单 / 热键 / CLI 手动切换。

## 文档边界

- 本文件：架构、模块地图、跨模块不变量、能力边界。每个任务必读。
- `docs/specs/relay-core.md`：需求与设计决策记录（D1–D10 逐条写了理由）。
- `README.md` / `README.zh-CN.md`：面向使用者——硬件前提、安装、第一次配置、能力边界。

## 技术栈与形态

- **Tauri 2**：Rust 后端 + WebView 前端（Vue 3 + Vite + TS，手写 macOS 风 CSS，无组件库）。参考形态：[AskHuman](https://github.com/Naituw/AskHuman)。
- **单一可执行文件 `relay`**，按 argv 切换角色：默认无参数 = 常驻 menu bar app；`relay switch <host>` / `relay --settings` / `relay --status` = 短命 CLI，经本地 IPC 与常驻实例通信。
- **仅 macOS / Apple Silicon**。跨平台抽象保留在 crate 边界上，但不做其它平台实现。
- **设备层直接复用 OpenLogi 的 crate**：`openlogi-hid`（IOHIDManager 枚举、传输、TCC 权限探测、热插拔流）+ `openlogi-hidpp`（HID++ 2.0 协议、`ChangeHost 0x1814`、接收器）。以钉死版本的 crates.io 依赖引入（`=0.8.5`），不改上游。
- **显示器层**：Rust FFI 调 `IOAVService*`（私有 API），逻辑移植自 m1ddc（Objective-C，MIT）。

## 运行架构

```
relay (menu bar, LSUIElement)                 relay CLI（短命）
┌───────────────────────────────────────┐     ┌──────────────────┐
│ Tauri runtime · tray · settings window│◄────┤ unix socket IPC  │
│                                       │     └──────────────────┘
│ relay-core（无 UI 依赖，可单测）         │
│  Coordinator ── 状态机：Idle/Confirming/Switching/Cooldown
│  Triggers    ── presence(IOHIDManager 回调) · manual(ipc/tray/settings) · hotkey(M4)
│  Devices     ── HostSwitchable ⇐ LogitechHidpp（openlogi-hid / openlogi-hidpp）
│  Display     ── DisplayInput   ⇐ DdcDisplay（IORegistry 找外接屏 + IOAVService 私有 FFI 写 DDC）
│  Config      ── JSON + notify 热加载
└───────────────────────────────────────┘
```

**进程职责**：
- **常驻实例**：持有 Coordinator、触发源、配置监听、tray；设置窗口按需创建、关闭即销毁，所有状态在 core。
- **CLI**：解析参数 → 经 IPC 发给常驻实例 → 打印结果 → 退出码 0 成功 / 1 失败 / 2 用法错误 / 3 常驻实例不在。
- 常驻实例不存在时 CLI 自启它（`detach` 方式，同 AskHuman `daemon/spawn.rs`）。

## 目录结构（M2 实际；标 ▸ 的为 M3+ 目标态）

```
relay/
  Cargo.toml          workspace 根：members = ["src-tauri", "crates/relay-core"]（Cargo 要求成员在根目录下）
  AGENTS.md  package.json  vite.config.ts  tsconfig.json  index.html
  docs/               overview.md · specs/relay-core.md
  scripts/            dev-install.sh（build --debug → /Applications/Relay-dev.app → 签名 → symlink relay）· gen-icons.py

  src/                Vue 前端（无组件库）
    App.vue           按 location.hash 路由 settings / log（▸ wizard，M4）
    views/SettingsView.vue（可编辑整份 config）· LogView.vue
    lib/ipc.ts        invoke 封装 + 与 Rust 结构一一对应的 TS 类型
    lib/i18n.ts       中英两份字典 + t()；语言取自 Status.language（核心已把 auto 解析成具体值）

  src-tauri/
    Cargo.toml        relay-app，[[bin]] name = "relay"
    tauri.conf.json   bundle id work.bam.relay（不再内嵌任何外部二进制）；tauri.dev.conf.json 覆盖为 Relay-dev / work.bam.relay.dev
    Info.plist        LSUIElement
    src/main.rs       cli::dispatch()
    src/cli.rs        argv 解析、switch/status/--settings/--devices/--displays、IPC client、自启常驻实例
    src/ipc.rs        unix socket server + NDJSON Request/Response
    src/app/          mod.rs（setup、单实例锁、autostart、TCC 提示）· tray.rs · commands.rs（含 scan_devices / list_displays）· windows.rs · paths.rs · example-config.json

  crates/relay-core/  纯 Rust，无 Tauri 依赖
    types.rs          HostIndex · DeviceId · DeviceRole · TriggerEvent
    config.rs         模型、校验（含 cooldown/debounce 下限）、原子写；config_watch.rs：notify 目录监听 + 300ms 去抖
    plan.rs           build_plan · target_for_leave · next_host
    coordinator.rs    sans-IO 状态机（事件 + now → 动作）
    executor.rs       按 SwitchPlan 驱动 trait 对象：先显示器（重试 1+ddc_retries）后设备
    runtime.rs        事件循环、单定时器、切换串行化、配置仅在 Idle 时替换、CoreHandle
    trigger/          mod.rs（PresenceTracker：多 HID 节点折叠为一次 Left/Arrived）· presence.rs（IOHIDManager 回调 FFI、list_hid_devices）
    device/           mod.rs（HostSwitchable trait）· logitech.rs（LogitechHidpp）· discovery.rs（scan_switchable_devices）· replay_support.rs（test only）
    display/          mod.rs（DisplayInput trait）· ioav_ffi.rs（三个 IOKit 私有符号：create / write / read）· ddc.rs（IORegistry 发现 + DDC 写入，DdcTransport 测试缝）capabilities.rs（能力串 0xF3 → 它支持哪几个输入源 + MCCS 名字表）
    permissions.rs    IOHIDCheckAccess / IOHIDRequestAccess
    log.rs            tracing → 按天文件 + 200 条环形缓冲供 UI
    examples/          hid_watch.rs（观察插拔事件）· scan.rs（扫描可切换设备；需输入监控，勿在 agent shell 里跑）· ddc.rs（`list` / `set <edid_uuid|first> <code>`；无需权限，但写前先确认用户在本机）
```

## 跨模块不变量

1. **所有硬件动作只经 `Coordinator`**。触发源、CLI、设置页的"试切"都只发 `TriggerEvent`，不直接碰设备/显示器。
2. **切换顺序固定**：先显示器，后设备；设备按 `follow=true` 顺序逐个。
3. **HID++ 句柄用完即关**；不与 Logi Options+ 长期争抢设备。
4. **只对已声明的主机槽发 ChangeHost**；配置校验在加载时完成，非法配置拒绝启动 Coordinator 并在 tray 提示。
5. **取回前先问显示器画面在哪儿，而且等防抖到期才问**：触发设备离开本机时动作（D4）；到达分两条互不重叠的路径——`Switching` + `Cooldown` 窗口里触发设备回来 → 冷却结束时切回本机（`options.switch_back_on_reconnect`，默认开）；`Idle` 里触发设备到达 → 先防抖，**到期那一刻**才读一次第一台显示器的当前输入源（VCP 0x60；到达那一刻不读，此时距对面写入才约 1 秒，显示器还在重新同步、常常读不准），画面在别处就把画面与跟随设备取回本机，画面已在本机就不动作（`options.pull_on_arrival`，默认开）。**画面在别处时，本机刚启动、从没切过也照样取回**。读不出来（显示器 0x60 只写 / 睡眠，或连问两次都失败、或读超过 1.5s 的决策预算）才退回旧的兜底规则：只在本机上次切换的目标是别的主机时取回，没切过就不动作。这次读只发生在这一条路径上，且预算内必须返回，不能拖住事件循环。**离开永远压过取回**：防抖期间（以及正在读显示器的那一刻）同一台触发设备再离开，不是取消取回，而是原样改走离开序列——重新起一段离开方向的防抖，把画面和跟随设备送去对面；正在进行的那次读当场丢掉，按「读不出来」处理。
6. **主机索引 0 基**，仅 UI 层 +1。
7. **配置文件是唯一真源**（`~/Library/Application Support/Relay/config.json`），UI 改动写文件 → notify → core 重载；core 不持有 UI 状态。
8. CLI 的 stdout 只输出结果，日志走 stderr（与 AskHuman 一致）。

## 能力边界

- 能切：支持 HID++ `ChangeHost` 的罗技设备（BLE 直连 / Bolt / Unifying）；支持 DDC 写 VCP 0x60 的外接显示器。
- 能读：显示器肯回答 VCP 0x60 读时，能读回它当前显示的输入源，用来判断画面在不在本机（设置页也用它回填输入源）。读不回来不是错误，按"不知道"走兜底规则。
- 也能读：显示器肯回答 VCP 0xF3 能力串时，能读回它**支持哪几个输入源**，设置页据此列出下拉选项（名字来自 MCCS 标准表）。读不回来同样不是错误，那一格退回手填编号。
- 不能：非罗技设备；Intel Mac；内置屏；0x60 只写或睡眠中的显示器读不出当前输入源。
- 权限：打开键鼠 HID 需「输入监控」（TCC，绑定 bundle id + 签名 Team）；DDC 与枚举不需要。

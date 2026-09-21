# Spec：Relay 核心设计（Tauri 2 + Rust）

> 版本 v0.3.2　2026-09-18（v0.3.1：§4 presence 触发改为 IOHIDManager 回调；§6 明确 `hosts[].index` 即 HID++ 槽号。v0.3.2：§2.1 openlogi 改为 crates.io 依赖；§6 设备加可选 `serial`）　取代更早的 Swift 方案（v0.2）
> 工程名 `Relay`　Bundle ID `work.bam.relay`　CLI `relay`

## 0. 决策记录

| # | 决策 | 理由 |
|---|---|---|
| D1 | 范围 = 支持 HID++ `ChangeHost (0x1814)` 的罗技设备 + 支持 DDC 的显示器；BLE 直连与 Bolt/Unifying 都覆盖 | 用户确认 |
| D2 | 零硬编码设备信息，运行时发现 + 用户选择 | 设备会更新 |
| D3 | 主机索引内部 0 基，UI +1 | 与 HID++ 一致 |
| D4 | 默认只在"触发设备离开本机"时动作 | 避免两机互相打架 |
| D4a | P3 起补一条对称规则：触发设备**到达**本机时也动作（`options.pull_on_arrival`，默认开）。P5 起「画面在不在本机」改为**直接问显示器**（VCP 0x60）：在别处就取回、在本机就不动；只有读不出来时才退回 P3 的 `last_target` 规则 | 对面 Relay 没在跑 / 从未见到键盘离开时，只有到达这一侧还有机会。`last_target` 是猜：刚启动的机器猜不出来（笔记本因此干等），正常往返又会白切一次；显示器答得又快又准（约 60ms），问一句就都对了 |
| D5 | 配置以主机列表建模（非本机/对端二元） | 3 主机零改动 |
| D6 | 核心逻辑在独立 crate `relay-core`，无 Tauri 依赖 | 可单测、可被 CLI 复用 |
| D7 | 自签名自用，不上 App Store | 私有 API + 输入监控 |
| **D8** | **技术栈 Tauri 2 + Rust，设备层复用 OpenLogi 的 `openlogi-hid` / `openlogi-hidpp`** | HID++、枚举、权限探测、热插拔、接收器均已实现且经 host-switch-tool 验证；省掉 M2 重写 |
| D9 | 运行形态参考 AskHuman：默认静默常驻，设置窗口按需呼出，单二进制多角色 | 用户确认 |
| D10 | 一台设备只能是「触发」或「跟随」之一（2026-09-20 用户拍板，不加开关） | 「按鼠标也能带走键盘」的双向用法需要设备同时担任两角，但键盘能否被遥控切换（HID++ ChangeHost）从未验证过——日志里键盘永远是 `already at target`；前提不成立时该选项没有价值 |

## 1. 范围

| 维度 | 覆盖 | 不覆盖 |
|---|---|---|
| 输入设备 | 支持 HID++ 2.0 `ChangeHost` 的罗技设备 | 非罗技 |
| 连接 | BLE 直连、Bolt、Unifying | 有线 |
| 显示器 | DDC/CI 可写 VCP 0x60 的外接屏 | 内置屏；不支持 DDC 者手动值兜底 |
| 主机 | Apple Silicon | Intel |
| 主机数 | ≥2，配置支持 3 | — |

## 2. 设备层（复用 OpenLogi）

### 2.1 引入方式
- `openlogi-hid` / `openlogi-hidpp` 已发布在 crates.io（0.8.5），relay-core 直接以 **钉死版本的 crates.io 依赖**（`= "0.8.5"`）引入，不再用 submodule（v0.3.2 改）。不修改上游；需要改动时在 relay-core 内包一层。
- 上游 `rust-version = 1.98`、edition 2024，本机 rustup 需满足。
- 已验证用法即 git 历史里的 M0 版本 `mac2-deploy/host-switch-tool`（目录已在 M1 删除）的 59 行 `main.rs`：`openlogi_hid::host::backend().enumerate()` → `open_hidpp(node)` → `hidpp::device::Device::new(channel, 0xFF)` → `root().get_feature(ChangeHostFeature::ID)` → `add_feature::<ChangeHostFeature>` → `get_host_info()` / `set_current_host(n)`。

### 2.2 relay-core 封装
```rust
pub trait HostSwitchable: Send + Sync {
    fn id(&self) -> &DeviceId;                     // 配置里的 `vid:pid`
    fn name(&self) -> &str;                        // 仅作显示
    async fn host_info(&self) -> Result<HostInfo>; // count, current
    async fn switch_to_host(&self, index: u8) -> Result<()>;
}
```
- `LogitechHidpp` 实现：每次调用都 **enumerate → open → act → drop**（不变量 3）。
- 发现：对所有 `openlogi-hid` 枚举到的节点探测 `get_feature(0x1814)`，非 0 即入列表；同时读 `DeviceTypeAndName (0x0005)` 做显示名、`DeviceInformation (0x0003)` 做持久身份。
- 接收器：`openlogi-hid` 的 transport 已区分 `0xFF43`（BLE 长报告）与 `0xFF00`（USB/接收器），`hidpp::receiver` 处理 device index。relay-core 不重复实现，M3 只做验证与 UI。
- `set_current_host` 发出后设备立即断开，超时视为成功，随后以 hotplug 移除事件确认。
- 权限：`openlogi_hid::permissions` 已封装 TCC 输入监控探测；relay 启动时检查，未授权则 tray 提示并打开设置页引导。

## 3. 显示器层（DDC）

- `display/ioav_ffi.rs`（`pub(crate)`）：只声明用到的三个 IOKit 私有符号 `IOAVServiceCreateWithService`、`IOAVServiceWriteI2C`、`IOAVServiceReadI2C`；`write_i2c` / `read_i2c` 为 `unsafe`（service 必须来自 `create_with_service`）。
- `display/ddc.rs`：用 `objc2-io-kit` 递归遍历 IOService plane；显示器 = `IOObjectConformsTo("IOMobileFramebuffer")` 的节点，身份 = 其子树里的 `EDID UUID`，显示名 = `DisplayAttributes.ProductAttributes.ProductName`（缺省 `Unknown Display`）；它的 DDC 通道 = 遍历顺序上紧随其后、下一个 framebuffer 之前的第一个 `DCPAVServiceProxy`（`Location == External`）；父节点 `EPICProviderClass == AppleDCPMCDP29XX` 时 chip 地址 0xB7，否则 0x37。不用 CoreGraphics / CoreDisplay。
- 配置 `edid_uuid` 留空 = 第一块有外接通道的显示器；非空须相符（忽略大小写），否则该步 `display not present`；同一 `edid_uuid` 不得配置两次。
- 写入：`[0x84, 0x03, 0x60, 0x00, value, checksum]`（checksum = `0x6E ^ 0x51 ^ 前五字节`）→ chip 0x37 / data 0x51，连写 2 次、每次前等 10ms（与 m1ddc 一致）；整个 `set_input` 在 `spawn_blocking` 里跑，外套 8s 超时；executor 重试 `1 + ddc_retries` 次，退避 0.5/1/2s。句柄现查现建、写完即释放。
- 能力读取（VCP 0xF3 Capabilities → 支持的输入源集合与名字映射）推到 M4；现在手填输入源码 + "试切"。
- 读取（P5）：VCP 0x60 **实测可读**（2026-09-20 AOC U2790R3B 连问两次都答 17，与 m1ddc 一致），原先「0x60 常见只写不读」的说法作废。请求包 `[0x82, 0x01, 0x60, chk]`（`chk = 0x6E ^ 0x82 ^ 0x01 ^ 0x60`，**不**含 0x51，与写包算法不同）走同一条写路径发两次，再等一会儿 `IOAVServiceReadI2C(chip, 0x51, buf, 12)`；应答须 `buf[2] == 0x02 && buf[3] == 0x00 && buf[4] == 0x60`，当前值 = `u16::from_be_bytes([buf[8], buf[9]])`。等待时间只有**读前**这一次按桥接芯片区分：MCDP29xx（chip 0xB7）等 50ms（10ms 会读回空），其余 10ms；写的节奏一律仍是 10ms。
- 读**最多问两次**（P5，2026-09-20 实测）：显示器刚把画面切到本机的那一两秒里，这台机器的 DDC 通道还没准备好——显示器根本枚举不出来，读在切换后约 1.5s 直接失败（同一窗口里写入侧也会先报一次 `display not present`、约 0.6s 后重试才成功），约 2s 后就能读到了。所以第一次读不出来时等 400ms 再整套重来一遍（重新遍历 registry → 开通道 → 问，因为失败的是「找不到显示器」而不是应答不合格式），两次都失败才算「不知道」。健康时仍是一次约 60ms，重试时整体约 540ms，仍在 §5 的决策预算之内。
- 读不出来就是「不知道」：任何一步失败、应答不合格式、值 > 255 → `None` + 一行 `debug!`，**绝不**当成错误、也绝不因此改变切换行为（见 §5 的 `None` 分支）。读与写共用同一套 RAII，返回前释放 `IOAVService` 与所有 `io_object_t`，不缓存；整个读在 `spawn_blocking` 里跑，外套 8s 超时。
- core 只维护 `believed_input` 供 UI；显示器当前输入源只在需要判断时现问现用（§5），不落盘、不做状态。
- 移植来源：m1ddc（MIT）的 `i2c.m` / `ioregistry.m`；`examples/ddc.rs` 可在无权限的 shell 里验证（`list` / `set` / `get`）。

```rust
pub trait DisplayInput: Send + Sync {
    fn identity(&self) -> &DisplayIdentity;
    async fn supported_inputs(&self) -> Vec<InputSource>;
    async fn set_input(&self, code: u8) -> Result<()>;
    /// 显示器现在显示的输入源；读不出来（或后端根本不问）时 None。
    async fn current_input(&self) -> Option<u8> { None }
}
```

## 4. 触发层

```rust
pub enum TriggerEvent { DeviceLeft(DeviceIdentity), DeviceArrived(DeviceIdentity), Manual { target: u8, source: &'static str } }
pub trait TriggerSource { fn stream(&self) -> BoxStream<TriggerEvent>; }
```

| 触发源 | 实现 | 权限 |
|---|---|---|
| 设备离开/到达 | `IOHIDManager` 设备 matching/removal 回调（objc2-io-kit），按配置中的 vid/pid 匹配，`PresenceTracker` 把一台设备的多个 HID 节点折叠为一次 `DeviceLeft/DeviceArrived`。（v0.3.1 改：OpenLogi 的 `HotplugEvent` 只有无负载的 Connected/Disconnected，无法按设备过滤） | 无 |
| 学习模式 | 设置页"点击后按一次 Easy-Switch"，记录 3s 内离开的设备 | 无 |
| 全局热键 | `tauri-plugin-global-shortcut`，默认 ⌃⌥1 / ⌃⌥2 | 无 |
| CLI / IPC | `relay switch <host>`、`relay switch next` | 无 |
| tray 菜单 | 各主机一项 | 无 |

## 5. 编排：状态机

```
Idle ─DeviceLeft(trigger)─► Confirming(debounce) ─未回连─► Switching ─► Cooldown ─► Idle
  ▲                              │ DeviceArrived → 取消                        │
  └──────────────────────────────┘◄── Switching / Cooldown 期间的事件不动作，但记录「待返回」──┘
                            Cooldown ─到期且有「待返回」─► Switching（返回本机）─► Cooldown ─► Idle

Idle ─DeviceArrived(trigger)─► Confirming(debounce) ─未再离开、且到期时「画面不在本机」─► Switching（取回本机）─► Cooldown ─► Idle
                                          │ DeviceLeft → 改走离开序列  └─ 到期时「画面已在本机」→ 回 Idle，不动作
```
- debounce 默认 800ms，cooldown 默认 5s，均可配。`Clock` trait 注入，测试用手动时钟。
- Switching：① 每个显示器 `set_input(input_by_host[target])`；② 每个 `follow=true` 设备：`host_info()`，已在目标跳过，否则 `switch_to_host(target)`。
- `Switching` / `Cooldown` + `DeviceArrived(is_trigger)`：若本次切换的目标不是本机（确实切走了），记住「待返回」，并记住是哪一台设备记下的；只有同一台设备在窗口内再次离开才清除（别的设备——第二台触发设备，或被本次切换带走的 `follow` 设备——在窗口内报 `DeviceLeft` 时，不能抹掉键盘刚记下的「待返回」）。事件本身仍不打断当前切换 / 不缩短 cooldown。
- `Cooldown` 到期（`now >= until`）且有「待返回」且 `options.switch_back_on_reconnect` → 发起 `StartSwitch { target: this_host, manual: false }`，重新进入 `Switching` → `Cooldown`；否则照常回 `Idle`。
- `Idle` + `DeviceArrived(is_trigger)`（P3，`options.pull_on_arrival`，默认开）：**离开规则的镜像**。到达本身只进 `Confirming`（`target = this_host`）：除了「不是触发设备」和「选项关着」这两条拦在防抖之前，到达一律先防抖，此刻不问画面在哪儿。**「要不要取回」留到防抖到期那一刻再判**，分三支，依据是**画面在不在本机**（P5）：
  - **在别处** → 发起 `StartSwitch { target: this_host, manual: false }`；**`last_target` 是 `None` 也照样取回**（刚启动的机器就属于这种）。
  - **在本机** → 回 `Idle`，不动作，理由 `the screen is already here`；即使 `last_target` 指向别的主机也不动（正常往返里对面已经把画面送回来了，省掉一次多余切换和一段 cooldown）。
  - **读不出来 / 没读** → 退回 P3 的 `last_target` 兜底规则，一字不改：`Coordinator` 用 `last_target: Option<HostIndex>` 记住本机最后一次**完成**的切换把东西送去了哪儿（内存里，不落盘）；`last_target == Some(t) && t != this_host` 才取回，为 `None`（刚启动 / 配置热重载后重建）或已经是本机时不动作（理由分别是 `no switch to pull back from` 和 `the screen is already here`）。
- 事实由 runtime 提供，`Coordinator` 仍 sans-IO：runtime 只在「取回方向的 `Confirming` 防抖到期、把 `TimerFired` 交给 `Coordinator` 之前」这一条路径上问一次显示器（健康时约 60ms，读不出来时会再问一次、整体约 540ms；决策只等 1.5s，超时按「读不出来」处理——这次读是在事件循环里等的，不能让一台睡着的显示器把切换回报、配置重载和 `status()` 一起拖住；HID 事件另有一条更快的路，见下「离开压过取回」），调 `Coordinator::observe_screen(Option<bool>)` 记下，然后照常 `handle(Event::TimerFired, ...)`。**到达那一刻不问**：那时距对面写入输入源才约 1 秒，显示器 / DCP 还在重新同步，读到的要么是旧输入源、要么干脆读不出来（写入侧在同一窗口里也会先报一次 `display not present`、约 0.6s 后重试才成功）；防抖到期时它已经稳定，答案也更新。离开、冷却到期、切换完成、跟随设备热插拔都不问，免得每次插拔都打扰显示器。观测**用一次即失效**（`pull_verdict` 取走），下次防抖到期重新读。
- 这一问写进日志（`info` 级：读回值 + 结论），事后翻 `~/Library/Logs/Relay/relay.log.*` 能看出某次到达是「画面已在本机所以没取回」，还是「显示器说在别处 / 没答上来所以取回了」。
- 「画面在本机」的定义：配置里**第一台**显示器的 `input_by_host[this_host]` 与读回的值相等。多显示器的判定留到以后。
- **防抖两个方向对称，但不是镜像**：`Confirming` 记住方向。离开方向由「同一设备到达」取消（它其实没走，回 `Idle`，不动作）。取回方向遇到「同一设备离开」时**不是取消**：设备真的走了（键盘刚落到本机，用户又按了一次 Easy-Switch 直接去了对面），所以画面和跟随设备得跟过去——按 `Coordinator::start_leave` 原样重新起一段**离开方向**的防抖，和 `Idle` 收到 `DeviceLeft` 完全一致；只有当这台设备没有离开目标（三主机且没填 `leave_to`）时才退化为取消、回 `Idle`。同一设备重复上报同方向的事件不取消也不重置，别的设备一律不影响。两个方向共用 `timing.debounce_ms` 与同一套 `Switching` → `Cooldown`。
- 这条「离开压过取回」的优先级一直管到**正在问显示器的那一刻**：读显示器是在事件循环里等的，runtime 因此把这次读与 HID 事件通道**赛跑**，任何设备事件一到就当场丢掉这次读、先处理事件（读不出来正好退回兜底规则）。否则 `DeviceLeft` 会排在读后面，等轮到它时 `TimerFired` 已经把状态机推进了 `Switching（取回本机）`，而 `Switching` 里的离开只会清「待返回」——本机就会把画面抢过来并留住，而键盘在对面。
- 两条到达规则不重叠：P1 的「待返回」只在本机自己刚切走的 `Switching` / `Cooldown` 窗口里生效，P3/P5 的取回只在 `Idle` 里生效；显示器的观测同样只影响 `Idle` 这一条，`Switching` / `Cooldown` 里的到达照旧只走「待返回」。
- cooldown 期间的手动切换（热键 / 托盘 / CLI）同样忽略。返回切换自己又带一段 cooldown，所以一次回连会把「按下 Easy-Switch 也不响应」的窗口拉长到两段 cooldown（默认共约 10s），期间用户有意的切换会被丢弃。
- 执行器：**自动**计划（`manual == false`）里设备层返回「设备不在本机」时该步记为跳过（`ok = true`，detail `not on this Mac, skipped`）——对方主机已把它搬走是正常情况；手动计划（`relay switch` / 托盘 / 试切）仍是失败 `device not present`。设备层分不出「被对方主机搬走」和「关机 / 睡眠了」，两者一样算作跳过，只有日志里那一步的 detail 能看出来。
- 目标推断：触发设备离开 → 目标 = `hosts` 中除本机外唯一者；多于一个取触发设备上的 `leave_to`（3 主机时向导要求填写）。
- Manual：无 debounce；键盘与鼠标都发 `switch_to_host`；仍走 cooldown。
- 每步写结构化日志：`tracing` → `~/Library/Logs/Relay/relay.log`（按天）+ 内存环形缓冲 200 条供设置页 Log 视图。

## 6. 配置

`~/Library/Application Support/Relay/config.json`；`notify` 监听、去抖 300ms、原子写。

```json
{
  "schema_version": 1,
  "this_host": 0,
  "hosts": [ { "index": 0, "name": "Bam.Work" }, { "index": 1, "name": "Bam.Mini" } ],
  "displays": [
    { "edid_uuid": "B9B87925-…", "name": "AOC U2790R3B", "input_by_host": { "0": 17, "1": 18 } }
  ],
  "devices": [
    { "id": "046d:b366:…", "name": "MX Mechanical", "role": "keyboard", "transport": "ble",
      "is_trigger": true,  "follow": false, "leave_to": null },
    { "id": "046d:b023:…", "name": "MX Master 3",   "role": "mouse",    "transport": "ble",
      "is_trigger": false, "follow": true }
  ],
  "timing":  { "debounce_ms": 800, "cooldown_ms": 5000, "ddc_retries": 3 },
  "hotkeys": { "0": "Ctrl+Alt+1", "1": "Ctrl+Alt+2" },
  "options": { "switch_back_on_reconnect": true, "pull_on_arrival": true, "launch_at_login": true,
               "language": "auto" }
}
```
- `hosts[].index` 就是设备上的 HID++ 槽号（0 基），由用户在配置/向导中声明；未配对的槽不得声明（当前两台机器：Mac1 = 槽 1、Mac2 = 槽 2，槽 0 空）。
- 设备身份：`id` = `vid:pid`；`serial` 可选（M1 起由 HID++ 0x0003 读出并在扫描时写入），匹配时 serial 优先、否则 vid:pid；`name` 仅作显示。
- 加载时校验：`this_host ∈ hosts`；每个 display 对每个 host 都有 input，且同一台 display 上两个已声明主机不得共用同一个输入源（取回判断要把读回的输入源反查成主机，共用就分不出画面在谁那儿）；`follow`/`is_trigger` 设备至少各一个；同一设备不得同时 `is_trigger` 与 `follow`（两种角色互斥）；`devices[].id` 不得重复（plan/executor 只按 `id` 取设备）；`displays[].name` 不得重复（plan/executor 只按 `name` 取显示器）（否则 tray 显示"未配置"，Coordinator 不启动）。
- `options.switch_back_on_reconnect`（默认 `true`，缺省字段按 `true` 解析，旧配置显式 `false` 仍尊重）：**只在本机刚切走之后的窗口内生效**——本机自己发起的切换（`Switching`）及其后的 `Cooldown` 期间，某个 `is_trigger` 设备又回到本机时记住「待返回」，在 **cooldown 到期那一刻**发起一次回到 `this_host` 的自动切换（显示器 + `follow` 设备；不在本机的设备按下条跳过）。窗口内该触发设备再次离开 → 清除待返回。切换目标就是本机（没有切走）时不记。`Idle` / `Confirming` 里的到达行为不变。托盘 / CLI / 热键的手动切换若切走后同样弹回，也按此规则拉回。
- `options.language`（`auto` | `zh-Hans` | `en`，默认 `auto`）：界面与托盘菜单的语言。`auto` 由 relay-core 按系统首选语言解析成具体值放进 `Status.language`，窗口和托盘都用它，保证两边一致。CLI 与 `ConfigError` 仍为英文。
- `options.pull_on_arrival`（默认 `true`，缺省字段按 `true` 解析，显式 `false` 仍尊重）：**只在 `Idle` 里生效**——某个 `is_trigger` 设备到达本机时先按 `timing.debounce_ms` 防抖，**防抖到期那一刻**由 runtime 读一次第一台显示器当前显示的输入源（VCP 0x60，健康时约 60ms，读不出来时会再问一次、整体约 540ms；到达那一刻不读，显示器那时还在重新同步），按读到的结果分三支（判定细节见 §5）：
  - **画面在别处** → 发起一次回到 `this_host` 的自动切换（显示器 + `follow` 设备）。**`last_target` 为 `None` 也照样取回**：画面在哪儿是问出来的，刚启动 / 配置热重载后重建的 `Coordinator` 不必再猜。
  - **画面已在本机** → 不动作（理由 `the screen is already here`），即使 `last_target` 指向别的主机。正常往返里对面已经把画面送回来了，这一支省掉那次多余的同向 DDC 写和随之而来的一段 5 秒冷却。
  - **读不出来（两次都失败）/ 读超过 1.5s 的决策预算** → 退回兜底规则：`last_target == Some(t) && t != this_host` 才取回；`last_target` 为 `None` 时不取回，因为刚开机的机器画面本来就在自己这儿，贸然取回只会白切一次并进入 5 秒冷却，把用户紧接着按的 Easy-Switch 吞掉（2026-09-20 重启故障现场 14:28:05 → 14:28:10 正是这个时序）。`last_target` 不落盘：上一次运行留下的答案可能早就过期了。
  - 对面的 Relay 没在跑、或它从未见过键盘离开（重启后键盘迟迟没连上系统就是这种情况）时，这是唯一能把画面收回来的路径。本项关掉后，到达路径完全不读显示器。
- 导出/导入：另一台导入后只改 `this_host`。
- 向导（首次运行或 `relay --wizard`）：命名主机 → 选本机 → 扫描显示器并为每主机选输入源 → 扫描 HID++ 设备勾选触发/跟随 → 申请输入监控 → 试切。

## 7. 进程与 IPC

- 同一二进制：无参数 → 常驻 app（tray + core）；有子命令 → CLI。
- IPC：`~/Library/Application Support/Relay/relay.sock`，NDJSON；消息 `hello / switch{target} / status / open_settings / open_wizard / reload_config`；单实例由 `flock(relay.lock)` 保证。
- CLI 找不到常驻实例：`relay switch` 自启常驻实例后重试一次；`--no-spawn` 则退 3。
- 设置窗口：Tauri window，URL 路由 `settings / wizard / log`；关闭即销毁；通过 `commands.rs` 读写配置与触发"试切"（试切也是 `TriggerEvent::Manual`）。

## 8. 权限、签名、安装

| 项 | 做法 |
|---|---|
| 输入监控 | 启动检测（openlogi-hid permissions）；未授权 → tray 徽标 + 设置页引导。授权绑定 bundle id + 签名 Team：Tauri `bundle.macOS.signingIdentity` 用 Personal Team 证书，**不用 ad-hoc** |
| 登录启动 | `tauri-plugin-autostart`（macOS 走 LaunchAgent）或 `SMAppService` FFI；M0 先用 plugin |
| Gatekeeper | 不公证；另一台首次右键"打开" |
| tray | Tauri tray；注意 AskHuman `docs/investigations/tray-menu-close-on-first-hover.md` 的坑 |
| 私有 API | `ioav_ffi.rs` 单文件隔离 |

## 9. 测试

| 层 | 方式 |
|---|---|
| Capabilities 解析 | 单测，用 AOC 实机字符串 + 常见变体 |
| 状态机 | 单测 + 手动时钟 + Fake `HostSwitchable` / `DisplayInput`：debounce 取消、cooldown 忽略、已在目标跳过、部分失败重试 |
| IPC | 临时 socket 集成测试：switch/status 往返、退出码 |
| 硬件 | 手动矩阵：0→1、1→0 各 ≥5 次；键盘快速来回；显示器断电重连；app 重启后授权仍在；接收器模式（M3）。结果逐次记录在案 |

## 10. 里程碑

| 阶段 | 内容 | 验收 |
|---|---|---|
| **M0 壳** | Tauri 工程 + relay-core 骨架 + tray + 状态机 + 配置 + IPC/CLI；显示器与设备用 `legacy_process` 调内嵌 `m1ddc` / `host-switch-tool` | 登录自启后 0→1、1→0 各 5 次成功；输入监控卡点解决 |
| **M1 设备原生** | `LogitechHidpp` 基于 openlogi crates；设备发现 + 扫描 UI（学习模式挪到 M4） | 去掉 host-switch-tool；换任一罗技 BLE 设备无需改代码 |
| **M2 DDC 原生** ✅ | ioav_ffi + ddc（发现 + 写入）+ 扫描显示器 UI + `relay --displays`（capabilities 挪到 M4） | 去掉 m1ddc（2026-09-20 两台实机验证，显示器步骤 51–75ms：往返 51–62ms，手动 switch 74ms） |
| **M3 接收器验证** | Bolt/Unifying 实机验证与 UI | 接收器设备可发现、可切 |
| **M4 打磨** | 向导、导入导出、Log 视图、热键设置、学习模式 UI、DDC Capabilities、3 主机验证 | 另一台从零安装 ≤ 5 分钟 |

（相对 v0.2 把设备原生提前到 M1：因为复用 OpenLogi 后它比 DDC 移植更便宜。）

## 11. 风险

| 风险 | 对策 |
|---|---|
| OpenLogi crate API 变动 | crates.io 版本钉死（`= "0.8.5"`）；relay-core 内只用 `backend/enumerate/open_hidpp/Device/ChangeHostFeature/watch_hotplug/permissions` 几个入口 |
| OpenLogi 要求 Rust 1.98 / edition 2024 | rustup 升到 stable |
| `IOAVService` 私有 API 变动 | `ioav_ffi.rs` 单文件隔离；回退 = git 历史里的 M1 版本（内嵌 m1ddc） |
| Tauri tray 在 macOS 的怪癖 | 照 AskHuman investigations 处理；必要时 objc2 直接建 NSStatusItem |
| 重连抖动 / 两机打架 / 未配对槽 | 状态机 + D4 + 不变量 4 |
| TCC 授权丢失 | 稳定签名；启动检测 |
| Logi Options+ 抢设备 | 用完即关；文档提示 |

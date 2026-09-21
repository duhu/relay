# Relay

**多台 Mac 共用一台显示器和一套罗技键鼠时的软 KVM。**

[English](README.md)

你在键盘上按 **Easy-Switch 2**。键盘自己跳到另一台 Mac 上——Relay 把剩下的都跟着送过去：
显示器经 DDC/CI 换输入源，鼠标经 HID++ 换主机。大约一秒后，画面、键盘、鼠标全在那一台上。

桌上不用多一个盒子，不用拔线，也不用起服务。每台 Mac 各跑一份 Relay，各自指挥它本来
就连着的硬件。

Relay 常驻菜单栏，不打开设置就不显示任何窗口。

---

## 它是怎么做到的

每台 Mac 只盯着自己看得见的 HID 设备。**键盘从哪台离开，就由哪台动手**：

```
Mac A                                           Mac B
  │
  │ 键盘从本机消失
  ▼
  防抖 800ms  ─ 还没回来？那就走 ─┐
                                  │
  ├── DDC/CI   VCP 0x60 := 18  ───┼──▶  画面到了 Mac B
  └── HID++    ChangeHost := 2 ───┘     鼠标也到了 Mac B
                                          │
                                          │ 键盘到达本机
                                          ▼
                                  问一句显示器：画面现在在哪
                                （读 VCP 0x60）——不在本机，
                                  就把画面和鼠标取回来
```

两条规则让多台机器同时跑 Relay 也不会打架：

- **只有送走的那一侧推。** 一台 Mac 只对「自己的键盘离开了」做反应，绝不去猜别人的状态。
- **到达的那一侧问显示器。** 键盘一到，那台先读显示器当前的输入源再决定动不动。画面已经
  在本机就什么都不做。正因为有这一条，对面睡着了、或者对面压根没看见键盘离开，切换照样成立。

内部是一个状态机（`Idle → Confirming → Switching → Cooldown`），所有动作串行，并且强制冷却。
冷却不是装饰：连续密集地发 `ChangeHost`，实测能把蓝牙栈搞到要重启才恢复。

## 硬件前提

| | |
|---|---|
| **Mac** | Apple Silicon，macOS 14 及以上。不支持 Intel。 |
| **显示器** | 外接屏，且 DDC/CI 能写 VCP `0x60`（输入源）。能读回来更好，但不是必须。内置屏切不了。 |
| **键盘 / 鼠标** | 实现了 HID++ 2.0 `ChangeHost`（`0x1814`）的罗技设备，也就是带 Easy-Switch 键的那些。蓝牙直连、Bolt、Unifying 都行。 |
| **权限** | 输入监控——Relay 靠它看见键盘来去。在系统设置里授权一次即可。 |

设备上的 Easy-Switch 通道号和显示器上的输入源，就是你在设置里要对应起来的两样东西。代码里
没有任何硬编码的设备信息，也不靠品牌名去认。

## 安装

暂时没有签好名的发布版——Relay 用到私有 IOKit 符号和输入监控权限，注定上不了 App Store。
自己编：

```bash
git clone git@github.com:duhu/relay.git
cd relay
pnpm install
pnpm tauri build
```

然后装进去并签名。**用真实证书签很重要**：ad-hoc 签名也能跑，但 macOS 把输入监控的授权
绑在签名上，ad-hoc 的话每次重新编译都要重新授权一遍。

```bash
cp -R target/release/bundle/macos/Relay.app /Applications/
codesign --force --sign "Apple Development: 你的邮箱" --identifier work.bam.relay /Applications/Relay.app
ln -sf /Applications/Relay.app/Contents/MacOS/relay ~/.local/bin/relay
open -a /Applications/Relay.app
```

编译需要：Rust 1.98+、pnpm、Xcode 命令行工具。

## 第一次配置

从菜单栏图标打开**设置**。四个标签页，按你需要的顺序排好了：

1. **机器与屏幕**——给每台 Mac 起名字、填通道号。通道号就是键盘上的 Easy-Switch 键：按 2
   能到的那台就是通道 2。标出哪一行是**本机**。然后添加显示器（**扫描显示器**会自动填好
   名字和 EDID），再从显示器报上来的清单里给每台机器挑输入源——「HDMI 1」「DisplayPort 2」，
   都是名字。显示器现在正显示的那个排在最上面、归在「现在显示的」底下，你坐在哪台机器前面
   就能认出哪个口是它的；EDID 那一行的**读取本机当前输入源**直接把本机这一格填好。碰上不肯
   报清单的显示器，那一格还是原来的数字框，手填编号就行。清单最后那个「自定义…」是留给报得
   不全的显示器的，按 ↩ 就回到清单。
2. **键盘鼠标**——先授权输入监控，然后**扫描设备**。把键盘标成**触发**（它离开本机时启动
   切换），鼠标标成**跟随**（切换时被送过去）。一台设备只能是其中之一。
3. **高级**——防抖、冷却、重试次数，以及三个行为开关。默认值都是实机用下来留下的那套。
4. **概览**——画面、键盘、鼠标现在各自在哪，每台机器一个手动切换按钮，以及上一次切换每一
   步的结果。

每台 Mac 都配一遍。机器之间唯一的差别就是「本机」标在哪一行。配置文件在
`~/Library/Application Support/Relay/config.json`，改动后自动热加载。

界面有中英两种语言，默认跟随系统，也可以在「高级」里指定。

## 命令行

同一个二进制就是 CLI，经 unix socket 跟常驻实例通信；实例没起会自动拉起来。

```
relay switch <host|next>    切到某个主机槽，或切到本机的下一台
relay status                状态、配置、权限、上次结果
relay --settings            打开设置窗口
relay --devices             本机能看到的罗技 HID 设备
relay --displays            本机能驱动的外接显示器
```

退出码：`0` 成功，`1` 失败，`2` 用法错误，`3` 常驻实例不在。

## 它做不到的事

- **非罗技的键鼠**，以及有线设备。整套机制就建立在 `ChangeHost` 上。
- **Intel Mac 和内置屏。**
- **只肯写不肯读 `0x60` 的显示器。** 照样能切，只是判断画面在哪时退回「记住上次送去了哪」，
  不再是问出来的。
- **谁也没看见鼠标离开时，把它叫回来。** 三台以上的时候，拿着鼠标的那台并不知道键盘去了
  另外哪一台，鼠标就可能落在原地——做一次正常往返就能带回来。真正的解法是让几台机器互相
  通气，还没做。
- **同步配置。** 现在每台机器都是手工配的。

## 设计文档

- [`docs/overview.md`](docs/overview.md)——模块地图、进程职责，以及必须守住的跨模块不变量。
- [`docs/specs/relay-core.md`](docs/specs/relay-core.md)——设计记录：每个决策连同它的理由、
  配置格式、状态机，以及 DDC 和 HID++ 的协议细节。

简单说：`crates/relay-core` 是不依赖 Tauri 的纯 Rust，状态机是 sans-IO 的，所以几乎全部逻辑
都能脱离硬件单测；`src-tauri` 是 app 外壳、托盘和 IPC；`src` 是一个不用组件库的小 Vue 3 前端。

```bash
cargo test --workspace && cargo clippy --workspace --all-targets && pnpm build
```

## 致谢

- **[m1ddc](https://github.com/waydabber/m1ddc)**（MIT）——Apple Silicon 上用 `IOAVService`
  走 DDC 的办法。Relay 用 Rust 重新实现了一遍，没有内嵌它的二进制。
- **[`openlogi-hid`](https://crates.io/crates/openlogi-hid)** 与
  **[`openlogi-hidpp`](https://crates.io/crates/openlogi-hidpp)**——HID 枚举、传输，以及
  包含 `ChangeHost` 在内的 HID++ 2.0 协议。

## 许可证

MIT，见 [LICENSE](LICENSE)。

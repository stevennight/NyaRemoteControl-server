# NyaRemoteControl 被控端（nya-server）

装在**被远程控制的电脑**上。负责截屏、硬件编码（NVIDIA NVENC / Intel QSV，没有独显时用软件编码）、系统声音、键鼠注入和剪贴板同步。

网络穿透不在本程序范围内。请用 Tailscale / EasyTier / ZeroTier 等工具把两台电脑组到同一个网络，客户端直接连被控端在该网络里的 IP。

## 两个程序

| 文件 | 作用 |
|---|---|
| `nya-server.exe` | **管理程序**：图形界面和命令行。双击打开的是它。 |
| `nya-server-svc.exe` + FFmpeg DLL | **被控端本体**：Windows 服务、采集进程、开发模式、诊断。不用直接运行它。 |

管理程序通过本机的控制管道（`\\.\pipe\NyaRemoteControl.control`）和运行中的服务通信：查看状态、改设置、管理配对都是**立即生效**的，不需要重启服务。服务没运行时，管理程序直接读写配置文件，服务下次启动时生效。

两者互不依赖对方的文件：管理界面开着时也可以停掉服务、替换 `nya-server-svc.exe` 和 DLL，再启动服务（为以后的自动更新做准备）。只有管理员能通过控制管道查看配对码、改设置；普通用户只能看到服务状态和当前是否有人连接。

## 图形界面

双击 `nya-server.exe` 打开管理界面（会请求管理员权限；需要 Windows 自带的 WebView2 运行库）：

- **概览**：服务状态与安装 / 启动 / 停止 / 重启；配对码（可复制、重新生成）；当前连接（客户端、地址、画面参数，可断开）；证书指纹；最近事件
- **已配对客户端**：查看、移除（正在连接的会被断开）
- **设置**：端口、监听地址、名称、编码器、码率、帧率、声音、日志级别（服务运行中保存即生效）；重新安装 / 卸载服务
- **可选组件**：虚拟显示器、虚拟声卡、手柄、USB 透传，一键安装
- **诊断**：运行诊断并查看 / 复制结果
- **日志**：服务、采集进程、管理界面、虚拟显示器测试的日志，打开日志目录

下面的命令行用法仍然可用。

## 快速开始（命令行：安装为服务）

以**管理员身份**打开终端，进入程序所在目录：

```powershell
.\nya-server.exe install
```

- 服务名 `NyaRemoteControl`，开机自启，运行的是同一目录下的 `nya-server-svc.exe`；移动程序目录前先卸载。
- 从旧版本（只有一个 `nya-server.exe`）升级：先停止服务，替换全部文件，再执行一次 `install`（或在界面里点“重新安装服务”），配对信息会保留。
- 会自动完成：防火墙放行 UDP 47100；开启 `SoftwareSASGeneration` 策略（让远程 Ctrl+Alt+Del 可用）；数据目录 `C:\ProgramData\NyaRemoteControl` 仅允许 SYSTEM / 管理员访问。
- 最后会打印**配对码**，客户端第一次连接时输入。

服务模式下可以操作锁屏、登录界面和 UAC 弹窗，注销、切换用户后连接也不会断。

## 开发模式

```powershell
.\nya-server-svc.exe standalone
```

以当前用户身份单进程运行，窗口中直接显示配对码。**开发模式无法操作锁屏、登录界面和 UAC 弹窗**，也不能远程发送 Ctrl+Alt+Del。下面的 `nya-server` 命令同样可以管理运行中的开发模式（控制管道 `NyaRemoteControl.control.standalone`）。

## 常用命令

这些命令优先作用于运行中的服务（其次是运行中的开发模式），都没运行时直接读写数据目录；加 `--data-dir` 则总是直接读写指定目录。

| 命令 | 说明 |
|---|---|
| `nya-server status` | 运行状态、监听地址、当前连接、最近事件 |
| `nya-server pair` | 查看配对码（服务模式需要管理员） |
| `nya-server pair --reset` | 重新生成配对码，立即生效，已配对的客户端不受影响 |
| `nya-server clients` | 列出已配对的客户端 |
| `nya-server clients --remove <指纹前缀>` | 移除某个客户端（正在连接的会被断开） |
| `nya-server config` | 查看设置 |
| `nya-server config --set port=47101 --set encoder=nvenc` | 修改设置，服务运行中立即生效 |
| `nya-server disconnect` | 断开当前的远程连接 |
| `nya-server diag` | **诊断**：显卡、显示器、各编码器能力和耗时、截屏测试、跨显卡传输耗时、音频。结果保存到 `nya-diag.txt` |
| `nya-server uninstall [--purge]` | 卸载服务；`--purge` 同时删除证书、配对信息和日志 |

## 配置 `server.toml`

服务模式在 `C:\ProgramData\NyaRemoteControl\server.toml`，开发模式在 `%LOCALAPPDATA%\NyaRemoteControl\server\server.toml`。首次运行时自动生成：

```toml
port = 47100
bind = "::"                 # 改成组网 IP（如 100.x.y.z）则只接受该网卡的连接
name = ""                   # 显示给客户端的名字，空 = 计算机名
encoder = "auto"            # auto | nvenc | qsv | amf | software
office_bitrate_kbps = 0     # 0 = 按分辨率自动
game_bitrate_kbps = 0
max_fps = 144
audio = true
log_level = "info"
```

建议用界面或 `nya-server config --set` 修改：服务运行中立即生效（画面相关设置会让采集进程重启一下；改端口或监听地址会断开当前连接，并自动更新防火墙规则；日志级别在服务重启后生效）。直接编辑文件则需要重启服务：`sc stop NyaRemoteControl`，然后 `sc start NyaRemoteControl`。

## 日志

保存在数据目录下的 `logs\`（`service.*.log`、`helper.*.log`、`gui.*.log`、`standalone.*.log`），保留 7 天。FFmpeg 自身的输出默认关闭（探测和降级时的报错是正常现象）；排查编码问题时可设置环境变量 `NYA_FFMPEG_LOG=warning`（或 `verbose`）后再运行。排查问题时请连同 `nya-diag.txt` 一起提供。

## 虚拟显示器、隐私屏和 HDR

- 客户端可以在被控端新建 1–4 个**虚拟显示器**（第一个设为主显示器，分辨率由客户端决定，可跟随客户端窗口），选择物理显示器保持显示（扩展屏）还是关闭（黑屏），以及是否屏蔽本机键盘鼠标（三项合起来就是“隐私屏”）。需要在管理界面“可选组件”里安装虚拟显示器，并以服务模式运行。
- 虚拟显示器平时处于停用状态，只在有客户端要求时启用；客户端断开 15 秒后停用，Windows 恢复原来的显示器布局。采集进程异常退出时，下次启动会自动恢复。
- 虚拟显示器驱动的设置文件 `C:\VirtualDisplayDriver\vdd_settings.xml` 由本程序在每次连接时重写（分辨率列表），手动修改会被覆盖。
- 显示器开启 HDR 时，按 HDR 格式截屏并在显卡上转换为 SDR（以 Windows 设置里的“SDR 内容亮度”为白色）后再编码，客户端看到的画面和 SDR 显示器上一样，不再发白过曝。`nya-server diag` 会列出哪些显示器开了 HDR。

## 编码器选择（多显卡 / 笔记本）

- 截屏必须在显示器所连接的显卡上完成；编码可以在任意显卡上完成。
- 默认优先使用同一块显卡编码（不需要拷贝）。如果办公模式需要 4:4:4 而这块显卡不支持（例如较老的核显），会自动改用 N 卡编码，画面通过内存在两块显卡之间传递。
- 如果某个编码器在运行中连续出错，本次运行会自动换用下一个方案。

## 构建

依赖同级目录的 `../common`（公共仓库）和 `../third_party/ffmpeg`（运行 `../common/scripts/fetch-ffmpeg.ps1` 下载）。

```powershell
cargo build --release      # 会自动用 npm 构建管理界面（../common/web），需要安装 Node.js
.\scripts\package.ps1      # 生成 dist\nya-server（两个 exe + 3 个 FFmpeg DLL）
```

管理界面是 `../common/web` 里的 Svelte 页面（manager.html），编译时嵌入 `nya-server.exe`。

本仓库是一个 Cargo workspace：

| 目录 | 包 | 产物 |
|---|---|---|
| `.` | `nya-server` | 被控端本体 `nya-server-svc.exe`（服务、采集进程、网络、编码） |
| `core/` | `nya-server-core` | 两个程序共用：配置、配对数据、路径、日志、安装、控制管道协议（`core/proto/control.proto`）和客户端 |
| `manager/` | `nya-server-manager` | 管理程序 `nya-server.exe`（界面 + 命令行） |

`manager` 只依赖 `core`，不能依赖 `nya-server`，否则会把采集 / 编码和 FFmpeg 一起链接进来。`control.proto` 是兼容性接口：升级过程中管理程序和服务可能版本不同，只能新增字段和请求。

## 仓库布局

本项目由三个仓库组成，需要克隆到同一个父目录下（server / client 通过 `../common` 引用公共库）：

```powershell
gh repo clone stevennight/NyaRemoteControl-common common
gh repo clone stevennight/NyaRemoteControl-server server
gh repo clone stevennight/NyaRemoteControl-client client
```

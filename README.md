# NyaRemoteControl 被控端（nya-server）

装在**被远程控制的电脑**上。负责截屏、硬件编码（NVIDIA NVENC / Intel QSV，没有独显时用软件编码）、系统声音、键鼠注入和剪贴板同步。

网络穿透不在本程序范围内。请用 Tailscale / EasyTier / ZeroTier 等工具把两台电脑组到同一个网络，客户端直接连被控端在该网络里的 IP。

## 快速开始（正式使用：安装为服务）

以**管理员身份**打开终端，进入程序所在目录：

```powershell
.\nya-server.exe install
```

- 服务名 `NyaRemoteControl`，开机自启；程序路径就是当前位置，移动文件前先卸载。
- 会自动完成：防火墙放行 UDP 47100；开启 `SoftwareSASGeneration` 策略（让远程 Ctrl+Alt+Del 可用）；数据目录 `C:\ProgramData\NyaRemoteControl` 仅允许 SYSTEM / 管理员访问。
- 最后会打印**配对码**，客户端第一次连接时输入。

服务模式下可以操作锁屏、登录界面和 UAC 弹窗，注销、切换用户后连接也不会断。

## 开发模式

```powershell
.\nya-server.exe standalone
```

以当前用户身份单进程运行，窗口中直接显示配对码。**开发模式无法操作锁屏、登录界面和 UAC 弹窗**，也不能远程发送 Ctrl+Alt+Del。

## 常用命令

| 命令 | 说明 |
|---|---|
| `nya-server pair` | 查看配对码（服务模式需要管理员） |
| `nya-server pair --reset` | 重新生成配对码，已配对的客户端不受影响 |
| `nya-server clients` | 列出已配对的客户端 |
| `nya-server clients --remove <指纹前缀>` | 移除某个客户端 |
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

修改后重启服务：`sc stop NyaRemoteControl`，然后 `sc start NyaRemoteControl`。

## 日志

保存在数据目录下的 `logs\`（`service.*.log`、`helper.*.log`、`standalone.*.log`），保留 7 天。FFmpeg 自身的输出默认关闭（探测和降级时的报错是正常现象）；排查编码问题时可设置环境变量 `NYA_FFMPEG_LOG=warning`（或 `verbose`）后再运行。排查问题时请连同 `nya-diag.txt` 一起提供。

## 编码器选择（多显卡 / 笔记本）

- 截屏必须在显示器所连接的显卡上完成；编码可以在任意显卡上完成。
- 默认优先使用同一块显卡编码（不需要拷贝）。如果办公模式需要 4:4:4 而这块显卡不支持（例如较老的核显），会自动改用 N 卡编码，画面通过内存在两块显卡之间传递。
- 如果某个编码器在运行中连续出错，本次运行会自动换用下一个方案。

## 构建

依赖同级目录的 `../common`（公共仓库）和 `../third_party/ffmpeg`（运行 `../common/scripts/fetch-ffmpeg.ps1` 下载）。

```powershell
cargo build --release
.\scripts\package.ps1      # 生成 dist\nya-server（exe + 3 个 FFmpeg DLL）
```

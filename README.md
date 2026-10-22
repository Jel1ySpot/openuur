# openuur

`openuur` 是 UU 远程协议的开源兼容实现。

## 快速开始（安装为 systemd 服务）

### 1. 安装二进制

`cargo install` 可以安装到 `~/.cargo/bin`。默认构建包含 Wayland portal 与 X11 桌面采集，需要 PipeWire/SPA 开发头文件和 libclang：

```bash
# Debian / Ubuntu
sudo apt install build-essential pkg-config clang libclang-dev \
  libpipewire-0.3-dev libspa-0.2-dev \
  ffmpeg xdg-desktop-portal libpam0g wireplumber wl-clipboard
# 按桌面环境再安装对应 portal 后端：
#   xdg-desktop-portal-gnome / xdg-desktop-portal-kde / xdg-desktop-portal-wlr

# Fedora（ffmpeg 需启用 RPM Fusion）
sudo dnf install gcc pkgconf-pkg-config clang clang-devel pipewire-devel \
  ffmpeg xdg-desktop-portal pam wireplumber wl-clipboard
# 按桌面环境再安装对应 portal 后端：
#   xdg-desktop-portal-gnome / xdg-desktop-portal-kde / xdg-desktop-portal-wlr

# Arch
sudo pacman -S base-devel pkgconf clang pipewire \
  ffmpeg xdg-desktop-portal pam wireplumber wl-clipboard
# 按桌面环境再安装对应 portal 后端：
#   xdg-desktop-portal-gnome / xdg-desktop-portal-kde / xdg-desktop-portal-wlr
```

然后从源码目录或 git 安装：

```bash
# 从 git 仓库安装
cargo install --git https://github.com/jel1yspot/openuur --locked
```

只安装终端功能、不编译桌面采集时，可以关闭默认的 `desktop` feature，此时不需要 PipeWire 头文件和 libclang：

```bash
cargo install --git https://github.com/jel1yspot/openuur --locked --no-default-features
```

安装后：

```bash
openuur --version
openuur install          # 用户级 systemd 服务
```

注意 `cargo install` 默认忽略 `Cargo.lock`，请始终加 `--locked`；本项目依赖 `vendor/webrtc-dtls` 的补丁，crates.io 上的包不会携带该补丁，请用 `--path` 或 `--git` 安装，不要使用 `cargo install openuur`。

运行时依赖：

- `ffmpeg`：视频编码
- `xdg-desktop-portal` 及对应桌面后端（`xdg-desktop-portal-gnome` / `xdg-desktop-portal-kde` / `xdg-desktop-portal-wlr`）：Wayland 屏幕采集与远程输入（X11 会话直接使用 X 协议，不需要 portal）
- `libpam`：锁屏自动解锁
- `wireplumber`：提供 `wpctl`，用于静音运行
- `wl-clipboard`：文本剪贴板

使用 `--no-default-features` 安装时，以上仍是远程桌面、自动解锁、静音和剪贴板功能的运行时依赖。

### 2. 登录账号

```bash
openuur login --qr
```

可选地选择采集源（默认 `monitor`）：

```bash
openuurd display source monitor   # 显示器
openuurd display source window    # 单个窗口
openuurd display source virtual   # compositor 虚拟源
```

### 3. 安装服务

```bash
# 当前用户：~/.config/systemd/user/openuurd.service
openuur install

# 系统服务：/etc/systemd/system/openuurd.service
# 使用 sudo 发起者的账号作为服务账号
sudo openuur install

# 或显式指定拥有图形会话的账号
sudo openuur install --user alice
```

手动安装：

```bash
mkdir -p ~/.config/systemd/user
cp contrib/openuurd-user.service ~/.config/systemd/user/openuurd.service
systemctl --user daemon-reload
systemctl --user enable --now openuurd
```

`contrib/openuurd.service` 是普通系统服务示例；系统服务只有在写入目标账号的 `XDG_RUNTIME_DIR`、session bus 和 `WAYLAND_DISPLAY` 时才能访问 Wayland/portal（`openuur install` 会自动生成这些设置）。没有活动图形会话时只提供终端。

### 4. 查看状态

```bash
systemctl --user status openuurd.service
journalctl --user -u openuurd -f
```

### 5. 连接

在另一台设备的官方 UU 客户端中选择该设备并打开「远程桌面」。首次连接仍需要在图形会话中确认 portal 屏幕共享/远程输入授权。

> 提示：系统服务适合需要开机自启且账号会话稳定的机器。若没有活动 Wayland 会话，`openuurd` 仍可提供终端，但远程桌面需要图形会话和 portal 权限。推荐优先使用用户级服务。

## 快速开始

### 1. 登录

```bash
# 短信验证码
openuur login --mobile 13800000000 --send-code
openuur login --mobile 13800000000 --code 123456

# 终端二维码（UU App 扫码）
openuur login --qr

# 或 UU access token
openuur login --token '<uu-access-token>'

# 或 MuMu token
openuur login --mumu-token '<token>'
```

### 2. 设备

```bash
openuur devices
openuur devices --json
openuur device rename home '书房电脑'
openuur device wallpaper home --url 'https://example.com/wallpaper.png'
openuur device info home
```

### 3. 在被控设备启动 `openuurd`

被控设备登录同一账号后：

```bash
openuurd run
```

需要终端解锁密码时：

```bash
openuurd run --password 'your-password'
```

远程桌面要求 `openuurd` 能找到该账号**已登录的图形会话**：Wayland 会话通过 xdg-desktop-portal（首次连接会弹出屏幕共享 + 远程输入授权；系统服务会通过 logind 投影 `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`/session bus），X11 会话直接使用 XDamage/XTest（不需要 portal）。在没有图形会话的 greeter 阶段仍只提供终端。

`--no-online` 只生成/显示本地设备身份，不连接 UU 信令：

```bash
openuurd run --no-online
```

### 4. 控制端连接终端

```bash
openuur term '书房电脑'
openuur term '书房电脑' --shell bash
openuur term '书房电脑' --session-id 12   # 附着已有会话
openuur term --local                    # 仅本机 PTY 自测
```

## Nix / NixOS

包内已包含 ffmpeg、PipeWire/WirePlumber、PulseAudio、wl-clipboard、`gio`/`gtk-launch`、`qdbus`、PAM 等运行时依赖。需要额外编码器时可覆盖 `ffmpeg`：

```nix
openuur.override { ffmpeg = pkgs.ffmpeg-full; }
```

### NixOS 模块

```nix
{
  inputs.openuur.url = "path:/path/to/openuur";

  # 在 nixosSystem modules 中
  imports = [ inputs.openuur.nixosModules.default ];

  services.openuur = {
    enable = true;
    user = "alice";                 # 必须拥有活动的 Wayland 图形会话
    displaySource = "monitor";      # monitor | window | virtual
    extraGroups = [ "video" "render" "audio" ];
    fileSavePath = "/home/alice/Downloads";

    video = {
      encoder = "auto";             # auto | cpu | nvenc | vaapi | qsv | amf
      codec = "auto";               # auto | h264 | h265
    };

    # 非敏感配置写入只读 TOML；不要在这里放 token。
    settings = {
      device_name = "living-room";
      shell = "bash";
    };

    # token 等敏感配置放在 Nix store 之外：
    # configFile = "/run/secrets/openuur-config.toml";
    # environmentFile = "/run/secrets/openuur-env";  # 例如 OPENUUR_TERMINAL_PASSWORD
  };
}
```

- `configFile` 与 `settings` 互斥；`settings` 会复制到可写状态文件 `~/.local/state/openuur/config.toml`（系统服务和 Home Manager 相同），仅在文件不存在时复制；修改 Nix 配置后删除该文件即可重新应用。
- `settings` 不要放 `token`。可用该账号执行 `openuur --config ~/.local/state/openuur/config.toml login --qr` 登录/注册。也可以直接用 `configFile` 指向 secret 管理的可写配置；如果该文件只读，需已包含注册后的 `device_id` 和 `device_registered = true`。
- 服务默认读取目标账号的 `~/.local/state/openuur/config.toml`；先以该账号执行 `openuur login --qr`。旧的 `~/.config/openuur/config.toml` 在状态目录没有配置时仍会继续使用。
- 系统服务会在启动时检测 `$XDG_RUNTIME_DIR/wayland-*`，因此 `wayland-0`/`wayland-1` 都可用。
- `services.openuur.ffmpegPackage` 可选择 `pkgs.ffmpeg-full` 以获得更多硬件编码器。

### Home Manager 模块

```nix
{
  imports = [ inputs.openuur.homeManagerModules.default ];

  services.openuur = {
    enable = true;
    displaySource = "window";
    video.encoder = "vaapi";
  };
}
```

用户服务依赖 `graphical-session.target`，推荐在用户的 Home Manager 配置中启用。

## 构建

需要 Rust 1.87+，以及桌面采集所需的 PipeWire/SPA 开发头文件和 libclang（bindgen）。

```bash
# Debian / Ubuntu
sudo apt install build-essential pkg-config clang libclang-dev \
  libpipewire-0.3-dev libspa-0.2-dev
cargo build --release

# RHEL / Fedora
# RHEL 需先启用 CRB（CodeReady Builder）仓库
sudo dnf install gcc gcc-c++ make pkgconf-pkg-config clang clang-devel \
  pipewire-devel
cargo build --release

# Arch
sudo pacman -S --needed base-devel pkgconf clang pipewire
cargo build --release

# Nix

P=$(nix eval --raw nixpkgs#pipewire.dev)
C=$(nix eval --raw nixpkgs#llvmPackages.libclang.lib)
PKG_CONFIG_PATH=$P/lib/pkgconfig LIBCLANG_PATH=$C/lib \
nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#pkg-config nixpkgs#openssl \
  nixpkgs#pipewire.dev nixpkgs#clang \
  -c cargo build --release
```

只构建终端功能时可关闭默认的 `desktop` feature，此时不需要 PipeWire 头文件和 libclang：

```bash
cargo build --release --no-default-features
```

运行时还需要 `ffmpeg`（含 `libx264`）；Wayland 会话还需要 `xdg-desktop-portal` 和对应桌面后端（KDE/GNOME/wlroots），X11 会话不需要。

产物：

```text
target/release/openuur
target/release/openuurd
```

## 远程桌面功能与开关

| 功能 | 说明 |
|---|---|
| 多显示器 | portal 一次授权多个 monitor；官方客户端通过 `CaptureConfig.screen_id` 切换 |
| 采集后端 | `openuurd run --desktop-backend auto\|portal\|wlroots\|x11`、`UU_DESKTOP_BACKEND`；auto 按会话类型选择（GNOME/KDE Wayland → portal，其它 Wayland → wlroots，X11 → x11）；原生 wlroots 尚未落地，auto 选中 wlroots 时暂用 portal |
| X11 后端 | XDamage + MIT-SHM 采集（缺 DAMAGE/SHM 时退化轮询或 `GetImage`）、XTest 绝对/相对指针与按键、120 单位滚轮、临时 keycode 文本输入、XFixes 光标形状同步（serial 去重、≤30 Hz、PNG）、RandR 活动显示器切换；窗口/虚拟源仍请使用 portal；X11 会话暂不支持文本剪贴板 |
| 会话发现 | 系统服务通过 logind 找到账号的活动图形会话，投影 `WAYLAND_DISPLAY`/`DISPLAY`/`XDG_RUNTIME_DIR`/session bus 等 7 个变量；每 2 秒复查，会话切换只断开桌面 peer，终端/文件/端口映射保留；没有会话时明确报「等待图形会话」 |
| 显示源 | `openuurd display source monitor\|window\|virtual`，或 `openuurd run --display-source ...` / `UU_DISPLAY_SOURCE` |
| 分辨率 | 编码器缩放，不修改宿主机显示模式；显示配置变化时重建采集并保留编码尺寸 |
| H.264/H.265 | `UU_VIDEO_CODEC=h264\|h265`；默认跟随官方协商 |
| 真彩 4:4:4 | NVENC / libx264 / libx265；VAAPI/QSV/AMF 保持 4:2:0 |
| 硬件编码 | `UU_VIDEO_ENCODER=auto\|cpu\|nvenc\|vaapi\|qsv\|amf`；失败时回退软件 |
| hevc_vaapi | 支持无 AUD 的 HEVC 图像边界切分；驱动不可用时回退软件 |
| 系统音频 | Opus 128k，`UU_AUDIO=0` 关闭，`UU_AUDIO_SOURCE` 指定 source |
| 静音运行 | null sink + monitor：本机扬声器静音，控制端仍能听到声音；会话结束恢复 |
| 锁屏状态/自动解锁 | logind `LockedHint` + PAM；锁屏时发送合成黑帧，官方客户端可进入自动解锁 |
| 休眠抑制/唤醒 | 有远端 peer 时持有 logind `sleep:idle` block 抑制器，最后一个 peer 断开即释放；`UU_PREVENT_SLEEP=0` 关闭；系统 `PrepareForSleep` 时暂停采集与输入，恢复后唤醒显示器 |
| 系统操作 | 锁屏、显示桌面、任务管理器、重启、关机 |
| 文本剪贴板 | GNOME/KDE portal、KDE Klipper、wl-clipboard；聚焦时重读 |
| 文件传输 | 浏览/上传/下载/改名/删除、压缩目录、续传、跳过/覆盖/重名 |
| 文件保存路径 | `--file-save-path` / `UU_FILE_SAVE_PATH`；通过 SystemMetrics/RemoteDownloadPath 上报 |
| 端口映射 | 官方端口映射工具；`UU_PORT_MAPPING=0` 关闭 |
| 应用快捷启动 | 导出 XDG `.desktop` 应用，RPC 23/24 + `LaunchApp` |
| FeatureFlag/echo | 采集设置、简单动作、系统指标、文件传输、剪贴板能力按实际就绪状态声明 |
| UU RTP 扩展 | `video-capture-index`、`video-frame-is-new-frame`（SDP extmap + RTP header） |
| restore token | portal 权限 token 持久化（0600），锁屏/显示变化后尝试免弹窗恢复 |
| 网络诊断/切换 | 不记录地址/凭据的 ICE 路径与码率日志；断开 4s 后 host 发起 ICE 重启 |
| 二维码登录 | `openuur login --qr` 终端二维码、状态轮询、过期刷新 |
| DC-KCP | 核心、外层帧和协商属性已实现，默认仍走 SCTP；QUIC 仅保留 SDP 兼容属性 |

## 协议参考

原生 UU 终端字段和互操作行为参考了公开项目 [`VallierDev/uu-remote-linux`](https://github.com/VallierDev/uu-remote-linux) 的验证结果；本仓库的 `src/uu_native/` 是根据二进制分析的独立实现。

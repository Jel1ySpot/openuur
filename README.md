# openuur

`openuur` 是 UU 远程协议的开源兼容实现。

## 快速开始（安装为 systemd 服务）

### 1. 安装二进制

`cargo install` 可以安装到 `~/.cargo/bin`。默认构建包含 Wayland portal、原生 wlroots 与 X11 桌面采集，需要 PipeWire/SPA、libxkbcommon 开发头文件和 libclang：

```bash
# Debian / Ubuntu
sudo apt install build-essential pkg-config clang libclang-dev \
  libpipewire-0.3-dev libspa-0.2-dev libxkbcommon-dev xkb-data \
  ffmpeg xdg-desktop-portal libpam0g wireplumber wl-clipboard \
  libei1 x11-xserver-utils
# 按桌面环境再安装对应 portal 后端：
#   xdg-desktop-portal-gnome / xdg-desktop-portal-kde / xdg-desktop-portal-wlr

# Fedora（ffmpeg 需启用 RPM Fusion）
sudo dnf install gcc pkgconf-pkg-config clang clang-devel pipewire-devel \
  libxkbcommon-devel xkeyboard-config \
  ffmpeg xdg-desktop-portal pam wireplumber wl-clipboard libei xrandr
# 按桌面环境再安装对应 portal 后端：
#   xdg-desktop-portal-gnome / xdg-desktop-portal-kde / xdg-desktop-portal-wlr

# Arch
sudo pacman -S base-devel pkgconf clang pipewire libxkbcommon xkeyboard-config \
  ffmpeg xdg-desktop-portal pam wireplumber wl-clipboard libei xorg-xrandr
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
- `xdg-desktop-portal` 及对应桌面后端（`xdg-desktop-portal-gnome` / `xdg-desktop-portal-kde` / `xdg-desktop-portal-wlr`）：GNOME/KDE Wayland 的屏幕采集与远程输入；wlroots 桌面（Sway/Hyprland 等）默认走原生协议，不需要 portal
- `libxkbcommon` + `xkeyboard-config`：原生 wlroots 键盘输入（编译与运行时）
- `libpam`：锁屏自动解锁
- `libei`（≥1.1，可选）：portal 远程输入的 EIS 后端；缺少时自动回退 Notify
- `xrandr`：X11 控制端切换分辨率时新建/切换显示模式（`x11-xserver-utils` / `xorg-xrandr`）
- `wireplumber`：提供 `wpctl`，用于静音运行
- `wl-clipboard`：Wayland 文本剪贴板（X11 使用内置 XFixes，无需 `xclip`）

`/dev/uinput` 可写时，Wayland/portal 会话默认使用内核虚拟键鼠（布局无关、锁屏可用）。如未生效，安装 udev 规则：

```bash
echo 'KERNEL=="uinput", SUBSYSTEM=="misc", OPTIONS+="static_node=uinput", TAG+="uaccess"' \
  | sudo tee /etc/udev/rules.d/70-openuur-uinput.rules
sudo udevadm control --reload && sudo udevadm trigger
```

`UU_INPUT_BACKEND=auto|uinput|native` 可选择输入后端；X11 默认仍用 XTest。

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
# 当前用户（systemd user 服务）：~/.config/systemd/user/openuurd.service
openuur install

# 系统服务：/etc/systemd/system/openuurd.service
# 使用 sudo 发起者的账号作为服务账号
sudo openuur install

# 或显式指定拥有图形会话的账号
sudo openuur install --user alice
```

安装器会探测 init（systemd / OpenRC / runit / s6 / SysV / XDG autostart），也可以显式指定：

```bash
# 开机启动的系统服务（不依赖登录）
sudo openuur install --init openrc --scope system --user alice
sudo openuur install --init runit  --scope system --user alice
sudo openuur install --init s6     --scope system --user alice
sudo openuur install --init sysv   --scope system --user alice

# 登录后自动启动的用户级入口（非 systemd init 会回退到 XDG autostart）
openuur install --init xdg --scope user

# 只写入并启用服务，不立即启动（runit/s6 链接后可能由扫描器启动）
sudo openuur install --init systemd --no-start
```

| init | 系统级路径 | 启用/启动 |
|---|---|---|
| systemd | `/etc/systemd/system/openuurd.service` | `systemctl daemon-reload && systemctl enable --now openuurd` |
| OpenRC | `/etc/init.d/openuurd`（共享启动脚本 `/etc/openuurd/start`） | `rc-update add openuurd default && rc-service openuurd start` |
| runit | `/etc/sv/openuurd/run` + `/var/service/openuurd` 链接 | `sv up openuurd` |
| s6 | `/etc/s6/sv/openuurd/{run,type}` | 链接到扫描目录后 `s6-svscanctl -a /run/service`；s6-rc 系统需重建数据库 |
| SysV | `/etc/init.d/openuurd` | `update-rc.d openuurd defaults && service openuurd start` |
| XDG | `~/.config/autostart/openuurd.desktop` | 下次登录自动启动 |

系统服务在**开机时即启动，不等待图形登录**：终端、文件传输和端口映射在登录前就可用；桌面采集在账号登录并出现图形会话后自动接入（每 2 秒复查会话），**不会采集显示管理器的 greeter**。systemd 用户服务仍以 `graphical-session.target` 为依赖。

手动安装示例：

```bash
# systemd（用户）
mkdir -p ~/.config/systemd/user
cp contrib/openuurd-user.service ~/.config/systemd/user/openuurd.service
systemctl --user daemon-reload
systemctl --user enable --now openuurd

# OpenRC（系统）
sudo mkdir -p /etc/openuurd
sudo cp contrib/templates/openuurd.openrc /etc/init.d/openuurd
sudo cp contrib/templates/openuurd.start /etc/openuurd/start   # 按需修改路径/账号
sudo chmod 755 /etc/init.d/openuurd /etc/openuurd/start
sudo rc-update add openuurd default && sudo rc-service openuurd start
```

`contrib/openuurd.service` 是普通系统服务示例；系统服务由安装器写入目标账号的 `XDG_RUNTIME_DIR` 与 session bus，但**不写死 `WAYLAND_DISPLAY` 或 `XDG_SESSION_TYPE`**——`openuurd` 会在运行时发现当前图形会话（见下表「会话发现」）。

### 4. 查看状态

```bash
systemctl --user status openuurd.service
journalctl --user -u openuurd -f

# 非 systemd
rc-service openuurd status
sv status openuurd
service openuurd status

# 一键诊断（17 项只含状态，可直接粘贴到 issue）
openuurd doctor
```

### 5. 连接

在另一台设备的官方 UU 客户端中选择该设备并打开「远程桌面」。portal 后端会在 `openuurd` 服务启动时立即申请屏幕共享/远程输入授权，并在服务运行期间保持该会话；后续连接直接复用，不再重复确认。若服务启动时还没有活动的图形会话，则会在首次桌面连接时申请。

> 提示：系统服务适合需要开机自启且账号会话稳定的机器。没有活动图形会话时，`openuurd` 仍提供终端/文件/端口映射，远程桌面会在登录后恢复；桌面请求在登录前会快速失败。推荐优先使用系统级服务以获得开机可用性。

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

远程桌面要求 `openuurd` 能找到该账号**已登录的图形会话**：Wayland 会话通过 xdg-desktop-portal（`openuurd` 启动时就会申请屏幕共享 + 远程输入授权，并在运行期间保持；之后的连接复用该会话；系统服务会通过 logind 投影 `WAYLAND_DISPLAY`/`XDG_RUNTIME_DIR`/session bus），X11 会话直接使用 XDamage/XTest（不需要 portal）。在没有图形会话的 greeter 阶段仍提供终端、文件传输与端口映射，但不会采集 greeter 桌面；登录后桌面自动接入。

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
    user = "alice";                 # 必须拥有活动的图形会话
    displaySource = "monitor";      # monitor | window | virtual
    desktopBackend = "auto";        # auto | portal | x11 | wlroots
    preventSleep = true;            # 有任意远端会话时阻止系统休眠/空闲
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
- 系统服务在开机时启动，不写死 `WAYLAND_DISPLAY` 或 `XDG_SESSION_TYPE`；`openuurd` 通过 logind 在运行时发现目标账号的图形会话，登录后自动接入桌面，显示管理器阶段不采集 greeter。
- `desktopBackend` 通过 `UU_DESKTOP_BACKEND` 传给服务（配置文件中显式的 `desktop_backend` 优先）；`preventSleep = false` 写入 `UU_PREVENT_SLEEP=0`。
- `services.openuur.ffmpegPackage` 可选择 `pkgs.ffmpeg-full` 以获得更多硬件编码器。

### Home Manager 模块

```nix
{
  imports = [ inputs.openuur.homeManagerModules.default ];

  services.openuur = {
    enable = true;
    displaySource = "window";
    desktopBackend = "portal";
    preventSleep = true;
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

运行时还需要 `ffmpeg`（含 `libx264`；H.265 需要 `libx265` 或硬件编码器）；GNOME/KDE Wayland 会话还需要 `xdg-desktop-portal` 和对应桌面后端，wlroots 桌面（Sway/Hyprland）使用原生协议并需要 `libxkbcommon`/`xkeyboard-config`，X11 会话不需要。X11 分辨率切换需要 `xrandr`；portal EIS 输入需要可选运行时库 `libei`（≥1.1）。

产物：

```text
target/release/openuur
target/release/openuurd
```

## 远程桌面功能与开关

| 功能 | 说明 |
|---|---|
| 多显示器 | portal 一次授权多个 monitor；官方客户端通过 `CaptureConfig.screen_id` 切换 |
| 采集后端 | `openuurd run --desktop-backend auto\|portal\|wlroots\|x11`、`UU_DESKTOP_BACKEND`；auto 按会话类型选择（GNOME/KDE Wayland → portal，其它 Wayland → wlroots，X11 → x11）。wlroots 不可用（例如脚本自启动的合成器未暴露 wlr 协议）时，auto 回退 portal 并记录日志；显式 `wlroots` 则直接报错 |
| portal 输入 | 优先 `ConnectToEIS`/libei，按流的 `mapping_id` 匹配区域，缩放、旋转和多显示器下绝对坐标仍准确；portal 不支持 EIS 或缺少 `mapping_id` 时回退 `Notify*`。可见 `/dev/uinput` 时默认改用内核虚拟键鼠 |
| portal 拒绝 | 用户拒绝/取消（4）、授权超时（5）、缺少显示器或键鼠权限（6）、帧格式/尺寸不可用（8）、尺寸变化需重连（14）视为终止，不再反复重开授权框；其它错误按退避重试 |
| portal 会话复用 | `openuurd` 启动时即申请 portal 屏幕共享/远程输入授权并保持采集会话；控制端断开重连直接复用，不再重复确认（compositor 未返回 restore token 时同样生效）；图形会话切换或进程退出时释放。服务启动时没有图形会话则延后到首次桌面连接 |
| portal 旋转 | 处理 `SPA_META_VideoTransform`/`wl_output.transform` 的 90/180/270 与翻转，输出方向正确的帧；输入区域由 EIS 映射 |
| portal 光标 | 默认 `CursorMode::Hidden`，由控制端自绘指针，避免 GNOME/KDE Wayland 上出现合成器光标 + 控制端指针的双指针；`UU_PORTAL_CURSOR=embedded` 可恢复内嵌光标 |
| 锁屏/挂起输入释放 | 锁屏、休眠、桌面 peer 结束时释放 portal 会话中仍按下的按键/鼠标键（portal 没有 release-all，宿主逐键补发，单次调用 2 秒超时；失败项保留到会话重建后重试），解锁后不会残留 Ctrl/Shift 或鼠标键 |
| wlroots 后端 | 直接使用 `zwlr_screencopy_manager_v1`（wl_shm 双缓冲、Y_INVERT/XBGR/ABGR 转换）、`zwlr_virtual_pointer_v1`、`zwp_virtual_keyboard_v1`（xkbcommon keymap、修饰键计数、Caps/Num Lock、文本临时 keycode）与 `zwlr_output_power_manager_v1` 唤醒；多显示器绝对坐标使用 `zxdg_output_v1` 逻辑布局（wl_output.geometry 在 wlroots 恒为 0）；锁屏/挂起时立即停帧并释放输入，长文本粘贴也会中止；不支持窗口/虚拟源与光标形状，旋转输出以合成器返回的缓冲区尺寸为准 |
| X11 后端 | XDamage + MIT-SHM 采集（缺 DAMAGE/SHM 时退化轮询或 `GetImage`）、XTest 绝对/相对指针与按键、120 单位滚轮、临时 keycode 文本输入、XFixes 光标形状同步（serial 去重、≤30 Hz、PNG）、RandR 活动显示器切换；XFixes 原生文本剪贴板（任意 X11，无需 xclip/xsel）；窗口/虚拟源仍请使用 portal。`UU_INPUT_BACKEND=uinput` 可强制内核键鼠 |
| uinput 键鼠 | `/dev/uinput` 虚拟键盘 + 绝对/相对指针、按键、滚轮；Wayland/portal 默认优先，锁屏和 greeter 也可用；失败回退 portal/XTest。设备随会话销毁并逐键释放 |
| 快捷键映射 | macOS 控制端的 Cmd+C/V/A/F/X/Z 映射为 Ctrl（聚焦终端内 Cmd+C/V 为 Ctrl+Shift），其余 Cmd 组合与单独 Cmd 保持 Super；X11 终端内 Ctrl+V 自动变为 Ctrl+Shift+V；`UU_MAC_SHORTCUTS=0\|1` 强制控制端类型，`UU_TERMINAL_SHORTCUTS=0` 关闭终端转换 |
| 会话发现 | 系统服务通过 logind 找到账号的活动图形会话，投影 `WAYLAND_DISPLAY`/`DISPLAY`/`XDG_RUNTIME_DIR`/session bus 等 7 个变量；每 2 秒复查，会话切换只断开桌面 peer，终端/文件/端口映射保留；没有会话时明确报「等待图形会话」。没有 logind 时仅在非 root 进程中使用经过 socket 校验的进程环境兜底；root 进程需显式设置 `UU_SESSION_ENV_FALLBACK=1` |
| 显示源 | `openuurd display source monitor\|window\|virtual`，或 `openuurd run --display-source ...` / `UU_DISPLAY_SOURCE` |
| 分辨率 | X11 直接修改显示模式：优先切换已有模式，缺失时用 CVT reduced-blanking 新建（可高于原生，单边上限 4096），选「原始」或最后一个桌面会话结束时恢复原模式；`UU_DESKTOP_RESIZE=0` 关闭，仅缩放编码画面。Wayland 仍由合成器决定尺寸并编码缩放 |
| H.264/H.265 | `UU_VIDEO_CODEC=h264\|h265`；硬件编码器默认优先 H.265（Mac 控制端列表首位），软件编码默认 H.264。启动时真实编码一帧 H.265 探测驱动/库，失败则只声明 H.264，不再仅凭 `ffmpeg -encoders` 猜测 |
| CQ/CRF | NVENC 使用 `-rc vbr -cq`，libx264/libx265 在自动画质使用 `-crf`，固定码率档改为 ABR；画质档 1–5 映射 CQ 30/26/21/17/18，VAAPI 使用 `-qmin` 质量下限 |
| 真彩 4:4:4 | NVENC / libx264 / libx265；VAAPI/QSV/AMF 保持 4:2:0 |
| 硬件编码 | `UU_VIDEO_ENCODER=auto\|cpu\|nvenc\|vaapi\|qsv\|amf`；失败时回退软件 |
| hevc_vaapi | 支持无 AUD 的 HEVC 图像边界切分；驱动不可用时回退软件 |
| 系统音频 | Opus 128k，`UU_AUDIO=0` 关闭，`UU_AUDIO_SOURCE` 指定 source |
| 静音运行 | null sink + monitor：本机扬声器静音，控制端仍能听到声音；会话结束恢复 |
| 锁屏状态/自动解锁 | logind `LockedHint` + PAM；锁屏时发送合成黑帧，官方客户端可进入自动解锁 |
| 休眠抑制/唤醒 | 有远端 peer 时持有 logind `sleep:idle` block 抑制器，最后一个 peer 断开即释放；`UU_PREVENT_SLEEP=0` 关闭；系统 `PrepareForSleep` 时暂停采集与输入，恢复后唤醒显示器 |
| 系统操作 | 锁屏、显示桌面、任务管理器、重启、关机 |
| 文本剪贴板 | GNOME/KDE portal、KDE Klipper、wl-clipboard，以及任意 X11 的原生 XFixes `CLIPBOARD`（TARGETS/UTF8_STRING/TEXT/STRING，30 KiB 上限）；聚焦时重读，不记录内容 |
| 文件传输 | 浏览/上传/下载/改名/删除、压缩目录、续传、跳过/覆盖/重名 |
| 文件保存路径 | `--file-save-path` / `UU_FILE_SAVE_PATH`；通过 SystemMetrics/RemoteDownloadPath 上报 |
| 端口映射 | 官方端口映射工具；`UU_PORT_MAPPING=0` 关闭 |
| 应用快捷启动 | 导出 XDG `.desktop` 应用，RPC 23/24 + `LaunchApp` |
| FeatureFlag/echo | 采集设置、简单动作、系统指标、文件传输、剪贴板能力按实际就绪状态声明 |
| 自定义画质 | 控制端自定义码率按 **bit/s** 解析（1 Mbps = 1,000,000），编码上限 100 Mbit/s；`CaptureSettingSync` 以同一单位回读 |
| 登录过期 | 房间接口返回 HTTP 422 / code 1120 时标记登录过期；过期重试前重读配置文件，`openuur login` 写入新 token 后无需重启即可接续，未更新时只提示一次并保持 30 秒低频重试 |
| RTCP PLI/FIR | 读取视频 RTCP，收到 PLI/FIR 立即重建编码器产生关键帧（≤500 ms 节流），弱网丢包后无需等 1 秒 GOP |
| 端口映射背压 | 端口映射使用有界队列且不在数据通道回调里等待（避免阻塞同一 SCTP 关联上的桌面输入/终端）；队列满时干净关闭该映射流（FIN + 关闭目标连接），不再静默丢字节破坏 TCP 流 |
| UU RTP 扩展 | `video-capture-index`、`video-frame-is-new-frame`（SDP extmap + RTP header） |
| restore token | portal 权限 token 持久化（0600），锁屏/显示变化后尝试免弹窗恢复 |
| 网络诊断/切换 | 不记录地址/凭据的 ICE 路径与码率日志；断开 4s 后 host 发起 ICE 重启 |
| 跨网络 ICE | 主机/控制端候选携带协商的 BUNDLE `sdpMid`/`sdpMLineIndex`（webrtc-rs 原始候选为空），并排除 ZeroTier、容器桥、libvirt 等不可达虚拟网卡；物理网卡、公网与 TURN 候选正常参与检查 |
| 信令租约 | 房间空闲 900 秒后刷新信令凭据；仍有已连接 peer 时推迟，避免打断远控 |
| STREAMER 通道 | 回应官方控制端的 `{"type":"ping"}` 为 `{"type":"pong"}`，遥测消息不进入输入/剪贴板解析 |
| 输入所有权 | 每个桌面 peer 持有 DesktopOwner 租约；新 peer 顶替旧 peer，旧 peer 的延迟按键/释放不会注入或清除新会话的按键，租约随连接状态激活/释放 |
| 诊断 | `openuurd doctor` / `openuur doctor`：17 项探针（系统、会话、显示、后端、portal、PipeWire、wlroots、X11 扩展、uinput、FFmpeg、H.264/H.265 真实探测、编码器、4:4:4、音频、账号/服务），输出不含 token、设备 ID、地址或文件内容 |
| 二维码登录 | `openuur login --qr` 终端二维码、状态轮询、过期刷新 |
| DC-KCP | 核心、外层帧和协商属性已实现，默认仍走 SCTP；QUIC 仅保留 SDP 兼容属性 |

## 协议参考

原生 UU 终端字段和互操作行为参考了公开项目 [`VallierDev/uu-remote-linux`](https://github.com/VallierDev/uu-remote-linux) 的验证结果；本仓库的 `src/uu_native/` 是根据二进制分析的独立实现。

## 许可证

本项目以 **MIT** 许可证发布，完整文本见 [LICENSE](LICENSE)。`src/uu_native/` 与各桌面后端均为独立实现，不包含参考项目的 GPL 代码或资源。

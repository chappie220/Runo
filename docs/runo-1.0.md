# Runo 1.0 设计

Runo 1.0 是一个专为树莓派打造、以 Agent 为主要操作界面的精简 Linux 系统。
用户接通模型后，由 Agent 检查环境、配置功能、启动服务并验证结果，取代逐项填写的互动引导。

本文记录已定的设计、尚待定案的可靠性设计（附建议方案）、开发顺序和验收流程。
标注“建议”的条目还没有定案。

## 1. 已定设计

| 项目 | 设计 |
|---|---|
| 硬件平台 | 仅支持树莓派，不做 x86。**支持树莓派 5 和树莓派 4**，各出一个镜像（见第 2 节） |
| 系统底座 | **Buildroot 构建，BusyBox 提供 init、Shell 和基础命令**；只保留必要的驱动、存储、TTY/PTY、基础网络和运行依赖（见第 3 节） |
| 开机界面 | 直接进入 Agent；首次使用时在这里填写模型连接信息 |
| 自动配置 | 模型检查环境、配置功能、启动服务并验证；缺少必要信息时才询问 |
| 命令行入口 | 用户手动输入 `!shell` 才进入 Shell，退出后回到 Agent |
| 故障行为 | 模型不可用时保留管理界面，不自动掉进 Shell；`!shell` 仍然有效 |
| 远程工具 | 自有管理客户端，设备以列表展示，支持局域网发现、直连和公网访问 |
| 信任建立 | 必须先在局域网完成认证与配对，公网不允许首次配对 |
| 登录方式 | 支持密码和密钥；公网访问还必须具备已登记的客户端信任 |
| 设备识别 | 持久设备 ID 加身份密钥；安装时写入的主机地址只是连接线索 |
| 远程操作 | 登录后默认进入 Agent，可配置 API、查看状态和日志、传文件，通过 `!shell` 进入终端 |

## 2. 硬件支持

| 型号 | 1.0 状态 | 说明 |
|---|---|---|
| 树莓派 5（2/4/8/16 GB） | **支持**，验收主平台 | BCM2712；以太网、USB、GPIO 经 RP1 南桥 |
| 树莓派 4 B | **支持**，同样走完整验收 | BCM2711；单独一个镜像（见下） |
| 树莓派 400 / CM4 | 随 Pi 4 镜像附带，不单独验收 | Pi 4 配置里已含它们的设备树；CM4 的启动介质和网卡因载板而异 |
| 树莓派 3 / Zero 2 W | 不支持 | 内存 512 MB–1 GB，不够运行 Runo 加本地界面 |
| 32 位系统 | 不支持 | 只发 aarch64 |

树莓派 5 的具体要求：

- **内核与固件**：沿用 Buildroot 2025.02.x 的 `raspberrypi5_defconfig`：树莓派官方内核分支、`bcm2712` 内核配置、
  `rpi-firmware` 包，设备树 `bcm2712-rpi-5-b`、`bcm2712d0-rpi-5-b`（D0 步进）。
  2025.02 里没有 Pi 500 的设备树（`bcm2712-rpi-500`），Pi 500 不在 1.0 范围内。
- **页大小**：Buildroot 的 Pi 5 配置用 `linux-4k-page-size.fragment` 把内核编成 4K 页，避开 16K 页带来的兼容问题；Runo 沿用。
- **Pi 4**：Buildroot 按板子分配置，Pi 4 用 `raspberrypi4_64_defconfig`（内核配置 `bcm2711`，本来就是 4K 页），
  是第二个镜像。两个镜像共用同一个外部树、同一套覆盖层和 Runo 二进制，只有板级配置不同。
  - Pi 4 的驱动：以太网 `genet`；Wi‑Fi 同为 brcmfmac（CYW43455）。
  - Pi 4 **没有板载 RTC**：没联网时时间可能不对，TLS 校验会失败，所以对时要在联网后先做，管理界面要显示时间是否已同步。
  - Pi 4 的 `tryboot` 需要 2020 年后的 EEPROM 引导程序，首次开机时检查版本，太旧就在管理界面提示升级。
- **驱动**：RP1 以太网（macb）、Wi‑Fi/蓝牙（brcmfmac，CYW43455 固件）、USB 3、SD（sdhci）、PCIe（NVMe 启动可选）、
  风扇（pwm-fan + 温控）、板载 RTC（rtc-rpi，可在无网络时提供大致正确的时间，便于 TLS 校验）。
- **启动**：依赖 Pi 5 的 EEPROM 引导程序；`tryboot` 用于更新回退（见 4.5）。
- **电源**：官方 5V 5A 电源；供电不足时 USB 限流，需在状态页提示（读取 `vcgencmd get_throttled` 或等价接口）。

## 3. 系统构建：Buildroot + BusyBox

### 3.1 构建方式

- 本仓库新增 Buildroot 外部树 `buildroot/`（`BR2_EXTERNAL`），不分叉 Buildroot 本身。
- **Buildroot 版本（已定）：2025.02.x 长期支持分支**，跟随它的修复版（2026-10 时最新为 2025.02.18）。
  - 构建时把 Buildroot 作为子模块或固定版本下载，升级修复版走普通 PR。
  - 2025.02 的支持期结束前，再评估是否换到 2026.02。
- 外部树内容：
  - `configs/runo_rpi5_defconfig`、`configs/runo_rpi4_defconfig`：分别以 `raspberrypi5_defconfig`、`raspberrypi4_64_defconfig` 为起点。
  - `package/runo`：把 CI 已经在出的 `aarch64-unknown-linux-musl` 静态二进制放进镜像。
    以后也可以改为 Buildroot 的 `cargo-package` 从源码构建。
  - `board/runo/`：根文件系统覆盖层、`post-build.sh`，以及 `post-image.sh` 和 `genimage.cfg`（A/B 分区）。
- **工具链（已定）：musl**。2025.02 的树莓派配置用 Buildroot 自己编译的 glibc 工具链；改用 Bootlin 预编译的 aarch64 musl 稳定版
  （`BR2_TOOLCHAIN_EXTERNAL_BOOTLIN_AARCH64_MUSL_STABLE`），省去编译工具链的时间，整个镜像与 Runo 用同一种 C 库，体积更小。
  - 工具链的内核头文件版本不能高于镜像内核（2025.02 的 Pi 配置是 6.6）。
  - 代价是偏离 Buildroot 官方测过的组合，所以选进镜像的每个包都要在 musl 下构建并实测。
  - 域名解析走 musl 的实现，它和 glibc 不同（例如不读 `nsswitch.conf`），网络相关的验收要在镜像上做。
- **CI**：在发布流程里加一个镜像构建任务，产出 `sdcard.img.xz` 和更新包；Buildroot 的下载缓存和 `ccache` 需要缓存，否则单次构建要一小时以上。

### 3.2 BusyBox init 与服务

- 用 BusyBox init（`BR2_INIT_BUSYBOX`），不用 OpenRC 或 systemd。服务是 `/etc/init.d/S??*` 脚本，由 `rcS` 按序号启动。
- 开机直达 Agent 靠 `/etc/inittab`：
  - 阶段 3：`tty1::respawn:/usr/libexec/runo-console`。这个脚本在 tty1 上运行 `runo console`（Runo 自绘的终端，见 4.7，里面运行 `runo tui`；没有 `/dev/fb0` 时直接运行 `runo tui`），退出或崩溃后由 init 重新拉起。
  - 有了 `runod` 之后：改由 `runod` 管理 tty1，就不再提供 `getty` 登录。
  - 串口控制台（调试用）保留 `ttyAMA10` 上的 `getty`，只在开发镜像里启用。
- 网络：有线用 BusyBox `udhcpc`；Wi‑Fi 用 `wpa_supplicant` 加 `udhcpc`（已定，不用 iwd，因此镜像里不需要 D‑Bus）。
  `runod` 通过 `wpa_supplicant` 的控制接口配置 Wi‑Fi。对时用 BusyBox `ntpd`，加上 Pi 5 板载 RTC（`hwclock`）。
- 日志：BusyBox `syslogd`/`klogd` 写入 `data` 分区上的轮转日志，管理界面的“日志”一并显示。

### 3.3 镜像里有什么

| 类别 | 内容 |
|---|---|
| 基础 | BusyBox（`ash`、coreutils 子集、`udhcpc`、`ntpd`、`syslogd`、`hwclock`、`mount`）、`kmod`（带 xz，内核模块是 `.ko.xz`）、`e2fsprogs`（首次开机扩展 `data` 分区） |
| 排查用 | `htop`、`bash`（`/bin/sh` 仍是 BusyBox ash）、`lsblk`（util-linux） |
| 固件与驱动 | `rpi-firmware`、`linux-firmware` 中的 Broadcom Wi‑Fi 固件（CYW43455） |
| 网络 | `wpa_supplicant`、`nftables`（IPv6 公网入站防火墙，见 4.9）、`ca-certificates`（Runo 用系统根证书验证 TLS）；局域网发现由 Runo 自带（见 4.8），不装 avahi、mdnsd |
| 时间与语言 | `tzdata`；中文字体 `wqy-zenhei`（Buildroot 已有；Noto CJK 没有现成包） |
| Runo | `runo` 静态二进制；以后加 `runod` |
| 本地中文显示 | Runo 自己绘制（`runo console`，见 4.7）；阶段 3 第一版镜像曾用 `fbterm` 验证，已去掉 |
| 不放 | 包管理器、`sudo`、D‑Bus、Chromium（Buildroot 没有 Chromium 包）、`getty` 登录（发布镜像） |

### 3.4 对 Agent 和现有代码的影响

- **镜像里没有包管理器**：Agent 不能安装软件，能用的功能都要事先编进镜像。
  - 自配置能做的只有：写配置文件、启停 `/etc/init.d` 服务、验证。
  - 系统提示要告诉 Agent 这一点，并告诉它系统是 BusyBox：可用的命令和参数比 GNU 少。
  - `elevate.rs` 的装包路径、首次引导里的“安装浏览器”一步，在 Runo 系统上要关掉。
- **浏览器工具**：没有 Chromium，`browser` 工具默认关闭，网页靠 `web_fetch`。
- **服务管理**：`service.rs` 目前只支持 OpenRC（`rc-service`、`/sbin/openrc-run`）。Runo 系统上 Gateway 改为一个 BusyBox init 脚本；
  `runo service` 要能识别这种情况，否则会报“没有安装 OpenRC”。
- **更新**：`update.rs` 现在只替换单个二进制。Runo 系统上改为整个镜像的 A/B 更新（见 4.5），`install.sh` 不适用。
- **Shell**：`!shell` 打开的是 BusyBox `ash`（`/bin/sh`），已经可用。
- **管理界面的系统状态**：只读 `/proc`、`/sys`，不依赖外部命令，BusyBox 下可用。
- **只读根文件系统**：根分区只读挂载。需要可写的 `/etc` 内容（网络、Wi‑Fi、主机名、时区）放在 `data` 分区，用符号链接或覆盖挂载接入，
  使系统更新不影响用户配置。

## 4. 可靠性设计（尚未全部定案，附建议方案）

### 4.1 远程管理独立于 Agent

建议：设备上分两个进程。

- `runod`（管理守护进程，root 或专用用户，开机即起）：负责配对、登录、PTY、文件传输、日志、状态、配置事务和更新。
  它**不依赖模型**，模型故障、Agent 崩溃时仍然可以连接和修复。
- `runo`（Agent，普通用户）：本地和远程的“Agent 界面”都连到它；它通过本地套接字向 `runod` 申请需要特权的操作。

本地控制台同理：开机由 `runod` 在 tty1 上启动 Agent 界面，Agent 退出或崩溃后由 `runod` 重新拉起管理界面，而不是登录 Shell。

### 4.2 配置修改：记录、验证、回退

建议：所有系统配置修改（网络、服务、用户、API 设置）都经过 `runod` 的事务：

1. 快照当前配置（`/data/runo/config-history/<序号>/`）。
2. 应用修改，运行该修改声明的验证（例如：网关可达、DNS 可解析、服务处于运行状态）。
3. 验证失败自动回退到快照。
4. **可能导致失联的修改（网络、防火墙、`runod` 自身）采用“确认提交”**：应用后启动倒计时（默认 120 秒），
   必须由发起修改的客户端在新地址上重新连上并确认，否则自动回退。
5. 每次事务记录发起者（本地/客户端 ID/Agent）、差异、验证结果，可在状态页查看，可手动回退到任一历史点。

Agent 修改配置也只能走这条路径，不直接写 `/etc`。

### 4.3 断线不终止后台任务

建议：Agent 的回合、后台任务（现有 `tasks.rs`）和 `!shell` 的 PTY 都归 `runod` 持有，而不是归某个连接持有。
断线后它们继续运行；重连时客户端列出未结束和最近结束的任务、会话和 PTY，可接回或查看输出。
PTY 输出保留一段环形缓冲（如 1 MB），重连时补发。

### 4.4 配对窗口与撤销

建议：

- 配对窗口默认关闭。打开方式：本地界面执行“允许配对”，或首次开机（尚无可信客户端）时自动开放；窗口限时 5 分钟，配对成功一次后关闭。
- 配对过程：客户端经局域网发现设备 → 双方交换身份公钥 → 双方显示同一个 6 位确认码（由双方公钥和随机数派生）→
  用户在客户端输入设备密码（首次开机设定）并确认码一致 → 设备登记该客户端公钥。
- 只接受来自局域网地址（RFC 1918、链路本地、ULA）的配对请求；公网入口上配对接口完全不开放。
- 可在本地界面或任一可信客户端上列出并撤销可信客户端；撤销立即断开其现有连接。

### 4.5 系统更新与数据分离（已定：自己实现）

分区布局：

| 分区 | 内容 |
|---|---|
| `boot` (FAT) | 固件、`config.txt`；每个槽的内核、设备树和 `cmdline.txt` 放在 `a/`、`b/` 下，由 `config.txt` 的 `os_prefix` 选择 |
| `rootfs-A` / `rootfs-B` | 只读系统，A/B 轮换 |
| `data` | 人格、记忆、会话、配置、配对信息、日志 |

分区镜像由 Buildroot 的 `genimage` 生成。更新包是一个 `rootfs.ext4` 镜像（压缩并签名），写入非活动槽，使用 Pi 固件的 `tryboot` 只试启动一次；新系统启动后 `runod` 自检通过才将其标记为默认，
否则下次重启自动回到旧槽。人格、记忆、会话在 `data` 分区，系统更新不触碰；数据格式迁移前先做备份（复用 `backup.rs`）。
**已定：由 `runod` 自己实现，不用 RAUC、SWUpdate、Mender**。范围只有四步：

1. **下载并校验**：更新包用发布密钥签名，私钥只在发布流程里；设备内置公钥，校验通过前不写盘。
2. **写入备用分区**：写完回读校验；任何一步失败都不碰当前分区和 `config.txt`，写入中途断电也只损坏备用分区。
3. **试启动**：用 `tryboot` 重启进新系统，仅此一次。
4. **确认**：新系统里 `runod` 自检（`data` 挂载、网络、`runod` 可连、Runo 能启动）通过后，才把新分区写成默认；
   否则重启回到旧分区，并在管理界面报告失败原因。

还必须打开硬件看门狗（Pi 5 的 BCM2712、Pi 4 的 BCM2711，内核驱动都是 `bcm2835_wdt`），由 `runod` 喂狗：新系统卡死时自动重启，从而回到旧分区。没有看门狗，卡死的系统不会自己回退。

### 4.6 自主配置的次数与费用上限（阶段 1 已实现，见第 6 节）

建议：

- 每个配置目标最多重试 3 次，整次自配置最多 N 轮工具调用（默认 50），超出后停下并说明卡在哪里。
- 自配置单独设费用上限（默认 1 美元，可改），复用 `limits.rs` 的单回合 / 每日上限。
- 达到上限时不回退已经验证成功的步骤，只停止后续步骤。

### 4.7 本地中文显示与输入

Linux 内核的 VT 控制台字形数有限，**不能显示中文**；这一项必须单独验证。建议：

- 本地界面不用内核 VT 直接显示，改由用户态渲染，自带 CJK 字体（`wqy-zenhei`）。**已定分两步：**
  1. **阶段 3 第一版镜像用 `fbterm` 验证**：Buildroot 现成，几乎不用开发，先确认 Pi 5 的 HDMI 输出、字体和宽字符显示没问题。
     它停止维护、只有 256 色、没有输入法，所以不作为 1.0 方案。
  2. **1.0 由 Runo 自己经 DRM/KMS 绘制**：一个最小的 Rust 终端，承载 Agent 界面和 `!shell`。
     - 字体渲染用 `wqy-zenhei`。
     - 键盘直接从 evdev 读取。
     - 在这里内置拼音输入法（见下）。
     - 已实现为 `runo console`（`src/console/`）：先经帧缓冲 `/dev/fb0`（DRM 的 fbdev 兼容层）绘制，支持 32 位和 16 位像素；
       字体用 `ab_glyph` 按需读取字形（`fontdue` 会在加载时解析全部 4.5 万个字形，占用两百多 MB 内存）；
       终端解析用 `vte`；最下面一行是输入法栏（中/英、拼写、候选），Ctrl+空格或单按 Shift 切换；
       字体和字号可用 `RUNO_CONSOLE_FONT`、`RUNO_CONSOLE_FONT_SIZE`（每 em 像素数）改；不能绘制时退回直接运行 `runo tui`。
- 输入：先支持远程客户端输入中文（客户端本机输入法）；本地键盘中文输入作为单独验收项，在 Runo 自绘终端里内置（Buildroot 没有 fcitx5）。
- **拼音输入（已定：带词库联想）**：
  - 全拼，支持词组、整句切分（如 `nihao`、`xian` 的多种切法）和简拼（如 `zg` → 中国）。
  - 候选排序见下（随附数据没有词频），用户选过的词提高权重。
  - 联想：上屏一个词后，按用户词库里记下的搭配给出下一个词的候选。
  - **词库来源（已定）：[pinyin-data](https://github.com/mozillazg/pinyin-data) 加 [phrase-pinyin-data](https://github.com/mozillazg/phrase-pinyin-data)**，
    都是 MIT 许可，随镜像附上两者的许可证声明，其他词库不进镜像。
    - pinyin-data：汉字 → 读音（基于 Unicode Unihan）。
    - phrase-pinyin-data：**只取 `pinyin.txt`**（约 4.7 万个词语的读音，0.19.0 版）。
      不取 `cc_cedict.txt`（CC BY‑SA，要求同样许可公开）、`zdic_*.txt`（采自汉典网，来源版权不明），
      也不取合并了它们的 `large_pinyin.txt`。
  - 两份数据都**没有词频，也没有词与词的搭配**，因此：
    - **单字**：候选按读音排序，每个字列在首位的读音视为常用读音、排在前面。
    - **词组和简拼开箱可用**：4.7 万个常用词语的全拼和简拼（如 `zg` → 中国）直接给出。
      没有词频时，同音词组先按词长（整句切分时长词优先）、再按数据里的顺序排列。
    - 之后按用户实际选用的次数调整排序，用户输入过的新词也记入用户词库。
    - **联想**：数据里没有搭配，联想全部来自用户词库（上屏时记下前后搭配），刚装好时为空，用一段时间后才好用。
    - 用户词库存在 `data` 分区，系统更新和重装都会保留。
  - 体积：pinyin-data 不到 1 MB；phrase-pinyin-data 的 `pinyin.txt` 约 1.2 MB（gzip 后约 0.5 MB），预处理成紧凑索引后放进镜像；用户词库设上限（如 10 万条，按最近使用淘汰），常驻内存约 30 MB 以内。
  - 输入法逻辑写成独立的 Rust 模块，管理客户端以后也可以复用。
- 验收：Pi 5 和 Pi 4 接 HDMI，开机界面、Agent 回复、错误提示中的中文全部正常显示；本地键盘可用拼音输入中文。单字和常用词组（全拼、简拼）开箱可用；用户输入过的新词再次输入时能给出，上屏后有联想。

### 4.8 管理协议

建议：自定义帧格式和消息，加密与认证复用成熟实现，不自研密码学。

- 传输：TCP 上的 TLS 1.3（`rustls`，已是依赖），双向认证；双方证书为自签名，按配对时登记的公钥固定（pinning），不依赖 CA。
- 设备身份：Ed25519 密钥，首次开机生成，存于 `data` 分区；设备 ID 由公钥派生，重装系统后若保留 `data` 则不变。
- 应用层：长度前缀帧，多路复用通道（Agent 会话、PTY、文件、日志、控制）。
- 局域网发现（已定：Runo 自带，不装 avahi、mdnsd）：mDNS/DNS-SD `_runo._tcp`，TXT 记录只含设备 ID、协议版本和配对窗口是否开着。
  由 `runod` 用 Rust mDNS 库应答，管理客户端用同一个库发现。网卡增减、IP 变化和名字冲突要单独测试；
  库在这些情况下不可靠时，退回 Buildroot 的 `mdnsd`。
- 公网（已定：必须支持 IPv6）：见 4.9。无论哪种接入方式，都在 TLS 内要求已登记客户端的密钥，加上密码或密钥登录。

阶段 2 的实现（`link/`，`runo-link` 库，设备和客户端共用）：

- 端口 7340。TLS 1.3 only，双方证书自签名、带 Ed25519 公钥；设备端要求客户端证书，客户端按记下的设备公钥核对（配对时先接受任意公钥，配对完成后记下）。
  设备 ID / 客户端 ID 是公钥 SHA‑256 前 10 字节的 Crockford base32，写成 `xxxx-xxxx-xxxx-xxxx`。
- 帧：`[u32 长度][u32 通道][内容]`，内容最多 256 KiB。通道 0 是 JSON 控制消息（请求带 `id`，回复带 `re`，另有事件）；
  其他通道由客户端编号，承载 PTY 字节流和文件内容，空帧表示文件结束。
- 连上后设备先发 `welcome`（设备 ID、名字、协议版本、是否已信任、配对窗口是否开着）；客户端核对 ID 与 TLS 里的公钥一致。
- 确认码：客户端先发自己随机数的承诺（SHA‑256），设备再给出它的随机数，客户端最后公开自己的随机数；
  确认码 = H("runo pairing code", TLS 导出的会话密钥, 设备随机数, 客户端随机数) mod 10⁶。中间人无法让两段 TLS 得出同一个码。
- 设备密码用 PBKDF2‑HMAC‑SHA256（20 万次）存储；配对窗口里连续输错 5 次即关闭窗口。
- 未配对的客户端只能来自局域网地址（RFC 1918、链路本地、ULA、回环），否则设备回一个 `closed` 事件后断开。
- 请求：配对三步、状态、日志、会话列表、打开 / 接回 / 调整大小 / 离开 / 结束 PTY 会话、取文件、送文件、列出和撤销客户端、打开配对窗口。

### 4.9 公网访问：IPv6

已定：**1.0 的公网访问必须支持 IPv6**。设备有全球 IPv6 地址时，客户端直连设备，不经过中继，也不需要端口映射。

- **地址**：
  - 内核按路由器通告自动配置地址（SLAAC）。网络只提供 DHCPv6 时，用 BusyBox 的 `udhcpc6`。
  - 设备对外用稳定地址（`addr_gen_mode` 稳定隐私地址），不用临时地址，以免客户端记下的地址很快失效。
- **地址变化**：家庭宽带的 IPv6 前缀会变，所以地址只是连接线索，客户端以设备 ID 认人（与“设备识别”的设计一致）。
  - **已定：动态域名**。设备地址变化时由 `runod` 更新自己域名的 AAAA 记录；客户端按域名连接，记下的上次地址只作备用。
    - 用户自备域名，`runod` 通过 DNS 服务商的 API 更新记录。先支持 Cloudflare、阿里云 DNS、腾讯云 DNSPod，令牌存 `data` 分区，只有 `runod` 能读。
    - 可以在配对时，或之后由 Agent 帮用户设置。没有域名时，只能在局域网里使用。
    - AAAA 记录是公开的，等于公开了设备地址；因此防火墙和登录要求（下条）是公网访问的前提。
      DNS 被篡改最多导致连不上，冒充不了设备，因为客户端还要核对 TLS 里的设备身份。
  - 客户端连接后，用 TLS 里的设备身份核对是不是那台设备，不只看地址。
- **防火墙**：镜像加入 `nftables`。
  - 入站默认拒绝，只放行 `runod` 的端口，以及 IPv6 运作必需的 ICMPv6（邻居发现、路由通告、Packet Too Big）。
  - 公网来源只允许已登记的客户端登录；配对接口只接受局域网和链路本地来源（与 4.4 一致）。
- **路由器**：很多家用路由器默认拦截 IPv6 入站。管理界面要能测出“从外面连不进来”，并告诉用户去路由器放行哪个端口。
- **没有 IPv6 的情况（已定：1.0 不提供 IPv4 回退）**：设备或客户端所在网络只有 IPv4 时，不能从公网访问，只能在局域网里用。
  管理界面和客户端要明确说明原因：“这个网络没有 IPv6”，而不是笼统地报连接失败。
- **验收**：客户端在另一个只有 IPv6 的网络（如手机流量）上，通过公网连上已配对的设备；未配对客户端从公网连接被拒绝；
  设备前缀变化后，动态域名随之更新，客户端仍能连上。

### 4.10 管理客户端

**已定：Rust 核心 + Flutter 界面**（Flutter 与 Rust 之间用 `flutter_rust_bridge` 连接）。

- **Rust 核心**是独立的库：协议、TLS、mDNS 发现、设备列表、动态域名解析与 IPv6 直连、断线重连、文件传输、密钥存储接口。
  设备端的 `runod` 用同一个库，保证两端协议一致。
- **Flutter 界面**：设备列表、配对（显示确认码）、Agent 对话、状态与日志、文件、终端（`!shell`，用 Flutter 终端组件，如 `xterm.dart`）。
- **密钥存放**：客户端私钥放进系统钥匙串。实现上由 Flutter 端的 `flutter_secure_storage` 存取（Rust 核心只接收 PEM），各平台对应：
  - Android：Keystore；
  - Windows：Credential Manager（DPAPI 保护）；
  - Linux：Secret Service。没有钥匙串（如没有登录密钥环的桌面）时，退回应用目录里只有本用户可读（0600）的 `identity.pem`，与 `runo remote` 相同；
    这不是加密文件，加密文件需要另一个口令，留到以后。
- **各平台要处理的事**：
  - Android：mDNS 需要组播锁。
  - Windows：首次运行时防火墙会弹窗，要放行局域网发现。
- **平台（已定）：Windows、Linux 桌面、Android**。1.0 不做 macOS 和 iOS（没有测试设备）；
  因为界面用 Flutter、核心用 Rust，以后要加这两个平台时不必重写，只需补钥匙串接入和平台权限。
- 不做网页版主客户端：浏览器做不到 mDNS、自定义 TLS 认证和钥匙串。

### 4.11 主界面：结构化事件流

建议：主应用重写时，Agent 界面不再是“终端里画出来的聊天记录”，而是一条**结构化事件流**：
设备只产出事件（消息、卡片、状态），客户端和本地屏幕各用自己的方式原生绘制。终端只留给 `!shell`。

**为什么。** 现在客户端的 Agent 页面是在 `xterm.dart` 里显示 `runo tui` 的 PTY 输出，所有东西都是格子里的字符。实际使用中暴露的问题都出在这里：

| 现象 | 根源 |
|---|---|
| Agent 页面残留旧内容、emoji 和全角标点后错位 | 两端对字符宽度的算法不一致，局部重绘时留下残影 |
| 长文本粘贴不进去，粘贴内容一遇到换行就当回车发出 | 输入就是逐个按键送进 PTY |
| API token 被当成命令敲进 shell，留在屏幕和截图里 | 密钥和普通输入走同一条路 |
| 想看内存、磁盘写入、zram，要手动 `grep /proc/diskstats` 或另装 htop | 设备状态只能靠命令查 |
| Agent 装软件、改配置时，只能从文字里看出进度 | “做一件事”没有结构，`changes.rs` 的记录和撤销在界面上不可见 |

#### 4.11.1 分层

```text
Agent 回合（agent.rs）、system_* 工具、changes.rs、tasks.rs、状态采集
        │  产出 Feed 事件（带序号）
        ▼
runod：按会话保存事件（环形缓冲，见 4.3），记下每张卡片的最新状态
        │  管理协议的 feed 通道（JSON），或本机进程内直接订阅
        ▼
前端：Flutter 客户端（原生控件） / runo console 与 runo tui（终端网格）
```

- 事件和卡片的数据结构放在 `runo-link`（`link/src/feed.rs`），设备和客户端共用，和现有协议一样保证两端一致。
- 会话归 `runod` 持有（4.3）。`runod` 不存原始事件，而是记下每条消息和每张卡片的最新状态（最近 1000 项）及最后改动它的序号。
  断线重连时，客户端带上最后收到的序号，设备把这之后改动过的消息和卡片各发一次完整的最新状态，再接着推新事件；不丢也不重复。
- 本地屏幕不走网络，在进程内订阅同一条事件流。

#### 4.11.2 协议扩展

协议版本不变（`proto::VERSION` 仍为 1，新增的都是追加的请求和字段）；`welcome` 多一个 `feed: true`，表示设备有事件流。
`Kind` 加一种 `feed`；`open` / `attach` 一个 `feed` 会话后，这个通道上的每一帧是一个 JSON 事件，不再是 PTY 字节；`feed` 会话没有终端大小，`cols` / `rows` 可省略。

新请求（走通道 0）：

| 请求 | 作用 |
|---|---|
| `feed_send { session, text, attachments }` | 发一条消息。`attachments` 是此前用 `file_put` 送到设备上的文件路径，拖进输入框的文件就走这条路 |
| `feed_answer { session, card, action, value? }` | 回答一张卡片：确认或取消、选一项、填密钥。`value` 只在密钥卡片里出现，设备不把它写进任何日志 |
| `feed_cancel { session, card? }` | 停止当前回合，或停止某个任务卡片对应的任务 |
| `attach { session, since }` | 重连时从某个序号补发（`since` 为 0 或不给：设备保留的全部） |

事件（`feed` 通道上）：

| 事件 | 字段 | 说明 |
|---|---|---|
| `message` | `seq, id, role, text, append, done` | `role` 为 `you` / `agent` / `note`；回复按片段流式到达（`append` 为真：接在同一 `id` 已有的文字后面；为假：整条替换），`done` 为真表示结束 |
| `card` | `seq, card` | 新卡片或卡片的新状态，按 `card.id` 覆盖，同一张卡片后到的 `rev` 覆盖先到的 |
| `status` | `seq, status` | 设备状态快照，订阅期间每 2 秒一次 |
| `turn` | `seq, busy` | 回合开始或结束，前端据此显示“正在回复”和“停止”按钮 |

没有事件流的设备仍然只给 PTY：新客户端看到 `welcome` 里没有 `feed`，就退回现在的终端方式显示 Agent。
旧客户端连新设备时，照旧用 `Kind::Agent` 的 PTY，`runo tui` 保留这条路直到 1.0 之后。

#### 4.11.3 卡片

所有卡片有同一个外壳：

```rust
pub struct Card {
    pub id: String,        // 设备生成，会话内唯一
    pub rev: u32,          // 每次更新加一
    pub title: String,
    pub state: CardState,  // Waiting（等人回答）| Running | Done | Failed | Cancelled
    pub body: CardBody,
    pub actions: Vec<Action>, // 当前能点的按钮
}

pub struct Action {
    pub id: String,        // feed_answer 里的 action
    pub label: String,
    pub style: Style,      // Primary | Normal | Danger
}
```

`CardBody` 的几种：

| 种类 | 字段 | 用途 |
|---|---|---|
| `Task` | `steps: [{ text, state, detail? }]`，`progress: Option<(done, total)>`，`change: Option<u64>` | Agent 做的每件事：装软件、改配置、下载。`change` 是 `changes.rs` 的记录号；有它，就有“撤销”按钮，点了走 `runo changes revert` 的同一条路径 |
| `Confirm` | `summary`，`effects: [String]`，`risk: Low/High`，`timeout_s` | 危险操作先列出会做什么（改防火墙、重启、系统更新、可能失联的修改），确认了才执行；超时视为取消 |
| `Secret` | `name`，`purpose`，`min_len`，`saved: bool` | 要 API key、token、密码时出这张卡片，不在对话里要。前端用不回显的输入框；设备存进密钥存储，卡片只显示“已保存” |
| `Choice` | `question`，`options: [{ id, label, detail? }]`，`multi` | Agent 需要用户在几项里选 |
| `File` | `path`，`size`，`direction`，`progress` | 文件进出设备 |
| `Checklist` | `items: [{ id, text, state, fix: Option<Action> }]` | 首次开机检查单：网络、时间、设备密码、模型、代理、公网访问、更新。每项打勾或标红；点“处理”就让 Agent 去做这一项 |

**密钥的规则：**
- 密钥的值不进对话历史、上下文、日志和 `changes.rs` 的差异；模型只看到 `{{secret:cloudflare_token}}` 这样的占位符，由工具在执行时替换。
- Agent 发现用户把像密钥的内容直接发进对话（如 `cfut_`、`sk-` 开头）时，从历史里抹掉，并提示用户这条密钥已经暴露、应当作废。

**Agent 怎么产出卡片：**
- `AgentEvent::Text` 变成 `message`；`ToolStart` / `ToolEnd` 变成当前任务卡片里的步骤（工具名换成给人看的说法，原始参数折叠在 `detail` 里）。
- 新增给模型用的工具 `ask_secret`、`ask_confirm`、`ask_choice`：出一张卡片，等用户回答再返回结果。现有 `system_*` 工具里需要确认的操作改走 `ask_confirm`。
- `changes.rs` 每记录一次修改，就把记录号挂到对应的任务卡片上；“确认提交”（4.2 第 4 条）的倒计时也显示在卡片上。

#### 4.11.4 状态

```rust
pub struct Status {
    pub cpu_percent: f32,
    pub temp_c: Option<f32>,
    pub mem: (u64, u64),          // 已用、总量（字节）
    pub swap: Vec<Swap>,          // 含 zram：设备、大小、已用、压缩后
    pub disk_write_bps: u64,      // data 分区的写入速度
    pub addresses: Vec<String>,
    pub proxy: Option<ProxyState>, // 地址、能否连上
    pub pairing_open_until: Option<i64>,
    pub update: Option<String>,    // 正在更新或上次更新的结果
}
```

全部读 `/proc` 和 `/sys`，不调外部命令，BusyBox 下也能用；管理界面“状态”页现有的读取代码（`tui/manage.rs`）挪进 `system.rs` 共用。

#### 4.11.5 Flutter 客户端的画法

- **宽屏三栏**：左边设备列表，中间对话，右边状态栏和“最近修改”（带撤销）。**窄屏**（手机）：对话为主，状态和设备收进顶部的下拉和抽屉。
- **消息**：原生文本控件，Markdown 渲染，代码块可复制；字宽和换行交给 Flutter 的排版，不再有格子对齐问题。
- **卡片**：Material 卡片。`Task` 是步骤列表加进度条；`Confirm` 的“危险”按钮用警示色；`Secret` 是 `obscureText` 的输入框，提交后马上清空本地内容；`Checklist` 每项一行，带图标和“处理”按钮。
- **输入框**：真正的多行文本框，Enter 发送、Shift+Enter 换行；粘贴大段文字原样进入输入框；拖文件或从剪贴板粘贴图片时，先用 `file_put` 送到设备，再作为 `attachments` 发出。
- **`!shell`**：照旧用终端组件，只有它是 PTY。

#### 4.11.6 本地屏幕的画法（runo console / runo tui）

- 同一条事件流，画在终端网格上：消息按宽度折行，卡片画成带边框的块，按钮是 `[ 确认 ]` 这样的文字按钮。
- **焦点**：Tab / Shift+Tab 在输入框和等人回答的卡片之间移动，Enter 按下当前按钮，Esc 回到输入框。
- **密钥卡片**：输入时只显示 `•`，内容不经过输入法栏的候选（拼音输入在这里自动关掉）。
- **状态**：顶部一行，放 CPU、温度、内存、zram、写入速度、代理；屏幕宽度不够时只留最要紧的几项。
- **字宽**：网格里每个字符的宽度只用一个函数算（`unicode-width`，emoji 和全角标点按 2），绘制和光标定位共用它；被宽字符盖住的格子显式清空，不靠上一帧的内容。

#### 4.11.7 验收

- 断线后重连，对话、进行中的任务卡片和状态都接得上，不丢也不重复。
- 粘贴一段几千字、带换行的文本，作为一条消息完整发出。
- Agent 要 API key 时出密钥卡片；之后在对话历史、`runod.log`、`changes.rs` 的记录里都找不到这个值。
- Agent 装一个软件：任务卡片显示每一步，完成后点“撤销”能恢复原状。
- 改防火墙前出确认卡片，取消后什么都没改。
- 客户端和本地屏幕显示同一段中英混排、带 emoji 的回复，都没有错位和残影。
- 没有事件流的设备（`welcome` 里没有 `feed`），新客户端仍能用终端方式进入 Agent。

## 5. 开机与界面状态

```text
开机 → runod 启动 → tty1 上的 Agent 界面
  ├─ 无网络          → 管理界面：有线自动 DHCP；可填写 Wi‑Fi（不需要模型）
  ├─ 未配置模型      → 填写模型连接信息（提供方、API 密钥或登录）
  ├─ 模型可用        → Agent 对话；首次进入时开始自配置
  └─ 模型不可用      → 管理界面：状态、日志、网络、重新填写模型信息、配对；!shell 可用
任何状态下输入 !shell → Shell；exit 后回到原状态
```

注意：验收流程里“联网”在“填入模型信息”之前，因此 Wi‑Fi 配置必须在没有模型时也能完成——
可以在本地管理界面填写，或在烧录时写进 `boot` 分区（如 `runo-wifi.txt`，首次开机读取后删除）。

## 6. 开发顺序

| 阶段 | 内容 | 与现有代码的关系 |
|---|---|---|
| 1 | 在树莓派现有 Linux（Raspberry Pi OS / Alpine，Pi 5）上做通 Agent 自配置和 `!shell`；只用 BusyBox 也有的命令，避免依赖 GNU 工具或包管理器 | `runo tui` 已有全屏 Agent 界面和首次引导（`onboard.rs`）；本阶段已加入 `!shell`、管理界面、自配置（`system_*` 工具、`system-setup` skill、`/setup`）、配置事务（`changes.rs`、`runo changes`）和自配置上限（`setup_run.rs`）|
| 2 | 局域网发现与可信配对 | 新增 `runod`、管理协议、共用的 Rust 核心库、Flutter 管理客户端 |
| 3 | 开机直达 Agent 的镜像（Pi 5、Pi 4） | Buildroot 2025.02.x 外部树（`buildroot/`）、BusyBox init 与 `inittab` 接管 tty1、`service.rs` 支持 BusyBox init、A/B 分区、中文显示、CI 出镜像 |
| 4 | 公网连接、更新与恢复 | `runod` 自己实现 A/B 更新（签名、`tryboot`、看门狗），取代 `update.rs` 的单文件替换；IPv6 公网接入与防火墙；Runo 自绘终端（含拼音词库联想）取代 `fbterm` |
| 5 | 主界面重写：结构化事件流（4.11） | `runo-link` 加 `feed.rs` 和协议版本 2；`agent.rs` 的 `AgentEvent`、`changes.rs`、`tasks.rs` 接到事件流；Flutter 客户端用原生控件画 Agent 页面；`runo console` / `runo tui` 改画同一条事件流 |

### 阶段 1 当前进度

- [x] `runo tui` 中输入 `!shell` 打开 Shell（`$SHELL`，没有则 `/bin/sh`），`exit` 后回到 Agent。
  不经过模型，模型不可用时同样有效；Shell 里 Ctrl+C 只作用于 Shell，不会结束 Runo；Shell 的环境变量带 `RUNO_SHELL=1`。
- [x] 模型不可用时的管理界面（`runo tui` 里的“管理”视图，`/manage` 也能打开）：
  - 何时出现：启动时做一次不花钱的检查（OpenRouter 的 `/key`，或 ChatGPT 登录状态）。检查发现没网、密钥被拒、没有额度、服务商故障或设置缺失时，自动打开；
    一条消息因同类原因失败时，也会自动打开。请求本身有问题（如上下文过长）不算模型不可用。
  - 菜单：重新检查模型（让模型回答一个词）、修改模型连接（走首次引导的步骤，保存后窗口带着新设置重新打开，Gateway 在运行则重启）、
    系统状态（设备型号、主机名、地址、运行时间、负载、内存、磁盘、温度、Gateway）、日志（`tui.log` 和 Gateway 日志的末尾）、Shell、返回对话。
  - 都不需要模型；打字会进入输入框，所以 `!shell` 在这里同样有效。
- [x] 自配置：环境检查 → 计划 → 逐项应用并验证 → 汇报；缺信息时询问。
  - 工具：`system_inspect`（板子、系统、init、是否 root、主机名、时区、时钟、网卡、路由、DNS、已配置的 Wi‑Fi 名称（不含密码）、
    现有命令、服务）、`system_service`（通过 systemd、OpenRC 或 `/etc/init.d` 脚本做 status/start/stop/restart/enable/disable）、
    `system_check`（域名解析、TCP 端口、HTTP、进程）。查看和验证不需要批准；改动服务和 `shell` 一样需要批准，`tools.shell = "deny"` 时都不提供。
    写配置文件用现有的 `read_file`、`write_file`、`edit_file`。
  - 步骤写在内置 skill `system-setup` 里：先检查，不是 root 就停下并说明；列出简短计划和要问的问题（一次问完），不再单独确认计划；
    逐个目标修改：先读文件、用 `system_change` 一次写入、重启、验证（失败自动恢复）；
    失败先查原因（`logread`、`journalctl` 或 `/var/log/messages`），每个目标最多 3 次，仍失败就继续下一个；最后逐项汇报，
    并存一条不含密码的记忆。
  - 目标：网络与 Wi‑Fi（不改当前默认路由所在的网卡，除非用户要求）、时间与时区、Runo Gateway。主机名、用户、SSH 等不动。
  - 触发：`runo tui` 里输入 `/setup`；在树莓派上第一次打开窗口、模型可用时自动开始一次（状态目录里的 `system-setup-started` 记录已开始过）。
    模型不可用时等到恢复后再开始。
  - 每次修改都走下面的配置事务（`system_change`）；次数和费用上限由程序强制，见下面的 4.6 一项。
- [x] 配置事务与回退（4.2）。阶段 1 还没有 `runod`，事务先在 Runo 进程里做（`src/changes.rs`）：
  - Agent 改系统配置只能用 `system_change`；工具开放时，`write_file`、`edit_file` 拒绝写 `/etc`。
  - 一次修改：快照每个文件（内容、属主、权限）到 `<状态目录>/config-history/<编号>/`，记为 pending →
    通过临时文件加改名整体写入（符号链接写到目标）→ 按顺序重启服务 → 运行验证，最多等 `settle_secs`（默认 20 秒，最多 120 秒）。
  - 写入、重启或验证失败：恢复文件、再重启服务、再验证一次，记为 rolled back。
  - 修改进行中崩溃或断电：下次启动（`runo tui`、`serve`、`changes` 等）时自动恢复；用文件锁区分"进行中"和"被打断"，同一时间只跑一个修改。
  - 记录发起者、时间、文件、服务、验证和经过。查看与手动回退：`runo changes [list|show <编号>|revert <编号> [--force]]`
    （不需要模型和配置），管理界面的"配置修改记录"，Agent 的 `system_history`、`system_revert`。
    之后又被改过的文件，回退时默认拒绝。
  - 还没做的"确认提交"（客户端在新地址上重连确认，否则倒计时回退）要等阶段 2 有了 `runod` 和客户端；
    阶段 1 里，网络类修改靠验证（如能连上模型服务）在失联时自动回退。
- [x] 自配置的次数与费用上限（4.6）（`src/setup_run.rs`）：
  - 一次 `/setup` 是一个跨多个回合的"配置过程"，记在 `<状态目录>/system-setup-run.json`；同一会话里的回合都计入。
    Agent 调用 `system_setup_done`、达到上限、下一次 `/setup` 或 6 小时没有回合时结束。
  - 上限在 `[limits]`：`setup_tool_calls`（默认 50 次工具调用）、`setup_usd`（默认 1 美元）、`setup_attempts`（每个目标默认 3 次失败）。
    达到前两者时，回合在下一步之前停下并说明；最后的回答只计数、不截断。`limits.turn_usd`、每日和每月上限照常生效。
  - `system_change` 带 `target`（network、time、gateway……）；配置过程中，同一目标被回退满 3 次后，再改这个目标会被拒绝，Agent 继续下一项并在汇报里说明。
  - 已经验证成功的修改不回退，只停止后面的步骤。

### 阶段 2 当前进度

- [x] 共用库 `runo-link`（`link/`）：身份、TLS、帧、协议、确认码、mDNS 发现与通告、客户端连接和已配对设备列表，见 4.8。
- [x] `runod`：阶段 2 先作为 `runo daemon` 子命令，与 Agent 同一个二进制。
  - `runo daemon run`（默认）：监听 7340，通告 `_runo._tcp`（主机名 `runo-<ID 前 8 位>.local`），状态在 `<状态目录>/runod/`（0700）：
    `identity.pem`、`clients.json`、`password`、本地控制套接字 `control.sock`。
  - `runo daemon password` 设设备密码（至少 8 个字符）；`pair` 打开配对窗口并显示确认码，Ctrl+C 关闭；`clients`、`revoke <ID>`、`status`。
  - `runo daemon install` / `uninstall`：systemd、OpenRC 或 `/etc/init.d` 脚本，日志在 `/var/log/runod.log`。
  - 配对窗口（4.4）：5 分钟；设了密码且还没有客户端时自动开着；配对成功一次或密码错 5 次后关闭。撤销客户端立即断开它的连接。
- [x] 断线不终止（4.3 的 PTY 部分）：Agent（`runo tui`）和 Shell 运行在 `runod` 持有的 PTY 里，断线后继续；
  每个会话保留 1 MiB 输出，接回时补发；结束的会话保留 1 小时可查看。多个客户端可同时查看同一会话。
  会话的环境带 `RUNO_REMOTE=1`。远程打开 Agent 时，首次引导（填写模型连接信息）同样可以完成。
- [x] 本地管理界面加入“配对与客户端”：显示配对窗口和正在配对的确认码，列出可信客户端，可以打开配对窗口。
- [x] 命令行客户端 `runo remote`：`discover`、`pair <地址|ID>`、`devices`、`forget`、`connect`（Agent）、`shell`、`sessions`、`attach`、`kill`、
  `status`、`logs`、`get`、`put`、`clients`、`revoke`、`open-pairing`。终端里 Ctrl+] 离开会话（会话继续运行）。
  连接顺序：记下的地址 → 域名 → 局域网 mDNS。
- [x] Flutter 客户端（`client/`，Windows、Linux、Android）：设备列表（已配对 + 本网络中）、按地址配对、确认码与密码、
  Agent 与 Shell 终端（`xterm.dart`，Android 上带 Esc / Tab / Ctrl / 方向键一行）、会话、状态、日志、文件收发、客户端与撤销、打开配对窗口。
  CI（`.github/workflows/client.yml`）构建三个平台。
- [x] 网络类修改的“确认提交”（4.2 第 4 点）：
  - 在客户端打开的会话里（`RUNO_REMOTE=1`），Agent 的 `system_change` 若涉及网络、防火墙或 `runod`
    （服务名或路径含 network、wpa_supplicant、dhcpcd、resolv、nftables、iptables、firewall、runod 等），
    验证通过后进入“等待确认”，120 秒内没人确认就由 `runod` 撤回（每秒检查一次；设备重启后照样到期撤回）。
  - 确认要能连上设备才做得到，这本身就证明修改没有让客户端失联：Flutter 客户端在设备页和终端页顶部显示“保留 / 撤回”；
    命令行用 `runo remote confirm <设备> [--reject]`，设备上用 `runo changes confirm|reject`。协议新增 `unconfirmed`、`confirm`。
  - 有修改在等待确认时，新的修改一律拒绝，所以撤回不会覆盖之后的修改。
  - 与 4.2 原方案的差别：配置事务仍在 Runo 进程里执行，`runod` 只负责到期撤回和接收确认；任何已信任的客户端都能确认，不限于发起者。
- [x] 公网登录时额外要求密码（4.8 最后一条）：已信任的客户端从局域网之外连来时，`welcome` 带 `login: true`，
  它要先用 `login` 请求给出设备密码，之前其他请求和通道一律拒绝；密码不在客户端保存，每次连接询问。
  同一客户端连续输错 5 次锁定 15 分钟，每次输错延迟 1 秒。局域网上只用已登记的客户端密钥。
  `runo remote` 在终端里询问；Flutter 客户端弹出密码框，并且也按记下的域名连接。

### 阶段 3 当前进度

- [x] Buildroot 外部树 `buildroot/`（说明见 `buildroot/README.md`）：`runo_rpi5_defconfig`、`runo_rpi4_defconfig`，Bootlin aarch64 musl 工具链，
  树莓派内核（6.12）与固件，BusyBox init，`runo` 包安装 CI 产出的静态二进制。
- [x] 分区（MBR，磁盘签名固定为 `52554e4f`，所以 `cmdline.txt` 用 `root=PARTUUID=52554e4f-02`，SD、U 盘、NVMe 都能启动）：
  1 `boot` FAT32 256 MiB（固件在根目录，`config.txt` 用 `os_prefix=a/` 指向槽 A 的内核、设备树、`cmdline.txt`）；
  2 `rootfs-A` ext4 只读；3 `rootfs-B` 预留；4 `data` ext4，首次开机由 `S00data` 扩展到整盘。
  4.5 的 `tryboot` 用 `tryboot.txt` 写 `os_prefix=b/` 即可，不需要 `autoboot.txt`。
- [x] 只读根文件系统：`/etc/hostname`、`timezone`、`localtime`、`wpa_supplicant.conf`、`/root`、`/var/log` 链接到 `/data`；状态目录 `/data/runo`。
- [x] 服务：`S00data`（挂载、扩展 `data`）、`S40network`（有线 DHCP、稳定 IPv6 地址、Wi‑Fi；首次开机可从 boot 分区的 `runo-wifi.txt` 导入）、
  `S48time`（时区、RTC 或上次保存的时间、`ntpd`）、`S50runod`、`S60gateway`（`runo service install` 在镜像上即启用它）。
- [x] Runo 识别自己运行在镜像上（`/etc/runo-system`）：状态目录默认 `/data/runo`；`runo daemon install` 提示已内置；
  不提供安装浏览器；`system_inspect` 和 `system-setup` skill 说明镜像上 Wi‑Fi 与时区的位置。
- [x] tty1 上首次启动时询问设备密码（可跳过）；管理界面加入“设备密码”。
- [x] CI（`.github/workflows/image.yml`）构建 Pi 5 和 Pi 4 镜像，产出 `runo-<板>.img.xz`。
- [x] 电脑上的烧录工具 `runo-flash`（`flash/`，x86_64 的 Linux 和 Windows，终端界面）：选型号 → 选镜像（从 GitHub 发布下载并核对
  `SHA256SUMS-<板>`，或本地的 `.img.xz`、GitHub Actions 下载的 `.zip`、`.img`）→ 可选填写 Wi‑Fi（写入 boot 分区的 `runo-wifi.txt`）→
  设备密码（默认随机生成，可改可留空；写入 boot 分区的 `runo-password.txt`，无屏幕的树莓派首次开机即可配对）→
  选 SD 卡（只列可移动磁盘，不列系统盘）→ 输入 `yes` 确认 → 写入、读回校验。分区表最后写，写完之前系统不会挂载半张卡；
  SD 卡比镜像小时在写入前拒绝。需要 root（Linux 自动用 `sudo` 重新运行）或管理员（Windows 弹出 UAC）。
  CI 和发布流程产出 `runo-flash-x86_64-linux` 和 `runo-flash-x86_64-windows.exe`。
- [ ] 真机验收（第 7 节）：HDMI 中文显示、Wi‑Fi、首次扩展分区、配对。

### 阶段 4 当前进度

- [x] 公网登录密码（见阶段 2 进度最后一条）。
- [x] 防火墙：`/etc/nftables.conf`（`S35firewall` 在网络之前加载）。入站默认拒绝，只放行已建立的连接、回环、
  IPv4/IPv6 必需的 ICMP（邻居发现、路由通告、Packet Too Big 等）、DHCP/DHCPv6 应答、mDNS 和 `runod` 的 7340；转发一律拒绝。
  自己的规则放在 `/data/etc/nftables.d/*.nft`；这些规则加载失败时退回内置规则。
- [x] 动态域名：`runo daemon ddns set <cloudflare|aliyun|dnspod> <域名> [--zone <注册域名>]`，在终端里询问凭据
  （Cloudflare 的 API 令牌；阿里云的 AccessKey ID 和 Secret；DNSPod 的 SecretId 和 SecretKey），服务商接受后才保存到
  `<状态目录>/runod/ddns.json`（0600）。`runod` 每分钟看一次地址（全球单播、非临时、非 ULA，有线优先），变了就更新 AAAA 记录，
  失败 5 分钟后重试。`runo daemon ddns`、管理界面“状态”里的“公网访问”显示域名、地址、上次更新或失败原因；
  没有公网 IPv6 时明确说明“这个网络没有给设备分配公网 IPv6 地址”。
  - `welcome` 带上设备的域名，客户端（`runo remote`、Flutter）配对和每次连接时记下，之后按“记下的地址 → 域名 → 局域网 mDNS”连接。
  - 阿里云（签名 1.0，HMAC‑SHA1）和腾讯云（TC3‑HMAC‑SHA256）的签名用两家文档里的示例核对过；三家的接口还没有用真实账号测过。
- [x] 从外面连不进来时的提示：`runo remote` 和客户端连接失败时说明要检查什么——设备有域名时，域名要指向设备的公网 IPv6、当前网络要有 IPv6、路由器要放行到 7340 的 IPv6 入站；没有域名时提示先设置 `runo daemon ddns`。设备自己从外面探测连通需要一个外部探测点，不在 1.0 范围。
- [x] A/B 更新、看门狗、恢复（4.5）。
  - 更新包 `runo-<板子>.update` 是一个 tar：`manifest.json`（板子、版本、各文件和解压后根文件系统的大小与 SHA‑256）、
    `manifest.sig`（发布密钥的 Ed25519 签名）、`rootfs.ext4.gz`、`boot.tar.gz`（内核、设备树、overlays）。
    镜像工作流用仓库密钥 `RUNO_UPDATE_SIGNING_KEY`（PEM 格式的 Ed25519 私钥）签名，并把公钥写进镜像的 `/etc/runo/update-keys`；
    打标签发布时，镜像和更新包一起附到 Release。没有这个密钥构建的镜像不接受系统更新。
  - Runo 系统上 `runo update [文件或 https 地址] [--force]` 走系统更新（不带参数就取最新 Release 里本板子的更新包）：
    先完整校验签名和哈希、不写盘；再把根文件系统写入备用分区并回读核对，启动文件写进 `boot` 分区的 `<槽>.new/` 后改名，
    `cmdline.txt` 改指新分区；最后写 `tryboot.txt`，以 `tryboot` 重启一次。
  - 新系统里 `runod` 启动后自检（`/data` 挂载、状态目录在、`runo` 能运行、更新前有网的话 90 秒内要有默认路由），
    通过才把 `config.txt` 的 `os_prefix` 改成新槽；不通过就重启回旧槽。结果记在状态目录，管理界面“状态”的“系统”一行显示。
  - `runod` 打开 `/dev/watchdog`，由单独线程每 5 秒喂一次，正常停止时“魔术关闭”；系统卡死会自动重启，试启动中的新系统卡死即回到旧槽。
  - 恢复：在任何电脑上往 `boot` 分区放一个 `runo-reset.txt`，内容 `pairing` 忘掉已配对客户端和设备密码，
    `all` 清空 `/data` 回到首次启动的状态；启动时执行一次后删除该文件。
  - 固件（Pi 4 的 `start4.elf` 等）不随更新包更换；Pi 5 的引导程序在 EEPROM 里，用官方工具更新。
- [x] Runo 自绘终端与拼音输入（4.7）。
  - `runo console`：直接画到 `/dev/fb0`（32/16 位），文泉驿正黑等宽字体（按需读取字形，约 18 MB），evdev 读键盘（美式布局、
    重复、CapsLock 灯），里面在 PTY 中运行 `runo tui`；只重画变了的行，空闲时不占 CPU。底部一行显示中/英、正在输入的拼音和候选。
  - 拼音：`Ctrl+空格` 或单按 `Shift` 切换；数据来自 pinyin-data 0.15.0 和 phrase-pinyin-data 0.19.0（压缩后约 0.7 MB，编进程序）。
    全拼、简拼（`zg` → 中国）、整句切分；选过的词和接在后面的词记在 `<状态目录>/pinyin-words.tsv`，再次输入时排前并有联想。
  - 已知不足：phrase-pinyin-data 主要收多音词，不少常用词（如 西安、公司）开箱不在词库，要输入一次后才排在前面；
    帧缓冲、键盘和速度还没有在真机上测过。

### 阶段 5 当前进度

- [x] 数据结构（`link/src/feed.rs`）：`Body`（`message` / `card` / `status` / `turn`）、带序号的 `Event`、`Input`（`send` / `answer` / `cancel`），
  卡片外壳和六种 `CardBody`，`Status`；`Log` 记各消息和卡片的最新状态，`since(seq)` 给出某个序号之后改动过的部分。
- [x] 协议：`Kind::Feed`、`feed_send` / `feed_answer` / `feed_cancel`、`attach` 带 `since`、`welcome.feed`；协议版本不变，旧客户端照旧可用。
  `runo-link` 客户端有 `attach_feed` 和 `feed`。
- [x] `runo feed`（`src/feed.rs`）：没有屏幕的 Agent，标准输入一行一个 `Input`，标准输出一行一个 `Body`（原来的标准输出改指标准错误，
  工具和库打印的东西不会混进事件流）。开始时发会话里已有的消息；回复按片段流式发出；一个回合用到工具时出一张任务卡片，每个工具一步，
  参数和输出折叠在 `detail`（各留 4 KiB）；需要确认的操作出确认卡片（5 分钟没回答算取消）；停止、出错各有一条 `note`。
  回合进行中发来的消息排队，回合结束后依次处理。
- [x] `runod`（`src/daemon/feed.rs`）：`feed` 会话起一个 `runo feed` 子进程（错误写到 `<状态目录>/feed.log`），给它的输出编号、
  保存并转发；有人看时每 2 秒推一次状态：CPU、温度、内存、交换区（zram 带压缩后大小）、状态目录所在分区的写入速度、地址、
  `network.proxy` 及能否连上（30 秒查一次）、配对窗口、系统更新。
- [x] 测试：断线重连只补发错过的部分、新客户端拿到整段对话、子进程结束后通道收到 `exit`。
- [ ] `ask_secret`、`ask_choice` 工具和密钥卡片；密钥不进历史和日志、发现明文密钥时抹掉并提示。
- [ ] 任务卡片挂上 `changes.rs` 的记录号和“撤销”；确认提交的倒计时；首次开机检查单。
- [ ] 工具名换成给人看的说法。
- [ ] Flutter 客户端用原生控件画 Agent 页面（4.11.5）；现在的客户端先不在会话列表里显示 `feed` 会话。
- [ ] `runo console` / `runo tui` 改画同一条事件流（4.11.6）。

## 7. 1.0 验收流程

在**树莓派 5** 和**树莓派 4 B** 上各完成一遍：

1. **烧录**：用 `runo-flash`（或 Raspberry Pi Imager 的“使用自定义镜像”）把镜像写入 SD 卡（Pi 5 另测 NVMe）。
2. **联网**：有线自动获取地址；或在本地界面 / 启动分区填写 Wi‑Fi。
3. **局域网配对**：客户端发现设备，确认码一致，配对成功；公网地址上的配对请求被拒绝。
4. **填入模型连接信息**：本地或在已配对的客户端上填写。
5. **Agent 完成配置**：检查环境、配置、启动服务、验证并汇报；缺信息时只询问缺的那一项；不超过次数和费用上限。
6. **重启后继续使用**：重启后直接回到 Agent，人格、记忆、会话、配对和已配置的服务都在。

附加检查（不在主流程里，但 1.0 需通过）：

- 模型不可用时，本地和远程都还能进管理界面，`!shell` 可用。
- 改错网络配置会在倒计时后自动回退，设备重新可达。
- 断开远程连接后后台任务继续，重连后可看到进度和结果。
- 撤销客户端后它立即无法连接。
- 更新失败（新系统自检不通过）后自动回到旧系统，数据完好。
- 本地中文显示与输入正常（含词库联想）。
- 通过 IPv6 从公网连上已配对设备；未配对客户端从公网被拒绝。

系统更新与看门狗的具体做法：

1. **准备签名密钥**（一次）：`openssl genpkey -algorithm ed25519 -out runo-update.pem`，把文件内容存为仓库密钥
   `RUNO_UPDATE_SIGNING_KEY`（Settings → Secrets and variables → Actions），妥善保管私钥。之后的镜像工作流会多产出
   `runo-<板>.update`；用这次的镜像烧卡，旧镜像不认这个密钥。
2. **看门狗**：设备正常运行时在 `!shell` 里执行 `kill -9 $(pidof runod)`。约 15 秒内设备应自己重启。
   （QEMU 模拟的树莓派一打开看门狗就立即复位，这一项只能在真机上测。）
3. **正常更新**：推一个新提交让工作流再出一个更新包，`runo update <更新包文件或地址>`。重启后进入新槽，
   管理界面“状态”的“系统”一行显示保留了新版本；再重启仍在新槽。
4. **更新失败回退**：联网执行更新，设备重启时拔掉网线（更新前有网、新系统 90 秒内没有默认路由即自检失败），
   或在 `tryboot` 那次启动时于 runod 起来之前断电。下一次启动应回到旧槽，“系统”一行显示放回的原因，`/data` 完好。

## 8. 待定问题

- **4.11 的结构化事件流是否全部放进 1.0**：设备一侧（阶段 5 前几项）已经做了，两个前端改画法是否赶在 1.0 之前。其余设计都已定案。

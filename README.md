# ftool

**Fedora 系统工具** —— 专为 Fedora Linux 设计的多功能命令行工具，提供显卡模式切换与管理、内核签名、系统版本升级、文件哈希计算等功能。

## 概述

ftool 是一个用 Rust 编写的系统级命令行工具，主要面向 **NVIDIA Optimus 双显卡笔记本** 用户。它接管了显卡模式切换的全流程配置管理，同时附带几个实用的系统维护功能。

该项目参考了 [system76-power](https://github.com/pop-os/system76-power) 和 [envycontrol](https://github.com/bayasdev/envycontrol) 的设计，使用纯 Rust 实现，不依赖 Python 运行时。

## 功能特性

### 🎮 显卡模式切换与检测（`-g`）

针对 NVIDIA Optimus 双显卡笔记本，提供四种工作模式：

| 模式 | 说明 | 功耗 | 适用场景 |
|------|------|------|----------|
| `integrated` | 仅使用集成显卡，完全禁用 NVIDIA 驱动 | 最低 | 办公、网页浏览、轻量任务 |
| `compute` | 集成显卡输出画面，NVIDIA 专供 CUDA 计算 | 较低 | 机器学习训练、视频编码 |
| `hybrid` | PRIME 按需渲染，根据负载自动切换 | 适中 | 日常综合使用（默认推荐） |
| `nvidia` | 仅使用 NVIDIA 独立显卡 | 最高 | 游戏、3D 渲染、外接显示器 |

切换模式后需要 **重启系统** 生效（`power` 命令除外）。

#### 高级选项

在切换命令后可附加以下参数：

- `--rtd3 <0-3>` —— 在 Hybrid 模式下启用 RTD3 运行时动态电源管理（默认值：2），级别越高省电效果越好
- `--force-comp` —— 在 Nvidia 模式下启用 ForceCompositionPipeline，解决某些显示器的画面撕裂问题
- `--coolbits <值>` —— 在 Nvidia 模式下启用 Coolbits 超频/调压选项（默认值：28）
- `--dm <gdm|sddm|lightdm>` —— 手动指定显示管理器，仅在 Nvidia 模式下使用
- `--use-nvidia-current` —— 使用 `nvidia-current` 内核模块替代默认的 `nvidia` 模块

示例：

```bash
# 切换到 Hybrid 模式并启用 RTD3 级别 2
sudo ftool -g hybrid --rtd3 2

# 切换到 Nvidia 模式，指定 GDM 并启用 ForceCompositionPipeline
sudo ftool -g nvidia --dm gdm --force-comp
```

#### 电源控制

运行时电源控制允许在不重启系统的情况下立即生效：

```bash
# 查询当前 NVIDIA GPU 电源状态
ftool -g power

# 开启 NVIDIA GPU（唤醒）
sudo ftool -g power on

# 关闭 NVIDIA GPU（休眠）
sudo ftool -g power off

# 自动电源管理（由 udev 规则动态控制）
sudo ftool -g power auto
```

#### 查询与检测命令

```bash
# 查询当前显卡模式
ftool -g query

# 检测系统是否支持 GPU 切换
ftool -g switchable

# 检测外接显示器是否需要 NVIDIA 独显
ftool -g ext-display

# 检测 GPU 是否支持运行时电源管理
ftool -g runtimepm

# 根据硬件推荐默认模式
ftool -g default
```

#### 管理与恢复

```bash
# 还原 ftool 做出的所有 GPU 配置修改（重启生效）
sudo ftool -g reset

# 恢复 SDDM 的默认 Xsetup 脚本
sudo ftool -g reset-sddm
```

### 📦 GPU 缓存管理

ftool 会将检测到的 NVIDIA GPU PCI 总线地址缓存到 `/var/cache/ftool/gpu-cache.json`，避免在后续切换操作中重复检测。

```bash
# 创建显卡缓存（需处于 hybrid 或 compute 模式）
sudo ftool -g cache-create

# 查询显卡缓存内容
ftool -g cache-query

# 删除显卡缓存
sudo ftool -g cache-delete
```

### 🔐 内核签名（`-S`）

使用 `sbsign` 对内核文件进行 Secure Boot 签名，用于签名自定义内核或第三方内核模块。

```bash
# 签名指定内核文件
sudo ftool -S /boot/vmlinuz-6.8.5-200.fc40.x86_64
```

签名流程会自动：
1. 检查 `sbsign` 命令是否可用，不可用时自动通过 `dnf` 安装 `sbsigntools`
2. 验证公私钥文件是否存在（默认路径：`/etc/pki/akmods/`）
3. 幂等性校验 —— 若内核已使用当前公钥签名则跳过
4. 使用临时文件写入签名结果，再原子替换原文件

### ⬆️ 系统版本升级（`-U`）

一键式 Fedora 大版本升级（如 Fedora 40 → 41），基于 `dnf system-upgrade`：

```bash
# 执行系统升级
sudo ftool -U
```

升级流程：
1. 自动检测当前 Fedora 主版本号
2. 确认目标版本（当前版本号 + 1）的软件源可用性
3. 可选：更新当前系统到最新状态
4. 可选：禁用 COPR 仓库防止依赖冲突
5. 下载目标版本的所有软件包
6. 触发离线重启升级

### 🔢 文件哈希计算（`-H`）

计算文件的 MD5、SHA1、SHA256、SHA512 哈希值，以 1MB 块为单位流式读取，支持大文件。

```bash
# 计算 SHA256 哈希
ftool -H sha256 /path/to/file.iso

# 计算 MD5 哈希
ftool -H md5 /path/to/file.iso
```

支持的算法：`md5`、`sha1`、`sha256`、`sha512`

### ⚙️ Shell 补全

ftool 提供了 Bash 和 Fish 的自动补全脚本，位于 `completions/` 目录。

**Bash：**
```bash
source completions/ftool.bash
```

**Fish：**
```bash
source completions/ftool.fish
```

## 安装

### 从源码编译

```bash
# 确保已安装 Rust 工具链（edition 2024 需要 Rust 1.85+）
git clone <repo-url>
cd ftool
cargo build --release
sudo cp target/release/ftool /usr/local/bin/
```

### 系统要求

- **操作系统**：Fedora Linux（主要目标平台）
- **权限**：除 `-H`、`-g query`、`-g switchable`、`-g cache-query`、`-g default`、`-g ext-display`、`-g runtimepm`、`-V`、`-h` 外，其余命令均需 root 权限
- **依赖**：sbsigntools（`-S` 命令自动安装）、nvidia 驱动（显卡切换功能）

## 项目结构

```
ftool/
├── Cargo.toml              # 项目配置与依赖
├── completions/
│   ├── ftool.bash           # Bash 自动补全
│   └── ftool.fish           # Fish 自动补全
└── src/
    ├── main.rs              # 入口与命令行参数解析
    ├── core/
    │   ├── mod.rs           # 核心模块导出
    │   ├── error.rs         # 自定义错误类型
    │   ├── privilege.rs     # root 权限检查
    │   ├── prompter.rs      # 交互式提示工具
    │   └── runner.rs        # 系统命令执行器
    └── features/
        ├── mod.rs           # 功能模块导出
        ├── hasher.rs        # 文件哈希计算
        ├── signer.rs        # 内核签名
        ├── upgrader.rs      # 系统版本升级
        └── gpu/             # 显卡管理（核心功能）
            ├── mod.rs       # GpuController、GpuMode、NvidiaOptions 等
            ├── cli.rs       # 显卡子命令解析与分发
            ├── constants.rs # 路径与配置常量
            ├── detector.rs  # GPU 硬件检测（sysfs）
            ├── generator.rs # 系统配置文件生成
            ├── helper.rs    # 系统配置写入、清理、initramfs 重建
            └── cache.rs     # GPU 缓存管理
```

## 技术细节

### GPU 模式切换原理

- **Integrated**：通过 `modprobe.d` 黑名单禁用所有 NVIDIA 内核模块，通过 udev 规则在 PCI 设备出现时自动移除 NVIDIA 设备
- **Compute**：黑名单仅禁用显示相关模块（nvidia-drm、nvidia-modeset），保留 nvidia 核心驱动和 nvidia-uvm 供 CUDA 使用
- **Hybrid**：允许所有驱动正常加载，配置 modeset=1 和 RTD3 电源管理，通过 udev 规则实现运行时电源管理
- **Nvidia**：将 NVIDIA 设为主 GPU，配置 Xorg OutputClass 或完整 Xorg 配置，根据显示管理器配置 xrandr 初始化脚本

所有模式切换后会自动重建 initramfs（支持 dracut、update-initramfs、rpm-ostree）。

### 配置文件路径

ftool 在 `/etc/` 下生成以下配置文件（`reset` 命令会清理它们）：

| 文件路径 | 用途 |
|----------|------|
| `/etc/modprobe.d/ftool-gpu.conf` | GPU 相关 modprobe 配置 |
| `/etc/modprobe.d/ftool-nvidia-modeset.conf` | NVIDIA DRM modeset 配置 |
| `/etc/udev/rules.d/50-remove-nvidia.rules` | Integrated 模式 udev 规则 |
| `/etc/udev/rules.d/80-nvidia-pm.rules` | NVIDIA 运行时电源管理 udev 规则 |
| `/etc/X11/xorg.conf` / `xorg.conf.d/*.conf` | Xorg 显示配置 |
| `/etc/prime-discrete` | PRIME 离散模式标志 |
| `/var/cache/ftool/gpu-cache.json` | GPU 检测缓存 |

### 驱动兼容性

- 支持 `nvidia`（开源内核模块）和 `nvidia-current`（新版驱动模块）
- 兼容 Fedora Silverblue 等 OSTree 系统的 initramfs 重建
- 自动识别 GDM、SDDM、LightDM 显示管理器并做相应配置
- 支持 S0ix（s2idle）和 S3（deep）两种挂起模式的电源管理

## 许可证

MIT
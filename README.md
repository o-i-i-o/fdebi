# fdebi

用于在 Ubuntu 新版本系统中停用 snap 后，可视化安装本地 deb 包。基于 Rust + GTK4 实现。

fdebi 只负责 deb 安装；deb 的列表、卸载等包管理功能由独立项目 fdebm 实现。
### fdebi-core只用于测试，不进入正式打包！！！

## 架构

自 v0.2 起，依赖求解不再使用纯 Rust 实现的求解器，而是复用系统 `libapt-pkg`（C++ FFI）：依赖求解、版本比较、仓库候选选择、包下载、apt-mark 标记等能力全部来自 apt 自身，与系统包管理器的行为保持一致；Rust 侧仅保留 deb 解析（`ar`/`tar`/`xz`/`zstd`，纯 Rust）与 conffile 冲突预检（md5 比对）。

```
┌──────────────────┐   D-Bus (org.fdebi)   ┌──────────────────────┐
│  fdebi-gtk       │ ────────────────────▶ │  fdebi-daemon        │
│  GTK4 前端       │ ◀──────────────────── │  zbus 守护进程       │
└──────────────────┘   进度/结果信号       └──────────┬───────────┘
                                                      │ Polkit 授权
                                                      ▼
                                           ┌──────────────────────┐
                                           │  fdebi-core (lib)    │
                                           │  解析 → 求解 → 下载  │
                                           │  → dpkg 批装 → 标记  │
                                           └──────────┬───────────┘
                                                      │ C ABI
                                                      ▼
                                           ┌──────────────────────┐
                                           │  apt_shim (C++)      │
                                           │  libapt-pkg          │
                                           └──────────────────────┘
```

各组件职责：

- **fdebi-gtk**：GTK4 图形界面。展示 deb 信息、发起安装、订阅守护进程信号、处理 Polkit 授权与交互式安装；不直接求解或执行 dpkg。
- **fdebi-daemon**：zbus（纯 Rust D-Bus）系统守护进程，持有安装事务状态机（`analyzing → waiting-auth → downloading → installing → done/failed/cancelled`），通过 Polkit 校验权限，广播进度信号。
- **fdebi-core**：安装管线核心库。deb 解析、conffile 预检、依赖求解、下载、dpkg 批量安装、apt 标记。
- **native/apt_shim**：C++ 桥接层，封装 libapt-pkg 会话（apt cache 构建、本地 deb volatile 注册、依赖求解、下载等），以 C ABI 暴露给 Rust，内部用互斥锁保护 libapt-pkg 的进程级状态。

## CLI（开发测试用）

`fdebi-core` 是独立 bin target，与守护进程走同一安装管线，便于命令行验证，不参与 deb 打包：

```bash
fdebi-core -n foo.deb              # 仅分析求解，打印安装方案
fdebi-core foo.deb                 # 分析后确认，再执行安装
fdebi-core -y --keep a.deb b.deb   # 脚本化批量安装（按依赖排序）
```

## 构建要求

- Rust（edition 2024）
- C/C++ 编译器（编译 apt_shim）
- `libapt-pkg-dev` 与 `pkg-config`（构建脚本通过 `pkg-config --cflags apt-pkg` 探测头文件；非 Linux 平台使用 `apt_stub.c` 桩，仅保证 `cargo check` 通过）
- GTK4 开发库（fdebi-gtk）
- `dpkg-deb`（打包）

## 构建与安装

```bash
cargo build --release
```

完整 deb 打包（编译 release、编译翻译、组装 D-Bus / Polkit / desktop / 图标文件并生成 `.deb`）：

```bash
scripts/build-deb.sh
```

## 国际化

界面文案位于 `po/`（gettext），构建 deb 时一并编译安装。

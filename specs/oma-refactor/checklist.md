# oma-apt 重构验收清单

分支：`oma`  
规格：[spec.md](./spec.md)  
任务：[tasks.md](./tasks.md)  
验证记录：[spec.md §8.4](./spec.md#84-实际验证结果2026-11-13ubuntu-2610本机-root--临时测试包)

## 接口与协议

- [x] `fdebi-apt-proto` 定义 `Mode`/`Request`/`Item`/`Stamp`/`Event`/`ItemResult` 等类型，JSON Lines 序列化往返通过单测
- [x] `Stamp::capture/matches` 能识别内容替换与删除
- [x] D-Bus 信号与方法签名保持不变（`fdebi-daemon/src/iface.rs` 仅调用层改写）

## worker

- [x] worker 非 root 运行时拒绝执行（退出码 77）
- [x] 请求文件校验：非普通文件/权限过宽/硬链接/非 root 属主均被拒绝，读取后删除
- [x] `apt_opts` 过滤 `Dir`/`Dir::Cache*` 并输出告警
- [x] analyze 对本地 deb 给出 install/remove 变更与 `installable`
- [x] FIFO 等非普通文件被拒绝且不会阻塞
- [x] install 使用 `AptLock` + `get_archives` + `do_install`，失败时解锁
- [x] install 前复核 `Stamp`，防止请求后被替换
- [x] 同版本本地 deb 触发 reinstall
- [x] remove 前拒绝连带删除反向依赖
- [x] 卸载成功，`dpkg-query` 状态符合 dpkg -r 语义

## daemon

- [x] daemon 通过 PTY 驱动 worker：stdout/PTY 输出转 `OutputLine`，fd 3 JSON 转内部事件（假 worker 单测）
- [x] `SendInput` 写入 PTY，worker 侧可读到（假 worker 单测）
- [x] `Cancel` 以 SIGTERM 终止整个 worker 进程组（假 worker 单测）
- [x] analyze/analyze_batch 改用 worker，保留已安装版本、候选版本、conffile、broken、apt_stale 信息
- [x] install/remove 通过 `install_gate` 串行化，状态机仍为 analyzing→waiting-auth→downloading/installing→done/failed/cancelled
- [x] 安装后健康检查只报告本次新增异常包
- [x] 事务仅对发起者可见/可取消/可应答

## 行为与交互

- [x] debconf `readline` 问题经 PTY 呈现，答案能传递到 postinst（真实测试包验证）
- [x] conffile `keep` 生成 `.dpkg-dist`，`replace` 生成 `.dpkg-old`
- [x] 下载/安装进度事件（stage、percent、package）能够转发
- [ ] 真实网络依赖下载流程在一次性环境中验证（本机未执行）
- [ ] 长事务中手工取消并确认无残留 worker/dpkg（仅单测覆盖）

## 打包与构建

- [x] `cargo check/test/build --release --workspace` 通过
- [x] `scripts/build-deb.sh` 产物包含 `fdebi-apt-worker`
- [x] 包 `Depends` 含 `libapt-pkg6.0 | libapt-pkg6.0t64`
- [ ] 干净容器安装产物并验证 D-Bus/systemd 激活（本机未执行）

## 范围控制

- [x] 旧 solver/CLI 模块保留未删除
- [x] 未提交 `main`，改动仅在 `oma` 分支
- [ ] 与 `main` 旧求解器的分析结果对照及差异记录（本机未执行，差异见 spec §6.1）

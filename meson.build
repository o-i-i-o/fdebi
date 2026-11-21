# fdebi — 本地 .deb 安装器(GTK4 前端 + 特权 D-Bus 守护进程)
#
# meson 是生产构建入口:cargo 负责编译 Rust workspace,meson 负责构建编排与
# 安装布局(二进制、D-Bus/Polkit/桌面/图标、AppStream 元数据、gettext 翻译)。
#
# 纯 GUI 项目:fdebi-core CLI 仅用于开发测试,不构建、不安装;生产只装
# fdebi-gtk 与 fdebi-daemon。
#
# 构建依赖:cargo、C/C++ 编译器、libgtk-4-dev、libapt-pkg-dev、pkg-config、
#           gettext(msgfmt/xgettext/msgmerge)。
# 本地试装:meson setup builddir && meson compile -C builddir && DESTDIR=/tmp/stage meson install -C builddir
project(
  'fdebi',
  version: '0.3.0',
  license: 'GPL-3.0-or-later',
  meson_version: '>= 0.61.0',
)

cargo = find_program('cargo')
cargo_build_sh = find_program('meson/cargo-build.sh')

# 二进制:cargo release 构建两个生产 target(-p 不含 fdebi-core CLI),
# 桥接脚本将产物复制进 @OUTDIR@ 并安装到 bindir。
# build_always_stale:文件级增量判断交给 cargo(mtime 未变时空跑秒级),
# meson 只跟踪 Cargo 清单变更没有意义。
custom_target(
  'cargo-release-build',
  input: [
    'Cargo.toml',
    'Cargo.lock',
    'crates/fdebi-core/Cargo.toml',
    'crates/fdebi-gtk/Cargo.toml',
    'crates/fdebi-daemon/Cargo.toml',
  ],
  output: ['fdebi-gtk', 'fdebi-daemon'],
  command: [
    cargo_build_sh,
    meson.project_source_root(),
    # @OUTDIR@ 会展开为相对路径且 custom command 的 cwd 是源码根,产物会写错位置;
    # 这里必须用绝对路径
    meson.current_build_dir(),
    join_paths(meson.current_build_dir(), 'cargo-target'),
  ],
  build_always_stale: true,
  install: true,
  install_dir: [get_option('bindir'), get_option('bindir')],
)

# 许可证文本(/usr/share/licenses/fdebi/LICENSE;flatpak 与 deb 的文档惯例)
install_data(
  'LICENSE',
  install_dir: join_paths(get_option('datadir'), 'licenses', 'fdebi'),
)

subdir('data')
subdir('po')

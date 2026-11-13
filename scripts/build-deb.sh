#!/usr/bin/env bash
# fdebi .deb 打包脚本:cargo release 构建 → staging 组装 → dpkg-deb --build。
#
# 布局(与 data/ 内各部署文件头部注释一致):
#   usr/local/bin/fdebi-daemon           特权 D-Bus 守护进程
#   usr/local/bin/fdebi-apt-worker       apt 事务 worker(daemon 按需启动)
#   usr/local/bin/fdebi-gtk              GTK4 瘦客户端
#   usr/share/dbus-1/system.d/           org.fdebi.conf(总线策略)
#   usr/share/dbus-1/system-services/    org.fdebi.service(激活文件,Exec 指向 /usr/local/bin)
#   usr/share/polkit-1/actions/          org.fdebi.policy(授权策略)
#   usr/share/locale/*/LC_MESSAGES/      fdebi.mo(po/Makefile install)
#   usr/share/applications/              org.fdebi.gtk.desktop(桌面入口)
#   usr/share/icons/hicolor/scalable/apps/ org.fdebi.gtk.svg(应用图标)
#   usr/share/doc/fdebi/copyright        LICENSE
#
# 不引入 cargo-deb;依赖:cargo、dpkg-deb、msgfmt(gettext)、strip(binutils)。
# 产物:dist/fdebi_<版本>_<架构>.deb
set -euo pipefail

cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
ARCH=$(dpkg --print-architecture)
NAME=fdebi
STAGE="dist/${NAME}_${VERSION}_${ARCH}"
DEB="dist/${NAME}_${VERSION}_${ARCH}.deb"

[[ -n "$VERSION" ]] || { echo "错误:无法从 Cargo.toml 解析版本" >&2; exit 1; }

echo "==> cargo build --release"
cargo build --release --workspace --locked

echo "==> 编译翻译(po/*.po → locale/*/LC_MESSAGES/fdebi.mo)"
make -C po mo

echo "==> 组装 $STAGE"
rm -rf "$STAGE" "$DEB"
install -d "$STAGE/DEBIAN" \
    "$STAGE/usr/local/bin" \
    "$STAGE/usr/share/dbus-1/system.d" \
    "$STAGE/usr/share/dbus-1/system-services" \
    "$STAGE/usr/share/polkit-1/actions" \
    "$STAGE/usr/share/applications" \
    "$STAGE/usr/share/icons/hicolor/scalable/apps" \
    "$STAGE/usr/share/doc/$NAME"

install -m755 target/release/fdebi-daemon      "$STAGE/usr/local/bin/"
install -m755 target/release/fdebi-apt-worker  "$STAGE/usr/local/bin/"
install -m755 target/release/fdebi-gtk         "$STAGE/usr/local/bin/"
install -m644 data/dbus/org.fdebi.conf         "$STAGE/usr/share/dbus-1/system.d/"
install -m644 data/dbus/org.fdebi.service      "$STAGE/usr/share/dbus-1/system-services/"
install -m644 data/polkit/org.fdebi.policy     "$STAGE/usr/share/polkit-1/actions/"
install -m644 data/applications/org.fdebi.gtk.desktop "$STAGE/usr/share/applications/"
install -m644 data/icons/org.fdebi.gtk.svg     "$STAGE/usr/share/icons/hicolor/scalable/apps/"
install -m644 LICENSE                          "$STAGE/usr/share/doc/$NAME/copyright"

# 翻译:复用 po/Makefile 的 install 目标(DESTDIR 指向 staging)
make -C po install "DESTDIR=$PWD/$STAGE"

if command -v strip >/dev/null; then
    strip --strip-unneeded \
        "$STAGE/usr/local/bin/fdebi-daemon" \
        "$STAGE/usr/local/bin/fdebi-apt-worker" \
        "$STAGE/usr/local/bin/fdebi-gtk"
fi

INSTALLED_SIZE=$(du -sk "$STAGE" | cut -f1)

# worker 动态链接 libapt-pkg,其运行时包名随发行版变化(Debian/Ubuntu 24.04
# 为 libapt-pkg6.0t64,22.04 为 libapt-pkg6.0,26.10 起为 libapt-pkg7.0)。
# 写死任一名字都会在其它发行版上导致依赖不满足,因此按实际链接的 SONAME
# 反查提供该文件的包。
APT_PKG_LIB=$(ldd target/release/fdebi-apt-worker | awk '/libapt-pkg\.so/{print $3; exit}')
APT_PKG_PKG=$(dpkg-query -S "$APT_PKG_LIB" 2>/dev/null | head -1 | cut -d: -f1 || true)
[[ -n "$APT_PKG_PKG" ]] || { echo "错误:无法确定 $APT_PKG_LIB 所属的软件包" >&2; exit 1; }
echo "==> libapt-pkg 运行时依赖: $APT_PKG_PKG"

cat > "$STAGE/DEBIAN/control" <<EOF
Package: $NAME
Version: $VERSION
Section: admin
Priority: optional
Architecture: $ARCH
Maintainer: fdebi maintainers <fdebi@localhost>
Depends: libgtk-4-1, dbus, polkitd | policykit-1, apt, $APT_PKG_PKG, file, shared-mime-info
Installed-Size: $INSTALLED_SIZE
Description: 本地 .deb 安装器(fdebi)
 特权分离架构:D-Bus 守护进程经 Polkit 授权后启动 apt worker(PTY 交互),
 由 libapt-pkg 求解/下载/安装;GTK4 瘦客户端无特权运行。
EOF

echo "==> dpkg-deb --build"
dpkg-deb --build --root-owner-group "$STAGE" "$DEB"

echo "==> 产物校验"
dpkg-deb --info "$DEB" | sed -n '1,13p'
echo
ls -lh "$DEB"

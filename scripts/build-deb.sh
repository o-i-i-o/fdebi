#!/usr/bin/env bash
# fdebm .deb 打包脚本:cargo release 构建 → staging 组装 → dpkg-deb --build。
#
# 布局(与 data/ 内各部署文件头部注释一致):
#   usr/local/bin/fdebm-daemon           特权 D-Bus 守护进程
#   usr/local/bin/fdebm-gtk              GTK4 瘦客户端
#   usr/share/dbus-1/system.d/           org.fdebm.conf(总线策略)
#   usr/share/dbus-1/system-services/    org.fdebm.service(激活文件,Exec 指向 /usr/local/bin)
#   usr/share/polkit-1/actions/          org.fdebm.policy(授权策略)
#   usr/share/locale/*/LC_MESSAGES/      fdebm.mo(po/Makefile install)
#   usr/share/applications/              org.fdebm.gtk.desktop(桌面入口)
#   usr/share/icons/hicolor/scalable/apps/ org.fdebm.gtk.svg(应用图标)
#   usr/share/doc/fdebm/copyright        LICENSE
#
# 注:crates/fdebm-core 另产出同名测试 CLI(target/release/fdebm-core,
#     命令行直跑 deb 安装管线,验证安装流程用),仅供开发,不装进包。
#
# 不引入 cargo-deb;依赖:cargo、dpkg-deb、msgfmt(gettext)、strip(binutils)。
# 产物:dist/fdebm_<版本>_<架构>.deb
set -euo pipefail

cd "$(dirname "$0")/.."

VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)
ARCH=$(dpkg --print-architecture)
NAME=fdebm
STAGE="dist/${NAME}_${VERSION}_${ARCH}"
DEB="dist/${NAME}_${VERSION}_${ARCH}.deb"

[[ -n "$VERSION" ]] || { echo "错误:无法从 Cargo.toml 解析版本" >&2; exit 1; }

echo "==> cargo build --release"
cargo build --release --workspace --locked

echo "==> 编译翻译(po/*.po → locale/*/LC_MESSAGES/fdebm.mo)"
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

install -m755 target/release/fdebm-daemon      "$STAGE/usr/local/bin/"
install -m755 target/release/fdebm-gtk         "$STAGE/usr/local/bin/"
install -m644 data/dbus/org.fdebm.conf         "$STAGE/usr/share/dbus-1/system.d/"
install -m644 data/dbus/org.fdebm.service      "$STAGE/usr/share/dbus-1/system-services/"
install -m644 data/polkit/org.fdebm.policy     "$STAGE/usr/share/polkit-1/actions/"
install -m644 data/applications/org.fdebm.gtk.desktop "$STAGE/usr/share/applications/"
install -m644 data/icons/org.fdebm.gtk.svg     "$STAGE/usr/share/icons/hicolor/scalable/apps/"
install -m644 LICENSE                          "$STAGE/usr/share/doc/$NAME/copyright"

# 翻译:复用 po/Makefile 的 install 目标(DESTDIR 指向 staging)
make -C po install "DESTDIR=$PWD/$STAGE"

if command -v strip >/dev/null; then
    strip --strip-unneeded \
        "$STAGE/usr/local/bin/fdebm-daemon" \
        "$STAGE/usr/local/bin/fdebm-gtk"
fi

INSTALLED_SIZE=$(du -sk "$STAGE" | cut -f1)
cat > "$STAGE/DEBIAN/control" <<EOF
Package: $NAME
Version: $VERSION
Section: admin
Priority: optional
Architecture: $ARCH
Maintainer: fdebm maintainers <fdebm@localhost>
Depends: libgtk-4-1, dbus, polkitd | policykit-1, apt, shared-mime-info
Installed-Size: $INSTALLED_SIZE
Description: 本地 .deb 安装管理器(fdebm)
 特权分离架构:D-Bus 守护进程执行解析/下载/dpkg 安装,GTK4 瘦客户端
 无特权运行;依赖求解为纯 Rust SAT 实现。
EOF

echo "==> dpkg-deb --build"
dpkg-deb --build --root-owner-group "$STAGE" "$DEB"

echo "==> 产物校验"
dpkg-deb --info "$DEB" | sed -n '1,13p'
echo
ls -lh "$DEB"

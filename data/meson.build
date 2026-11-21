# 部署数据文件:布局与 scripts/build-deb.sh 一致(头部注释互为对照)。
install_data(
  'dbus/org.fdebi.conf',
  install_dir: join_paths(get_option('datadir'), 'dbus-1', 'system.d'),
)
install_data(
  'dbus/org.fdebi.service',
  install_dir: join_paths(get_option('datadir'), 'dbus-1', 'system-services'),
)
install_data(
  'polkit/org.fdebi.policy',
  install_dir: join_paths(get_option('datadir'), 'polkit-1', 'actions'),
)
install_data(
  'applications/org.fdebi.gtk.desktop',
  install_dir: join_paths(get_option('datadir'), 'applications'),
)
install_data(
  'icons/org.fdebi.gtk.svg',
  install_dir: join_paths(get_option('datadir'), 'icons', 'hicolor', 'scalable', 'apps'),
)
install_data(
  'metainfo/org.fdebi.gtk.metainfo.xml',
  install_dir: join_paths(get_option('datadir'), 'metainfo'),
)

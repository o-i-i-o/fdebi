# gettext 翻译:编译 po/*.po → <localedir>/<lang>/LC_MESSAGES/fdebi.mo 并安装。
# 语言清单取自本目录 LINGUAS;pot 目标参数与 po/Makefile 的 XGETTEXT_ARGS 一致
# (源码 msgid 经 gettext()/ngettext()/tf() 标注)。
i18n = import('i18n')

gettext_args = [
  '--language=Rust',
  '--from-code=UTF-8',
  '--add-comments=TRANSLATORS',
  '--keyword=gettext',
  '--keyword=ngettext:1,2',
  '--keyword=tf:1',
  '--package-name=fdebi',
  '--package-version=@0@'.format(meson.project_version()),
  '--copyright-holder=fdebi contributors',
  '--msgid-bugs-address=sebastian.heinlein@web.de',
]

i18n.gettext(
  'fdebi',
  args: gettext_args,
)

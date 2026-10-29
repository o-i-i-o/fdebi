//! GTK4 前端(gtk4 为硬依赖)—— 瘦客户端。
//! 布局：菜单栏(File/Help)+ 左侧信息表(Package/Status)+
//! 右侧动作按钮列 + Notebook(Description/Details)+ 底部缓存进度条与状态栏 +
//! 安装/移除进度对话框(Terminal expander + autoclose)。
//! 文案经 gettext 国际化。
//!
//! 本前端永不 root、不做依赖求解、不执行 dpkg —— 三者全部委托特权守护进程 org.fdebm
//! (zbus 纯 Rust 客户端):
//! - 分析(AnalyzeLocalFile):本地解析 deb 展示元数据/文件树/lintian,
//!   依赖结论异步取自守护进程(带代际号丢弃过期结果)
//! - 安装/移除(InstallLocalFile/RemovePackage):返回事务 id,界面订阅
//!   OutputLine/ProgressChanged/Finished 信号驱动进度对话框
//! - 提权:Polkit 由守护进程侧发起,客户端不再 pkexec 重启自身

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::mpsc as std_mpsc;

use clap::{CommandFactory, FromArgMatches, Parser};
use futures_lite::StreamExt;
use gtk4 as gtk;
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, ButtonsType, FileChooserNative, MessageDialog, MessageType,
    ResponseType, gio, glib,
};

use fdebm_core::flow::{self, PackageInfo};
use fdebm_core::i18n;
use fdebm_core::i18n::{gettext, ngettext, tf};
use fdebm_core::solver::Changes;
use fdebm_core::ui::{AlertKind, Ui};
use fdebm_core::{DebFile, FdebmError};

#[derive(Parser)]
#[command(name = "fdebm-gtk", version)]
pub struct Args {
    /// Set an APT configuration option
    #[arg(short = 'o', long = "option")]
    pub apt_opts: Vec<String>,

    /// 要打开的 .deb 文件
    #[arg(value_name = "FILE")]
    pub file: Option<PathBuf>,
}

/// 构建带翻译文案的命令。derive 属性内的字符串不被 xgettext 提取,
/// about/help 统一在普通调用点经 gettext 注入(调用点即提取点);
/// 字段 doc 注释仅供开发者,用户可见 help 全部由此覆盖
fn build_command() -> clap::Command {
    Args::command()
        .about(gettext(
            "simple tool to view and install deb files - GTK frontend",
        ))
        .mut_arg("apt_opts", |a| {
            a.help(gettext("Set an APT configuration option"))
        })
        .mut_arg("file", |a| a.help(gettext("Debian package to open")))
}

// ---------------------------------------------------------------------------
// Ui trait 对话框实现(主线程)
// ---------------------------------------------------------------------------

/// Ui 回调 → GTK 模态对话框
struct GtkUi {
    parent: gtk::Window,
}

impl Ui for GtkUi {
    fn alert(&self, kind: AlertKind, header: &str, body: Option<&str>) -> Result<(), FdebmError> {
        show_alert(&self.parent, kind, header, body);
        Ok(())
    }

    /// 模态同步语义:嵌套迭代主循环直至用户响应
    fn confirm(&self, header: &str, body: Option<&str>) -> Result<bool, FdebmError> {
        let (tx, rx) = std_mpsc::channel::<bool>();
        let dlg = MessageDialog::builder()
            .transient_for(&self.parent)
            .modal(true)
            .message_type(MessageType::Question)
            .buttons(ButtonsType::OkCancel)
            .text(header)
            .build();
        if let Some(b) = body {
            dlg.set_secondary_text(Some(b));
        }
        dlg.connect_response(move |d, r| {
            let _ = tx.send(r == ResponseType::Ok);
            d.destroy();
        });
        dlg.present();
        loop {
            glib::MainContext::default().iteration(true);
            if let Ok(v) = rx.try_recv() {
                return Ok(v);
            }
        }
    }
}

fn show_alert<W: IsA<gtk::Window>>(parent: &W, kind: AlertKind, header: &str, body: Option<&str>) {
    let ty = match kind {
        AlertKind::Error => MessageType::Error,
        AlertKind::Warning => MessageType::Warning,
        AlertKind::Info => MessageType::Info,
    };
    let text = match body {
        Some(b) => format!("{header}\n\n{b}"),
        None => header.to_string(),
    };
    let dlg = MessageDialog::builder()
        .transient_for(parent)
        .modal(true)
        .message_type(ty)
        .buttons(ButtonsType::Close)
        .text(text)
        .build();
    dlg.connect_response(|d, _| d.destroy());
    dlg.present();
}

// ---------------------------------------------------------------------------
// 守护进程客户端(zbus,系统总线 org.fdebm)
// ---------------------------------------------------------------------------

/// 依赖检查结论(守护进程 AnalyzeLocalFile 返回值)
struct Analysis {
    installable: bool,
    failure: String,
    changes: Changes,
    installed_version: String,
    /// conffile 冲突列表(包名, 路径)
    conflicts: Vec<(String, String)>,
}

#[zbus::proxy(
    interface = "org.fdebm",
    default_service = "org.fdebm",
    default_path = "/org/fdebm"
)]
trait Fdebm {
    /// 只读依赖分析(免 Polkit 授权)
    fn analyze_local_file(
        &self,
        path: &str,
        apt_opts: &[&str],
    ) -> zbus::Result<fdebm_core::AnalyzeResult>;

    /// 安装事务(授权/下载/dpkg 在守护进程内执行)→ 事务 id;
    /// conf_policy: conffile 预检决策("" / "keep" / "replace")
    fn install_local_file(
        &self,
        path: &str,
        apt_opts: &[&str],
        conf_policy: &str,
    ) -> zbus::Result<u32>;

    /// 取消运行中的事务
    fn cancel(&self, id: u32) -> zbus::Result<bool>;

    /// 卸载事务 → 事务 id
    fn remove_package(&self, name: &str) -> zbus::Result<u32>;
}

/// 系统总线连接(事务与信号须在同一连接上:信号按唯一名投递)
async fn daemon() -> Result<zbus::Connection, String> {
    zbus::Connection::system().await.map_err(|e| e.to_string())
}

/// 守护进程侧依赖分析(重活在守护进程线程执行,客户端只等结果)
async fn analyze_via_daemon(opts: &[String], file: &Path) -> Result<Analysis, String> {
    let conn = daemon().await?;
    let client = FdebmProxy::builder(&conn)
        .build()
        .await
        .map_err(|e| e.to_string())?;
    let opts: Vec<&str> = opts.iter().map(String::as_str).collect();
    let (installable, failure, install, remove, unauthenticated, installed_version, conflicts) =
        client
            .analyze_local_file(&file.to_string_lossy(), &opts)
            .await
            .map_err(|e| e.to_string())?;
    Ok(Analysis {
        installable,
        failure,
        changes: Changes {
            install,
            remove,
            unauthenticated,
        },
        installed_version,
        conflicts,
    })
}

// ---------------------------------------------------------------------------
// 主窗口
// ---------------------------------------------------------------------------

/// window_main 的控件集合(Rc 共享给菜单动作)
struct MainWindow {
    window: gtk::ApplicationWindow,
    label_name: gtk::Label,
    label_status: gtk::Label,
    button_install: gtk::Button,
    button_remove: gtk::Button,
    textview_description: gtk::TextView,
    details_version: gtk::Label,
    details_maintainer: gtk::Label,
    details_priority: gtk::Label,
    details_section: gtk::Label,
    details_size: gtk::Label,
    tree_files: gtk::TreeView,
    file_content: gtk::TextView,
    lintian_output: gtk::TextView,
    statusbar: gtk::Statusbar,
    status_ctx: u32,
    apt_opts: Vec<String>,
    current_file: Rc<RefCell<Option<PathBuf>>>,
    changes: Rc<RefCell<Option<Changes>>>,
    /// 分析代际号:文件切换/刷新后丢弃过期的守护进程结论
    analysis_gen: Cell<u64>,
    /// 当前文件的 conffile 冲突(包名, 路径);安装前呈现覆盖决策对话框
    conffiles: Rc<RefCell<Vec<(String, String)>>>,
}

pub fn run() {
    // 首次取串(含 clap about)前完成 locale 与消息目录绑定
    i18n::init();
    let cmd = build_command();
    let matches = cmd.get_matches();
    let args = Args::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    let app = Application::builder()
        .application_id("org.fdebm.gtk")
        .build();
    let action = args.into_action();
    app.connect_activate(move |app| activate(app, action.clone()));
    // 参数已由 clap 解析,不交给 GApplication(位置参数会触发 GFile open 路径)
    app.run_with_args(&["fdebm-gtk"]);
}

/// 启动动作上下文:命令行给的文件与 apt 选项
#[derive(Debug, Clone)]
struct LaunchAction {
    file: Option<PathBuf>,
    apt_opts: Vec<String>,
}

impl Args {
    fn into_action(self) -> LaunchAction {
        LaunchAction {
            file: self.file,
            apt_opts: self.apt_opts,
        }
    }
}

fn activate(app: &Application, action: LaunchAction) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title(gettext("Package Installer").as_str())
        .default_width(550)
        .default_height(400)
        //GTK4 默认不渲染 app menubar
        .show_menubar(true)
        .build();

    let gui = build_window(&window, action.apt_opts);
    setup_actions(app, &gui);

    if let Some(f) = action.file {
        load_deb(&gui, &f);
    }
    window.present();
}

fn build_window(window: &gtk::ApplicationWindow, apt_opts: Vec<String>) -> Rc<MainWindow> {
    // 左侧信息表:Package / Status
    let label_name = gtk::Label::default();
    label_name.set_markup("<b><big>   </big></b>");
    label_name.set_halign(gtk::Align::Start);
    label_name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let label_status = gtk::Label::default();
    label_status.set_halign(gtk::Align::Start);
    label_status.set_valign(gtk::Align::Start);
    label_status.set_wrap(true);
    label_status.set_use_markup(true);

    let grid = gtk::Grid::builder()
        .column_spacing(12)
        .row_spacing(8)
        .vexpand(true)
        .valign(gtk::Align::Start)
        .build();
    let labels = [(0, gettext("Package:")), (1, gettext("Status:"))];
    for (row, text) in labels {
        let l = gtk::Label::new(Some(text.as_str()));
        l.set_halign(gtk::Align::Start);
        l.set_valign(gtk::Align::Start);
        grid.attach(&l, 0, row, 1, 1);
    }
    grid.attach(&label_name, 1, 0, 1, 1);
    grid.attach(&label_status, 1, 1, 1, 1);

    // 右侧动作按钮列:Install / Remove
    let button_install = gtk::Button::with_mnemonic(&gettext("_Install Package"));
    let button_remove = gtk::Button::with_mnemonic(&gettext("_Remove Package"));
    for b in [&button_install, &button_remove] {
        b.set_hexpand(true);
        b.set_sensitive(false);
    }
    button_remove.set_visible(false);
    let btn_col = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .valign(gtk::Align::Start)
        .build();
    btn_col.append(&button_install);
    btn_col.append(&button_remove);

    let hbox_main = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .vexpand(true)
        .build();
    hbox_main.append(&grid);
    hbox_main.append(&btn_col);

    // Notebook:Description 全文 + Details 五项
    let textview_description = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk::WrapMode::Word)
        .build();
    let tab_desc = gtk::Label::new(Some(gettext("Description").as_str()));
    let details_version = gtk::Label::default();
    let details_maintainer = gtk::Label::default();
    let details_priority = gtk::Label::default();
    let details_section = gtk::Label::default();
    let details_size = gtk::Label::default();
    let details_grid = gtk::Grid::builder()
        .column_spacing(12)
        .row_spacing(8)
        .margin_top(12)
        .margin_start(12)
        .margin_end(12)
        .margin_bottom(12)
        .valign(gtk::Align::Start)
        .build();
    let details = [
        (0, gettext("Version:"), &details_version),
        (1, gettext("Maintainer:"), &details_maintainer),
        (2, gettext("Priority:"), &details_priority),
        (3, gettext("Section:"), &details_section),
        (4, gettext("Size:"), &details_size),
    ];
    for (row, text, val) in details {
        let l = gtk::Label::new(Some(text.as_str()));
        l.set_markup(&format!("<b>{text}</b>"));
        l.set_halign(gtk::Align::Start);
        details_grid.attach(&l, 0, row, 1, 1);
        val.set_halign(gtk::Align::Start);
        val.set_selectable(true);
        details_grid.attach(val, 1, row, 1, 1);
    }
    let tab_details = gtk::Label::new(Some(gettext("Details").as_str()));
    let notebook = gtk::Notebook::new();
    notebook.set_vexpand(true);
    notebook.append_page(&scroll(&textview_description), Some(&tab_desc));
    notebook.append_page(&details_grid, Some(&tab_details));

    // Included files:左文件树,右内容文本(等宽)
    let tree_files = gtk::TreeView::new();
    tree_files.set_headers_visible(false);
    let cell = gtk::CellRendererText::new();
    let col = gtk::TreeViewColumn::new();
    col.pack_start(&cell, true);
    col.add_attribute(&cell, "text", 0);
    tree_files.append_column(&col);
    let file_content = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::None)
        .build();
    let files_paned = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .position(220)
        .start_child(&scroll(&tree_files))
        .end_child(&scroll(&file_content))
        .hexpand(true)
        .vexpand(true)
        .build();
    let tab_files = gtk::Label::new(Some(gettext("Included files").as_str()));
    notebook.append_page(&files_paned, Some(&tab_files));

    // Lintian output:等宽文本,后台线程填充
    let lintian_output = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::None)
        .build();
    let tab_lintian = gtk::Label::new(Some(gettext("Lintian output").as_str()));
    notebook.append_page(&scroll(&lintian_output), Some(&tab_lintian));

    // 底部:progressbar_cache + statusbar_main
    let progressbar_cache = gtk::ProgressBar::new();
    progressbar_cache.set_hexpand(true);
    progressbar_cache.set_pulse_step(0.05);
    let statusbar = gtk::Statusbar::new();
    let status_ctx = statusbar.context_id("context_main_window");
    let hbox3 = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .build();
    hbox3.append(&progressbar_cache);
    hbox3.append(&statusbar);

    let vbox = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(12)
        .margin_end(12)
        .build();
    vbox.append(&hbox_main);
    vbox.append(&notebook);
    vbox.append(&hbox3);
    window.set_child(Some(&vbox));

    let gui = Rc::new(MainWindow {
        window: window.clone(),
        label_name,
        label_status,
        button_install,
        button_remove,
        textview_description,
        details_version,
        details_maintainer,
        details_priority,
        details_section,
        details_size,
        tree_files,
        file_content,
        lintian_output,
        statusbar,
        status_ctx,
        apt_opts,
        current_file: Rc::new(RefCell::new(None)),
        changes: Rc::new(RefCell::new(None)),
        analysis_gen: Cell::new(0),
        conffiles: Rc::new(RefCell::new(Vec::new())),
    });

    // Install / Remove 接线
    let gui_btn = gui.clone();
    gui.button_install.connect_clicked(move |_| {
        on_install_clicked(&gui_btn);
    });
    let gui_rm = gui.clone();
    gui.button_remove.connect_clicked(move |_| {
        on_remove_clicked(&gui_rm);
    });
    // Included files:选中成员 → 右侧内容视图
    let gui_sel = gui.clone();
    gui.tree_files
        .selection()
        .connect_changed(move |sel| on_file_selected(&gui_sel, sel));

    gui
}

/// 文件树选中回调:组(0=control,1=data)与成员名决定内容来源
fn on_file_selected(gui: &MainWindow, sel: &gtk::TreeSelection) {
    let Some((model, iter)) = sel.selected() else {
        return;
    };
    let store = model
        .downcast::<gtk::TreeStore>()
        .expect("files tree store");
    let path = store.path(&iter);
    // 顶层为组标题:不处理内容
    if path.depth() < 2 {
        return;
    }
    // 组号取路径首段(gtk4 的 TreePath 无 indices 访问器,经字符串解析)
    let group: u32 = path
        .to_str()
        .unwrap_or_default()
        .split(':')
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(u32::MAX);
    let name: String = store.get_value(&iter, 0).get().unwrap_or_default();
    if name.ends_with('/') {
        gui.file_content
            .buffer()
            .set_text(&gettext("Selection is a directory"));
        return;
    }
    let content = match group {
        0 => DebFile::open(
            gui.current_file
                .borrow()
                .as_deref()
                .unwrap_or(Path::new("")),
        )
        .and_then(|deb| deb.control_content(&name)),
        1 => DebFile::open(
            gui.current_file
                .borrow()
                .as_deref()
                .unwrap_or(Path::new("")),
        )
        .and_then(|deb| deb.data_content(&name)),
        _ => return,
    };
    // 文本原样显示;二进制显示文件类型(core 统一判定,规避内嵌 NUL)
    let text = match content {
        Ok(bytes) if !bytes.is_empty() => flow::content_preview(&bytes),
        Ok(_) => gettext("File content can not be extracted"),
        Err(e) => tf("Error reading file content '{0}'", &[&e.to_string()]),
    };
    gui.file_content.buffer().set_text(&text);
}

/// Included files 页签填充
fn fill_files(gui: &MainWindow, deb: &DebFile) {
    let store = gtk::TreeStore::new(&[glib::Type::STRING]);
    let res = (|| -> Result<(), FdebmError> {
        // 组 0:Package control data;组 1:Upstream data
        let control_header = store.append(None);
        store.set(&control_header, &[(0, &gettext("Package control data"))]);
        for n in deb.control_filelist()? {
            let child = store.append(Some(&control_header));
            store.set(&child, &[(0, &n)]);
        }
        let data_header = store.append(None);
        store.set(&data_header, &[(0, &gettext("Upstream data"))]);
        for n in deb.data_filelist()? {
            let child = store.append(Some(&data_header));
            store.set(&child, &[(0, &n)]);
        }
        Ok(())
    })();
    if let Err(e) = res {
        eprintln!("Exception while reading the filelist: '{e}'");
        store.clear();
        let iter = store.append(None);
        store.set(&iter, &[(0, &gettext("Error reading filelist"))]);
    }
    gui.tree_files.set_model(Some(&store));
    gui.tree_files.expand_all();
}

/// 后台运行 lintian 并流式回填
fn run_lintian(gui: &MainWindow, file: &Path) {
    let buf = gui.lintian_output.buffer();
    if !Path::new("/usr/bin/lintian").exists() {
        buf.set_text(&gettext(
            "No lintian available.\nPlease install using sudo apt-get install lintian",
        ));
        return;
    }
    buf.set_text(&gettext("Running lintian..."));

    enum LintianEvent {
        Line(String),
        Finished(i32),
    }
    let (tx, rx) = async_channel::unbounded::<LintianEvent>();
    let path = file.to_path_buf();
    std::thread::spawn(move || {
        let child = Command::new("/usr/bin/lintian")
            .arg(&path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send_blocking(LintianEvent::Line(tf(
                    "Failed to run lintian: {0}",
                    &[&e.to_string()],
                )));
                let _ = tx.send_blocking(LintianEvent::Finished(-1));
                return;
            }
        };
        let out = child.stdout.take().expect("piped stdout");
        let err = child.stderr.take().expect("piped stderr");
        let tx2 = tx.clone();
        let t = std::thread::spawn(move || {
            use std::io::BufRead;
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                let _ = tx2.send_blocking(LintianEvent::Line(line));
            }
        });
        use std::io::BufRead;
        for line in std::io::BufReader::new(err).lines().map_while(Result::ok) {
            let _ = tx.send_blocking(LintianEvent::Line(line));
        }
        let _ = t.join();
        let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
        let _ = tx.send_blocking(LintianEvent::Finished(code));
    });

    // 回填:文件已切换则丢弃过期输出
    let watch = gui.current_file.clone();
    let watched = file.to_path_buf();
    let view = gui.lintian_output.clone();
    glib::spawn_future_local(async move {
        while let Ok(ev) = rx.recv().await {
            if watch.borrow().as_deref() != Some(watched.as_path()) {
                break;
            }
            match ev {
                LintianEvent::Line(line) => {
                    let buf = view.buffer();
                    let mut end = buf.end_iter();
                    buf.insert(&mut end, &format!("{line}\n"));
                }
                LintianEvent::Finished(code) => {
                    let buf = view.buffer();
                    let mut end = buf.end_iter();
                    buf.insert(
                        &mut end,
                        &format!(
                            "\n{}",
                            tf(
                                "Lintian finished with exit status {0}",
                                &[&code.to_string()]
                            )
                        ),
                    );
                    break;
                }
            }
        }
    });
}

fn setup_actions(app: &Application, gui: &Rc<MainWindow>) {
    // 菜单栏:File: Open…/Refresh/Quit;Help: Keyboard Shortcuts/About
    let file_menu = gio::Menu::new();
    file_menu.append(Some(gettext("_Open…").as_str()), Some("app.open"));
    file_menu.append(Some(gettext("_Refresh").as_str()), Some("app.refresh"));
    file_menu.append(Some(gettext("_Quit").as_str()), Some("app.quit"));
    // Help 保持两项:GTK 弹出菜单只有一项时,内核滚动区按 46px 最小高度撑开,
    // 底部会多出一条空白行(两项及以上不复现)
    let help_menu = gio::Menu::new();
    help_menu.append(
        Some(gettext("_Keyboard Shortcuts").as_str()),
        Some("app.shortcuts"),
    );
    help_menu.append(Some(gettext("_About").as_str()), Some("app.about"));
    let menubar = gio::Menu::new();
    menubar.append_submenu(Some(gettext("_File").as_str()), &file_menu);
    menubar.append_submenu(Some(gettext("_Help").as_str()), &help_menu);
    app.set_menubar(Some(&menubar));

    let open = gio::SimpleAction::new("open", None);
    let gui_open = gui.clone();
    open.connect_activate(move |_, _| {
        let chooser = FileChooserNative::builder()
            .action(gtk::FileChooserAction::Open)
            .transient_for(&gui_open.window)
            .title(gettext("Open Debian package").as_str())
            .build();
        let gui_fc = gui_open.clone();
        chooser.connect_response(move |c, resp| {
            if resp == ResponseType::Accept
                && let Some(f) = c.file().and_then(|f| f.path())
            {
                load_deb(&gui_fc, &f);
            }
        });
        chooser.show();
    });
    app.add_action(&open);

    let refresh = gio::SimpleAction::new("refresh", None);
    let gui_ref = gui.clone();
    refresh.connect_activate(move |_, _| {
        let f = gui_ref.current_file.borrow().clone();
        if let Some(f) = f {
            load_deb(&gui_ref, &f);
        }
    });
    app.add_action(&refresh);

    let quit = gio::SimpleAction::new("quit", None);
    let app_quit = app.clone();
    // 强制退出:主窗口 close() 在存在存活对话框(事务/确认/关于)时不会
    // 终结应用,曾表现为"无法关闭";GApplication::quit 无条件收尾
    quit.connect_activate(move |_, _| app_quit.quit());
    app.add_action(&quit);

    // 快捷键速查(Help 的第二项)
    let shortcuts = gio::SimpleAction::new("shortcuts", None);
    let win_shortcuts = gui.window.clone();
    shortcuts.connect_activate(move |_, _| {
        show_alert(
            &win_shortcuts,
            AlertKind::Info,
            gettext("Keyboard Shortcuts").as_str(),
            Some(gettext("Ctrl+O  Open a Debian package\nCtrl+Q  Quit").as_str()),
        );
    });
    app.add_action(&shortcuts);

    let about = gio::SimpleAction::new("about", None);
    let win_about = gui.window.clone();
    about.connect_activate(move |_, _| {
        // GTK4 的 AboutDialog 直接继承 Window(非 Dialog),自带关闭按钮
        let dlg = gtk::AboutDialog::builder()
            .transient_for(&win_about)
            .modal(true)
            .program_name("fdebm")
            .version(env!("CARGO_PKG_VERSION"))
            .comments(gettext("simple tool to view and install deb files").as_str())
            .build();
        dlg.present();
    });
    app.add_action(&about);

    // 快捷键(Ctrl+O / Ctrl+Q)
    app.set_accels_for_action("app.open", &["<Control>O"]);
    app.set_accels_for_action("app.quit", &["<Control>Q"]);
}

fn scroll(w: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .child(w)
        .hexpand(true)
        .vexpand(true)
        .build()
}

/// 打开并填充窗口:本地解析元数据/文件树/lintian,
/// 依赖结论异步取自守护进程(瘦客户端不读 apt 索引)
fn load_deb(gui: &Rc<MainWindow>, file: &Path) {
    gui.statusbar
        .push(gui.status_ctx, gettext("Loading...").as_str());
    // 新代际:使在途的旧分析结果失效
    let generation = gui.analysis_gen.get() + 1;
    gui.analysis_gen.set(generation);
    *gui.changes.borrow_mut() = None;
    gui.conffiles.borrow_mut().clear();
    *gui.current_file.borrow_mut() = Some(file.to_path_buf());

    // 文件不存在直接提示
    if !file.exists() {
        show_alert(
            &gui.window,
            AlertKind::Error,
            &tf(
                "fdebm error, file not found: {0}",
                &[&file.display().to_string()],
            ),
            None,
        );
        gui.statusbar.pop(gui.status_ctx);
        return;
    }

    let deb = match DebFile::open(file) {
        Ok(d) => d,
        Err(e) => {
            show_alert(
                &gui.window,
                AlertKind::Error,
                &gettext("Could not open the software package"),
                Some(&e.to_string()),
            );
            gui.statusbar.pop(gui.status_ctx);
            return;
        }
    };
    let info = flow::describe_deb(&deb);
    fill_static(gui, &info, &deb);

    // 文件树与 lintian(顺序:文件树 → lintian → 依赖检查)
    fill_files(gui, &deb);
    run_lintian(gui, file);

    // 依赖结论取自守护进程:先复位按钮,呈现检查中状态
    gui.button_install.set_sensitive(false);
    gui.button_remove.hide();
    gui.label_status
        .set_text(&gettext("Checking dependencies..."));
    gui.statusbar.pop(gui.status_ctx);

    let gui_wait = gui.clone();
    let file2 = file.to_path_buf();
    let opts = gui.apt_opts.clone();
    let version = info.version.clone();
    glib::spawn_future_local(async move {
        let analysis = analyze_via_daemon(&opts, &file2).await;
        if gui_wait.analysis_gen.get() != generation {
            return; // 过期结果:期间已打开别的文件
        }
        apply_verdict(&gui_wait, &version, analysis);
    });
}

/// 呈现依赖检查结论(守护进程分析结果)
fn apply_verdict(gui: &Rc<MainWindow>, version: &str, analysis: Result<Analysis, String>) {
    let analysis = match analysis {
        Ok(a) => a,
        Err(e) => {
            // 守护进程把缓存打开失败与求解错误折叠为 Err;
            show_alert(
                &gui.window,
                AlertKind::Error,
                &gettext("Dependency check failed"),
                Some(&e),
            );
            gui.label_status.set_markup(&tf(
                "<span foreground=\"red\" weight=\"bold\">Error: {0}</span>",
                &[&glib::markup_escape_text(&e)],
            ));
            return;
        }
    };

    *gui.conffiles.borrow_mut() = analysis.conflicts;

    if !analysis.installable {
        // 检查失败:红色错误 + install 禁用
        gui.label_status.set_markup(&tf(
            "<span foreground=\"red\" weight=\"bold\">Error: {0}</span>",
            &[&glib::markup_escape_text(&analysis.failure)],
        ));
        gui.button_install.set_sensitive(false);
        gui.button_remove.hide();
        return;
    }

    let changes = analysis.changes;
    let (inst, rm) = (&changes.install, &changes.remove);
    // 依赖变更摘要 markup(ngettext 处理复数)
    let mut deps = String::new();
    if !rm.is_empty() {
        deps.push_str(&tf(
            &ngettext(
                "Requires the <b>removal</b> of {0} package",
                "Requires the <b>removal</b> of {0} packages",
                rm.len() as u32,
            ),
            &[&rm.len().to_string()],
        ));
        deps.push('\n');
    }
    if !inst.is_empty() {
        deps.push_str(&tf(
            &ngettext(
                "Requires the installation of {0} package",
                "Requires the installation of {0} packages",
                inst.len() as u32,
            ),
            &[&inst.len().to_string()],
        ));
        deps.push('\n');
    }

    let same_installed =
        !analysis.installed_version.is_empty() && analysis.installed_version == version;

    if same_installed {
        // 同版本已装:状态文案 + Reinstall + Remove 显示
        gui.label_status
            .set_text(&gettext("Same version is already installed"));
        gui.button_install.set_label(&gettext("_Reinstall Package"));
        gui.button_remove.set_visible(true);
        gui.button_remove.set_sensitive(true);
    } else {
        gui.button_install.set_label(&gettext("_Install Package"));
        gui.button_remove.hide();
        if !rm.is_empty() || !inst.is_empty() {
            gui.label_status.set_markup(&deps);
        } else {
            gui.label_status
                .set_text(&gettext("All dependencies are satisfied"));
        }
    }
    gui.button_install.set_sensitive(true);
    *gui.changes.borrow_mut() = Some(changes);
}

/// Details 页签与描述的静态填充
fn fill_static(gui: &MainWindow, info: &PackageInfo, deb: &DebFile) {
    gui.label_name
        .set_markup(&format!("<b><big>{}</big></b>", info.package));
    gui.textview_description
        .buffer()
        .set_text(&info.description);

    let set = |l: &gtk::Label, v: Option<String>| l.set_text(&v.unwrap_or_default());
    set(&gui.details_version, Some(info.version.clone()));
    set(
        &gui.details_maintainer,
        deb.field_trimmed("Maintainer").map(String::from),
    );
    set(
        &gui.details_priority,
        deb.field_trimmed("Priority").map(String::from),
    );
    set(
        &gui.details_section,
        deb.field_trimmed("Section").map(String::from),
    );
    set(
        &gui.details_size,
        deb.field_trimmed("Installed-Size")
            .map(|s| format!("{s} KiB")),
    );
}

// ---------------------------------------------------------------------------
// 安装 / 移除动作(执行体在守护进程)
// ---------------------------------------------------------------------------

/// 事务请求(选择守护进程方法)
enum TxnRequest {
    Install {
        file: PathBuf,
        conf_policy: String,
    },
    Remove { name: String },
}

/// 事务进度对话框:label_action + progressbar + label_install_status +
/// Terminal expander + autoclose 勾选;Close 按钮运行中禁用;
/// 期间主窗口禁用,完成后恢复。
/// 执行流:连接系统总线 → 订阅信号(先于方法调用,防漏早期输出)→
/// 调用 InstallLocalFile/RemovePackage → 信号泵直至 Finished。
fn run_txn_dialog(
    gui: &Rc<MainWindow>,
    action: &str,
    complete: String,
    failed: String,
    req: TxnRequest,
) {
    let dialog = gtk::Dialog::builder()
        .transient_for(&gui.window)
        .modal(true)
        .title(gettext("Applying changes").as_str())
        .default_width(480)
        .build();
    dialog.add_button(&gettext("Close"), ResponseType::Close);
    dialog.set_default_response(ResponseType::Close);

    let label_action = gtk::Label::new(Some(action));
    label_action.set_halign(gtk::Align::Start);
    let bar = gtk::ProgressBar::new();
    let label_status = gtk::Label::new(None);
    label_status.set_halign(gtk::Align::Start);
    label_status.set_ellipsize(gtk::pango::EllipsizeMode::End);

    let textview = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .build();
    let exp = gtk::Expander::new(Some(gettext("Terminal").as_str()));
    exp.set_child(Some(&scroll(&textview)));
    let autoclose = gtk::CheckButton::with_label(&gettext(
        "Automatically close after the changes have been successfully applied",
    ));
    autoclose.set_active(true);
    autoclose.set_halign(gtk::Align::Start);

    let box_ = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(12)
        .margin_end(12)
        .build();
    box_.append(&label_action);
    box_.append(&bar);
    box_.append(&label_status);
    box_.append(&exp);
    box_.append(&autoclose);
    dialog.content_area().append(&box_);

    // Close 始终可用:运行中关闭 = 取消事务并立即恢复界面。
    // 此前运行中禁用 Close 且主窗口 insensitive,守护进程侧一旦挂起
    // (下载停滞/无 polkit agent/维护脚本等待)整个应用无法关闭。
    // 标题栏 X 发出的 DeleteEvent 响应与 Close 按钮同路处理
    gui.window.set_sensitive(false);

    // pulse 定时器(dpkg 行输出无精确百分比;收到数值进度后切换为 fraction,
    // 完成后置 1.0)
    let bar_pulse = bar.clone();
    let mut pulse_id = Some(glib::timeout_add_local(
        std::time::Duration::from_millis(100),
        move || {
            bar_pulse.pulse();
            glib::ControlFlow::Continue
        },
    ));

    // 用户关闭 → 经 channel 通知信号泵;对话框立即销毁,收尾在泵内完成
    let (close_tx, close_rx) = async_channel::bounded::<()>(1);
    dialog.connect_response(move |d, r| {
        if matches!(r, ResponseType::Close | ResponseType::DeleteEvent) {
            let _ = close_tx.try_send(());
            d.destroy();
        }
    });
    dialog.present();

    let statusbar = gui.statusbar.clone();
    let ctx = gui.status_ctx;
    let window = gui.window.clone();
    let gui_reload = gui.clone();
    let apt_opts = gui.apt_opts.clone();
    glib::spawn_future_local(async move {
        // 连接 + 订阅 + 发起事务;任一步失败即恢复界面并告警
        let started = async {
            let conn = daemon().await?;
            let proxy = FdebmProxy::builder(&conn)
                .build()
                .await
                .map_err(|e| e.to_string())?;
            let sigs = proxy
                .inner()
                .receive_all_signals()
                .await
                .map_err(|e| e.to_string())?;
            let id = match &req {
                TxnRequest::Install { file, conf_policy } => {
                    let opts: Vec<&str> = apt_opts.iter().map(String::as_str).collect();
                    proxy
                        .install_local_file(&file.to_string_lossy(), &opts, conf_policy)
                        .await
                        .map_err(|e| e.to_string())?
                }
                TxnRequest::Remove { name } => proxy
                    .remove_package(name)
                    .await
                    .map_err(|e| e.to_string())?,
            };
            Ok::<_, String>((conn, proxy, sigs, id))
        }
        .await;

        // conn/proxy 以 "_" 绑定保活:信号按唯一名投递,事务期间连接
        // 断开会让 daemon 侧 polkit 解析调用者失败(NameHasNoOwner)
        let (_conn, proxy, mut sigs, id) = match started {
            Ok((c, p, s, id)) => (c, p, s, id),
            Err(e) => {
                if let Some(pid) = pulse_id.take() {
                    pid.remove();
                }
                bar.set_fraction(1.0);
                window.set_sensitive(true);
                statusbar.push(ctx, &failed);
                show_alert(&window, AlertKind::Error, &gettext("fdebm error"), Some(&e));
                return;
            }
        };

        // 信号泵:OutputLine/ProgressChanged/Finished(按事务 id 过滤),
        // 与用户关闭事件竞争
        enum Ev {
            Sig(Option<zbus::Message>),
            Close,
        }
        let mut ok = false;
        let mut finished = false;
        let mut user_closed = false;
        loop {
            let ev = futures_lite::future::race(
                async { Ev::Sig(sigs.next().await) },
                async {
                    let _ = close_rx.recv().await;
                    Ev::Close
                },
            )
            .await;
            match ev {
                Ev::Close => {
                    user_closed = true;
                    break;
                }
                Ev::Sig(None) => break, // 信号流中断(守护进程退出/总线断开)
                Ev::Sig(Some(msg)) => {
                    let header = msg.header();
                    let Some(member) = header.member().map(|m| m.as_str()) else {
                        continue;
                    };
                    match member {
                        "OutputLine" => {
                            if let Ok((sid, line)) = msg.body().deserialize::<(u32, String)>()
                                && sid == id
                            {
                                label_status.set_text(&line);
                                let buf = textview.buffer();
                                let mut end = buf.end_iter();
                                buf.insert(&mut end, &format!("{line}\n"));
                                textview.scroll_to_iter(&mut end, 0.0, false, 0.0, 0.0);
                            }
                        }
                        "ProgressChanged" => {
                            if let Ok((sid, percent, desc)) =
                                msg.body().deserialize::<(u32, f64, String)>()
                                && sid == id
                            {
                                if percent < 0.0 {
                                    bar.pulse();
                                } else {
                                    // 首个数值进度:停 pulse 定时器,改精确显示
                                    if let Some(pid) = pulse_id.take() {
                                        pid.remove();
                                    }
                                    bar.set_fraction((percent / 100.0).min(1.0));
                                }
                                if !desc.is_empty() {
                                    label_status.set_text(&desc);
                                }
                            }
                        }
                        "Finished" => {
                            if let Ok((sid, finished_ok, detail)) =
                                msg.body().deserialize::<(u32, bool, String)>()
                                && sid == id
                            {
                                ok = finished_ok;
                                finished = true;
                                if let Some(pid) = pulse_id.take() {
                                    pid.remove();
                                }
                                bar.set_fraction(1.0);
                                if ok {
                                    statusbar.push(ctx, &complete);
                                    if autoclose.is_active() {
                                        dialog.destroy();
                                    }
                                } else {
                                    statusbar.push(ctx, &failed);
                                    show_alert(
                                        &window,
                                        AlertKind::Error,
                                        &gettext("fdebm error"),
                                        Some(&detail),
                                    );
                                }
                                window.set_sensitive(true);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // 用户运行中关闭:取消事务并立即恢复界面。cancel 以独立任务发射、
        // 不等守护进程应答(守护进程可能已挂起,等待会让"关闭"再次卡死);
        // 对话框已在响应回调中销毁,不再触碰其子控件
        if user_closed && !finished {
            let proxy_cancel = proxy.clone();
            glib::spawn_future_local(async move {
                let _ = proxy_cancel.cancel(id).await;
            });
            if let Some(pid) = pulse_id.take() {
                pid.remove();
            }
            window.set_sensitive(true);
            statusbar.push(ctx, gettext("operation cancelled").as_str());
            return;
        }

        // 信号流中断且未收到 Finished(守护进程退出/总线断开):兜底恢复
        if !finished {
            if let Some(pid) = pulse_id.take() {
                pid.remove();
            }
            window.set_sensitive(true);
            if !ok {
                statusbar.push(ctx, &failed);
            }
            return;
        }

        // 成功后重分析刷新:安装后状态即时更新
        if ok && let Some(f) = gui_reload.current_file.borrow().clone() {
            load_deb(&gui_reload, &f);
        }
    });
}

/// conffile 预检决策对话框:列出本地已修改且新包版本不同的配置文件,
/// 由用户选择覆盖策略。Some("keep"/"replace") = 继续安装,None = 放弃
fn confirm_conffiles(parent: &gtk::Window, conflicts: &[(String, String)]) -> Option<String> {
    let list = conflicts
        .iter()
        .map(|(_, p)| p.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let dlg = MessageDialog::builder()
        .transient_for(parent)
        .modal(true)
        .message_type(MessageType::Question)
        .text(gettext("Modified configuration files").as_str())
        .secondary_text(tf(
            "The following configuration files have been modified locally and the new package ships different versions:",
            &[&list],
        ))
        .build();
    dlg.add_button(&gettext("Keep _my version"), ResponseType::Yes);
    dlg.add_button(&gettext("Use the package's _new version"), ResponseType::No);
    dlg.add_button(&gettext("_Cancel"), ResponseType::Cancel);

    let (tx, rx) = std_mpsc::channel::<Option<String>>();
    dlg.connect_response(move |d, r| {
        let verdict = match r {
            ResponseType::Yes => Some("keep".to_string()),
            ResponseType::No => Some("replace".to_string()),
            _ => None,
        };
        let _ = tx.send(verdict);
        d.destroy();
    });
    dlg.present();
    loop {
        glib::MainContext::default().iteration(true);
        if let Ok(v) = rx.try_recv() {
            return v;
        }
    }
}

fn on_install_clicked(gui: &Rc<MainWindow>) {
    // 提权由守护进程侧 Polkit 发起(认证对话框自动弹出),客户端无需 root
    if let Some(c) = gui.changes.borrow().as_ref()
        && !c.unauthenticated.is_empty()
    {
        // 未认证包安装前确认
        let ui = GtkUi {
            parent: gui.window.clone().upcast(),
        };
        let ok = ui.confirm(
            &gettext("Install unauthenticated software?"),
            Some(&tf(
                "The following packages are UNAUTHENTICATED: {0}",
                &[&c.unauthenticated.join(" ")],
            )),
        );
        match ok {
            Ok(true) => {}
            _ => return,
        }
    }
    // conffile 预检:本地已修改且新包版本不同的配置,安装前定夺覆盖策略
    let conflicts = gui.conffiles.borrow().clone();
    let conf_policy = if conflicts.is_empty() {
        String::new()
    } else {
        match confirm_conffiles(gui.window.upcast_ref(), &conflicts) {
            Some(p) => p,
            None => return,
        }
    };
    let file = gui.current_file.borrow().clone().unwrap_or_default();
    gui.statusbar.push(
        gui.status_ctx,
        gettext("Installing package file...").as_str(),
    );
    run_txn_dialog(
        gui,
        &gettext("Installing package file..."),
        gettext("Installation complete").to_string(),
        gettext("Failed to install package file").to_string(),
        TxnRequest::Install { file, conf_policy },
    );
}

fn on_remove_clicked(gui: &Rc<MainWindow>) {
    let Some(pkgname) = gui
        .current_file
        .borrow()
        .clone()
        .and_then(|f| DebFile::open(&f).ok())
        .and_then(|d| d.field_trimmed("Package").map(String::from))
        .filter(|n| !n.is_empty())
    else {
        return;
    };
    gui.statusbar
        .push(gui.status_ctx, gettext("Removing package...").as_str());
    // dpkg -r 单包卸载;反向依赖会阻止卸载并报错(由输出呈现)
    run_txn_dialog(
        gui,
        &gettext("Removing package..."),
        gettext("Removal complete").to_string(),
        gettext("Failed to remove package").to_string(),
        TxnRequest::Remove { name: pkgname },
    );
}

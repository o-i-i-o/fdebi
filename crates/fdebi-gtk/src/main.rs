//! GTK4 前端入口(需要系统 libgtk-4-dev 与 pkg-config)。

mod gtk_app;

fn main() {
    gtk_app::run();
}

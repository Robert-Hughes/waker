fn main() -> eframe::Result {
    let diagnostics = waker_app::init_desktop_diagnostics();
    waker_app::run_desktop(diagnostics)
}

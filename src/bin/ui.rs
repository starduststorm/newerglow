fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // The Linux AppImage ships only the GUI binary, so the in-GUI
    // authorize banner re-execs current_exe() with this flag — the
    // re-exec lands here, before eframe starts, and dispatches to the
    // installer instead of opening a window.
    #[cfg(target_os = "linux")]
    if std::env::args().skip(1).any(|a| a == "--install-udev") {
        // The parent passes the vendor ids it resolved; as root we can't see
        // the user's board directory to rebuild them.
        let vids: Vec<u16> = std::env::args()
            .skip(1)
            .filter_map(|a| a.strip_prefix("--udev-vid=").map(str::to_string))
            .filter_map(|v| u16::from_str_radix(&v, 16).ok())
            .collect();
        match newerglow::install_udev::run(&vids) {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("error: {}", e);
                std::process::exit(1);
            }
        }
    }

    newerglow::ui::run()
}

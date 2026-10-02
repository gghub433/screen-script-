//! Revizor for Windows — sender.
//!
//! Starts a local UI server on 127.0.0.1 and opens it in an app-style browser
//! window. All streaming happens in this process; nothing is sent to any
//! server on the internet.

mod controller;
mod pipeline;
mod sources;
mod ui;
#[cfg(windows)]
mod win;

use std::path::PathBuf;
use std::sync::Arc;

fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("REVIZOR_HOME") {
        return PathBuf::from(d);
    }
    #[cfg(windows)]
    if let Some(a) = std::env::var_os("APPDATA") {
        return PathBuf::from(a).join("Revizor");
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".revizor")).unwrap_or_else(|| PathBuf::from(".revizor"))
}

fn open_in_browser(url: &str) {
    #[cfg(windows)]
    {
        // Prefer an app-style Edge window (no tabs/address bar); fall back to the default browser.
        let app = std::process::Command::new("cmd").args(["/C", "start", "", "msedge", &format!("--app={url}")]).spawn();
        if app.is_err() {
            let _ = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
        }
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

fn main() {
    let name = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "Windows PC".into());
    let no_browser = std::env::args().any(|a| a == "--no-browser");
    let port = std::env::args().skip_while(|a| a != "--port").nth(1).and_then(|p| p.parse().ok()).unwrap_or(0u16);
    let ctl = match controller::Controller::open(data_dir(), name) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            eprintln!("cannot open data directory: {e:?}");
            std::process::exit(1);
        }
    };
    let (ui, _) = ui::Ui::bind(port).unwrap_or_else(|e| {
        eprintln!("cannot start the local UI: {e}");
        std::process::exit(1);
    });
    println!("Revizor UI: {}", ui.url);
    if !no_browser {
        open_in_browser(&ui.url);
    }
    #[cfg(windows)]
    let sources: Arc<dyn Fn() -> Vec<sources::Source> + Send + Sync> = Arc::new(win::list_sources);
    #[cfg(not(windows))]
    let sources: Arc<dyn Fn() -> Vec<sources::Source> + Send + Sync> = Arc::new(Vec::new);
    // Look for receivers right away so the list is not empty on first paint.
    let c2 = ctl.clone();
    std::thread::spawn(move || {
        let _ = c2.scan(1500);
    });
    ui.serve(ctl, sources);
}

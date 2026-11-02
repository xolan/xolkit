// Release builds are GUI programs on Windows, so starting one opens no console window.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod assets;
mod settings;
mod shelf;
mod theme;

use gpui_kit::component::TitleBar;
use gpui_kit::*;
use xolkit_core::{Agent, Env};

use crate::shelf::Shelf;

actions!(xolkit, [Quit]);

const USAGE: &str = "\
Usage:
  xolkit                     Open the app
  xolkit xolkit://import/…   Open the app to import a shared profile
  xolkit mcp [--agent NAME]  Serve an agent session over stdio (agents run this)
  xolkit daemon              Run the background daemon (xolkit starts it itself)
  xolkit status              Print what the daemon reports, as JSON";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Release builds on Windows have no console of their own; print to the terminal that ran
    // them. Not for `mcp`, whose output is the agent's pipe.
    #[cfg(windows)]
    if args
        .first()
        .is_some_and(|a| a != "mcp" && a != "daemon" && !a.starts_with("xolkit://"))
    {
        attach_console();
    }
    match args.first().map(String::as_str) {
        Some("mcp") => exit_with(shim(&args[1..])),
        Some("daemon") => exit_with(Env::detect().and_then(xolkit_daemon::run_daemon)),
        Some("status") => exit_with(status()),
        Some("--help" | "-h" | "help") => println!("{USAGE}"),
        Some(link) if link.starts_with("xolkit://") => app(Some(link.to_owned())),
        Some(other) if other.starts_with('-') || args.len() > 1 => {
            eprintln!("xolkit: unknown argument {other}\n\n{USAGE}");
            std::process::exit(2);
        }
        _ => app(None),
    }
}

#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    // SAFETY: AttachConsole takes a process id and has no other preconditions.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

fn exit_with(result: anyhow::Result<()>) {
    if let Err(e) = result {
        eprintln!("xolkit: {e:#}");
        std::process::exit(1);
    }
}

/// `xolkit mcp --agent claude`: the agent is given by the registration; without it, the
/// session counts as one of the other MCP clients.
fn shim(args: &[String]) -> anyhow::Result<()> {
    let agent = match args {
        [] => None,
        [flag, name] if flag == "--agent" => {
            Some(Agent::from_key(name).ok_or_else(|| anyhow::anyhow!("unknown agent {name}"))?)
        }
        _ => anyhow::bail!("usage: xolkit mcp [--agent claude|codex|other]"),
    };
    let exe = std::env::current_exe()?;
    xolkit_daemon::run_shim(Env::detect_dirs()?, &exe, agent)
}

/// `xolkit status`: the Connections page, for a terminal. Starts the daemon if needed.
fn status() -> anyhow::Result<()> {
    let exe = std::env::current_exe()?;
    let mut client = xolkit_daemon::Client::new(Env::detect_dirs()?, &exe);
    println!("{}", serde_json::to_string_pretty(&client.status()?)?);
    Ok(())
}

fn app(import: Option<String>) {
    gpui_kit::application()
        .with_assets(assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            let fonts = assets::FONTS.iter().map(|f| (*f).into()).collect();
            if let Err(e) = cx.text_system().add_fonts(fonts) {
                eprintln!("xolkit: could not load the embedded fonts: {e:#}");
            }
            settings::init(cx);
            shelf::bind_keys(cx);
            cx.bind_keys([KeyBinding::new("ctrl-q", Quit, None)]);
            cx.on_action(|_: &Quit, cx| cx.quit());

            // The app draws its own title bar, but Windows shows the window's title in
            // Alt+Tab and on the taskbar.
            let titlebar = TitlebarOptions {
                title: cfg!(windows).then(|| "xolkit".into()),
                ..TitleBar::title_bar_options()
            };
            let options = WindowOptions {
                titlebar: Some(titlebar),
                window_bounds: Some(WindowBounds::centered(size(px(1180.), px(760.)), cx)),
                window_min_size: Some(size(px(760.), px(480.))),
                app_id: Some("xolkit".into()),
                ..TitleBar::window_options()
            };
            let import = import.clone();
            gpui_kit::open_window(options, cx, |window, cx| {
                theme::init(window, cx);
                cx.new(|cx| Shelf::new(import, window, cx))
            })
            .expect("failed to open window");
        });
}

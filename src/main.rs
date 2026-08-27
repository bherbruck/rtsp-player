// Without this a Windows release build is a console app, so launching the GUI
// pops an empty terminal behind the window. `--probe` output is still captured
// when redirected to a file or pipe.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod model;
mod stream;
mod ui;

use gpui::{
    AnyView, AppContext as _, Application, AssetSource, Bounds, SharedString, TitlebarOptions,
    WindowBounds, WindowOptions, px, size,
};
use gpui_component::Root;
use std::borrow::Cow;

/// gpui resolves `svg().path(..)` through the app's asset source, and
/// gpui-component ships no icons of its own, so carry ours in the binary.
struct Assets;

macro_rules! icon {
    ($path:literal) => {
        ($path, include_bytes!(concat!("../assets/", $path)).as_slice())
    };
}

const ICONS: &[(&str, &[u8])] = &[
    icon!("icons/chevron-right.svg"),
    icon!("icons/chevron-down.svg"),
    icon!("icons/folder.svg"),
    icon!("icons/folder-open.svg"),
    icon!("icons/camera.svg"),
];

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect())
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--probe") => {
            probe(&args[1..]);
            return;
        }
        // The config lives somewhere different on every platform; print it
        // rather than making people guess.
        Some("--config") => {
            match model::Library::path() {
                Ok(path) => println!("{}", path.display()),
                Err(e) => {
                    eprintln!("could not resolve config path: {e}");
                    std::process::exit(1);
                }
            }
            return;
        }
        _ => {}
    }

    // WSLg ships a Wayland compositor older than gpui's client supports, and
    // gpui prefers Wayland whenever WAYLAND_DISPLAY is set. XWayland is there
    // and works, so steer to it rather than panicking on startup.
    if is_wsl() && std::env::var_os("DISPLAY").is_some() {
        unsafe { std::env::remove_var("WAYLAND_DISPLAY") };
    }

    let app = Application::new().with_assets(Assets);

    app.run(|cx| {
        gpui_component::init(cx);
        cx.activate(true);

        let bounds = Bounds::centered(None, size(px(1280.0), px(820.0)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("RTSP Player".into()),
                ..Default::default()
            }),
            ..Default::default()
        };

        cx.open_window(options, |window, cx| {
            let view = cx.new(|cx| ui::PlayerApp::new(window, cx));
            cx.new(|cx| Root::new(AnyView::from(view), window, cx))
        })
        .expect("failed to open window");
    });
}

fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| {
            let s = s.to_ascii_lowercase();
            s.contains("microsoft") || s.contains("wsl")
        })
        .unwrap_or(false)
}

/// `rtsp-player --probe <url> [username] [password]`
///
/// Connects, decodes for a few seconds and reports what came back. Useful for
/// working out whether a camera is reachable without opening the window.
fn probe(args: &[String]) {
    let Some(url) = args.first() else {
        eprintln!("usage: rtsp-player --probe <rtsp-url> [username] [password]");
        std::process::exit(2);
    };

    let mut connection = model::Connection::new("probe", url.clone());
    connection.username = args.get(1).cloned();
    connection.password = args.get(2).cloned();

    println!("connecting to {url}");
    let player = stream::Player::start(connection);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut frames = 0usize;
    let mut last_status = String::new();
    // Timed from the first frame so connect and key-frame wait do not drag the
    // rate down.
    let mut first_frame_at = None;

    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(2));

        let status = format!("{:?}", player.status());
        if status != last_status {
            println!("status: {status}");
            last_status = status;
        }

        if let Some(frame) = player.take_frame() {
            if frames == 0 {
                println!("first frame: {}x{}", frame.width, frame.height);
                first_frame_at = Some((std::time::Instant::now(), player.decoded_count()));
            }
            frames += 1;
        }
        if first_frame_at.is_some_and(|(t, _)| t.elapsed().as_secs() >= 8) {
            break;
        }
    }

    // Count from the decoder, not from what this loop managed to pick up: the
    // mailbox deliberately drops frames a consumer is too slow to take.
    let steady = first_frame_at
        .map(|(t, base)| {
            (player.decoded_count().saturating_sub(base)) as f32 / t.elapsed().as_secs_f32()
        })
        .unwrap_or(0.0);
    println!(
        "decoder produced {} pictures, {steady:.1} fps steady state",
        player.decoded_count()
    );
    if frames == 0 {
        std::process::exit(1);
    }
}

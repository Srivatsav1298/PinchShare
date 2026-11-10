//! PinchShare tray app. Platform glue only; all transfer logic lives in pinchshare-core.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod gesture;
#[cfg(target_os = "macos")]
mod macos;
mod settings;
mod sysio;

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use pinchshare_core::controller::{Controller, ControllerConfig, Payload, UiEvent};
use pinchshare_core::slot::{SlotKind, Trigger};
use pinchshare_core::{Node, NodeConfig};
use serde::Serialize;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};

use settings::Settings;

struct AppState {
    controller: OnceLock<Arc<Controller>>,
    settings: Mutex<Settings>,
    data_dir: PathBuf,
    gestures_on: AtomicBool,
    toast_seq: AtomicU64,
}

#[derive(Serialize, Clone)]
struct Toast {
    text: String,
    kind: &'static str,
    countdown_ms: u64,
    cancellable: bool,
    progress: Option<f32>,
}

fn toast(app: &AppHandle, text: impl Into<String>, kind: &'static str, hide_after_ms: u64) {
    show_toast(app, Toast { text: text.into(), kind, countdown_ms: 0, cancellable: false, progress: None }, hide_after_ms);
}

fn show_toast(app: &AppHandle, t: Toast, hide_after_ms: u64) {
    let st = app.state::<AppState>();
    let seq = st.toast_seq.fetch_add(1, Ordering::SeqCst) + 1;
    if let Some(w) = app.get_webview_window("toast") {
        if let Ok(Some(m)) = w.primary_monitor() {
            let size = m.size();
            let x = m.position().x + (size.width as i32 - 380 * m.scale_factor() as i32) / 2;
            let _ = w.set_position(PhysicalPosition::new(x, m.position().y + (40.0 * m.scale_factor()) as i32));
        }
        let _ = w.show();
        let _ = w.emit("toast", t);
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(hide_after_ms)).await;
        if app.state::<AppState>().toast_seq.load(Ordering::SeqCst) == seq {
            if let Some(w) = app.get_webview_window("toast") {
                let _ = w.hide();
            }
        }
    });
}

#[tauri::command]
fn cancel_paste(state: tauri::State<'_, AppState>) {
    if let Some(c) = state.controller.get() {
        c.cancel();
    }
}

fn do_copy(app: &AppHandle, trigger: Trigger) {
    let app = app.clone();
    std::thread::spawn(move || {
        match sysio::copy_selection() {
            Ok(Some(content)) => {
                if let Some(c) = app.state::<AppState>().controller.get() {
                    c.copy(content, trigger);
                }
            }
            Ok(None) => toast(&app, "Nothing selected to copy.", "info", 2000),
            Err(e) => toast(&app, format!("Could not copy: {e}. Check Accessibility permission."), "error", 4000),
        }
    });
}

fn do_paste(app: &AppHandle, trigger: Trigger) {
    if let Some(c) = app.state::<AppState>().controller.get() {
        c.paste(trigger);
    }
}

fn describe(kind: SlotKind) -> &'static str {
    match kind {
        SlotKind::Text => "text",
        SlotKind::Image => "image",
        SlotKind::Files => "files",
    }
}

fn handle_ui_event(app: &AppHandle, ev: UiEvent) {
    match ev {
        UiEvent::Copied { .. } => toast(app, "Copied. Pinch out on another device to paste.", "info", 2500),
        UiEvent::SlotExpired => {}
        UiEvent::NothingToPaste => toast(app, "Nothing to paste. Copy something on another device first.", "info", 2500),
        UiEvent::Countdown { device_name, kind, secs, .. } => show_toast(
            app,
            Toast { text: format!("Receiving {} from {device_name}…", describe(kind)), kind: "info", countdown_ms: (secs * 1000.0) as u64, cancellable: true, progress: None },
            (secs * 1000.0) as u64 + 300,
        ),
        UiEvent::Cancelled { .. } => toast(app, "Cancelled.", "info", 1200),
        UiEvent::Pulling { device_name } => toast(app, format!("Getting it from {device_name}…"), "info", 4000),
        UiEvent::Progress { name, received, total } => {
            if total > 2 * 1024 * 1024 {
                let p = received as f32 / total as f32;
                show_toast(app, Toast { text: format!("Receiving {name} — {}%", (p * 100.0) as u32), kind: "info", countdown_ms: 0, cancellable: false, progress: Some(p) }, 4000);
            }
        }
        UiEvent::Deliver { from_name, payload } => deliver(app, &from_name, payload),
        UiEvent::Received { from_name, payload } => {
            toast(app, format!("Received from {from_name}."), "info", 2500);
            if let Payload::Files(p) = payload {
                if let Some(f) = p.first() {
                    sysio::reveal(f);
                }
            }
        }
        UiEvent::Error(e) => toast(app, e, "error", 4500),
    }
}

fn deliver(app: &AppHandle, from: &str, payload: Payload) {
    let app = app.clone();
    let from = from.to_string();
    std::thread::spawn(move || {
        let res = match &payload {
            Payload::Text(t) => sysio::paste_text(t),
            Payload::Image(p) => sysio::paste_image(p),
            Payload::Files(paths) => {
                if let Some(f) = paths.first() {
                    sysio::reveal(f);
                }
                Ok(())
            }
        };
        match (res, &payload) {
            (Err(e), _) => toast(&app, format!("Received, but could not paste: {e}"), "error", 4500),
            (Ok(()), Payload::Files(p)) => toast(&app, format!("Saved {} item(s) from {from} to your PinchShare folder.", p.len()), "info", 3500),
            (Ok(()), _) => toast(&app, format!("Pasted from {from}."), "info", 1500),
        }
    });
}

fn device_name() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().trim_end_matches(".local").to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "My device".into())
}

fn register_hotkeys(app: &AppHandle, s: &Settings) {
    let gs = app.global_shortcut();
    let _ = gs.unregister_all();
    for (name, key) in [("copy", &s.hotkey_copy), ("paste", &s.hotkey_paste)] {
        match Shortcut::from_str(key) {
            Ok(sc) => {
                if let Err(e) = gs.register(sc) {
                    toast(app, format!("Hotkey {key} ({name}) is taken by another app: {e}"), "error", 5000);
                }
            }
            Err(e) => toast(app, format!("Invalid {name} hotkey '{key}': {e}"), "error", 5000),
        }
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state != ShortcutState::Released {
                        return;
                    }
                    let s = app.state::<AppState>().settings.lock().unwrap().clone();
                    if Shortcut::from_str(&s.hotkey_copy).is_ok_and(|x| &x == shortcut) {
                        do_copy(app, Trigger::Hotkey);
                    } else if Shortcut::from_str(&s.hotkey_paste).is_ok_and(|x| &x == shortcut) {
                        do_paste(app, Trigger::Hotkey);
                    }
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![cancel_paste])
        .setup(|app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);

            let data_dir = dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("PinchShare");
            let settings = Settings::load(&data_dir);
            app.manage(AppState {
                controller: OnceLock::new(),
                gestures_on: AtomicBool::new(settings.gesture_enabled),
                settings: Mutex::new(settings.clone()),
                data_dir: data_dir.clone(),
                toast_seq: AtomicU64::new(0),
            });

            WebviewWindowBuilder::new(app, "toast", WebviewUrl::App("toast.html".into()))
                .title("PinchShare")
                .inner_size(380.0, 64.0)
                .decorations(false)
                .always_on_top(true)
                .skip_taskbar(true)
                .resizable(false)
                .focused(false)
                .visible(false)
                .build()?;

            // Tray
            let copy_i = MenuItem::with_id(app, "copy", "Copy selection (ready to send)", true, None::<&str>)?;
            let paste_i = MenuItem::with_id(app, "paste", "Paste here", true, None::<&str>)?;
            let cancel_i = MenuItem::with_id(app, "cancel", "Cancel pending paste", true, None::<&str>)?;
            let gest_i = CheckMenuItem::with_id(app, "gestures", "Pinch gestures", true, settings.gesture_enabled, None::<&str>)?;
            let pair_i = MenuItem::with_id(app, "pair", "Show pairing code", true, None::<&str>)?;
            let ax_i = MenuItem::with_id(app, "ax", "Open Accessibility settings…", true, None::<&str>)?;
            let quit_i = MenuItem::with_id(app, "quit", "Quit PinchShare", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&copy_i, &paste_i, &cancel_i, &PredefinedMenuItem::separator(app)?, &gest_i, &pair_i, &ax_i, &PredefinedMenuItem::separator(app)?, &quit_i])?;
            let gest_i2 = gest_i.clone();
            TrayIconBuilder::new()
                .icon(tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?)
                .tooltip("PinchShare")
                .menu(&menu)
                .on_menu_event(move |app, e| match e.id.as_ref() {
                    "copy" => do_copy(app, Trigger::Tray),
                    "paste" => do_paste(app, Trigger::Tray),
                    "cancel" => cancel_paste(app.state()),
                    "gestures" => {
                        let st = app.state::<AppState>();
                        let on = gest_i2.is_checked().unwrap_or(true);
                        st.gestures_on.store(on, Ordering::SeqCst);
                        let mut s = st.settings.lock().unwrap();
                        s.gesture_enabled = on;
                        s.save(&st.data_dir);
                    }
                    "pair" => {
                        if let Some(c) = app.state::<AppState>().controller.get() {
                            let code = c.show_pairing_code();
                            toast(app, format!("Pairing code: {code}  (valid 2 min)"), "info", 120_000);
                        }
                    }
                    #[cfg(target_os = "macos")]
                    "ax" => macos::open_accessibility_settings(),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            register_hotkeys(app.handle(), &settings);

            // macOS pinch gestures.
            #[cfg(target_os = "macos")]
            {
                if !macos::accessibility_granted() {
                    toast(app.handle(), "PinchShare needs Accessibility permission to copy/paste for you. Use the tray menu → Open Accessibility settings.", "error", 8000);
                }
                let h = app.handle().clone();
                let h2 = h.clone();
                let rec = Mutex::new(gesture::Recognizer::new(settings.pinch_threshold));
                macos::start_magnify_monitor(
                    move || h2.state::<AppState>().settings.lock().unwrap().gesture_modifier,
                    move |mag, phase, mod_ok| {
                        let st = h.state::<AppState>();
                        if !st.gestures_on.load(Ordering::SeqCst) || !mod_ok {
                            return;
                        }
                        let mut r = rec.lock().unwrap();
                        r.set_threshold(st.settings.lock().unwrap().pinch_threshold);
                        match r.feed(mag, phase) {
                            Some(gesture::Pinch::In) => do_copy(&h, Trigger::Gesture),
                            Some(gesture::Pinch::Out) => do_paste(&h, Trigger::Gesture),
                            None => {}
                        }
                    },
                );
            }

            // Networking + controller.
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let cfg = NodeConfig { data_dir: data_dir.clone(), device_name: device_name(), bind: "0.0.0.0:0".parse().unwrap(), mdns: true, save_dir: None };
                match Node::start(cfg).await {
                    Ok((node, rx)) => {
                        let ccfg = ControllerConfig { metrics_path: Some(data_dir.join("metrics.jsonl")), ..Default::default() };
                        let (c, mut ui) = Controller::start(Arc::new(node), rx, ccfg);
                        let _ = handle.state::<AppState>().controller.set(c);
                        while let Some(ev) = ui.recv().await {
                            handle_ui_event(&handle, ev);
                        }
                    }
                    Err(e) => toast(&handle, format!("PinchShare could not start networking: {e}"), "error", 10_000),
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running PinchShare");
}

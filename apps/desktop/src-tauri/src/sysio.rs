//! Clipboard access and simulated Cmd/Ctrl+C / +V. Contents are never logged.
use std::path::PathBuf;
use std::thread::sleep;
use std::time::Duration;

use clipboard_rs::common::RustImage;
use clipboard_rs::{Clipboard, ClipboardContext, ContentFormat, RustImageData};
use enigo::{Direction, Enigo, Key, Keyboard, Settings};
use pinchshare_core::slot::SlotContent;

#[cfg(target_os = "macos")]
const MOD: Key = Key::Meta;
#[cfg(not(target_os = "macos"))]
const MOD: Key = Key::Control;

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn to_path(s: &str) -> PathBuf {
    PathBuf::from(percent_decode(s.strip_prefix("file://").unwrap_or(s)))
}

/// Press the shortcut's modifier keys' counterparts up first: a global hotkey like Cmd+Alt+C
/// leaves Alt held, which would turn our simulated Cmd+C into Cmd+Alt+C.
fn release_held_modifiers(e: &mut Enigo) {
    for k in [Key::Alt, Key::Shift, Key::Control, Key::Meta] {
        let _ = e.key(k, Direction::Release);
    }
}

fn chord(letter: char) -> Result<(), String> {
    let mut e = Enigo::new(&Settings::default()).map_err(|e| e.to_string())?;
    release_held_modifiers(&mut e);
    sleep(Duration::from_millis(40));
    e.key(MOD, Direction::Press).map_err(|e| e.to_string())?;
    e.key(Key::Unicode(letter), Direction::Click).map_err(|e| e.to_string())?;
    e.key(MOD, Direction::Release).map_err(|e| e.to_string())?;
    Ok(())
}

/// Copy the current selection in the frontmost app and read it back from the clipboard.
/// Order of preference: files, image, text.
pub fn copy_selection() -> Result<Option<SlotContent>, String> {
    let ctx = ClipboardContext::new().map_err(|e| e.to_string())?;
    let before = fingerprint(&ctx);
    chord('c')?;
    // Apps update the clipboard asynchronously; poll briefly for a change.
    for _ in 0..15 {
        sleep(Duration::from_millis(40));
        if fingerprint(&ctx) != before {
            break;
        }
    }
    read_clipboard(&ctx)
}

fn fingerprint(ctx: &ClipboardContext) -> (Option<String>, usize, usize) {
    (ctx.get_text().ok(), ctx.get_files().map(|f| f.len()).unwrap_or(0), ctx.get_image().ok().map(|i| i.get_size().0 as usize).unwrap_or(0))
}

pub fn read_clipboard(ctx: &ClipboardContext) -> Result<Option<SlotContent>, String> {
    if ctx.has(ContentFormat::Files) {
        if let Ok(files) = ctx.get_files() {
            let paths: Vec<PathBuf> = files.iter().map(|f| to_path(f)).filter(|p| p.exists()).collect();
            if !paths.is_empty() {
                return Ok(Some(SlotContent::Files(paths)));
            }
        }
    }
    if ctx.has(ContentFormat::Image) {
        if let Ok(img) = ctx.get_image() {
            if let Ok(png) = img.to_png() {
                return Ok(Some(SlotContent::Image(png.get_bytes().to_vec())));
            }
        }
    }
    if let Ok(t) = ctx.get_text() {
        if !t.is_empty() {
            return Ok(Some(SlotContent::Text(t)));
        }
    }
    Ok(None)
}

pub fn paste_text(text: &str) -> Result<(), String> {
    ClipboardContext::new().map_err(|e| e.to_string())?.set_text(text.to_string()).map_err(|e| e.to_string())?;
    sleep(Duration::from_millis(80));
    chord('v')
}

pub fn paste_image(path: &std::path::Path) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let img = RustImageData::from_bytes(&bytes).map_err(|e| e.to_string())?;
    ClipboardContext::new().map_err(|e| e.to_string())?.set_image(img).map_err(|e| e.to_string())?;
    sleep(Duration::from_millis(120));
    chord('v')
}

/// Show a saved file or folder in the OS file manager.
pub fn reveal(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(path.parent().unwrap_or(path)).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_urls_decode() {
        assert_eq!(to_path("file:///Users/a/My%20Doc%C3%A9.txt"), PathBuf::from("/Users/a/My Docé.txt"));
        assert_eq!(to_path("/plain/path"), PathBuf::from("/plain/path"));
        assert_eq!(to_path("file:///100%25"), PathBuf::from("/100%"));
    }
}

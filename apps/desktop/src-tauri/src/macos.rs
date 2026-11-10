//! macOS: global trackpad pinch (NSEvent magnify) and Accessibility permission.
use std::ptr::NonNull;

use block2::RcBlock;
use objc2_app_kit::{NSEvent, NSEventMask, NSEventModifierFlags, NSEventPhase};

use crate::gesture::Phase;
use crate::settings::Modifier;

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

/// Needed to post simulated Cmd+C/V. (Global magnify monitoring itself may not need it; see spikes.)
pub fn accessibility_granted() -> bool {
    unsafe { AXIsProcessTrusted() }
}

pub fn open_accessibility_settings() {
    let _ = std::process::Command::new("open").arg("x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility").spawn();
}

pub fn modifier_held(m: Modifier, flags: NSEventModifierFlags) -> bool {
    match m {
        Modifier::Ctrl => flags.contains(NSEventModifierFlags::Control),
        Modifier::Alt => flags.contains(NSEventModifierFlags::Option),
        Modifier::Shift => flags.contains(NSEventModifierFlags::Shift),
        Modifier::Cmd => flags.contains(NSEventModifierFlags::Command),
    }
}

/// Install the global monitor. Must be called on the main thread.
/// `on_event(magnification, phase, modifiers_ok)` runs on the main thread for every magnify event
/// seen while another app is focused. `required` is read for every event so settings changes apply live.
pub fn start_magnify_monitor(required: impl Fn() -> Option<Modifier> + 'static, on_event: impl Fn(f64, Phase, bool) + 'static) {
    let block = RcBlock::new(move |ev: NonNull<NSEvent>| {
        let ev = unsafe { ev.as_ref() };
        let ph = ev.phase();
        let phase = if ph.contains(NSEventPhase::Began) {
            Phase::Began
        } else if ph.contains(NSEventPhase::Ended) || ph.contains(NSEventPhase::Cancelled) {
            Phase::Ended
        } else {
            Phase::Changed
        };
        let ok = required().is_none_or(|m| modifier_held(m, ev.modifierFlags()));
        on_event(ev.magnification(), phase, ok);
    });
    let monitor = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(NSEventMask::Magnify, &block);
    // The monitor lives for the whole process.
    std::mem::forget(monitor);
    std::mem::forget(block);
}

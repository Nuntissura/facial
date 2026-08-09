//! Focus-gated Windows input synthesis for controller couch operation.
//!
//! These functions are called only while Facial owns foreground focus. The
//! controller layer releases every held pointer button before focus handoff.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Right,
}

/// This binary also provides CLI/model commands, so it remains a console
/// subsystem executable. Hide the inherited console only for GUI launches;
/// command output remains intact for every headless subcommand.
#[cfg(windows)]
pub fn hide_console_for_gui() {
    use windows_sys::Win32::System::Console::GetConsoleWindow;
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};
    unsafe {
        let console = GetConsoleWindow();
        if !console.is_null() {
            ShowWindow(console, SW_HIDE);
        }
    }
}

#[cfg(not(windows))]
pub fn hide_console_for_gui() {}

#[cfg(target_os = "windows")]
mod windows {
    use super::PointerButton;
    use std::mem::size_of;
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_KEYUP,
        MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
        MOUSEEVENTF_RIGHTUP, MOUSEINPUT, VK_MENU, VK_TAB,
    };

    fn send(inputs: &[INPUT]) -> Result<(), String> {
        let sent = unsafe {
            SendInput(
                inputs.len() as u32,
                inputs.as_ptr(),
                size_of::<INPUT>() as i32,
            )
        };
        if sent == inputs.len() as u32 {
            Ok(())
        } else {
            Err(format!(
                "Windows accepted {sent}/{} input events: {}",
                inputs.len(),
                std::io::Error::last_os_error()
            ))
        }
    }

    fn key(vk: u16, up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    wScan: 0,
                    dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn mouse(dx: i32, dy: i32, flags: u32) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    pub fn switch_apps() -> Result<(), String> {
        // One balanced batch prevents an Alt key from remaining logically held
        // if Windows switches focus between the key-down and key-up events.
        send(&[
            key(VK_MENU, false),
            key(VK_TAB, false),
            key(VK_TAB, true),
            key(VK_MENU, true),
        ])
    }

    pub fn move_pointer(dx: i32, dy: i32) -> Result<(), String> {
        if dx == 0 && dy == 0 {
            return Ok(());
        }
        send(&[mouse(dx, dy, MOUSEEVENTF_MOVE)])
    }

    pub fn set_pointer_button(button: PointerButton, down: bool) -> Result<(), String> {
        let flags = match (button, down) {
            (PointerButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
            (PointerButton::Left, false) => MOUSEEVENTF_LEFTUP,
            (PointerButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
            (PointerButton::Right, false) => MOUSEEVENTF_RIGHTUP,
        };
        send(&[mouse(0, 0, flags)])
    }
}

#[cfg(target_os = "windows")]
pub use windows::{move_pointer, set_pointer_button, switch_apps};

#[cfg(not(target_os = "windows"))]
pub fn switch_apps() -> Result<(), String> {
    Err("controller app switching is currently available on Windows only".to_string())
}

#[cfg(not(target_os = "windows"))]
pub fn move_pointer(_dx: i32, _dy: i32) -> Result<(), String> {
    Ok(())
}

#[cfg(not(target_os = "windows"))]
pub fn set_pointer_button(_button: PointerButton, _down: bool) -> Result<(), String> {
    Ok(())
}

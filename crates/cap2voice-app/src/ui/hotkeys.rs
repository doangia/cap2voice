//! Global hotkey parsing + registration (`global-hotkey`).
//!
//! Hotkey strings use the config format `Ctrl+Alt+S` (case-insensitive,
//! `+` separated, at least one modifier, exactly one key). Parsing is pure
//! and fully unit-tested; only [`HotkeyManager::register_all`] touches the
//! OS, so the pure parts run on CI while registration is verified on the
//! real PC.

use crate::config::HotkeyConfig;
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
};
use std::collections::HashMap;

/// Actions a global hotkey can request from the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyAction {
    ToggleStartStop,
    SelectRegion,
    ToggleOverlay,
    ToggleMute,
}

/// Errors from parsing or registering a hotkey.
#[derive(Debug, thiserror::Error)]
pub enum HotkeyError {
    #[error("phím tắt không hợp lệ {combo:?}: {reason}")]
    Parse { combo: String, reason: String },
    #[error("không đăng ký được phím tắt {action:?} ({combo}): {source}")]
    Register {
        action: HotkeyAction,
        combo: String,
        #[source]
        source: global_hotkey::Error,
    },
}

fn parse_error(combo: &str, reason: &str) -> HotkeyError {
    HotkeyError::Parse {
        combo: combo.to_string(),
        reason: reason.to_string(),
    }
}

fn parse_modifiers(token: &str) -> Option<Modifiers> {
    match token {
        "ctrl" | "control" => Some(Modifiers::CONTROL),
        "alt" => Some(Modifiers::ALT),
        "shift" => Some(Modifiers::SHIFT),
        "super" | "win" | "meta" => Some(Modifiers::SUPER),
        _ => None,
    }
}

fn parse_code(token: &str) -> Option<Code> {
    if token.len() == 1 {
        let c = token.chars().next().unwrap_or('\0');
        if c.is_ascii_alphabetic() {
            return match c.to_ascii_uppercase() {
                'A' => Some(Code::KeyA),
                'B' => Some(Code::KeyB),
                'C' => Some(Code::KeyC),
                'D' => Some(Code::KeyD),
                'E' => Some(Code::KeyE),
                'F' => Some(Code::KeyF),
                'G' => Some(Code::KeyG),
                'H' => Some(Code::KeyH),
                'I' => Some(Code::KeyI),
                'J' => Some(Code::KeyJ),
                'K' => Some(Code::KeyK),
                'L' => Some(Code::KeyL),
                'M' => Some(Code::KeyM),
                'N' => Some(Code::KeyN),
                'O' => Some(Code::KeyO),
                'P' => Some(Code::KeyP),
                'Q' => Some(Code::KeyQ),
                'R' => Some(Code::KeyR),
                'S' => Some(Code::KeyS),
                'T' => Some(Code::KeyT),
                'U' => Some(Code::KeyU),
                'V' => Some(Code::KeyV),
                'W' => Some(Code::KeyW),
                'X' => Some(Code::KeyX),
                'Y' => Some(Code::KeyY),
                'Z' => Some(Code::KeyZ),
                _ => None,
            };
        }
        if c.is_ascii_digit() {
            return match c {
                '0' => Some(Code::Digit0),
                '1' => Some(Code::Digit1),
                '2' => Some(Code::Digit2),
                '3' => Some(Code::Digit3),
                '4' => Some(Code::Digit4),
                '5' => Some(Code::Digit5),
                '6' => Some(Code::Digit6),
                '7' => Some(Code::Digit7),
                '8' => Some(Code::Digit8),
                '9' => Some(Code::Digit9),
                _ => None,
            };
        }
        return None;
    }
    match token {
        "f1" => Some(Code::F1),
        "f2" => Some(Code::F2),
        "f3" => Some(Code::F3),
        "f4" => Some(Code::F4),
        "f5" => Some(Code::F5),
        "f6" => Some(Code::F6),
        "f7" => Some(Code::F7),
        "f8" => Some(Code::F8),
        "f9" => Some(Code::F9),
        "f10" => Some(Code::F10),
        "f11" => Some(Code::F11),
        "f12" => Some(Code::F12),
        "space" => Some(Code::Space),
        "enter" => Some(Code::Enter),
        "escape" | "esc" => Some(Code::Escape),
        "tab" => Some(Code::Tab),
        "up" => Some(Code::ArrowUp),
        "down" => Some(Code::ArrowDown),
        "left" => Some(Code::ArrowLeft),
        "right" => Some(Code::ArrowRight),
        "home" => Some(Code::Home),
        "end" => Some(Code::End),
        "pageup" => Some(Code::PageUp),
        "pagedown" => Some(Code::PageDown),
        "delete" | "del" => Some(Code::Delete),
        "insert" | "ins" => Some(Code::Insert),
        "backspace" => Some(Code::Backspace),
        _ => None,
    }
}

/// Parse `Ctrl+Alt+S` (case-insensitive, `+` separated) into a [`HotKey`].
/// Requires at least one modifier and exactly one key.
pub fn parse_hotkey(text: &str) -> Result<HotKey, HotkeyError> {
    let combo = text.trim();
    if combo.is_empty() {
        return Err(parse_error(text, "chuỗi phím tắt rỗng"));
    }
    let mut modifiers = Modifiers::empty();
    let mut code: Option<Code> = None;
    for raw in combo.split('+') {
        let token = raw.trim().to_ascii_lowercase();
        if token.is_empty() {
            return Err(parse_error(text, "có thành phần rỗng giữa dấu +"));
        }
        if let Some(m) = parse_modifiers(&token) {
            if modifiers.contains(m) {
                return Err(parse_error(text, "phím bổ trợ bị lặp"));
            }
            modifiers.insert(m);
        } else if let Some(c) = parse_code(&token) {
            if code.is_some() {
                return Err(parse_error(text, "có nhiều hơn một phím chính"));
            }
            code = Some(c);
        } else {
            return Err(parse_error(text, "thành phần không nhận dạng được"));
        }
    }
    if modifiers.is_empty() {
        return Err(parse_error(
            text,
            "thiếu phím bổ trợ (Ctrl/Alt/Shift/Super)",
        ));
    }
    match code {
        Some(c) => Ok(HotKey::new(Some(modifiers), c)),
        None => Err(parse_error(text, "thiếu phím chính")),
    }
}

/// Pure id → action map built from config, without touching the OS.
#[derive(Debug, Clone)]
pub struct HotkeyBindings {
    bindings: HashMap<u32, HotkeyAction>,
}

impl HotkeyBindings {
    /// Parse all four configured hotkeys into an id → action map.
    pub fn from_config(config: &HotkeyConfig) -> Result<Self, HotkeyError> {
        let pairs: [(HotkeyAction, &str); 4] = [
            (HotkeyAction::ToggleStartStop, &config.toggle_start_stop),
            (HotkeyAction::SelectRegion, &config.select_region),
            (HotkeyAction::ToggleOverlay, &config.toggle_overlay),
            (HotkeyAction::ToggleMute, &config.toggle_mute),
        ];
        let mut bindings = HashMap::with_capacity(4);
        for (action, combo) in pairs {
            if combo.trim().is_empty() {
                continue;
            }
            let hotkey = parse_hotkey(combo)?;
            bindings.insert(hotkey.id(), action);
        }
        Ok(Self { bindings })
    }

    /// Map a hotkey event id to its action (`None` for unknown ids).
    pub fn action_for_id(&self, id: u32) -> Option<HotkeyAction> {
        self.bindings.get(&id).copied()
    }

    /// Number of bound hotkeys (empty combos are skipped, not errors).
    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    /// Whether any hotkey is bound.
    pub fn is_empty(&self) -> bool {
        self.bindings.is_empty()
    }
}

/// Owns the OS-registered global hotkeys.
pub struct HotkeyManager {
    manager: GlobalHotKeyManager,
    bindings: HotkeyBindings,
}

impl HotkeyManager {
    /// Register all four hotkeys from config. Fails fast on the first
    /// invalid combo or OS registration error; nothing is left
    /// half-registered because [`GlobalHotKeyManager`] unregisters on drop.
    pub fn register_all(config: &HotkeyConfig) -> Result<Self, HotkeyError> {
        let pairs: [(HotkeyAction, &str); 4] = [
            (HotkeyAction::ToggleStartStop, &config.toggle_start_stop),
            (HotkeyAction::SelectRegion, &config.select_region),
            (HotkeyAction::ToggleOverlay, &config.toggle_overlay),
            (HotkeyAction::ToggleMute, &config.toggle_mute),
        ];
        let manager = GlobalHotKeyManager::new().map_err(|source| HotkeyError::Register {
            action: HotkeyAction::ToggleStartStop,
            combo: config.toggle_start_stop.clone(),
            source,
        })?;
        let mut registered: Vec<HotKey> = Vec::with_capacity(4);
        for (action, combo) in pairs {
            if combo.trim().is_empty() {
                continue;
            }
            let hotkey = parse_hotkey(combo)?;
            if let Err(source) = manager.register(hotkey) {
                for done in registered {
                    let _ = manager.unregister(done);
                }
                return Err(HotkeyError::Register {
                    action,
                    combo: combo.to_string(),
                    source,
                });
            }
            registered.push(hotkey);
        }
        let bindings = HotkeyBindings::from_config(config)?;
        Ok(Self { manager, bindings })
    }

    /// Map a hotkey event id to its action (`None` for unknown ids).
    pub fn action_for_id(&self, id: u32) -> Option<HotkeyAction> {
        self.bindings.action_for_id(id)
    }

    /// Map a full event to its action; only key presses count, releases
    /// are ignored so each physical press fires exactly once.
    pub fn action_for_event(&self, event: &GlobalHotKeyEvent) -> Option<HotkeyAction> {
        if event.state != HotKeyState::Pressed {
            return None;
        }
        self.action_for_id(event.id)
    }
}

impl Drop for HotkeyManager {
    fn drop(&mut self) {
        // `GlobalHotKeyManager` unregisters its hotkeys on drop; this is
        // explicit so a future refactor cannot silently keep them alive.
        let _ = &self.manager;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hotkey_accepts_the_four_defaults() {
        assert_eq!(
            parse_hotkey("Ctrl+Alt+S").expect("S parses"),
            HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyS)
        );
        assert_eq!(
            parse_hotkey("Ctrl+Alt+R").expect("R parses"),
            HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyR)
        );
        assert_eq!(
            parse_hotkey("Ctrl+Alt+H").expect("H parses"),
            HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyH)
        );
        assert_eq!(
            parse_hotkey("Ctrl+Alt+M").expect("M parses"),
            HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::KeyM)
        );
    }

    #[test]
    fn parse_hotkey_is_case_insensitive_and_trims_spaces() {
        assert_eq!(
            parse_hotkey("ctrl+alt+s").expect("lowercase parses"),
            parse_hotkey("Ctrl+Alt+S").expect("mixed case parses")
        );
        assert_eq!(
            parse_hotkey("  Ctrl + Alt + S  ").expect("spaced parses"),
            parse_hotkey("Ctrl+Alt+S").expect("compact parses")
        );
    }

    #[test]
    fn parse_hotkey_accepts_digits_function_keys_and_shift() {
        assert_eq!(
            parse_hotkey("Ctrl+Alt+1").expect("digit parses"),
            HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Digit1)
        );
        assert_eq!(
            parse_hotkey("Ctrl+Shift+F5").expect("F5 parses"),
            HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::F5)
        );
    }

    #[test]
    fn parse_hotkey_rejects_empty_missing_key_and_missing_modifier() {
        assert!(parse_hotkey("").is_err());
        assert!(parse_hotkey("   ").is_err());
        assert!(parse_hotkey("Ctrl+Alt").is_err());
        assert!(parse_hotkey("S").is_err());
    }

    #[test]
    fn parse_hotkey_rejects_unknown_tokens_duplicates_and_two_keys() {
        assert!(parse_hotkey("Ctrl+Foo").is_err());
        assert!(parse_hotkey("Ctrl+Ctrl+S").is_err());
        assert!(parse_hotkey("Ctrl+Alt+S+X").is_err());
        assert!(parse_hotkey("Ctrl++S").is_err());
    }

    #[test]
    fn bindings_from_default_config_map_all_four_ids() {
        let bindings =
            HotkeyBindings::from_config(&HotkeyConfig::default()).expect("defaults parse");
        assert_eq!(bindings.len(), 4);
        for (combo, action) in [
            ("Ctrl+Alt+S", HotkeyAction::ToggleStartStop),
            ("Ctrl+Alt+R", HotkeyAction::SelectRegion),
            ("Ctrl+Alt+H", HotkeyAction::ToggleOverlay),
            ("Ctrl+Alt+M", HotkeyAction::ToggleMute),
        ] {
            let id = parse_hotkey(combo).expect("combo parses").id();
            assert_eq!(bindings.action_for_id(id), Some(action), "{combo}");
        }
        assert_eq!(bindings.action_for_id(0), None);
    }

    #[test]
    fn bindings_skip_empty_combos_without_error() {
        let config = HotkeyConfig {
            toggle_mute: String::new(),
            ..HotkeyConfig::default()
        };
        let bindings = HotkeyBindings::from_config(&config).expect("empty skips");
        assert_eq!(bindings.len(), 3);
        assert_eq!(
            bindings.action_for_id(parse_hotkey("Ctrl+Alt+M").expect("M parses").id()),
            None
        );
    }

    #[test]
    #[ignore = "real-PC: registers OS-global hotkeys; run on Danh's PC only"]
    fn hotkey_manager_registers_all_defaults_on_real_pc() {
        let manager =
            HotkeyManager::register_all(&HotkeyConfig::default()).expect("register on real PC");
        let id = parse_hotkey("Ctrl+Alt+S").expect("S parses").id();
        assert_eq!(
            manager.action_for_id(id),
            Some(HotkeyAction::ToggleStartStop)
        );
        drop(manager);
    }
}

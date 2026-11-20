//! Windows tray icon + 5-item context menu (spec §3.7, `AC-TRAY-*`).

use super::icon::build_tray_icon;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem},
    TrayIcon, TrayIconBuilder,
};

/// Tooltip shown when hovering the tray icon.
pub const TRAY_TOOLTIP: &str = "cap2voice";

/// Stable menu ids, in the same order as [`tray_menu_labels`].
pub const MENU_ID_SHOW: &str = "show-window";
pub const MENU_ID_START: &str = "start-translation";
pub const MENU_ID_STOP: &str = "stop-translation";
pub const MENU_ID_SETTINGS: &str = "open-settings";
pub const MENU_ID_QUIT: &str = "quit";

/// Actions the tray menu (and tray clicks) can request from the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayMenuAction {
    ShowWindow,
    StartTranslation,
    StopTranslation,
    OpenSettings,
    Quit,
}

/// Exact menu labels in display order (spec §3.7 / `AC-TRAY-1`).
pub fn tray_menu_labels() -> [&'static str; 5] {
    ["Hiện cửa sổ", "Bắt đầu dịch", "Dừng", "Cài đặt", "Thoát"]
}

/// Stable menu ids in the same order as [`tray_menu_labels`].
pub fn tray_menu_ids() -> [&'static str; 5] {
    [
        MENU_ID_SHOW,
        MENU_ID_START,
        MENU_ID_STOP,
        MENU_ID_SETTINGS,
        MENU_ID_QUIT,
    ]
}

/// Map a muda menu id string to its action (`None` for unknown ids).
pub fn tray_action_for_menu_id(id: &str) -> Option<TrayMenuAction> {
    match id {
        MENU_ID_SHOW => Some(TrayMenuAction::ShowWindow),
        MENU_ID_START => Some(TrayMenuAction::StartTranslation),
        MENU_ID_STOP => Some(TrayMenuAction::StopTranslation),
        MENU_ID_SETTINGS => Some(TrayMenuAction::OpenSettings),
        MENU_ID_QUIT => Some(TrayMenuAction::Quit),
        _ => None,
    }
}

/// Map a muda menu event to its action (`None` for unknown ids).
pub fn tray_action_for_menu_event(event: &MenuEvent) -> Option<TrayMenuAction> {
    tray_action_for_menu_id(event.id.as_ref())
}

/// Close-request policy: cancel the close and hide to tray only when
/// close-to-tray is enabled and this is not a real quit request.
pub fn should_cancel_close(close_to_tray: bool, quit_requested: bool) -> bool {
    close_to_tray && !quit_requested
}

/// Errors from building the tray icon.
#[derive(Debug, thiserror::Error)]
pub enum TrayError {
    #[error("không tạo được biểu tượng khay hệ thống: {0}")]
    Build(#[from] tray_icon::Error),
    #[error("không tạo được ảnh biểu tượng khay hệ thống: {0}")]
    BadIcon(#[from] tray_icon::BadIcon),
    #[error("không thêm được mục vào menu khay hệ thống: {0}")]
    Menu(#[from] muda::Error),
}

/// Owns the Windows notification-area icon and its 5-item menu.
///
/// `Bắt đầu dịch` starts enabled and `Dừng` starts disabled (idle state);
/// [`TrayIconHandle::set_running`] flips them from the live pipeline state.
///
/// Dropping this handle drops the inner [`TrayIcon`], which removes the
/// icon from the notification area, so no ghost icon is left behind after
/// a clean exit.
pub struct TrayIconHandle {
    tray: TrayIcon,
    start_item: MenuItem,
    stop_item: MenuItem,
}

impl TrayIconHandle {
    /// Create the tray icon with tooltip `cap2voice` and the 5-item menu.
    /// Left click does NOT open the menu (it restores the overlay instead).
    pub fn new() -> Result<Self, TrayError> {
        let labels = tray_menu_labels();
        let ids = tray_menu_ids();
        let show_item = MenuItem::with_id(ids[0], labels[0], true, None);
        let start_item = MenuItem::with_id(ids[1], labels[1], true, None);
        let stop_item = MenuItem::with_id(ids[2], labels[2], false, None);
        let settings_item = MenuItem::with_id(ids[3], labels[3], true, None);
        let quit_item = MenuItem::with_id(ids[4], labels[4], true, None);

        let menu = Menu::new();
        menu.append(&show_item)?;
        menu.append(&start_item)?;
        menu.append(&stop_item)?;
        menu.append(&settings_item)?;
        menu.append(&quit_item)?;

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(TRAY_TOOLTIP)
            .with_icon(build_tray_icon()?)
            .with_menu_on_left_click(false)
            .build()?;
        Ok(Self {
            tray,
            start_item,
            stop_item,
        })
    }

    /// Enable `Bắt đầu dịch` / disable `Dừng` when idle, and vice versa
    /// while the pipeline is running.
    pub fn set_running(&self, running: bool) {
        self.start_item.set_enabled(!running);
        self.stop_item.set_enabled(running);
    }
}

impl Drop for TrayIconHandle {
    fn drop(&mut self) {
        // The inner `TrayIcon` is dropped here, which removes the icon from
        // the notification area, so no ghost icon is left behind.
        let _ = &self.tray;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_menu_labels_are_the_exact_five_vietnamese_items() {
        assert_eq!(
            tray_menu_labels(),
            ["Hiện cửa sổ", "Bắt đầu dịch", "Dừng", "Cài đặt", "Thoát"]
        );
    }

    #[test]
    fn tray_menu_ids_are_distinct_and_match_label_order() {
        let ids = tray_menu_ids();
        assert_eq!(ids.len(), tray_menu_labels().len());
        let mut sorted = ids.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "menu ids must be distinct");
    }

    #[test]
    fn tray_action_for_menu_id_maps_all_five_items() {
        assert_eq!(
            tray_action_for_menu_id(MENU_ID_SHOW),
            Some(TrayMenuAction::ShowWindow)
        );
        assert_eq!(
            tray_action_for_menu_id(MENU_ID_START),
            Some(TrayMenuAction::StartTranslation)
        );
        assert_eq!(
            tray_action_for_menu_id(MENU_ID_STOP),
            Some(TrayMenuAction::StopTranslation)
        );
        assert_eq!(
            tray_action_for_menu_id(MENU_ID_SETTINGS),
            Some(TrayMenuAction::OpenSettings)
        );
        assert_eq!(
            tray_action_for_menu_id(MENU_ID_QUIT),
            Some(TrayMenuAction::Quit)
        );
    }

    #[test]
    fn tray_action_for_menu_id_rejects_unknown_ids() {
        assert_eq!(tray_action_for_menu_id(""), None);
        assert_eq!(tray_action_for_menu_id("show"), None);
        assert_eq!(tray_action_for_menu_id("Quit"), None);
    }

    #[test]
    fn should_cancel_close_truth_table() {
        assert!(should_cancel_close(true, false));
        assert!(!should_cancel_close(true, true));
        assert!(!should_cancel_close(false, false));
        assert!(!should_cancel_close(false, true));
    }

    #[test]
    #[ignore = "real-PC: creates a Windows tray icon; run on Danh's PC only"]
    fn tray_icon_builds_set_running_and_drops_cleanly_on_real_pc() {
        let handle = TrayIconHandle::new().expect("tray icon builds on real PC");
        handle.set_running(true);
        handle.set_running(false);
        drop(handle);
    }
}
